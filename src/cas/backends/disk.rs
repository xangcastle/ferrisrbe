//! Disk-backed CAS.
//!
//! Blobs are stored by SHA-256 in the
//! [sharded layout](crate::cache::sharded_layout) under the backend root.
//! Every write is verified against its digest, staged and published by
//! rename, so a stored blob is always complete and correct. Total size is
//! bounded and least recently used blobs are evicted first, which makes the
//! backend a drop-in replacement for `bazel-remote-cache`.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest as Sha2Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use crate::cache::lru::LruState;
use crate::cache::sharded_layout::{self, STAGING_DIR};
use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;

/// Size limit in GiB used by [`DiskBackend::new`].
const DEFAULT_MAX_SIZE_GB: u64 = 100;

/// Chunk size of streamed reads.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Age after which a read refreshes a blob's mtime.
///
/// Recency is restored from mtimes when the process starts, so a blob that
/// is only ever read must have its mtime advanced from time to time to keep
/// its rank across restarts; refreshing at most every few hours bounds the
/// cost to one metadata write per blob per interval.
const MTIME_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// Content-addressed blob store on the local filesystem.
///
/// Size and entry counts start at zero and are completed by
/// [`Self::warm_up`]; blobs already on disk are readable immediately.
pub struct DiskBackend {
    root: PathBuf,
    staging_dir: PathBuf,
    /// Size budget in bytes; `0` disables eviction.
    max_size_bytes: u64,
    current_size_bytes: AtomicU64,
    current_entries: AtomicU64,
    lru: Mutex<LruState>,
    /// Set once [`Self::warm_up`] has accounted every blob found on disk.
    ready: AtomicBool,
    /// Flush every written blob to the device before publishing it.
    fsync: bool,
}

impl DiskBackend {
    /// Opens (creating if needed) a backend at `root` bounded to 100 GiB.
    pub async fn new(root: impl AsRef<Path>) -> CasResult<Self> {
        Self::with_max_size(root, DEFAULT_MAX_SIZE_GB).await
    }

    /// Opens (creating if needed) a backend at `root` bounded to
    /// `max_size_gb` GiB.
    pub async fn with_max_size(root: impl AsRef<Path>, max_size_gb: u64) -> CasResult<Self> {
        Self::with_max_size_bytes(root, max_size_gb * 1024 * 1024 * 1024).await
    }

    /// Opens (creating if needed) a backend at `root` bounded to
    /// `max_size_bytes` bytes; `0` means unbounded.
    pub async fn with_max_size_bytes(
        root: impl AsRef<Path>,
        max_size_bytes: u64,
    ) -> CasResult<Self> {
        let root = root.as_ref().to_path_buf();
        let staging_dir = root.join(STAGING_DIR);

        fs::create_dir_all(&root).await?;
        fs::create_dir_all(&staging_dir).await?;

        info!(
            "Opened DiskBackend at {:?}: max={} bytes; accounting fills in with warm_up",
            root, max_size_bytes
        );

        Ok(Self {
            root,
            staging_dir,
            max_size_bytes,
            current_size_bytes: AtomicU64::new(0),
            current_entries: AtomicU64::new(0),
            lru: Mutex::new(LruState::new()),
            ready: AtomicBool::new(false),
            fsync: true,
        })
    }

    /// The same backend without flushing writes to the device, for a cache
    /// that does not outlive its machine, such as a worker's blob cache on an
    /// emptyDir. A crashed process still leaves only complete blobs: writes
    /// are published by rename and the kernel keeps their pages.
    pub fn without_fsync(mut self) -> Self {
        self.fsync = false;
        self
    }

    /// Accounts every blob already on disk in the recency order, the size
    /// total and the entry count, ranked by mtime below blobs touched by
    /// this process.
    ///
    /// Safe to run while the backend serves traffic: a blob written during
    /// the scan is accounted by its write and skipped here. Idempotent.
    pub async fn warm_up(&self) -> CasResult<()> {
        let started = Instant::now();
        sharded_layout::sweep_staging_logged(&self.staging_dir).await;
        self.lru.lock().floor_counter(sharded_layout::now_millis());
        let scanned = sharded_layout::scan(self.root.clone()).await?;
        let restored = sharded_layout::restore_recency(
            &self.lru,
            scanned.iter().map(|entry| {
                let blob = DigestInfo {
                    size: entry.file_size as i64,
                    ..entry.digest
                };
                (blob, entry.file_size, entry.mtime_millis)
            }),
        )
        .await;
        self.current_size_bytes
            .fetch_add(restored.bytes, Ordering::Relaxed);
        self.current_entries
            .fetch_add(restored.entries, Ordering::Relaxed);
        self.ready.store(true, Ordering::Release);
        info!(
            "DiskBackend warm-up at {:?}: entries={} size={} bytes in {:?}",
            self.root,
            self.current_entries.load(Ordering::Relaxed),
            self.current_size_bytes.load(Ordering::Relaxed),
            started.elapsed()
        );
        Ok(())
    }

    /// Whether [`Self::warm_up`] has completed.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn blob_path(&self, digest: &DigestInfo) -> PathBuf {
        sharded_layout::entry_path(&self.root, digest)
    }

    fn compute_digest(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    fn verify_digest(&self, data: &[u8], expected: &DigestInfo) -> CasResult<()> {
        let computed = Self::compute_digest(data);
        let expected_hash = expected.hash_to_string();
        if computed != expected_hash {
            return Err(CasError::digest_mismatch(&expected_hash, &computed));
        }
        Ok(())
    }

    /// Completes a staged write before it is published: tokio hands each write
    /// to a blocking thread and returns before it runs, so without the flush a
    /// rename can publish the blob before its bytes reach the file.
    async fn finish_write(&self, mut file: File) -> CasResult<()> {
        file.flush().await?;
        if self.fsync {
            file.sync_all().await?;
        }
        Ok(())
    }

    /// Rejects a blob that alone would exceed the size budget.
    fn ensure_fits(&self, size: u64) -> CasResult<()> {
        if self.max_size_bytes > 0 && size > self.max_size_bytes {
            return Err(CasError::Storage(format!(
                "Blob size {} exceeds max cache size {}",
                size, self.max_size_bytes
            )));
        }
        Ok(())
    }

    fn touch_lru(&self, digest: DigestInfo, size: u64) {
        self.lru.lock().touch(digest, size);
    }

    /// Counts a newly stored blob and evicts down to the size budget.
    async fn account_stored(&self, digest: DigestInfo, size: u64) {
        self.current_size_bytes.fetch_add(size, Ordering::Relaxed);
        self.current_entries.fetch_add(1, Ordering::Relaxed);
        self.touch_lru(digest, size);
        self.evict_if_needed().await;
    }

    /// Publishes a fully written and verified staging file as the blob for
    /// `digest`; the staging file is removed either way.
    ///
    /// A hard link, unlike a rename, fails when another writer of the same
    /// digest published first: only the writer that created the blob counts
    /// its bytes, so concurrent downloads of one blob cannot inflate the
    /// accounting and evict live blobs to pay for bytes that do not exist.
    async fn publish(&self, staged: &Path, digest: &DigestInfo, size: u64) -> CasResult<PathBuf> {
        let final_path = self.blob_path(digest);
        match fs::hard_link(staged, &final_path).await {
            Ok(()) => {
                fs::remove_file(staged).await.ok();
                self.account_stored(*digest, size).await;
                Ok(final_path)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let intact = fs::metadata(&final_path)
                    .await
                    .is_ok_and(|m| m.len() == size);
                let replaced = if intact {
                    fs::remove_file(staged).await.ok();
                    Ok(())
                } else {
                    fs::rename(staged, &final_path).await
                };
                match replaced {
                    Ok(()) => {
                        self.touch_lru(*digest, size);
                        Ok(final_path)
                    }
                    Err(e) => {
                        fs::remove_file(staged).await.ok();
                        Err(e.into())
                    }
                }
            }
            Err(e) => {
                fs::remove_file(staged).await.ok();
                Err(e.into())
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
        let victims = self.lru.lock().evict(current - self.max_size_bytes);
        let (mut blobs, mut bytes) = (0u64, 0u64);
        for (digest, size) in victims {
            let path = self.blob_path(&digest);
            if fs::remove_file(&path).await.is_ok() {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                self.current_entries.fetch_sub(1, Ordering::Relaxed);
                debug!("Evicted CAS blob {:?} ({} bytes)", digest, size);
                blobs += 1;
                bytes += size;
            }
        }
        if blobs > 0 {
            info!(
                "Evicted {} blobs ({} bytes) from {}: {} of {} bytes accounted",
                blobs,
                bytes,
                self.root.display(),
                self.current_size_bytes.load(Ordering::Relaxed),
                self.max_size_bytes
            );
        }
    }

    /// Stores the file at `src` as the blob for `digest`, by hardlink when
    /// `src` is on the same filesystem and by copy otherwise.
    ///
    /// Promotes an artifact the pull-through proxy already downloaded into
    /// the CAS without a second copy of its bytes. The caller guarantees that
    /// `src` hashes to `digest`; the content is not re-read. Adopting a
    /// digest that is already stored, including one linked concurrently by
    /// another request, only marks it recently used.
    pub async fn adopt_link(&self, digest: &DigestInfo, src: &Path) -> CasResult<()> {
        let size = digest.size.max(0) as u64;
        if self.contains(digest).await? {
            self.touch_lru(*digest, size);
            return Ok(());
        }

        let dest = self.blob_path(digest);
        sharded_layout::create_parent_dirs(&dest).await?;

        match fs::hard_link(src, &dest).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.touch_lru(*digest, size);
                return Ok(());
            }
            Err(e) => {
                debug!(
                    "hardlink into CAS failed ({}), copying {:?} instead",
                    e, digest
                );
                fs::copy(src, &dest)
                    .await
                    .map_err(|e| CasError::Storage(format!("adopting blob into CAS: {e}")))?;
            }
        }

        self.account_stored(*digest, size).await;
        Ok(())
    }

    /// Bytes stored, as accounted so far.
    pub fn size_bytes(&self) -> u64 {
        self.current_size_bytes.load(Ordering::Relaxed)
    }

    /// Size budget in bytes; `0` means unbounded.
    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    /// Blobs stored, as accounted so far.
    pub async fn count_entries(&self) -> u64 {
        self.current_entries.load(Ordering::Relaxed)
    }
}

/// Advances the mtime of a blob that has not been refreshed within
/// [`MTIME_REFRESH_AFTER`]. Best effort: a failure only costs recency.
fn refresh_mtime_if_stale(path: &Path, metadata: &std::fs::Metadata) {
    let stale = metadata
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age > MTIME_REFRESH_AFTER);
    if !stale {
        return;
    }
    if let Err(e) = std::fs::File::open(path).and_then(|f| f.set_modified(SystemTime::now())) {
        debug!("could not refresh mtime of {:?}: {}", path, e);
    }
}

#[async_trait]
impl CasBackend for DiskBackend {
    /// A blob counts as present only if its file has the digest's size; a
    /// truncated file reads as missing so the client uploads it again.
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
                refresh_mtime_if_stale(&path, &metadata);
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// A blob whose content no longer matches its digest is deleted and
    /// reported as missing.
    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        let path = self.blob_path(digest);

        match fs::read(&path).await {
            Ok(data) => {
                if let Err(e) = self.verify_digest(&data, digest) {
                    error!("Corrupted blob at {:?}: {}", path, e);
                    fs::remove_file(&path).await.ok();
                    self.current_size_bytes
                        .fetch_sub(data.len() as u64, Ordering::Relaxed);
                    self.lru.lock().remove(*digest);
                    return Ok(None);
                }
                self.touch_lru(*digest, data.len() as u64);
                Ok(Some(Bytes::from(data)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Streams `limit` bytes (or the rest of the blob) from `offset`. A read
    /// of the whole blob is verified against the digest and ends with an
    /// error item if the content does not match.
    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        let path = self.blob_path(digest);

        let metadata = match fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let file_size = metadata.len() as usize;
        self.touch_lru(*digest, metadata.len());

        if offset >= file_size {
            return Ok(Some(Box::pin(futures::stream::empty())));
        }

        let remaining = file_size - offset;
        let read_size = limit.map(|l| l.min(remaining)).unwrap_or(remaining);
        let verify_whole_blob = offset == 0 && limit.is_none();
        let expected = *digest;

        let stream = async_stream::stream! {
            let mut file = match File::open(&path).await {
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
            let mut hasher = Sha256::new();

            while remaining > 0 {
                let mut buffer = vec![0u8; READ_CHUNK_BYTES.min(remaining)];
                if let Err(e) = file.read_exact(&mut buffer).await {
                    yield Err(CasError::Io(e));
                    return;
                }
                hasher.update(&buffer);
                remaining -= buffer.len();
                yield Ok(Bytes::from(buffer));
            }

            if verify_whole_blob {
                let computed = hex::encode(hasher.finalize());
                let expected = expected.hash_to_string();
                if computed != expected {
                    yield Err(CasError::digest_mismatch(&expected, &computed));
                }
            }
        };

        Ok(Some(Box::pin(stream)))
    }

    /// Writing a digest that is already stored is a no-op.
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
        let size = data.len() as u64;
        self.ensure_fits(size)?;

        let staged = sharded_layout::StagingFile::new(&self.staging_dir);
        sharded_layout::create_parent_dirs(&self.blob_path(digest)).await?;

        let mut file = File::create(staged.path()).await?;
        file.write_all(&data).await?;
        self.finish_write(file).await?;

        let published = self.publish(staged.path(), digest, size).await;
        staged.disarm();
        let final_path = published?;
        debug!("Wrote blob {} to {:?}", digest.hash_to_string(), final_path);
        Ok(())
    }

    /// Writing a digest that is already stored is a no-op; the stream is not
    /// consumed.
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

        let staged = sharded_layout::StagingFile::new(&self.staging_dir);
        sharded_layout::create_parent_dirs(&self.blob_path(digest)).await?;

        let mut file = File::create(staged.path()).await?;
        let mut hasher = Sha256::new();
        let mut total_size: usize = 0;

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            hasher.update(&chunk);
            total_size += chunk.len();
            file.write_all(&chunk).await?;
        }

        self.finish_write(file).await?;

        Self::check_streamed(digest, total_size, hasher)
            .and_then(|()| self.ensure_fits(total_size as u64))?;

        let published = self.publish(staged.path(), digest, total_size as u64).await;
        staged.disarm();
        let final_path = published?;
        info!(
            "Wrote blob {} ({} bytes, expected {} bytes) to {:?}",
            digest.hash_to_string(),
            total_size,
            digest.size,
            final_path
        );
        Ok(())
    }

    /// Deleting a digest that is not stored succeeds.
    async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
        let path = self.blob_path(digest);
        let size = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path).await {
            Ok(()) => {
                self.current_size_bytes.fetch_sub(size, Ordering::Relaxed);
                self.current_entries.fetch_sub(1, Ordering::Relaxed);
                self.lru.lock().remove(*digest);
                debug!("Deleted blob {} from {:?}", digest.hash_to_string(), path);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Like [`Self::contains`], a file whose size is not the digest's reads
    /// as missing: a caller that links it would hand out truncated content.
    async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        let path = self.blob_path(digest);
        match fs::metadata(&path).await {
            Ok(metadata) if metadata.len() != digest.size.max(0) as u64 => {
                warn!(
                    "Digest {} exists but size mismatch: expected {}, got {}",
                    digest.hash_to_string(),
                    digest.size,
                    metadata.len()
                );
                Ok(None)
            }
            Ok(metadata) => {
                self.touch_lru(*digest, metadata.len());
                Ok(Some(path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn size_bytes(&self) -> u64 {
        DiskBackend::size_bytes(self)
    }

    async fn count_entries(&self) -> u64 {
        DiskBackend::count_entries(self).await
    }
}

impl DiskBackend {
    /// Verifies a fully streamed blob: its hash first, then its length.
    fn check_streamed(digest: &DigestInfo, total_size: usize, hasher: Sha256) -> CasResult<()> {
        let computed_hash = hex::encode(hasher.finalize());
        if computed_hash != digest.hash_to_string() {
            return Err(CasError::digest_mismatch(
                &digest.hash_to_string(),
                &computed_hash,
            ));
        }
        if total_size != digest.size as usize {
            return Err(CasError::InvalidDigest(format!(
                "Size mismatch: expected {}, got {}",
                digest.size, total_size
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn a_truncated_copy_of_a_blob_is_replaced_by_a_new_write() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::with_max_size(temp_dir.path(), 1)
            .await
            .unwrap();
        let data = Bytes::from_static(b"the complete content of the blob");
        let digest =
            DigestInfo::new(&DiskBackend::compute_digest(&data), data.len() as i64).unwrap();
        let path = backend.blob_path(&digest);
        sharded_layout::create_parent_dirs(&path).await.unwrap();
        std::fs::write(&path, b"the complete").unwrap();

        backend.write(&digest, data.clone()).await.unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), data.to_vec());
        assert!(backend.local_path(&digest).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_stream_write_that_fails_or_is_cancelled_leaves_no_staging_file() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::with_max_size(temp_dir.path(), 1)
            .await
            .unwrap();
        let digest = DigestInfo::from_bytes(b"never completes");
        let staging = temp_dir.path().join(STAGING_DIR);

        let failing = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"never ")),
            Err(CasError::Storage("client went away".into())),
        ])
        .boxed();
        assert!(backend.write_stream(&digest, failing).await.is_err());

        let stalled = futures::stream::once(async { Ok(Bytes::from_static(b"never ")) })
            .chain(futures::stream::pending())
            .boxed();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            backend.write_stream(&digest, stalled),
        );
        assert!(cancelled.await.is_err());

        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_writes_of_one_blob_count_it_once() {
        let temp_dir = TempDir::new().unwrap();
        let backend = std::sync::Arc::new(
            DiskBackend::with_max_size(temp_dir.path(), 1)
                .await
                .unwrap(),
        );
        let blobs: Vec<Bytes> = (0..20u8).map(|i| Bytes::from(vec![i; 64 * 1024])).collect();

        let mut writers = Vec::new();
        for data in &blobs {
            let digest =
                DigestInfo::new(&DiskBackend::compute_digest(data), data.len() as i64).unwrap();
            for _ in 0..16 {
                let backend = backend.clone();
                let data = data.clone();
                writers.push(tokio::spawn(
                    async move { backend.write(&digest, data).await },
                ));
            }
        }
        for writer in writers {
            writer.await.unwrap().unwrap();
        }

        let stored: u64 = blobs.iter().map(|b| b.len() as u64).sum();
        assert_eq!(backend.size_bytes(), stored);
        assert_eq!(backend.count_entries().await, blobs.len() as u64);
    }

    #[tokio::test]
    async fn warm_up_folds_disk_into_accounting_without_double_counting() {
        let temp_dir = TempDir::new().unwrap();
        let first = Bytes::from_static(b"first blob");
        let second = Bytes::from_static(b"second");
        let live = Bytes::from_static(b"written before the scan finished");
        let digest_of = |data: &Bytes| {
            DigestInfo::new(&DiskBackend::compute_digest(data), data.len() as i64).unwrap()
        };
        {
            let backend = DiskBackend::new(temp_dir.path()).await.unwrap();
            backend.warm_up().await.unwrap();
            backend
                .write(&digest_of(&first), first.clone())
                .await
                .unwrap();
            backend
                .write(&digest_of(&second), second.clone())
                .await
                .unwrap();
        }

        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();
        assert!(!backend.is_ready());
        assert_eq!(backend.count_entries().await, 0);
        backend
            .write(&digest_of(&live), live.clone())
            .await
            .unwrap();
        assert_eq!(backend.count_entries().await, 1);

        backend.warm_up().await.unwrap();
        assert!(backend.is_ready());
        assert_eq!(backend.count_entries().await, 3);
        assert_eq!(
            backend.size_bytes(),
            (first.len() + second.len() + live.len()) as u64
        );

        backend.warm_up().await.unwrap();
        assert_eq!(backend.count_entries().await, 3);
    }

    #[tokio::test]
    async fn a_truncated_blob_has_no_local_path() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();
        let data = Bytes::from_static(b"twenty bytes of data");
        let digest =
            DigestInfo::new(&DiskBackend::compute_digest(&data), data.len() as i64).unwrap();
        backend.write(&digest, data).await.unwrap();
        let path = backend.local_path(&digest).await.unwrap().unwrap();

        std::fs::write(&path, b"").unwrap();

        assert_eq!(backend.local_path(&digest).await.unwrap(), None);
    }

    #[tokio::test]
    async fn written_blob_is_readable_and_has_a_local_path() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();

        let data = b"hello world";
        let digest = DigestInfo::from_bytes(data);

        backend
            .write(&digest, Bytes::from_static(data))
            .await
            .unwrap();

        let result = backend.read(&digest).await.unwrap();
        assert_eq!(result.unwrap().as_ref(), data);
        assert!(backend.contains(&digest).await.unwrap());

        let path = backend.local_path(&digest).await.unwrap();
        assert!(path.unwrap().exists());
    }

    #[tokio::test]
    async fn unknown_digest_is_absent() {
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
    async fn write_rejects_content_that_does_not_match_the_digest() {
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
    async fn streamed_write_reassembles_the_blob() {
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
    async fn a_one_byte_budget_keeps_only_the_newest_blob() {
        let temp_dir = TempDir::new().unwrap();
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

        assert!(
            !backend.contains(&digest1).await.unwrap(),
            "oldest blob must be evicted"
        );
        assert!(backend.contains(&digest2).await.unwrap());
    }
}
