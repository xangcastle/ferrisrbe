//! In-memory index over the on-disk artifact store.
//!
//! `moka` owns the disk budget: entries are weighed by their real byte size, so
//! `max_capacity` bounds bytes on the PVC rather than an entry count. Its
//! eviction listener keeps memory and filesystem in step: when an entry leaves
//! the index, its blob and sidecar are unlinked with it, so no unreferenced
//! bytes accumulate on disk and no separate sweeper is needed.
//!
//! Freshness is not delegated to moka's per-entry expiry. Mutable assets need
//! revalidation rather than deletion, so staleness is a timestamp check on
//! read, the same policy `DiskActionCache` applies.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use moka::future::Cache;
use moka::notification::RemovalCause;
use serde::Serialize;
use tracing::{info, warn};

use crate::proxy::classifier::AssetClass;
use crate::proxy::config::ProxyConfig;
use crate::proxy::error::ProxyResult;
use crate::proxy::store::{now_unix, BlobMeta, ProxyStore};

/// Maximum entries folded into sidecars per flush cycle. The rest of the
/// backlog waits for the next tick instead of spiking PVC I/O.
const FLUSH_BATCH: usize = 1000;

/// Outcome of an index lookup.
#[derive(Debug)]
pub enum Lookup {
    /// Entry is present and fresh.
    Fresh(Arc<BlobMeta>),
    /// Entry is absent, or was present but past its TTL.
    Miss,
}

/// One cached artifact as exposed by the externals listing.
#[derive(Debug, Serialize)]
pub struct ProxyEntrySnapshot {
    pub url: String,
    pub size: u64,
    pub sha256_hex: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub fetched_at_unix: u64,
    pub immutable: bool,
    /// Persisted count plus the un-flushed in-memory delta.
    pub downloads: u64,
    /// Ecosystem, package, version and docs page, read from the URL.
    pub artifact: crate::proxy::artifact::ArtifactInfo,
}

/// Index of cached artifacts, bounded by total bytes on disk.
pub struct ProxyIndex {
    entries: Cache<String, Arc<BlobMeta>>,
    store: ProxyStore,
    mutable_ttl_secs: u64,
    disk_bytes: Arc<AtomicU64>,
    ready: AtomicBool,
    /// Serves recorded since the last flush, per cache key. The sidecar's
    /// `downloads` field is the durable base; this holds the hot-path delta so
    /// serving never pays a disk write.
    deltas: DashMap<String, u64>,
}

impl ProxyIndex {
    /// Build the index and wire the eviction listener to the store.
    pub fn new(config: &ProxyConfig, store: ProxyStore) -> Self {
        let disk_bytes = Arc::new(AtomicU64::new(0));

        let listener_store = store.clone();
        let listener_bytes = disk_bytes.clone();
        let mut builder = Cache::builder()
            .weigher(|_key: &String, meta: &Arc<BlobMeta>| blob_weight(meta))
            .async_eviction_listener(move |key: Arc<String>, meta: Arc<BlobMeta>, cause| {
                let store = listener_store.clone();
                let bytes = listener_bytes.clone();
                Box::pin(async move { release_evicted(&store, &bytes, &key, &meta, cause).await })
            });

        if config.max_size_bytes > 0 {
            builder = builder.max_capacity(config.max_size_bytes);
        }

        Self {
            entries: builder.build(),
            store,
            mutable_ttl_secs: config.mutable_ttl.as_secs(),
            disk_bytes,
            ready: AtomicBool::new(false),
            deltas: DashMap::new(),
        }
    }

    /// Rebuild the index from disk, reclaiming anything left half-written, and
    /// mark the index ready.
    ///
    /// The index lives only in memory; this is what makes blobs committed by
    /// an earlier process servable and evictable again. Until it completes,
    /// [`Self::is_ready`] is `false` and lookups miss for artifacts that are
    /// already on disk.
    pub async fn warm_up(&self) -> ProxyResult<()> {
        let report = self.store.sweep().await?;
        let recovered = report.entries.len();

        for entry in report.entries {
            self.entries.insert(entry.key, Arc::new(entry.meta)).await;
        }
        self.entries.run_pending_tasks().await;
        self.disk_bytes.store(report.total_bytes, Ordering::Relaxed);
        self.ready.store(true, Ordering::Release);

        info!(
            recovered,
            bytes = report.total_bytes,
            torn_blobs = report.removed_torn_blobs,
            orphan_sidecars = report.removed_orphan_meta,
            temp_files = report.removed_temp_files,
            "proxy cache index rebuilt from disk"
        );
        Ok(())
    }

    /// Whether the startup sweep has completed.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Look up an entry, treating stale mutable assets as a miss.
    ///
    /// A stale entry is invalidated on the spot, which unlinks its files, so
    /// the caller's re-fetch replaces it rather than shadowing it.
    pub async fn get(&self, key: &str) -> Lookup {
        let Some(meta) = self.entries.get(key).await else {
            return Lookup::Miss;
        };
        if self.is_stale(&meta) {
            self.entries.invalidate(key).await;
            return Lookup::Miss;
        }
        Lookup::Fresh(meta)
    }

    /// Record a freshly committed artifact.
    pub async fn insert(&self, key: String, meta: Arc<BlobMeta>) {
        self.disk_bytes.fetch_add(meta.size, Ordering::Relaxed);
        self.entries.insert(key, meta).await;
    }

    /// Drop an entry and its files.
    pub async fn invalidate(&self, key: &str) {
        self.entries.invalidate(key).await;
    }

    /// Number of indexed artifacts.
    pub async fn entry_count(&self) -> u64 {
        self.entries.run_pending_tasks().await;
        self.entries.entry_count()
    }

    /// Count one serve of `key` (cache hit, cold fetch or Remote Asset
    /// resolve). RAM only; the flush task makes it durable.
    pub fn record_download(&self, key: &str) {
        *self.deltas.entry(key.to_string()).or_insert(0) += 1;
    }

    /// Durable base plus pending delta for one entry.
    fn downloads_of(&self, key: &str, meta: &BlobMeta) -> u64 {
        meta.downloads + self.deltas.get(key).map(|d| *d).unwrap_or(0)
    }

    /// Fold pending deltas into the sidecars so counts survive a restart.
    ///
    /// One sidecar rewrite per dirty entry; a cycle processes at most
    /// [`FLUSH_BATCH`] keys and leaves the rest of the backlog for the next
    /// tick, so a fetch storm can never turn the flush into unbounded I/O
    /// competing with the CAS. Entries evicted since their last flush drop
    /// the un-flushed delta together with the artifact itself.
    ///
    /// A key's delta is reset to zero rather than removed before its sidecar
    /// is rewritten, so a serve that races the flush is counted against the
    /// new base instead of being lost; a failed rewrite restores the delta.
    /// After a successful rewrite the indexed metadata is replaced with the
    /// persisted copy, keeping snapshots and disk in agreement on the base.
    pub async fn flush_downloads(&self) {
        let mut keys: Vec<String> = self
            .deltas
            .iter()
            .filter(|e| *e.value() > 0)
            .map(|e| e.key().clone())
            .collect();
        if keys.len() > FLUSH_BATCH {
            tracing::debug!(
                backlog = keys.len() - FLUSH_BATCH,
                "download-count flush backlog, deferring to next tick"
            );
            keys.truncate(FLUSH_BATCH);
        }

        for key in keys {
            let Some(delta) = self.deltas.get(key.as_str()).map(|d| *d) else {
                continue;
            };
            if delta == 0 {
                continue;
            }
            self.deltas.insert(key.clone(), 0);

            let Lookup::Fresh(meta) = self.get(&key).await else {
                self.deltas.remove(key.as_str());
                continue;
            };
            let updated = Arc::new(BlobMeta {
                downloads: meta.downloads + delta,
                ..(*meta).clone()
            });
            if let Err(e) = self.store.rewrite_meta(&key, &updated).await {
                warn!(key = %key, error = %e, "failed to flush download count");
                if let Some(mut pending) = self.deltas.get_mut(key.as_str()) {
                    *pending += delta;
                }
            } else {
                self.entries.insert(key, updated).await;
            }
        }
    }

    /// Snapshot of every indexed artifact for the externals listing.
    pub async fn snapshot(&self) -> Vec<ProxyEntrySnapshot> {
        self.entries.run_pending_tasks().await;
        let mut out: Vec<ProxyEntrySnapshot> = self
            .entries
            .iter()
            .map(|(key, meta)| ProxyEntrySnapshot {
                url: meta.url.clone(),
                size: meta.size,
                sha256_hex: meta.sha256_hex.clone(),
                content_type: meta.content_type.clone(),
                etag: meta.etag.clone(),
                fetched_at_unix: meta.fetched_at_unix,
                immutable: meta.immutable,
                downloads: self.downloads_of(key.as_str(), &meta),
                artifact: crate::proxy::artifact::classify(&meta.url),
            })
            .collect();
        out.sort_by(|a, b| b.downloads.cmp(&a.downloads).then(a.url.cmp(&b.url)));
        out
    }

    /// Bytes currently held on disk, as accounted by the index.
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes.load(Ordering::Relaxed)
    }

    /// Whether a cached entry has outlived its retention policy.
    fn is_stale(&self, meta: &BlobMeta) -> bool {
        if meta.immutable {
            return false;
        }
        let age = now_unix().saturating_sub(meta.fetched_at_unix);
        age > self.mutable_ttl_secs
    }
}

impl std::fmt::Debug for ProxyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyIndex")
            .field("mutable_ttl_secs", &self.mutable_ttl_secs)
            .field("disk_bytes", &self.disk_bytes())
            .field("ready", &self.is_ready())
            .finish()
    }
}

/// Cache weight of a blob: its size in bytes, saturating at `u32::MAX`
/// because moka weighs in `u32`. A blob of 4 GiB or more pins the maximum
/// weight instead of wrapping around to a small one.
fn blob_weight(meta: &BlobMeta) -> u32 {
    meta.size.min(u32::MAX as u64) as u32
}

/// Eviction listener: unlink the blob and sidecar of an entry that left the
/// index and release its bytes from the accounting.
///
/// A [`RemovalCause::Replaced`] entry is left alone: replacement only swaps
/// the metadata, and the same blob and sidecar keep serving.
async fn release_evicted(
    store: &ProxyStore,
    disk_bytes: &AtomicU64,
    key: &str,
    meta: &BlobMeta,
    cause: RemovalCause,
) {
    if matches!(cause, RemovalCause::Replaced) {
        return;
    }
    disk_bytes.fetch_sub(
        meta.size.min(disk_bytes.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
    store.remove(key).await;
    if matches!(cause, RemovalCause::Size) {
        tracing::debug!(%key, size = meta.size, "evicted under disk pressure");
    }
}

/// Build the sidecar for a freshly fetched artifact.
pub fn build_meta(
    url: &str,
    class: AssetClass,
    sha256_hex: String,
    size: u64,
    content_type: Option<String>,
    etag: Option<String>,
) -> BlobMeta {
    if size == 0 && class == AssetClass::Immutable {
        warn!(url, "caching a zero-length immutable artifact");
    }
    BlobMeta {
        url: url.to_string(),
        size,
        sha256_hex,
        content_type,
        etag,
        fetched_at_unix: now_unix(),
        immutable: class == AssetClass::Immutable,
        downloads: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::store::FinishedBlob;
    use std::time::Duration;
    use tempfile::TempDir;

    async fn index_with(ttl_secs: u64, max_bytes: u64) -> (TempDir, ProxyIndex) {
        let dir = TempDir::new().unwrap();
        let config = ProxyConfig {
            dir: dir.path().to_path_buf(),
            mutable_ttl: Duration::from_secs(ttl_secs),
            max_size_bytes: max_bytes,
            ..Default::default()
        };
        let store = ProxyStore::new(&config).await.unwrap();
        (dir, ProxyIndex::new(&config, store))
    }

    fn meta(url: &str, size: u64, immutable: bool, fetched_at_unix: u64) -> Arc<BlobMeta> {
        Arc::new(BlobMeta {
            url: url.to_string(),
            size,
            sha256_hex: "00".repeat(32),
            content_type: None,
            etag: None,
            fetched_at_unix,
            immutable,
            downloads: 0,
        })
    }

    async fn put_blob(store: &ProxyStore, key: &str, body: &[u8], m: &BlobMeta) {
        let mut w = store.begin_write().await.unwrap();
        w.write_chunk(body).await.unwrap();
        let finished: FinishedBlob = w.finish().await.unwrap();
        store.commit(key, finished, m).await.unwrap();
    }

    #[tokio::test]
    async fn immutable_entries_never_go_stale() {
        let (_d, index) = index_with(1, 0).await;
        index
            .insert("k".into(), meta("https://x/a.whl", 10, true, 0))
            .await;
        assert!(matches!(index.get("k").await, Lookup::Fresh(_)));
    }

    #[tokio::test]
    async fn mutable_entries_expire_after_their_ttl() {
        let (_d, index) = index_with(60, 0).await;
        let stale_at = now_unix().saturating_sub(120);
        index
            .insert(
                "k".into(),
                meta("https://x/index.json", 10, false, stale_at),
            )
            .await;
        assert!(matches!(index.get("k").await, Lookup::Miss));
    }

    #[tokio::test]
    async fn fresh_mutable_entries_are_served() {
        let (_d, index) = index_with(600, 0).await;
        index
            .insert(
                "k".into(),
                meta("https://x/index.json", 10, false, now_unix()),
            )
            .await;
        assert!(matches!(index.get("k").await, Lookup::Fresh(_)));
    }

    #[tokio::test]
    async fn expiring_an_entry_unlinks_its_files() {
        let (_d, index) = index_with(60, 0).await;
        let key = ProxyStore::cache_key("https://x/index.json");
        let m = meta("https://x/index.json", 4, false, now_unix() - 120);
        put_blob(&index.store, &key, b"aaaa", &m).await;
        index.insert(key.clone(), m).await;

        assert!(matches!(index.get(&key).await, Lookup::Miss));
        index.entries.run_pending_tasks().await;
        assert!(index.store.open_blob(&key).await.unwrap().is_none());
        assert!(index.store.read_meta(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_third_blob_over_a_two_blob_budget_evicts_and_unlinks() {
        let (_d, index) = index_with(600, 250).await;
        let mut keys = Vec::new();
        for i in 0..3 {
            let url = format!("https://x/pkg-{i}.whl");
            let key = ProxyStore::cache_key(&url);
            let m = meta(&url, 100, true, now_unix());
            put_blob(&index.store, &key, &[b'x'; 100], &m).await;
            index.insert(key.clone(), m).await;
            keys.push(key);
        }
        index.entries.run_pending_tasks().await;

        assert!(
            index.entry_count().await < 3,
            "expected at least one eviction under a 250-byte budget"
        );
        let surviving = futures::future::join_all(
            keys.iter()
                .map(|k| async { index.store.open_blob(k).await.unwrap().is_some() }),
        )
        .await
        .into_iter()
        .filter(|present| *present)
        .count();
        assert!(surviving < 3, "evicted blobs must be unlinked from disk");
    }

    #[tokio::test]
    async fn warm_up_rebuilds_the_index_and_flips_readiness() {
        let (dir, index) = index_with(600, 0).await;
        let key = ProxyStore::cache_key("https://x/a.whl");
        let m = meta("https://x/a.whl", 4, true, now_unix());
        put_blob(&index.store, &key, b"aaaa", &m).await;
        drop(index);

        let config_of_next_process = ProxyConfig {
            dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let store = ProxyStore::new(&config_of_next_process).await.unwrap();
        let reborn = ProxyIndex::new(&config_of_next_process, store);
        assert!(!reborn.is_ready());

        reborn.warm_up().await.unwrap();

        assert!(reborn.is_ready());
        assert_eq!(reborn.entry_count().await, 1);
        assert_eq!(reborn.disk_bytes(), 4);
        assert!(matches!(reborn.get(&key).await, Lookup::Fresh(_)));
    }

    #[test]
    fn build_meta_records_the_classification() {
        let immutable = build_meta(
            "https://x/a.whl",
            AssetClass::Immutable,
            "ab".into(),
            5,
            None,
            None,
        );
        assert!(immutable.immutable);
        let mutable = build_meta(
            "https://x/i.json",
            AssetClass::Mutable,
            "ab".into(),
            5,
            None,
            None,
        );
        assert!(!mutable.immutable);
    }

    #[tokio::test]
    async fn downloads_are_counted_and_reported_by_snapshot() {
        let (_d, index) = index_with(600, 0).await;
        let url = "https://x/pkg.whl";
        let key = ProxyStore::cache_key(url);
        index
            .insert(key.clone(), meta(url, 10, true, now_unix()))
            .await;

        for _ in 0..3 {
            index.record_download(&key);
        }

        let snapshot = index.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].downloads, 3);
        assert_eq!(snapshot[0].url, url);
    }

    #[tokio::test]
    async fn flush_folds_deltas_into_the_sidecar() {
        let (_d, index) = index_with(600, 0).await;
        let url = "https://x/pkg.whl";
        let key = ProxyStore::cache_key(url);
        let m = meta(url, 10, true, now_unix());
        put_blob(&index.store, &key, b"0123456789", &m).await;
        index.insert(key.clone(), m).await;
        index.record_download(&key);
        index.record_download(&key);

        index.flush_downloads().await;

        let persisted = index.store.read_meta(&key).await.unwrap().unwrap();
        assert_eq!(persisted.downloads, 2);
        let snapshot = index.snapshot().await;
        assert_eq!(snapshot[0].downloads, 2, "delta drained, base persisted");

        let config = ProxyConfig {
            dir: _d.path().to_path_buf(),
            ..Default::default()
        };
        let reborn_store = ProxyStore::new(&config).await.unwrap();
        let reborn = ProxyIndex::new(&config, reborn_store);
        reborn.warm_up().await.unwrap();
        let snapshot = reborn.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[0].downloads, 2,
            "a fresh index over the same store reads the persisted count"
        );
    }

    #[tokio::test]
    async fn flush_skips_evicted_entries_without_panicking() {
        let (_d, index) = index_with(600, 0).await;
        index.record_download("no-such-key");

        index.flush_downloads().await;

        assert_eq!(index.snapshot().await.len(), 0);
    }
}
