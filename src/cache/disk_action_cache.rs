//! Disk-backed, two-tier Action Cache.
//!
//! Results are persisted as protobuf-encoded `ActionResult` files in the
//! [sharded layout](crate::cache::sharded_layout) under the cache root, so
//! the cache survives restarts; an in-memory [`L1ActionCache`] in front of
//! the disk answers repeated lookups without I/O.
//!
//! An entry expires `ttl` after its file was last written. The disk tier is
//! bounded by total bytes and evicts least recently used entries first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use parking_lot::Mutex;
use prost::Message;
use tokio::fs;
use tracing::{debug, error, info, warn};

use crate::cache::action_cache::{
    ActionCacheGetResult, ActionCacheHitTier, ActionCacheStore, CacheActionResult, L1ActionCache,
};
use crate::cache::lru::LruState;
use crate::cache::sharded_layout::{self, STAGING_DIR};
use crate::proto::build::bazel::remote::execution::v2::ActionResult as ProtoActionResult;
use crate::types::DigestInfo;

/// Two-tier Action Cache: in-memory L1 over durable files on disk.
///
/// Size accounting starts at zero and is completed by [`Self::warm_up`];
/// until then only entries written by this process count toward the limit.
pub struct DiskActionCache {
    root: PathBuf,
    staging_dir: PathBuf,
    /// Disk budget in bytes; `0` disables eviction.
    max_size_bytes: u64,
    current_size_bytes: AtomicU64,
    /// Lifetime of an entry, measured from its file's mtime.
    default_ttl: Duration,
    l1: Arc<L1ActionCache>,
    lru: Mutex<LruState>,
    /// Set once [`Self::warm_up`] has accounted every entry found on disk.
    ready: AtomicBool,
}

impl DiskActionCache {
    /// Opens (creating if needed) a cache at `root` with a disk budget of
    /// `max_size_gb` GiB. See [`Self::with_max_size_bytes`].
    pub async fn new(
        root: impl AsRef<Path>,
        max_size_gb: u64,
        ttl: Duration,
        l1_capacity: usize,
        l1_ttl: Duration,
    ) -> crate::types::Result<Self> {
        Self::with_max_size_bytes(
            root,
            max_size_gb * 1024 * 1024 * 1024,
            ttl,
            l1_capacity,
            l1_ttl,
        )
        .await
    }

    /// Opens (creating if needed) a cache at `root`. Entries on disk stay
    /// readable immediately; their sizes are accounted by [`Self::warm_up`].
    pub async fn with_max_size_bytes(
        root: impl AsRef<Path>,
        max_size_bytes: u64,
        ttl: Duration,
        l1_capacity: usize,
        l1_ttl: Duration,
    ) -> crate::types::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let staging_dir = root.join(STAGING_DIR);

        fs::create_dir_all(&root).await?;
        fs::create_dir_all(&staging_dir).await?;

        info!(
            "Opened DiskActionCache at {:?}: max={} bytes; accounting fills in with warm_up",
            root, max_size_bytes
        );

        Ok(Self {
            root,
            staging_dir,
            max_size_bytes,
            current_size_bytes: AtomicU64::new(0),
            default_ttl: ttl,
            l1: Arc::new(L1ActionCache::new(l1_capacity, l1_ttl)),
            lru: Mutex::new(LruState::new()),
            ready: AtomicBool::new(false),
        })
    }

    /// Accounts every entry already on disk in the recency order and the
    /// size total, ranked by mtime below entries touched by this process.
    ///
    /// Safe to run while the cache serves traffic: an entry written during
    /// the scan is accounted by its write and skipped here. Idempotent.
    pub async fn warm_up(&self) -> crate::types::Result<()> {
        let started = Instant::now();
        sharded_layout::sweep_staging_logged(&self.staging_dir).await;
        self.lru.lock().floor_counter(sharded_layout::now_millis());
        let scanned = sharded_layout::scan(self.root.clone()).await?;
        let restored = sharded_layout::restore_recency(
            &self.lru,
            scanned
                .iter()
                .map(|entry| (entry.digest, entry.file_size, entry.mtime_millis)),
        )
        .await;
        self.current_size_bytes
            .fetch_add(restored.bytes, Ordering::Relaxed);
        self.ready.store(true, Ordering::Release);
        info!(
            "DiskActionCache warm-up at {:?}: entries={} size={} bytes in {:?}",
            self.root,
            self.lru.lock().len(),
            self.current_size_bytes.load(Ordering::Relaxed),
            started.elapsed()
        );
        Ok(())
    }

    /// Whether [`Self::warm_up`] has completed.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn entry_path(&self, digest: &DigestInfo) -> PathBuf {
        sharded_layout::entry_path(&self.root, digest)
    }

    /// The entry for `digest` from disk, if present and within its TTL.
    ///
    /// Expired and undecodable files are deleted and reported as a miss.
    async fn read_disk(
        &self,
        digest: &DigestInfo,
    ) -> crate::types::Result<Option<ProtoActionResult>> {
        let path = self.entry_path(digest);
        let metadata = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let age = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .elapsed()
            .unwrap_or_default();
        if age > self.default_ttl {
            debug!("Action cache entry expired for digest {:?}", digest);
            self.discard(digest, &path, metadata.len()).await;
            return Ok(None);
        }

        let data = fs::read(&path).await?;
        match ProtoActionResult::decode(&data[..]) {
            Ok(proto) => {
                self.lru.lock().touch(*digest, data.len() as u64);
                Ok(Some(proto))
            }
            Err(e) => {
                error!("Failed to decode action cache entry {:?}: {}", path, e);
                self.discard(digest, &path, data.len() as u64).await;
                Ok(None)
            }
        }
    }

    /// Deletes the file of an entry that must not be served again.
    async fn discard(&self, digest: &DigestInfo, path: &Path, size: u64) {
        if fs::remove_file(path).await.is_ok() {
            self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
        }
        self.lru.lock().remove(*digest);
    }

    /// The cached result for `digest` and the tier that answered: L1 first,
    /// then disk, promoting a disk hit into L1.
    pub async fn get_with_tier(&self, digest: &DigestInfo) -> ActionCacheGetResult {
        if let Some(result) = self.l1.get(digest) {
            return ActionCacheGetResult {
                result: Some(result),
                tier: ActionCacheHitTier::L1,
            };
        }

        match self.read_disk(digest).await {
            Ok(Some(proto)) => {
                let result = CacheActionResult::new(*digest, proto);
                self.l1.put(*digest, result.clone());
                ActionCacheGetResult {
                    result: Some(result),
                    tier: ActionCacheHitTier::L2,
                }
            }
            Ok(None) => ActionCacheGetResult {
                result: None,
                tier: ActionCacheHitTier::Miss,
            },
            Err(e) => {
                warn!("Failed to read action cache entry for {:?}: {}", digest, e);
                ActionCacheGetResult {
                    result: None,
                    tier: ActionCacheHitTier::Miss,
                }
            }
        }
    }

    /// The cached result for `digest`, from either tier.
    pub async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        self.get_with_tier(digest).await.result
    }

    /// Removes `digest` from both tiers.
    pub async fn remove(&self, digest: &DigestInfo) {
        self.l1.invalidate(digest);
        let removed = self.lru.lock().remove(*digest);
        if let Some(size) = removed {
            self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
        }
        let path = self.entry_path(digest);
        match fs::remove_file(&path).await {
            Ok(()) => info!("Removed action cache entry {:?}", digest),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("Failed to remove action cache entry {:?}: {}", digest, e),
        }
    }

    /// Stores `result` for `digest`.
    ///
    /// L1 always takes the new value. On disk the first write of a digest
    /// wins: an existing file is kept and only marked recently used, since
    /// every writer of an action digest produces an equivalent result. The
    /// file is published with a hard link, which, unlike a rename, fails when
    /// a concurrent writer of the digest published first, so only one of them
    /// counts its bytes. A
    /// result larger than the whole disk budget is kept in L1 only. Disk
    /// failures are logged, never returned: a cache write must not fail the
    /// build that produced it.
    pub async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        self.l1.put(digest, result.clone());

        let path = self.entry_path(&digest);
        match fs::metadata(&path).await {
            Ok(metadata) => {
                self.lru.lock().touch(digest, metadata.len());
                return;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }

        let data = result.proto.encode_to_vec();
        let size = data.len() as u64;
        if self.max_size_bytes > 0 && size > self.max_size_bytes {
            warn!(
                "Action result for {:?} ({} bytes) exceeds max cache size, skipping",
                digest, size
            );
            return;
        }

        let staged = sharded_layout::StagingFile::new(&self.staging_dir);
        if let Err(e) = sharded_layout::create_parent_dirs(&path).await {
            error!("Failed to create parent dirs for {:?}: {}", path, e);
            return;
        }

        if let Err(e) = fs::write(staged.path(), data).await {
            error!(
                "Failed to write action cache temp file {:?}: {}",
                staged.path(),
                e
            );
            return;
        }

        let published = fs::hard_link(staged.path(), &path).await;
        fs::remove_file(staged.path()).await.ok();
        staged.disarm();
        match published {
            Ok(()) => {
                self.current_size_bytes.fetch_add(size, Ordering::Relaxed);
                self.lru.lock().touch(digest, size);
                self.evict_if_needed().await;
                info!("Persisted action cache entry {:?} ({} bytes)", digest, size);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stored = fs::metadata(&path).await.map_or(size, |m| m.len());
                self.lru.lock().touch(digest, stored);
            }
            Err(e) => error!("Failed to publish action cache file: {}", e),
        }
    }

    async fn evict_if_needed(&self) {
        if self.max_size_bytes == 0 {
            return;
        }
        let current = self.current_size_bytes.load(Ordering::Relaxed);
        if current <= self.max_size_bytes {
            return;
        }
        let victims = self.lru.lock().evict(current - self.max_size_bytes);
        for (digest, size) in victims {
            let path = self.entry_path(&digest);
            if fs::remove_file(&path).await.is_ok() {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                self.l1.invalidate(&digest);
                debug!("Evicted action cache entry {:?} ({} bytes)", digest, size);
            }
        }
    }

    /// Bytes on disk, as accounted so far.
    pub fn size_bytes(&self) -> u64 {
        self.current_size_bytes.load(Ordering::Relaxed)
    }

    /// Disk budget in bytes; `0` means unbounded.
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// Number of entries held in the L1 tier.
    pub fn len(&self) -> usize {
        self.l1.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl ActionCacheStore for DiskActionCache {
    async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        DiskActionCache::get(self, digest).await
    }

    async fn get_with_tier(&self, digest: &DigestInfo) -> ActionCacheGetResult {
        DiskActionCache::get_with_tier(self, digest).await
    }

    async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        DiskActionCache::put(self, digest, result).await;
    }

    async fn remove(&self, digest: &DigestInfo) {
        DiskActionCache::remove(self, digest).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const MINUTE: Duration = Duration::from_secs(60);

    fn sample_result(digest: DigestInfo, exit_code: i32) -> CacheActionResult {
        CacheActionResult::new(
            digest,
            ProtoActionResult {
                exit_code,
                ..Default::default()
            },
        )
    }

    async fn open(root: &Path) -> DiskActionCache {
        DiskActionCache::new(root, 1, MINUTE, 100, MINUTE)
            .await
            .unwrap()
    }

    fn walkdir_size(dir: &Path) -> u64 {
        let mut total = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_str().unwrap_or("").starts_with('.') {
                continue;
            }
            let path = entry.path();
            total += if path.is_dir() {
                walkdir_size(&path)
            } else {
                entry.metadata().unwrap().len()
            };
        }
        total
    }

    #[tokio::test]
    async fn warm_up_folds_disk_into_accounting_without_double_counting() {
        let temp = TempDir::new().unwrap();
        let d1 = DigestInfo::new(&"1".repeat(64), 1).unwrap();
        let d2 = DigestInfo::new(&"2".repeat(64), 2).unwrap();
        let live = DigestInfo::new(&"3".repeat(64), 3).unwrap();
        {
            let cache = open(temp.path()).await;
            cache.put(d1, sample_result(d1, 0)).await;
            cache.put(d2, sample_result(d2, 0)).await;
        }
        let on_disk = walkdir_size(temp.path());

        let cache = open(temp.path()).await;
        assert!(!cache.is_ready());
        assert_eq!(cache.size_bytes(), 0);
        cache.put(live, sample_result(live, 7)).await;
        let live_size = cache.size_bytes();
        assert!(live_size > 0);

        cache.warm_up().await.unwrap();
        assert!(cache.is_ready());
        assert_eq!(cache.size_bytes(), on_disk + live_size);
        assert_eq!(cache.lru.lock().len(), 3);

        cache.warm_up().await.unwrap();
        assert_eq!(cache.size_bytes(), on_disk + live_size);
    }

    #[tokio::test]
    async fn entries_survive_reopening_the_cache() {
        let temp = TempDir::new().unwrap();
        let digest = DigestInfo::from_bytes(b"action-key");

        let cache = open(temp.path()).await;
        cache.put(digest, sample_result(digest, 42)).await;
        assert_eq!(cache.get(&digest).await.unwrap().proto.exit_code, 42);

        let reopened = open(temp.path()).await;
        let from_disk = reopened.get_with_tier(&digest).await;
        assert_eq!(from_disk.tier, ActionCacheHitTier::L2);
        assert_eq!(from_disk.result.unwrap().proto.exit_code, 42);
        assert_eq!(
            reopened.get_with_tier(&digest).await.tier,
            ActionCacheHitTier::L1
        );
    }

    #[tokio::test]
    async fn unknown_digest_is_a_miss() {
        let temp = TempDir::new().unwrap();
        let cache = open(temp.path()).await;
        let lookup = cache
            .get_with_tier(&DigestInfo::from_bytes(b"missing"))
            .await;
        assert!(lookup.result.is_none());
        assert_eq!(lookup.tier, ActionCacheHitTier::Miss);
    }

    #[tokio::test]
    async fn a_two_byte_budget_keeps_only_the_newest_entry() {
        let temp = TempDir::new().unwrap();
        let cache = DiskActionCache::with_max_size_bytes(temp.path(), 2, MINUTE, 100, MINUTE)
            .await
            .unwrap();

        let d1 = DigestInfo::from_bytes(b"first");
        cache.put(d1, sample_result(d1, 1)).await;
        let d2 = DigestInfo::from_bytes(b"second");
        cache.put(d2, sample_result(d2, 2)).await;

        assert!(
            cache.get(&d1).await.is_none(),
            "oldest entry must be evicted"
        );
        assert_eq!(cache.get(&d2).await.unwrap().proto.exit_code, 2);
    }

    #[tokio::test]
    async fn concurrent_puts_of_one_digest_stay_readable() {
        let temp = TempDir::new().unwrap();
        let cache = Arc::new(open(temp.path()).await);
        let digest = DigestInfo::from_bytes(b"concurrent-action");
        let result = sample_result(digest, 7);

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cache = cache.clone();
                let result = result.clone();
                tokio::spawn(async move { cache.put(digest, result).await })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(cache.get(&digest).await.unwrap().proto.exit_code, 7);
        assert_eq!(
            cache.size_bytes(),
            result.proto.encode_to_vec().len() as u64,
            "ten writers of one entry account for it once"
        );
    }

    #[tokio::test]
    async fn concurrent_puts_of_distinct_digests_are_all_readable() {
        let temp = TempDir::new().unwrap();
        let cache = Arc::new(open(temp.path()).await);
        let digest_of = |i: i32| DigestInfo::from_bytes(format!("action-{}", i).as_bytes());

        let handles: Vec<_> = (0..20)
            .map(|i| {
                let cache = cache.clone();
                tokio::spawn(async move {
                    cache
                        .put(digest_of(i), sample_result(digest_of(i), i))
                        .await
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }

        for i in 0..20 {
            let retrieved = cache.get(&digest_of(i)).await.unwrap();
            assert_eq!(retrieved.proto.exit_code, i);
        }
    }
}
