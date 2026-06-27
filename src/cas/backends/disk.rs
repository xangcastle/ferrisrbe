//! Disk-based CAS Backend
//!
//! Stores blobs in a local filesystem using content-addressed storage.
//! Blobs are organized in a two-level directory structure to avoid
//! having too many files in a single directory.
//!
//! Directory structure: `<root>/aa/bb/aabbccdd...`
//! where `aa` is the first 2 hex chars of the hash and `bb` is the next 2.
//!
//! Supports a configurable maximum size with LRU eviction so it can be used
//! as a drop-in replacement for `bazel-remote-cache`.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;

/// Default maximum cache size in GiB when none is configured.
const DEFAULT_MAX_SIZE_GB: u64 = 100;

/// In-memory LRU bookkeeping for disk blobs.
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

/// Disk-based CAS backend
pub struct DiskBackend {
    /// Root directory for blob storage
    root: PathBuf,
    /// Directory for temporary files during writes
    temp_dir: PathBuf,
    /// Maximum total size in bytes (0 means unlimited)
    max_size_bytes: u64,
    /// Current total size in bytes
    current_size_bytes: AtomicU64,
    /// LRU state
    lru: Mutex<LruState>,
}

impl DiskBackend {
    /// Create a new disk backend with the given root directory and default
    /// maximum size.
    pub async fn new(root: impl AsRef<Path>) -> CasResult<Self> {
        Self::with_max_size(root, DEFAULT_MAX_SIZE_GB).await
    }

    /// Create a new disk backend with a maximum size in GiB.
    pub async fn with_max_size(root: impl AsRef<Path>, max_size_gb: u64) -> CasResult<Self> {
        Self::with_max_size_bytes(root, max_size_gb * 1024 * 1024 * 1024).await
    }

    /// Create a new disk backend with a maximum size in bytes.
    pub async fn with_max_size_bytes(
        root: impl AsRef<Path>,
        max_size_bytes: u64,
    ) -> CasResult<Self> {
        let root = root.as_ref().to_path_buf();
        let temp_dir = root.join(".tmp");

        fs::create_dir_all(&root).await?;
        fs::create_dir_all(&temp_dir).await?;

        let mut lru = LruState::new();
        let mut current_size: u64 = 0;
        Self::initialize_lru(&root, &mut lru, &mut current_size).await?;

        info!(
            "Initialized DiskBackend at {:?}: size={} bytes, max={} bytes",
            root, current_size, max_size_bytes
        );

        Ok(Self {
            root,
            temp_dir,
            max_size_bytes,
            current_size_bytes: AtomicU64::new(current_size),
            lru: Mutex::new(lru),
        })
    }

    async fn initialize_lru(
        root: &Path,
        lru: &mut LruState,
        current_size: &mut u64,
    ) -> CasResult<()> {
        Self::initialize_lru_dir(root, root, lru, current_size).await
    }

    async fn initialize_lru_dir(
        root: &Path,
        dir: &Path,
        lru: &mut LruState,
        current_size: &mut u64,
    ) -> CasResult<()> {
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

    /// Compute the storage path for a digest
    ///
    /// Uses two-level nesting: `<root>/aa/bb/hash`
    fn blob_path(&self, digest: &DigestInfo) -> PathBuf {
        let hash = digest.hash_to_string();
        if hash.len() < 4 {
            return self.root.join(hash);
        }

        let prefix1 = &hash[0..2];
        let prefix2 = &hash[2..4];
        let remainder = &hash[4..];

        self.root.join(prefix1).join(prefix2).join(remainder)
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
        // Size is not encoded in the path; use the size stored in the digest
        // if available, otherwise 0. This is only used for LRU bookkeeping.
        DigestInfo::new(&hash, 0).ok()
    }

    /// Get a temporary file path for atomic writes
    fn temp_path(&self) -> PathBuf {
        let uuid = uuid::Uuid::new_v4().to_string();
        self.temp_dir.join(format!("tmp-{}", uuid))
    }

    /// Create parent directories if needed
    async fn ensure_parent(path: &Path) -> CasResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// Compute SHA256 digest of data
    fn compute_digest(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    /// Verify that data matches the expected digest
    fn verify_digest(&self, data: &[u8], expected: &DigestInfo) -> CasResult<()> {
        let computed = Self::compute_digest(data);
        let expected_hash = expected.hash_to_string();
        if computed != expected_hash {
            return Err(CasError::digest_mismatch(&expected_hash, &computed));
        }
        Ok(())
    }

    fn touch_lru(&self, digest: DigestInfo, size: u64) {
        let mut lru = self.lru.lock();
        lru.touch(digest, size);
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
            let path = self.blob_path(&digest);
            if fs::remove_file(&path).await.is_ok() {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                debug!("Evicted CAS blob {:?} ({} bytes)", digest, size);
            }
        }
    }

    /// Return the current total size of stored blobs in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.current_size_bytes.load(Ordering::Relaxed)
    }

    /// Return the configured maximum size in bytes (0 means unlimited).
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }
}

#[async_trait]
impl CasBackend for DiskBackend {
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        let path = self.blob_path(digest);
        match fs::metadata(&path).await {
            Ok(metadata) => {
                let expected_size = digest.size as u64;
                if metadata.len() != expected_size {
                    warn!(
                        "Digest {} exists but size mismatch: expected {}, got {}",
                        digest.hash_to_string(),
                        expected_size,
                        metadata.len()
                    );
                    return Ok(false);
                }
                self.touch_lru(*digest, metadata.len());
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        let path = self.blob_path(digest);

        match fs::read(&path).await {
            Ok(data) => {
                if let Err(e) = self.verify_digest(&data, digest) {
                    error!("Corrupted blob at {:?}: {}", path, e);
                    fs::remove_file(&path).await.ok();
                    self.current_size_bytes
                        .fetch_sub(data.len() as u64, Ordering::Relaxed);
                    let mut lru = self.lru.lock();
                    lru.remove(*digest);
                    return Ok(None);
                }
                self.touch_lru(*digest, data.len() as u64);
                Ok(Some(Bytes::from(data)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        let path = self.blob_path(digest);

        match fs::metadata(&path).await {
            Ok(metadata) => {
                let file_size = metadata.len() as usize;
                self.touch_lru(*digest, metadata.len());

                if offset >= file_size {
                    return Ok(Some(Box::pin(futures::stream::empty())));
                }

                let remaining = file_size - offset;
                let read_size = limit.map(|l| l.min(remaining)).unwrap_or(remaining);

                let path_clone = path.clone();
                let digest_clone = digest.clone();

                let stream = async_stream::stream! {
                    let mut file = match File::open(&path_clone).await {
                        Ok(f) => f,
                        Err(e) => {
                            yield Err(CasError::Io(e));
                            return;
                        }
                    };

                    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset as u64)).await {
                        yield Err(CasError::Io(e));
                        return;
                    }

                    let mut remaining = read_size;
                    let chunk_size = 64 * 1024;
                    let mut hasher = Sha256::new();

                    while remaining > 0 {
                        let to_read = chunk_size.min(remaining);
                        let mut buffer = vec![0u8; to_read];

                        match file.read_exact(&mut buffer).await {
                            Ok(_) => {
                                hasher.update(&buffer);
                                remaining -= to_read;
                                yield Ok(Bytes::from(buffer));
                            }
                            Err(e) => {
                                yield Err(CasError::Io(e));
                                return;
                            }
                        }
                    }

                    if offset == 0 && limit.is_none() {
                        let computed = hex::encode(hasher.finalize());
                        let expected = digest_clone.hash_to_string();
                        if computed != expected {
                            yield Err(CasError::digest_mismatch(&expected, &computed));
                        }
                    }
                };

                Ok(Some(Box::pin(stream)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        if self.contains(digest).await? {
            debug!(
                "Blob {} already exists, skipping write",
                digest.hash_to_string()
            );
            return Ok(());
        }

        if let Err(e) = self.verify_digest(&data, digest) {
            error!(
                "Digest verification failed for {}: {}",
                digest.hash_to_string(),
                e
            );
            return Err(e);
        }

        let final_path = self.blob_path(digest);
        let temp_path = self.temp_path();

        Self::ensure_parent(&final_path).await?;

        let mut file = File::create(&temp_path).await?;
        file.write_all(&data).await?;
        file.sync_all().await?;
        drop(file);

        let size = data.len() as u64;
        if size > self.max_size_bytes && self.max_size_bytes > 0 {
            fs::remove_file(&temp_path).await.ok();
            return Err(CasError::Storage(format!(
                "Blob size {} exceeds max cache size {}",
                size, self.max_size_bytes
            )));
        }

        match fs::rename(&temp_path, &final_path).await {
            Ok(()) => {
                self.current_size_bytes.fetch_add(size, Ordering::Relaxed);
                self.touch_lru(*digest, size);
                self.evict_if_needed().await;
                debug!("Wrote blob {} to {:?}", digest.hash_to_string(), final_path);
                Ok(())
            }
            Err(e) => {
                fs::remove_file(&temp_path).await.ok();
                Err(e.into())
            }
        }
    }

    async fn write_stream(
        &self,
        digest: &DigestInfo,
        mut stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        if self.contains(digest).await? {
            debug!(
                "Blob {} already exists, skipping stream write",
                digest.hash_to_string()
            );
            return Ok(());
        }

        let final_path = self.blob_path(digest);
        let temp_path = self.temp_path();

        Self::ensure_parent(&final_path).await?;

        let mut file = File::create(&temp_path).await?;
        let mut hasher = Sha256::new();
        let mut total_size: usize = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            hasher.update(&chunk);
            total_size += chunk.len();
            file.write_all(&chunk).await?;
        }

        file.sync_all().await?;
        drop(file);

        let size = total_size as u64;
        if size > self.max_size_bytes && self.max_size_bytes > 0 {
            fs::remove_file(&temp_path).await.ok();
            return Err(CasError::Storage(format!(
                "Blob size {} exceeds max cache size {}",
                size, self.max_size_bytes
            )));
        }

        let computed_hash = hex::encode(hasher.finalize());
        if computed_hash != digest.hash_to_string() {
            fs::remove_file(&temp_path).await.ok();
            return Err(CasError::digest_mismatch(
                &digest.hash_to_string(),
                &computed_hash,
            ));
        }

        if total_size != digest.size as usize {
            fs::remove_file(&temp_path).await.ok();
            return Err(CasError::InvalidDigest(format!(
                "Size mismatch: expected {}, got {}",
                digest.size, total_size
            )));
        }

        match fs::rename(&temp_path, &final_path).await {
            Ok(()) => {
                self.current_size_bytes.fetch_add(size, Ordering::Relaxed);
                self.touch_lru(*digest, size);
                self.evict_if_needed().await;
                info!(
                    "Wrote blob {} ({} bytes, expected {} bytes) to {:?}",
                    digest.hash_to_string(),
                    total_size,
                    digest.size,
                    final_path
                );
                Ok(())
            }
            Err(e) => {
                fs::remove_file(&temp_path).await.ok();
                Err(e.into())
            }
        }
    }

    async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
        let path = self.blob_path(digest);
        let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path).await {
            Ok(()) => {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                let mut lru = self.lru.lock();
                lru.remove(*digest);
                debug!("Deleted blob {} from {:?}", digest.hash_to_string(), path);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        let path = self.blob_path(digest);
        match fs::metadata(&path).await {
            Ok(metadata) => {
                self.touch_lru(*digest, metadata.len());
                Ok(Some(path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_disk_backend_write_read() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();

        let data = b"hello world";
        let digest = DigestInfo::from_bytes(data);

        backend
            .write(&digest, Bytes::from_static(data))
            .await
            .unwrap();

        let result = backend.read(&digest).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().as_ref(), data);

        assert!(backend.contains(&digest).await.unwrap());

        let path = backend.local_path(&digest).await.unwrap();
        assert!(path.is_some());
        assert!(path.unwrap().exists());
    }

    #[tokio::test]
    async fn test_disk_backend_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();

        let digest = DigestInfo::new(
            "7945bc2d6e4fd0a0be5216460557bef483a80b6af0acbcdf06866f5c473b9367",
            0,
        )
        .unwrap();

        assert!(!backend.contains(&digest).await.unwrap());
        assert!(backend.read(&digest).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_disk_backend_digest_verification() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();

        let data = b"test data";
        let wrong_digest = DigestInfo::new(
            "c5df841b903cb4ca2f171b0ce1cc391001d3aa38d22c9c8288da31c232404a2e",
            data.len() as i64,
        )
        .unwrap();

        let result = backend.write(&wrong_digest, Bytes::from_static(data)).await;
        assert!(matches!(result, Err(CasError::DigestMismatch { .. })));
    }

    #[tokio::test]
    async fn test_disk_backend_stream_write() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();

        let data = b"streaming data test";
        let digest = DigestInfo::from_bytes(data);

        let chunks = vec![
            Ok(Bytes::from_static(&data[0..5])),
            Ok(Bytes::from_static(&data[5..10])),
            Ok(Bytes::from_static(&data[10..])),
        ];
        let stream = futures::stream::iter(chunks).boxed();

        backend.write_stream(&digest, stream).await.unwrap();

        let result = backend.read(&digest).await.unwrap();
        assert_eq!(result.unwrap().as_ref(), data);
    }

    #[tokio::test]
    async fn test_disk_backend_lru_eviction() {
        let temp_dir = TempDir::new().unwrap();
        // 1 byte max: writing a second blob must evict the first one.
        let backend = DiskBackend::with_max_size_bytes(temp_dir.path(), 1)
            .await
            .unwrap();

        let data1 = b"1";
        let digest1 = DigestInfo::from_bytes(data1);
        backend
            .write(&digest1, Bytes::from_static(data1))
            .await
            .unwrap();

        let data2 = b"2";
        let digest2 = DigestInfo::from_bytes(data2);
        backend
            .write(&digest2, Bytes::from_static(data2))
            .await
            .unwrap();

        // digest1 should have been evicted to keep total size <= 1 byte.
        assert!(!backend.contains(&digest1).await.unwrap());
        assert!(backend.contains(&digest2).await.unwrap());
    }
}
