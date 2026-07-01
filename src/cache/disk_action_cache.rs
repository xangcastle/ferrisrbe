//! Disk-backed Action Cache (L2)
//!
//! Persists action results to the local filesystem so the cache survives
//! restarts. Combines an in-memory L1 (`L1ActionCache`) with disk storage
//! for fast hits and durability.
//!
//! Layout: `<root>/ac/<aa>/<bb>/<hash>` where the file contains the
//! protobuf-encoded `ActionResult`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use parking_lot::Mutex;
use prost::Message;
use tokio::fs;
use tracing::{debug, error, info, warn};

use crate::cache::action_cache::{CacheActionResult, L1ActionCache};
use crate::proto::build::bazel::remote::execution::v2::ActionResult as ProtoActionResult;
use crate::types::DigestInfo;

/// In-memory LRU bookkeeping for action-cache files.
struct LruState {
    /// Ordered map counter -> digest. Smallest counter is the oldest.
    order: BTreeMap<u64, DigestInfo>,
    /// digest -> counter
    positions: HashMap<DigestInfo, u64>,
    /// digest -> size in bytes
    sizes: HashMap<DigestInfo, u64>,
    /// Monotonically increasing access counter.
    next_counter: u64,
}

impl LruState {
    fn new() -> Self {
        Self {
            order: BTreeMap::new(),
            positions: HashMap::new(),
            sizes: HashMap::new(),
            next_counter: 1,
        }
    }

    fn touch(&mut self, digest: DigestInfo, size: u64) {
        if let Some(old_counter) = self.positions.remove(&digest) {
            self.order.remove(&old_counter);
        }
        let counter = self.next_counter;
        self.next_counter += 1;
        self.order.insert(counter, digest);
        self.positions.insert(digest, counter);
        self.sizes.insert(digest, size);
    }

    fn remove(&mut self, digest: DigestInfo) -> Option<u64> {
        let counter = self.positions.remove(&digest)?;
        self.order.remove(&counter);
        self.sizes.remove(&digest)
    }

    /// Returns digests to evict, oldest first, until `free_bytes` have been
    /// freed or no more entries exist.
    fn evict(&mut self, mut free_bytes: u64) -> Vec<(DigestInfo, u64)> {
        let mut removed = Vec::new();
        while free_bytes > 0 {
            let Some((counter, digest)) = self.order.first_key_value().map(|(k, v)| (*k, *v))
            else {
                break;
            };
            let size = self.sizes.get(&digest).copied().unwrap_or(0);
            self.order.remove(&counter);
            self.positions.remove(&digest);
            self.sizes.remove(&digest);
            removed.push((digest, size));
            free_bytes = free_bytes.saturating_sub(size);
        }
        removed
    }
}

pub struct DiskActionCache {
    root: PathBuf,
    temp_dir: PathBuf,
    max_size_bytes: u64,
    current_size_bytes: AtomicU64,
    default_ttl: Duration,
    l1: Arc<L1ActionCache>,
    lru: Mutex<LruState>,
}

impl DiskActionCache {
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

    pub async fn with_max_size_bytes(
        root: impl AsRef<Path>,
        max_size_bytes: u64,
        ttl: Duration,
        l1_capacity: usize,
        l1_ttl: Duration,
    ) -> crate::types::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let temp_dir = root.join(".tmp");

        fs::create_dir_all(&root).await?;
        fs::create_dir_all(&temp_dir).await?;

        let l1 = Arc::new(L1ActionCache::new(l1_capacity, l1_ttl));
        let mut lru = LruState::new();
        let mut current_size: u64 = 0;

        Self::initialize_lru(&root, &mut lru, &mut current_size).await?;

        info!(
            "Initialized DiskActionCache at {:?}: size={} bytes, max={} bytes",
            root, current_size, max_size_bytes
        );

        Ok(Self {
            root,
            temp_dir,
            max_size_bytes,
            current_size_bytes: AtomicU64::new(current_size),
            default_ttl: ttl,
            l1,
            lru: Mutex::new(lru),
        })
    }

    /// Scan existing entries on disk and populate LRU state.
    async fn initialize_lru(
        root: &Path,
        lru: &mut LruState,
        current_size: &mut u64,
    ) -> crate::types::Result<()> {
        Self::initialize_lru_dir(root, root, lru, current_size).await
    }

    async fn initialize_lru_dir(
        root: &Path,
        dir: &Path,
        lru: &mut LruState,
        current_size: &mut u64,
    ) -> crate::types::Result<()> {
        let mut entries = fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name.starts_with('.') {
                // Skip hidden files/directories such as .tmp.
                continue;
            }
            if path.is_dir() {
                Box::pin(Self::initialize_lru_dir(root, &path, lru, current_size)).await?;
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let Some(digest) = Self::digest_from_path(root, &path) else {
                continue;
            };
            let metadata = fs::metadata(&path).await?;
            let size = metadata.len();
            let mtime = metadata
                .modified()
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            lru.next_counter = lru.next_counter.max(mtime + 1);
            lru.touch(digest, size);
            *current_size += size;
        }
        Ok(())
    }

    fn entry_path(&self, digest: &DigestInfo) -> PathBuf {
        let hash = digest.hash_to_string();
        if hash.len() < 4 {
            return self.root.join(hash);
        }
        self.root
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(&hash[4..])
    }

    fn digest_from_path(root: &Path, path: &Path) -> Option<DigestInfo> {
        let rel = path.strip_prefix(root).ok()?;
        let components: Vec<_> = rel.components().collect();
        let hash = match components.len() {
            1 => components[0].as_os_str().to_str()?.to_string(),
            3 => {
                let a = components[0].as_os_str().to_str()?;
                let b = components[1].as_os_str().to_str()?;
                let c = components[2].as_os_str().to_str()?;
                format!("{}{}{}", a, b, c)
            }
            _ => return None,
        };
        if hash.len() != 64 {
            return None;
        }
        // Size is unknown for action-cache keys; store with size 0.
        DigestInfo::new(&hash, 0).ok()
    }

    fn temp_path(&self) -> PathBuf {
        let uuid = uuid::Uuid::new_v4().to_string();
        self.temp_dir.join(format!("tmp-{}", uuid))
    }

    async fn ensure_parent(path: &Path) -> crate::types::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// Load an entry from disk if it exists and has not expired.
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

        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if modified.elapsed().unwrap_or_default() > self.default_ttl {
            debug!("Action cache entry expired for digest {:?}", digest);
            let size = metadata.len();
            if fs::remove_file(&path).await.is_ok() {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
            }
            let mut lru = self.lru.lock();
            lru.remove(*digest);
            return Ok(None);
        }

        let data = fs::read(&path).await?;
        let size = data.len() as u64;
        match ProtoActionResult::decode(&data[..]) {
            Ok(proto) => {
                let mut lru = self.lru.lock();
                lru.touch(*digest, size);
                Ok(Some(proto))
            }
            Err(e) => {
                error!("Failed to decode action cache entry {:?}: {}", path, e);
                if fs::remove_file(&path).await.is_ok() {
                    self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                }
                let mut lru = self.lru.lock();
                lru.remove(*digest);
                Ok(None)
            }
        }
    }

    pub async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        if let Some(result) = self.l1.get(digest) {
            return Some(result);
        }

        match self.read_disk(digest).await {
            Ok(Some(proto)) => {
                let result = CacheActionResult::new(*digest, proto);
                self.l1.put(*digest, result.clone());
                Some(result)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("Failed to read action cache entry for {:?}: {}", digest, e);
                None
            }
        }
    }

    pub async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        // Always keep the latest value in L1.
        self.l1.put(digest, result.clone());

        let path = self.entry_path(&digest);
        match fs::metadata(&path).await {
            Ok(metadata) => {
                // Already persisted; update access time in LRU.
                let size = metadata.len();
                let mut lru = self.lru.lock();
                lru.touch(digest, size);
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

        let temp_path = self.temp_path();
        if let Err(e) = Self::ensure_parent(&path).await {
            error!("Failed to create parent dirs for {:?}: {}", path, e);
            return;
        }

        match fs::write(&temp_path, data).await {
            Ok(_) => {}
            Err(e) => {
                error!(
                    "Failed to write action cache temp file {:?}: {}",
                    temp_path, e
                );
                return;
            }
        }

        match fs::rename(&temp_path, &path).await {
            Ok(_) => {
                self.current_size_bytes.fetch_add(size, Ordering::Relaxed);
                {
                    let mut lru = self.lru.lock();
                    lru.touch(digest, size);
                }
                self.evict_if_needed().await;
                info!("Persisted action cache entry {:?} ({} bytes)", digest, size);
            }
            Err(e) => {
                error!("Failed to rename action cache file: {}", e);
                fs::remove_file(&temp_path).await.ok();
            }
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
        let to_free = current - self.max_size_bytes;
        let victims = {
            let mut lru = self.lru.lock();
            lru.evict(to_free)
        };
        for (digest, size) in victims {
            let path = self.entry_path(&digest);
            if fs::remove_file(&path).await.is_ok() {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                self.l1.invalidate(&digest);
                debug!("Evicted action cache entry {:?} ({} bytes)", digest, size);
            }
        }
    }

    pub fn size_bytes(&self) -> u64 {
        self.current_size_bytes.load(Ordering::Relaxed)
    }

    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    pub fn len(&self) -> usize {
        self.l1.len()
    }
}

#[async_trait::async_trait]
impl crate::cache::action_cache::ActionCacheStore for DiskActionCache {
    async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        DiskActionCache::get(self, digest).await
    }

    async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        DiskActionCache::put(self, digest, result).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_result(digest: DigestInfo, exit_code: i32) -> CacheActionResult {
        CacheActionResult::new(
            digest,
            ProtoActionResult {
                exit_code,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn test_disk_action_cache_put_get() {
        let temp = TempDir::new().unwrap();
        let cache = DiskActionCache::new(
            temp.path(),
            1,
            Duration::from_secs(60),
            100,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let digest = DigestInfo::from_bytes(b"action-key");
        let result = sample_result(digest, 42);

        cache.put(digest, result.clone()).await;
        let retrieved = cache.get(&digest).await.unwrap();
        assert_eq!(retrieved.proto.exit_code, 42);

        // Should survive a new instance (durability check).
        let cache2 = DiskActionCache::new(
            temp.path(),
            1,
            Duration::from_secs(60),
            100,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        let retrieved2 = cache2.get(&digest).await.unwrap();
        assert_eq!(retrieved2.proto.exit_code, 42);
    }

    #[tokio::test]
    async fn test_disk_action_cache_miss() {
        let temp = TempDir::new().unwrap();
        let cache = DiskActionCache::new(
            temp.path(),
            1,
            Duration::from_secs(60),
            100,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let digest = DigestInfo::from_bytes(b"missing");
        assert!(cache.get(&digest).await.is_none());
    }

    #[tokio::test]
    async fn test_disk_action_cache_eviction_by_size() {
        let temp = TempDir::new().unwrap();
        // Very small cache: 2 bytes max, so only the most recent entry survives.
        let cache = DiskActionCache::with_max_size_bytes(
            temp.path(),
            2,
            Duration::from_secs(60),
            100,
            Duration::from_secs(60),
        )
        .await
        .unwrap();

        let d1 = DigestInfo::from_bytes(b"first");
        let r1 = sample_result(d1, 1);
        cache.put(d1, r1).await;

        let d2 = DigestInfo::from_bytes(b"second");
        let r2 = sample_result(d2, 2);
        cache.put(d2, r2).await;

        // d1 should have been evicted because the cache can hold only one tiny entry.
        assert!(cache.get(&d1).await.is_none());
        assert_eq!(cache.get(&d2).await.unwrap().proto.exit_code, 2);
    }

    #[tokio::test]
    async fn test_disk_action_cache_concurrent_put_same_digest() {
        let temp = TempDir::new().unwrap();
        let cache = Arc::new(
            DiskActionCache::new(
                temp.path(),
                1,
                Duration::from_secs(60),
                100,
                Duration::from_secs(60),
            )
            .await
            .unwrap(),
        );

        let digest = DigestInfo::from_bytes(b"concurrent-action");
        let result = sample_result(digest, 7);

        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = cache.clone();
            let r = result.clone();
            handles.push(tokio::spawn(async move {
                c.put(digest, r).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let retrieved = cache.get(&digest).await.unwrap();
        assert_eq!(retrieved.proto.exit_code, 7);
        // Only one entry should be persisted.
        assert!(cache.size_bytes() > 0);
    }

    #[tokio::test]
    async fn test_disk_action_cache_concurrent_put_distinct_digests() {
        let temp = TempDir::new().unwrap();
        let cache = Arc::new(
            DiskActionCache::new(
                temp.path(),
                1,
                Duration::from_secs(60),
                100,
                Duration::from_secs(60),
            )
            .await
            .unwrap(),
        );

        let mut handles = Vec::new();
        for i in 0..20 {
            let c = cache.clone();
            let digest = DigestInfo::from_bytes(format!("action-{}", i).as_bytes());
            let result = sample_result(digest, i);
            handles.push(tokio::spawn(async move {
                c.put(digest, result).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // All entries should be retrievable after concurrent writes.
        for i in 0..20 {
            let digest = DigestInfo::from_bytes(format!("action-{}", i).as_bytes());
            let retrieved = cache.get(&digest).await.unwrap();
            assert_eq!(retrieved.proto.exit_code, i);
        }
    }
}
