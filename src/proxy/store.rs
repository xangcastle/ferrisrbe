//! On-disk storage for cached upstream artifacts.
//!
//! Layout mirrors the CAS `DiskBackend`: two levels of hex sharding keep the
//! directory fanout bounded. Each cached artifact is two files — the blob and a
//! `.meta` sidecar holding everything needed to serve it and to rebuild the
//! in-memory index after a restart.
//!
//! The blob is renamed into place first and the sidecar second, so the sidecar
//! is the commit marker. A blob without a sidecar is a torn write, and the
//! startup sweep reclaims it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::proxy::config::{blob_path, meta_path, ProxyConfig};
use crate::proxy::error::{ProxyError, ProxyResult};

/// Sidecar metadata persisted next to every cached blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobMeta {
    /// Absolute upstream URL this blob was fetched from.
    pub url: String,
    /// Size of the blob in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the blob contents.
    pub sha256_hex: String,
    /// `Content-Type` reported by the origin, if any.
    #[serde(default)]
    pub content_type: Option<String>,
    /// `ETag` reported by the origin, if any.
    #[serde(default)]
    pub etag: Option<String>,
    /// Seconds since the Unix epoch when the fetch completed.
    pub fetched_at_unix: u64,
    /// Whether the asset was classified as immutable.
    pub immutable: bool,
    /// Times this cached artifact was served to a client.
    #[serde(default)]
    pub downloads: u64,
}

/// A blob that finished streaming to disk but has not been committed yet.
#[derive(Debug)]
pub struct FinishedBlob {
    temp_path: PathBuf,
    /// Lowercase hex SHA-256 of the contents.
    pub sha256_hex: String,
    /// Size in bytes.
    pub size: u64,
}

/// How much crash-durability a sidecar write gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarDurability {
    /// fsync before rename: the sidecar is the commit marker and a torn
    /// write must never survive.
    Durable,
    /// Buffered write + rename: cheap, for counter flushes where losing the
    /// last delta on a crash costs nothing.
    BestEffort,
}

/// One committed entry recovered by the startup sweep.
#[derive(Debug)]
pub struct SweptEntry {
    pub key: String,
    pub meta: BlobMeta,
}

/// Outcome of the startup sweep: the live entries and what was reclaimed.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Committed entries whose blob matches its sidecar.
    pub entries: Vec<SweptEntry>,
    /// Total size of `entries`, in bytes.
    pub total_bytes: u64,
    /// Blobs without a sidecar, or whose size disagrees with it.
    pub removed_torn_blobs: u64,
    /// Sidecars whose blob is missing.
    pub removed_orphan_meta: u64,
    /// Leftovers of interrupted writes in the temporary directory.
    pub removed_temp_files: u64,
}

/// Streams bytes to a temporary file while hashing them in one pass.
///
/// A writer dropped before [`Self::finish`] belongs to an abandoned fetch and
/// deletes its temporary file on the spot. That removal is synchronous; it
/// runs only on the error path.
#[derive(Debug)]
pub struct BlobWriter {
    file: Option<File>,
    temp_path: PathBuf,
    hasher: Sha256,
    size: u64,
}

impl BlobWriter {
    /// Append a chunk to the temporary file and fold it into the digest.
    pub async fn write_chunk(&mut self, chunk: &[u8]) -> ProxyResult<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| ProxyError::Storage("writer already finished".into()))?;
        file.write_all(chunk).await?;
        self.hasher.update(chunk);
        self.size += chunk.len() as u64;
        Ok(())
    }

    /// Bytes written so far.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Flush and fsync the temporary file, yielding its digest.
    pub async fn finish(mut self) -> ProxyResult<FinishedBlob> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| ProxyError::Storage("writer already finished".into()))?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let sha256_hex = hex::encode(self.hasher.clone().finalize());
        let size = self.size;
        let temp_path = std::mem::take(&mut self.temp_path);
        Ok(FinishedBlob {
            temp_path,
            sha256_hex,
            size,
        })
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if self.file.is_some() && !self.temp_path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

/// Content-addressed-by-URL store for proxied artifacts.
#[derive(Debug, Clone)]
pub struct ProxyStore {
    blobs_dir: PathBuf,
    temp_dir: PathBuf,
}

impl ProxyStore {
    /// Create the store, ensuring its directories exist.
    pub async fn new(config: &ProxyConfig) -> ProxyResult<Self> {
        let blobs_dir = config.blobs_dir();
        let temp_dir = config.temp_dir();
        fs::create_dir_all(&blobs_dir).await?;
        fs::create_dir_all(&temp_dir).await?;
        Ok(Self {
            blobs_dir,
            temp_dir,
        })
    }

    /// Cache key for a URL: the hex SHA-256 of the absolute URL.
    pub fn cache_key(url: &str) -> String {
        hex::encode(Sha256::digest(url.as_bytes()))
    }

    /// Where the blob for `key` lives.
    pub fn blob_path(&self, key: &str) -> PathBuf {
        blob_path(&self.blobs_dir, key)
    }

    /// Where the sidecar for `key` lives.
    pub fn meta_path(&self, key: &str) -> PathBuf {
        meta_path(&self.blobs_dir, key)
    }

    /// Open a writer for a new blob.
    pub async fn begin_write(&self) -> ProxyResult<BlobWriter> {
        let temp_path = self.temp_dir.join(format!(
            "tmp-{:032x}",
            crate::types::DigestInfo::generate_uuid()
        ));
        let file = File::create(&temp_path).await?;
        Ok(BlobWriter {
            file: Some(file),
            temp_path,
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// Move a finished blob and its sidecar into place.
    ///
    /// The blob lands first; the sidecar is written last, durably, and acts as
    /// the commit marker. If the process dies between the two renames, the
    /// sweep finds a blob with no sidecar and reclaims it. If the sidecar
    /// cannot be written, the blob is removed again, so the pair is never
    /// half-present.
    pub async fn commit(&self, key: &str, blob: FinishedBlob, meta: &BlobMeta) -> ProxyResult<()> {
        let final_path = self.blob_path(key);
        ensure_parent(&final_path).await?;

        if let Err(e) = fs::rename(&blob.temp_path, &final_path).await {
            let _ = fs::remove_file(&blob.temp_path).await;
            return Err(ProxyError::Io(e));
        }

        if let Err(e) = self
            .write_sidecar(key, meta, SidecarDurability::Durable)
            .await
        {
            let _ = fs::remove_file(&final_path).await;
            return Err(e);
        }

        debug!(key, size = blob.size, "committed proxy blob");
        Ok(())
    }

    /// Atomically write the sidecar for `key`: temp file, rename.
    async fn write_sidecar(
        &self,
        key: &str,
        meta: &BlobMeta,
        durability: SidecarDurability,
    ) -> ProxyResult<()> {
        let meta_final = self.meta_path(key);
        ensure_parent(&meta_final).await?;
        let meta_temp = self.temp_dir.join(format!(
            "meta-{:032x}",
            crate::types::DigestInfo::generate_uuid()
        ));
        let encoded = serde_json::to_vec(meta)
            .map_err(|e| ProxyError::Storage(format!("encoding sidecar: {e}")))?;

        let write = async {
            let mut f = File::create(&meta_temp).await?;
            f.write_all(&encoded).await?;
            f.flush().await?;
            if matches!(durability, SidecarDurability::Durable) {
                f.sync_all().await?;
            }
            drop(f);
            fs::rename(&meta_temp, &meta_final).await?;
            Ok::<(), std::io::Error>(())
        };

        if let Err(e) = write.await {
            let _ = fs::remove_file(&meta_temp).await;
            return Err(ProxyError::Io(e));
        }
        Ok(())
    }

    /// Persist an updated sidecar for an entry whose blob is already in place.
    ///
    /// Used by the download-count flush. Only the sidecar is replaced, and the
    /// write skips fsync: it can run thousands of times per minute against a
    /// PVC shared with CAS fsyncs, and a crash costs at most the last counter
    /// delta, or one re-fetch if it tears the sidecar.
    pub async fn rewrite_meta(&self, key: &str, meta: &BlobMeta) -> ProxyResult<()> {
        self.write_sidecar(key, meta, SidecarDurability::BestEffort)
            .await
    }

    /// Read a sidecar, if the entry is present and intact.
    pub async fn read_meta(&self, key: &str) -> ProxyResult<Option<BlobMeta>> {
        match fs::read(self.meta_path(key)).await {
            Ok(bytes) => match serde_json::from_slice::<BlobMeta>(&bytes) {
                Ok(meta) => Ok(Some(meta)),
                Err(e) => {
                    warn!(key, error = %e, "corrupt sidecar, discarding entry");
                    self.remove(key).await;
                    Ok(None)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ProxyError::Io(e)),
        }
    }

    /// Open a cached blob for reading.
    pub async fn open_blob(&self, key: &str) -> ProxyResult<Option<File>> {
        match File::open(self.blob_path(key)).await {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ProxyError::Io(e)),
        }
    }

    /// Delete a cached entry. Best effort: a missing file is not an error.
    pub async fn remove(&self, key: &str) {
        let _ = fs::remove_file(self.blob_path(key)).await;
        let _ = fs::remove_file(self.meta_path(key)).await;
    }

    /// Scan the store, reclaiming incomplete state and returning live entries.
    ///
    /// This is how blobs committed by an earlier process become known to a
    /// new in-memory index. Reclaimed: every file in the temporary directory
    /// (an interrupted write), blobs without a sidecar or whose size
    /// disagrees with it, sidecars without a blob, and entries whose sidecar
    /// does not parse.
    pub async fn sweep(&self) -> ProxyResult<SweepReport> {
        let mut report = SweepReport::default();

        if let Ok(mut entries) = fs::read_dir(&self.temp_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if fs::remove_file(entry.path()).await.is_ok() {
                    report.removed_temp_files += 1;
                }
            }
        }

        self.sweep_dir(self.blobs_dir.clone(), &mut report).await?;
        Ok(report)
    }

    /// Walk the sharded blob tree under `dir`. Sidecars are judged through
    /// their blob; on their own they are only checked for a missing blob.
    async fn sweep_dir(&self, dir: PathBuf, report: &mut SweepReport) -> ProxyResult<()> {
        let mut stack = vec![dir];

        while let Some(current) = stack.pop() {
            let mut entries = match fs::read_dir(&current).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(ProxyError::Io(e)),
            };

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let file_type = entry.file_type().await?;

                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }

                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };

                if name.ends_with(".meta") {
                    let blob = path.with_file_name(name.trim_end_matches(".meta"));
                    if !blob.exists() {
                        let _ = fs::remove_file(&path).await;
                        report.removed_orphan_meta += 1;
                    }
                    continue;
                }

                let key = match key_from_path(&self.blobs_dir, &path) {
                    Some(k) => k,
                    None => continue,
                };

                match self.read_meta(&key).await? {
                    Some(meta) => {
                        let actual = entry.metadata().await?.len();
                        if actual != meta.size {
                            warn!(
                                key,
                                expected = meta.size,
                                actual,
                                "size mismatch against sidecar, discarding entry"
                            );
                            self.remove(&key).await;
                            report.removed_torn_blobs += 1;
                            continue;
                        }
                        report.total_bytes += meta.size;
                        report.entries.push(SweptEntry { key, meta });
                    }
                    None => {
                        let _ = fs::remove_file(&path).await;
                        report.removed_torn_blobs += 1;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Recover the cache key from a sharded blob path. Only a 64-character hex
/// SHA-256 is a cache key; any other file in the tree is foreign to it.
fn key_from_path(blobs_dir: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(blobs_dir).ok()?;
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let key = match parts.as_slice() {
        [a, b, rest] => format!("{a}{b}{rest}"),
        [only] => (*only).to_string(),
        _ => return None,
    };
    if key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()) {
        Some(key)
    } else {
        None
    }
}

/// Current wall-clock time in seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn ensure_parent(path: &Path) -> ProxyResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn store() -> (TempDir, ProxyStore) {
        let dir = TempDir::new().unwrap();
        let config = ProxyConfig {
            dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        let store = ProxyStore::new(&config).await.unwrap();
        (dir, store)
    }

    fn meta_for(url: &str, blob: &FinishedBlob) -> BlobMeta {
        BlobMeta {
            url: url.to_string(),
            size: blob.size,
            sha256_hex: blob.sha256_hex.clone(),
            content_type: Some("application/octet-stream".into()),
            etag: None,
            fetched_at_unix: now_unix(),
            immutable: true,
            downloads: 0,
        }
    }

    async fn put(store: &ProxyStore, url: &str, body: &[u8]) -> String {
        let key = ProxyStore::cache_key(url);
        let mut w = store.begin_write().await.unwrap();
        w.write_chunk(body).await.unwrap();
        let finished = w.finish().await.unwrap();
        let meta = meta_for(url, &finished);
        store.commit(&key, finished, &meta).await.unwrap();
        key
    }

    #[tokio::test]
    async fn round_trips_a_blob_with_its_sidecar() {
        let (_d, store) = store().await;
        let key = put(&store, "https://example.com/a.tar.gz", b"hello world").await;

        let meta = store.read_meta(&key).await.unwrap().unwrap();
        assert_eq!(meta.size, 11);
        assert_eq!(meta.sha256_hex, hex::encode(Sha256::digest(b"hello world")));
        assert!(store.open_blob(&key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn digest_is_computed_across_chunk_boundaries() {
        let (_d, store) = store().await;
        let mut w = store.begin_write().await.unwrap();
        w.write_chunk(b"hello ").await.unwrap();
        w.write_chunk(b"world").await.unwrap();
        let finished = w.finish().await.unwrap();
        assert_eq!(finished.size, 11);
        assert_eq!(
            finished.sha256_hex,
            hex::encode(Sha256::digest(b"hello world"))
        );
    }

    #[tokio::test]
    async fn writer_dropped_before_finish_leaves_no_temp_file() {
        let (dir, store) = store().await;
        {
            let mut w = store.begin_write().await.unwrap();
            w.write_chunk(b"partial").await.unwrap();
        }
        let temp_dir = dir.path().join(".tmp");
        let mut entries = fs::read_dir(&temp_dir).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweeper_recovers_committed_entries() {
        let (_d, store) = store().await;
        put(&store, "https://example.com/a.tar.gz", b"aaaa").await;
        put(&store, "https://example.com/b.whl", b"bbbbbb").await;

        let report = store.sweep().await.unwrap();
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.total_bytes, 10);
        assert_eq!(report.removed_torn_blobs, 0);
    }

    #[tokio::test]
    async fn sweeper_reclaims_blob_without_sidecar() {
        let (_d, store) = store().await;
        let key = put(&store, "https://example.com/a.tar.gz", b"aaaa").await;
        fs::remove_file(store.meta_path(&key)).await.unwrap();

        let report = store.sweep().await.unwrap();
        assert_eq!(report.removed_torn_blobs, 1);
        assert!(report.entries.is_empty());
        assert!(store.open_blob(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweeper_reclaims_sidecar_without_blob() {
        let (_d, store) = store().await;
        let key = put(&store, "https://example.com/a.tar.gz", b"aaaa").await;
        fs::remove_file(store.blob_path(&key)).await.unwrap();

        let report = store.sweep().await.unwrap();
        assert_eq!(report.removed_orphan_meta, 1);
        assert!(report.entries.is_empty());
        assert!(store.read_meta(&key).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sweeper_empties_the_temp_directory() {
        let (dir, store) = store().await;
        fs::write(dir.path().join(".tmp").join("tmp-leftover"), b"junk")
            .await
            .unwrap();

        let report = store.sweep().await.unwrap();
        assert_eq!(report.removed_temp_files, 1);
    }

    #[tokio::test]
    async fn sweeper_discards_entry_whose_size_disagrees_with_sidecar() {
        let (_d, store) = store().await;
        let key = put(&store, "https://example.com/a.tar.gz", b"aaaa").await;
        fs::write(store.blob_path(&key), b"truncated-differently")
            .await
            .unwrap();

        let report = store.sweep().await.unwrap();
        assert_eq!(report.removed_torn_blobs, 1);
        assert!(report.entries.is_empty());
    }

    #[tokio::test]
    async fn corrupt_sidecar_discards_the_entry() {
        let (_d, store) = store().await;
        let key = put(&store, "https://example.com/a.tar.gz", b"aaaa").await;
        fs::write(store.meta_path(&key), b"{not json")
            .await
            .unwrap();

        assert!(store.read_meta(&key).await.unwrap().is_none());
        assert!(store.open_blob(&key).await.unwrap().is_none());
    }

    #[test]
    fn cache_key_is_stable_and_url_specific() {
        let a = ProxyStore::cache_key("https://example.com/a");
        assert_eq!(a, ProxyStore::cache_key("https://example.com/a"));
        assert_ne!(a, ProxyStore::cache_key("https://example.com/b"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn key_survives_a_round_trip_through_the_sharded_path() {
        let blobs = Path::new("/data/proxy/blobs");
        let key = ProxyStore::cache_key("https://example.com/a");
        let path = blob_path(blobs, &key);
        assert_eq!(key_from_path(blobs, &path).as_deref(), Some(key.as_str()));
    }

    #[test]
    fn foreign_files_in_the_blob_tree_are_ignored() {
        let blobs = Path::new("/data/proxy/blobs");
        assert!(key_from_path(blobs, &blobs.join("ab").join("cd").join("nothex")).is_none());
        assert!(key_from_path(blobs, &blobs.join("README")).is_none());
    }
}
