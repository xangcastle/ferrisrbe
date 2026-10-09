//! Materialization of an action's input root from the CAS.
//!
//! The input root is a Merkle tree of REAPI `Directory` messages: each lists
//! its files and subdirectories by digest. [`Materializer`] walks the tree
//! from the root digest and reproduces it on disk, up to
//! [`DIRECTORY_CONCURRENCY`] directories and [`FILE_CONCURRENCY`] files of
//! each at once:
//!
//! - `Directory` messages are cached like blobs: the `node_modules` trees most
//!   actions share are fetched from the CAS once per worker, not once per
//!   action;
//! - every blob is downloaded once into a local, size-bounded LRU blob cache
//!   (a [`DiskBackend`]) and cloned into the execroot with a reflink: as cheap
//!   as a hard link, but copy-on-write, so an action that writes over an input
//!   never touches the cached blob. Where the filesystem cannot clone, inputs
//!   are hard-linked (sharing the inode with the cache) or copied; evicting a
//!   blob never breaks an execroot already linked to it;
//! - files already present with the right content are reused;
//! - every downloaded blob and directory is verified against its digest;
//! - large blobs are streamed to disk in constant memory, with a deadline;
//! - symlinks are created with their target exactly as sent, resolved
//!   relative to the link, never followed or checked: rules_js lays out
//!   `node_modules` as links whose targets only exist once the whole tree
//!   is in place.

use bytes::Bytes;
use futures::stream::{self, BoxStream, FuturesUnordered};
use futures::StreamExt;
use sha2::{Digest as Sha2Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

use crate::cas::backends::DiskBackend;
use crate::cas::{CasBackend, CasError, CasResult};
use crate::proto::build::bazel::remote::execution::v2::{Directory, FileNode, SymlinkNode};
use crate::types::DigestInfo;

/// Overrides [`MaterializerConfig::streaming_threshold`], in bytes.
pub const ENV_STREAMING_THRESHOLD: &str = "RBE_STREAMING_THRESHOLD";
/// Overrides [`MaterializerConfig::download_timeout`], in seconds.
pub const ENV_DOWNLOAD_TIMEOUT_SECS: &str = "RBE_DOWNLOAD_TIMEOUT_SECS";
/// Overrides [`MaterializerConfig::max_concurrent_downloads`].
pub const ENV_MAX_CONCURRENT_DOWNLOADS: &str = "RBE_MAX_CONCURRENT_DOWNLOADS";
/// Overrides [`MaterializerConfig::download_chunk_size`], in bytes.
pub const ENV_DOWNLOAD_CHUNK_SIZE: &str = "RBE_DOWNLOAD_CHUNK_SIZE";

/// A streamed download that yields no chunk for this long is abandoned.
const CHUNK_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// `EXDEV`: a hard link across filesystems.
const EXDEV: i32 = 18;

/// Directories of one input root materialized at once.
const DIRECTORY_CONCURRENCY: usize = 16;

/// Files of one directory placed at once.
const FILE_CONCURRENCY: usize = 16;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A flag that is on unless set to `false` or `0`.
fn env_flag_enabled(key: &str) -> bool {
    std::env::var(key)
        .map(|s| s != "false" && s != "0")
        .unwrap_or(true)
}

/// Tuning of [`Materializer`].
#[derive(Debug, Clone)]
pub struct MaterializerConfig {
    /// Upper bound on blobs downloaded at once. Default 10.
    pub max_concurrent_downloads: usize,
    /// Hard-link cached blobs into the execroot instead of copying them.
    /// Default on; `RBE_USE_HARDLINKS=false` disables it.
    pub use_hardlinks: bool,
    /// Verify every reused file against its digest. Default on;
    /// `RBE_VALIDATE_DIGESTS=false` disables it. Downloaded blobs and
    /// directories are always verified: the blob cache refuses a mismatch.
    pub validate_digests: bool,
    /// Read buffer when hashing existing files, in bytes. Default 64 KiB.
    pub download_chunk_size: usize,
    /// Deadline for a whole streamed download. Default 300 s.
    pub download_timeout: Duration,
    /// Blobs larger than this many bytes are streamed rather than buffered.
    /// Default 4 MiB.
    pub streaming_threshold: i64,
}

impl Default for MaterializerConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl MaterializerConfig {
    /// Reads the configuration from the environment; unset or unparsable
    /// variables take their defaults.
    pub fn from_env() -> Self {
        Self {
            max_concurrent_downloads: env_or(ENV_MAX_CONCURRENT_DOWNLOADS, 10),
            use_hardlinks: env_flag_enabled("RBE_USE_HARDLINKS"),
            validate_digests: env_flag_enabled("RBE_VALIDATE_DIGESTS"),
            download_chunk_size: env_or(ENV_DOWNLOAD_CHUNK_SIZE, 64 * 1024),
            download_timeout: Duration::from_secs(env_or(ENV_DOWNLOAD_TIMEOUT_SECS, 300)),
            streaming_threshold: env_or(ENV_STREAMING_THRESHOLD, 4 * 1024 * 1024),
        }
    }
}

/// Where the content of one input file comes from.
#[derive(Debug, Clone)]
pub enum FileState {
    /// Already on disk with the right content.
    Cached(PathBuf),
    /// Must be fetched from the CAS.
    NeedDownload(DigestInfo),
    /// Being fetched by another task.
    InProgress,
}

/// Summary of one materialized input root.
#[derive(Debug)]
pub struct MaterializedRoot {
    pub execroot: PathBuf,
    /// Files placed, cached or downloaded.
    pub file_count: usize,
    pub dir_count: usize,
    pub symlink_count: usize,
    pub bytes_downloaded: u64,
    /// Files reused from the execroot or the local blob cache.
    pub cached_files: usize,
    /// Size of every input file, downloaded or not.
    pub input_bytes: u64,
    /// The heaviest input groups (see [`input_group`]), most bytes first, at
    /// most [`MAX_INPUT_GROUPS`].
    pub input_groups: Vec<InputGroup>,
}

/// Files and bytes of the inputs under one group of paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputGroup {
    pub name: String,
    pub files: u64,
    pub bytes: u64,
}

/// How many input groups a materialized root reports.
pub const MAX_INPUT_GROUPS: usize = 12;

/// The group an input directory counts toward: the npm package of an
/// `aspect_rules_js` store entry (`npm:@scope/name`), the external
/// repository (`external/<repo>`), or else its first two path components.
pub fn input_group(relative_dir: &str) -> String {
    const STORE: &str = "node_modules/.aspect_rules_js/";
    if let Some(at) = relative_dir.find(STORE) {
        let entry = relative_dir[at + STORE.len()..]
            .split('/')
            .next()
            .unwrap_or_default();
        let name = match entry.get(1..).and_then(|rest| rest.find('@')) {
            Some(version) => &entry[..version + 1],
            None => entry,
        };
        return format!("npm:{}", name.replace('+', "/"));
    }
    let mut parts = relative_dir.split('/').filter(|p| !p.is_empty());
    match (parts.next(), parts.next()) {
        (Some("external"), Some(repo)) => format!("external/{repo}"),
        (Some(first), Some(second)) => format!("{first}/{second}"),
        (Some(first), None) => first.to_string(),
        (None, _) => "(root)".to_string(),
    }
}

/// Reproduces input roots from the CAS on the local filesystem.
pub struct Materializer {
    cas_backend: Arc<dyn CasBackend>,
    config: MaterializerConfig,
    /// Local blob cache every input file is linked from, bounded by size.
    blob_cache: Arc<DiskBackend>,
    download_semaphore: Arc<Semaphore>,
    /// Set after the first cross-device link failure; from then on files are
    /// copied without attempting a hard link, and the switch is logged once.
    hardlink_failed_exdev: AtomicBool,
    /// Set once the filesystem refuses a reflink; from then on files are
    /// hard-linked or copied, and the switch is logged once.
    reflink_unsupported: AtomicBool,
}

impl Materializer {
    pub fn new(
        cas_backend: Arc<dyn CasBackend>,
        blob_cache: Arc<DiskBackend>,
        config: MaterializerConfig,
    ) -> Self {
        let download_semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));

        Self {
            cas_backend,
            config,
            blob_cache,
            download_semaphore,
            hardlink_failed_exdev: AtomicBool::new(false),
            reflink_unsupported: AtomicBool::new(false),
        }
    }

    /// Reproduces the tree rooted at `root_digest` under `execroot`, fetching
    /// each `Directory` message as it is reached. A directory is created
    /// before any of its children is scheduled.
    ///
    /// # Errors
    ///
    /// When a directory or blob is missing from the CAS, fails verification,
    /// has an invalid name, or cannot be written; the first failure stops the
    /// walk.
    pub async fn materialize_directory_recursive(
        &self,
        root_digest: &DigestInfo,
        execroot: &Path,
    ) -> CasResult<MaterializedRoot> {
        info!(
            "Materializing directory {} to {}",
            root_digest.hash_to_string(),
            execroot.display()
        );

        fs::create_dir_all(execroot)
            .await
            .map_err(io_at("create directory", execroot))?;

        let mut stats = MaterializationStats::default();
        let mut groups: HashMap<String, (u64, u64)> = HashMap::new();
        let mut queued = VecDeque::from([(*root_digest, execroot.to_path_buf())]);
        let mut running = FuturesUnordered::new();
        loop {
            while running.len() < DIRECTORY_CONCURRENCY {
                let Some((digest, path)) = queued.pop_front() else {
                    break;
                };
                running.push(async move {
                    let group = path
                        .strip_prefix(execroot)
                        .map(|rel| input_group(&rel.to_string_lossy()))
                        .unwrap_or_default();
                    (group, self.materialize_one_directory(digest, path).await)
                });
            }
            let Some((group, done)) = running.next().await else {
                break;
            };
            let (directory_stats, children) = done?;
            stats.add(&directory_stats);
            if directory_stats.file_count > 0 {
                let entry = groups.entry(group).or_default();
                entry.0 += directory_stats.file_count as u64;
                entry.1 += directory_stats.input_bytes;
            }
            queued.extend(children);
        }
        let mut input_groups: Vec<InputGroup> = groups
            .into_iter()
            .map(|(name, (files, bytes))| InputGroup { name, files, bytes })
            .collect();
        input_groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        input_groups.truncate(MAX_INPUT_GROUPS);

        info!(
            "Materialized {} files ({} cached) and {} symlinks in {} directories, {} bytes downloaded",
            stats.file_count,
            stats.cached_files,
            stats.symlink_count,
            stats.dir_count,
            stats.bytes_downloaded
        );

        Ok(MaterializedRoot {
            execroot: execroot.to_path_buf(),
            file_count: stats.file_count,
            dir_count: stats.dir_count,
            symlink_count: stats.symlink_count,
            bytes_downloaded: stats.bytes_downloaded,
            cached_files: stats.cached_files,
            input_bytes: stats.input_bytes,
            input_groups,
        })
    }

    /// The `Directory` for `digest`, from the blob cache or else from the CAS,
    /// verified and cached on the way.
    async fn fetch_directory(&self, digest: &DigestInfo) -> CasResult<Directory> {
        trace!("Fetching directory {}", digest.hash_to_string());

        let data = match self.blob_cache.read(digest).await? {
            Some(data) => data,
            None => {
                let data = self
                    .cas_backend
                    .read(digest)
                    .await?
                    .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;
                self.blob_cache.write(digest, data.clone()).await?;
                data
            }
        };

        Directory::decode(data)
            .map_err(|e| CasError::InvalidData(format!("Failed to decode Directory: {}", e)))
    }

    /// Creates `path` and places the files and symlinks of the directory
    /// `digest` in it; returns what it placed and the subdirectories still
    /// to materialize.
    async fn materialize_one_directory(
        &self,
        digest: DigestInfo,
        path: PathBuf,
    ) -> CasResult<(MaterializationStats, Vec<(DigestInfo, PathBuf)>)> {
        let dir = self.fetch_directory(&digest).await?;
        trace!("Materializing directory contents to {}", path.display());
        fs::create_dir_all(&path)
            .await
            .map_err(io_at("create directory", &path))?;

        let mut stats = MaterializationStats {
            dir_count: 1,
            input_bytes: dir
                .files
                .iter()
                .filter_map(|f| f.digest.as_ref())
                .map(|d| d.size_bytes.max(0) as u64)
                .sum(),
            ..Default::default()
        };
        let mut files = dir.files.iter();
        let mut placing = FuturesUnordered::new();
        loop {
            while placing.len() < FILE_CONCURRENCY {
                let Some(file_node) = files.next() else {
                    break;
                };
                placing.push(self.materialize_file(file_node, &path));
            }
            let Some(placed) = placing.next().await else {
                break;
            };
            stats.record(placed?);
        }

        for symlink_node in &dir.symlinks {
            materialize_symlink(symlink_node, &path).await?;
            stats.symlink_count += 1;
        }

        let children = dir
            .directories
            .iter()
            .map(|node| {
                let name = single_path_component(&node.name)?;
                let digest = digest_info_from_proto(node.digest.as_ref().ok_or_else(|| {
                    CasError::InvalidData(format!("directory {name:?} has no digest"))
                })?)?;
                Ok((digest, path.join(name)))
            })
            .collect::<CasResult<_>>()?;
        Ok((stats, children))
    }

    /// Places one file, preferring in order: the file already at its
    /// destination, the local blob cache, a buffered or streamed download.
    /// The download semaphore is only taken for a download: linking from the
    /// cache never waits behind another action's transfers.
    async fn materialize_file(
        &self,
        file_node: &FileNode,
        parent_path: &Path,
    ) -> CasResult<Placement> {
        let file_path = parent_path.join(single_path_component(&file_node.name)?);
        let digest = digest_info_from_proto(file_node.digest.as_ref().ok_or_else(|| {
            CasError::InvalidData(format!("file {:?} has no digest", file_node.name))
        })?)?;

        trace!(
            "Materializing file {} ({})",
            file_path.display(),
            digest.hash_to_string()
        );

        if self.validate_existing_file(&file_path, &digest).await? {
            trace!("File {} already exists and is valid", file_path.display());
            return Ok(Placement::Reused);
        }

        if let Some(cached) = self.blob_cache.local_path(&digest).await? {
            match self
                .link_file(&cached, &file_path, file_node.is_executable)
                .await
            {
                Ok(()) => return Ok(Placement::Reused),
                Err(CasError::Io(e)) if e.kind() == ErrorKind::NotFound => {
                    debug!(
                        "Blob {} was evicted while linking it; downloading it again",
                        digest.hash_to_string()
                    );
                }
                Err(e) => return Err(e),
            }
        }

        debug!(
            "Downloading file {} from CAS (size: {} bytes)",
            digest.hash_to_string(),
            digest.size
        );
        {
            let _permit = self.download_semaphore.acquire().await;
            if digest.size > self.config.streaming_threshold {
                self.download_streaming(&digest).await?;
            } else {
                self.download_buffered(&digest).await?;
            }
        }
        let cached = self.blob_cache.local_path(&digest).await?.ok_or_else(|| {
            CasError::Storage(format!(
                "blob {} was evicted right after its download",
                digest.hash_to_string()
            ))
        })?;
        self.link_file(&cached, &file_path, file_node.is_executable)
            .await?;
        Ok(Placement::Downloaded(digest.size.max(0) as u64))
    }

    /// Downloads a blob into memory and stores it in the blob cache, which
    /// verifies it.
    async fn download_buffered(&self, digest: &DigestInfo) -> CasResult<()> {
        let data = self
            .cas_backend
            .read(digest)
            .await?
            .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;
        self.blob_cache.write(digest, data).await
    }

    /// Streams a blob into the blob cache in constant memory. The download
    /// fails when it exceeds [`MaterializerConfig::download_timeout`] or goes
    /// [`CHUNK_READ_TIMEOUT`] without a chunk; the blob cache verifies the
    /// content and discards a partial write.
    async fn download_streaming(&self, digest: &DigestInfo) -> CasResult<()> {
        let source = self
            .cas_backend
            .read_stream(digest, 0, None)
            .await?
            .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;
        let deadline = self.config.download_timeout;
        info!(
            "Starting streaming download for {} (timeout: {:?})",
            digest.hash_to_string(),
            deadline
        );
        let stored = self
            .blob_cache
            .write_stream(digest, with_chunk_timeout(source));
        match timeout(deadline, stored).await {
            Ok(stored) => stored,
            Err(_) => Err(CasError::Storage(format!(
                "Download timeout after {deadline:?}"
            ))),
        }
    }

    /// Whether `path` already holds exactly the content of `expected_digest`:
    /// the size must match and, when validation is on, the hash too (computed
    /// in constant memory).
    async fn validate_existing_file(
        &self,
        path: &Path,
        expected_digest: &DigestInfo,
    ) -> CasResult<bool> {
        if !path.exists() {
            return Ok(false);
        }

        let metadata = fs::metadata(path).await.map_err(CasError::Io)?;
        if metadata.len() as i64 != expected_digest.size {
            return Ok(false);
        }

        if self.config.validate_digests {
            let mut file = fs::File::open(path).await.map_err(CasError::Io)?;
            let mut hasher = Sha256::new();
            let mut buffer = vec![0u8; self.config.download_chunk_size];

            loop {
                let n = tokio::io::AsyncReadExt::read(&mut file, &mut buffer)
                    .await
                    .map_err(CasError::Io)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }

            let computed_hash = hex::encode(hasher.finalize());
            Ok(computed_hash == expected_digest.hash_to_string())
        } else {
            Ok(true)
        }
    }

    /// Places `source` at `dest`, replacing whatever is there, as a reflink
    /// when the filesystem supports it and otherwise as a hard link or a copy,
    /// and sets the execute bits when `is_executable`. The parent of `dest`
    /// must exist. A missing `source` (evicted meanwhile) is a `NotFound` I/O
    /// error.
    async fn link_file(&self, source: &Path, dest: &Path, is_executable: bool) -> CasResult<()> {
        if !self.reflink_unsupported.load(Ordering::Relaxed) {
            match reflink_replacing(source, dest).await {
                Ok(()) => return set_executable(dest, is_executable).await,
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(link_error(source, dest, e).await);
                }
                Err(e) if reflink_refused(&e) => {
                    if !self.reflink_unsupported.swap(true, Ordering::Relaxed) {
                        warn!(
                            "Reflinks unsupported here ({}); inputs are hard-linked to the blob cache, so an action that writes over one corrupts the cached blob",
                            e
                        );
                    }
                }
                Err(e) => return Err(link_error(source, dest, e).await),
            }
        }

        let use_hardlinks =
            self.config.use_hardlinks && !self.hardlink_failed_exdev.load(Ordering::Relaxed);

        if use_hardlinks {
            match hard_link_replacing(source, dest).await {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    return Err(link_error(source, dest, e).await);
                }
                Err(e) => {
                    let is_exdev =
                        e.kind() == ErrorKind::InvalidInput || e.raw_os_error() == Some(EXDEV);

                    if is_exdev {
                        if !self.hardlink_failed_exdev.swap(true, Ordering::Relaxed) {
                            info!("Cross-device link detected, switching to copy mode for remaining files (CAS and execroot on different filesystems)");
                        }
                    } else {
                        warn!("Hardlink failed ({}), falling back to copy", e);
                    }

                    copy_replacing(source, dest)
                        .await
                        .map_err(io_at("copy into", dest))?;
                }
            }
        } else {
            copy_replacing(source, dest)
                .await
                .map_err(io_at("copy into", dest))?;
        }

        set_executable(dest, is_executable).await
    }

    /// Removes `execroot` and everything under it; a missing execroot is not
    /// an error.
    pub async fn cleanup_execroot(&self, execroot: &Path) -> CasResult<()> {
        info!("Cleaning up execroot {}", execroot.display());

        if execroot.exists() {
            fs::remove_dir_all(execroot).await.map_err(CasError::Io)?;
        }

        Ok(())
    }
}

/// Maps an I/O error of `operation` on `path` to a [`CasError::Io`] that names
/// both, keeping its kind: callers still match on `NotFound`.
fn io_at<'a>(
    operation: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> CasError + 'a {
    move |e| {
        CasError::Io(std::io::Error::new(
            e.kind(),
            format!("{operation} {}: {e}", path.display()),
        ))
    }
}

/// A failed link of `source` at `dest`, saying which of the two sides was
/// missing: the source was evicted or replaced, or the destination's
/// directory is gone.
async fn link_error(source: &Path, dest: &Path, e: std::io::Error) -> CasError {
    let source_exists = fs::try_exists(source).await.unwrap_or(false);
    let parent_exists = match dest.parent() {
        Some(parent) => fs::try_exists(parent).await.unwrap_or(false),
        None => true,
    };
    CasError::Io(std::io::Error::new(
        e.kind(),
        format!(
            "link {} -> {}: {e} (source exists: {source_exists}, destination directory exists: {parent_exists})",
            source.display(),
            dest.display()
        ),
    ))
}

/// Sets the execute bits of `path` when `is_executable`. On a hard link this
/// changes the cached blob's mode too, which only ever adds execute bits.
async fn set_executable(dest: &Path, is_executable: bool) -> CasResult<()> {
    if is_executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(dest)
                .await
                .map_err(CasError::Io)?
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(dest, perms)
                .await
                .map_err(CasError::Io)?;
        }
    }
    Ok(())
}

/// `FICLONE` (`_IOW(0x94, 9, int)`): share all extents of one file with
/// another, copy-on-write. The same number on every Linux architecture.
#[cfg(target_os = "linux")]
const FICLONE: u32 = 0x4004_9409;

/// Whether a failed reflink means the filesystem cannot clone at all, as
/// opposed to a problem with these two files.
fn reflink_refused(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::Unsupported
        || matches!(
            e.raw_os_error(),
            Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL | libc::ENOSYS)
        )
}

/// Clones `source` to `dest` with a reflink, replacing a file already
/// there. A missing `source` is a `NotFound` error; a filesystem that
/// cannot clone fails as [`reflink_refused`] says.
async fn reflink_replacing(source: &Path, dest: &Path) -> std::io::Result<()> {
    let (source, dest) = (source.to_path_buf(), dest.to_path_buf());
    tokio::task::spawn_blocking(move || reflink_blocking(&source, &dest))
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(target_os = "linux")]
fn reflink_blocking(source: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let source = std::fs::File::open(source)?;
    match std::fs::remove_file(dest) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    // SAFETY: both descriptors outlive the call; FICLONE reads the source fd by value. pruner:keep
    let rc = unsafe { libc::ioctl(clone.as_raw_fd(), FICLONE as _, source.as_raw_fd()) };
    if rc == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    drop(clone);
    std::fs::remove_file(dest).ok();
    Err(e)
}

#[cfg(not(target_os = "linux"))]
fn reflink_blocking(_source: &Path, _dest: &Path) -> std::io::Result<()> {
    Err(ErrorKind::Unsupported.into())
}

/// Hard-links `source` at `dest`, replacing a file already there; a fresh
/// execroot takes one syscall per file.
async fn hard_link_replacing(source: &Path, dest: &Path) -> std::io::Result<()> {
    match fs::hard_link(source, dest).await {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(dest).await?;
            fs::hard_link(source, dest).await
        }
        other => other,
    }
}

/// Copies `source` to `dest` after removing what is there: copying onto a
/// file hard-linked from the blob cache would overwrite the cached blob.
async fn copy_replacing(source: &Path, dest: &Path) -> std::io::Result<()> {
    match fs::remove_file(dest).await {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::copy(source, dest).await.map(|_| ())
}

/// Creates `node` as a symlink in `dir`, replacing whatever is already there.
async fn materialize_symlink(node: &SymlinkNode, dir: &Path) -> CasResult<()> {
    let link = dir.join(single_path_component(&node.name)?);
    match fs::symlink(&node.target, &link).await {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(&link)
                .await
                .map_err(io_at("create symlink", &link))?;
            fs::symlink(&node.target, &link)
                .await
                .map_err(io_at("create symlink", &link))
        }
        other => other.map_err(io_at("create symlink", &link)),
    }
}

/// `name` when it names one entry of its directory, as a REAPI node name must.
fn single_path_component(name: &str) -> CasResult<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(CasError::InvalidData(format!(
            "invalid node name {name:?}: not a single path component"
        )));
    }
    Ok(name)
}

/// Counters accumulated while walking one tree.
#[derive(Debug, Default)]
struct MaterializationStats {
    file_count: usize,
    dir_count: usize,
    symlink_count: usize,
    bytes_downloaded: u64,
    cached_files: usize,
    input_bytes: u64,
}

impl MaterializationStats {
    fn add(&mut self, other: &Self) {
        self.input_bytes += other.input_bytes;
        self.file_count += other.file_count;
        self.dir_count += other.dir_count;
        self.symlink_count += other.symlink_count;
        self.bytes_downloaded += other.bytes_downloaded;
        self.cached_files += other.cached_files;
    }

    fn record(&mut self, placement: Placement) {
        self.file_count += 1;
        match placement {
            Placement::Reused => self.cached_files += 1,
            Placement::Downloaded(bytes) => self.bytes_downloaded += bytes,
        }
    }
}

/// Where one input file came from.
#[derive(Debug, Clone, Copy)]
enum Placement {
    /// Already in the execroot or the blob cache.
    Reused,
    /// Fetched from the CAS, this many bytes.
    Downloaded(u64),
}

fn digest_info_from_proto(
    digest: &crate::proto::build::bazel::remote::execution::v2::Digest,
) -> CasResult<DigestInfo> {
    if digest.hash.is_empty() {
        return Err(CasError::InvalidData(format!(
            "Digest hash is empty (size={})",
            digest.size_bytes
        )));
    }
    trace!(
        "Converting proto Digest to DigestInfo: hash={}, size={}",
        digest.hash,
        digest.size_bytes
    );
    DigestInfo::new(&digest.hash, digest.size_bytes)
        .map_err(|e| CasError::InvalidDigest(format!("Invalid digest '{}': {}", digest.hash, e)))
}

trait Decode: Sized {
    fn decode(data: Bytes) -> Result<Self, prost::DecodeError>;
}

impl Decode for Directory {
    fn decode(data: Bytes) -> Result<Self, prost::DecodeError> {
        prost::Message::decode(data)
    }
}

/// `source`, ending with an error once it yields no chunk for
/// [`CHUNK_READ_TIMEOUT`]: a stuck connection would otherwise hold the
/// download until its overall deadline.
fn with_chunk_timeout(
    source: BoxStream<'static, CasResult<Bytes>>,
) -> BoxStream<'static, CasResult<Bytes>> {
    stream::unfold(Some(source), |state| async move {
        let mut source = state?;
        match timeout(CHUNK_READ_TIMEOUT, source.next()).await {
            Ok(Some(chunk)) => Some((chunk, Some(source))),
            Ok(None) => None,
            Err(_) => Some((
                Err(CasError::Storage(format!(
                    "no data for {CHUNK_READ_TIMEOUT:?}; the connection may be stuck"
                ))),
                None,
            )),
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    #[test]
    fn inputs_group_by_npm_package_external_repo_or_directory() {
        let store = "bazel-out/k8-fastbuild/bin/node_modules/.aspect_rules_js";
        assert_eq!(
            input_group(&format!(
                "{store}/@ts-morph+common@0.25.0/node_modules/@ts-morph/common/dist"
            )),
            "npm:@ts-morph/common"
        );
        assert_eq!(
            input_group(&format!(
                "{store}/lucide-react@0.575.0_react@19.2.1/node_modules/lucide-react"
            )),
            "npm:lucide-react"
        );
        assert_eq!(
            input_group("external/rules_nodejs++node+nodejs_linux_arm64/bin/nodejs/bin"),
            "external/rules_nodejs++node+nodejs_linux_arm64"
        );
        assert_eq!(input_group("shop/portal/src/pages"), "shop/portal");
        assert_eq!(input_group("BUILD"), "BUILD");
        assert_eq!(input_group(""), "(root)");
    }

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_config_enables_hardlinks_and_validation() {
        let config = MaterializerConfig::default();
        assert_eq!(config.max_concurrent_downloads, 10);
        assert!(config.use_hardlinks);
        assert!(config.validate_digests);
        assert_eq!(config.download_chunk_size, 64 * 1024);
    }

    async fn disk_materializer() -> (
        TempDir,
        Arc<crate::cas::backends::DiskBackend>,
        Materializer,
    ) {
        let temp = TempDir::new().unwrap();
        let cas = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("store"))
                .await
                .unwrap(),
        );
        let blob_cache = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("cache"))
                .await
                .unwrap(),
        );
        let materializer =
            Materializer::new(cas.clone(), blob_cache, MaterializerConfig::default());
        (temp, cas, materializer)
    }

    /// A root `Directory` holding one regular file `name` with `content`,
    /// stored in `cas` together with the file's blob.
    async fn put_single_file(cas: &dyn CasBackend, name: &str, content: &[u8]) -> DigestInfo {
        use crate::proto::build::bazel::remote::execution::v2::Digest;
        let blob = DigestInfo::from_bytes(content);
        cas.write(&blob, Bytes::copy_from_slice(content))
            .await
            .unwrap();
        let root = Directory {
            files: vec![FileNode {
                name: name.into(),
                digest: Some(Digest {
                    hash: blob.hash_to_string(),
                    size_bytes: blob.size,
                }),
                is_executable: false,
                node_properties: None,
            }],
            ..Default::default()
        };
        put_directory(cas, &root).await
    }

    #[tokio::test]
    async fn the_blob_cache_evicts_past_its_budget_without_breaking_linked_execroots() {
        let temp = TempDir::new().unwrap();
        let cas = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("store"))
                .await
                .unwrap(),
        );
        let blob_cache = Arc::new(
            crate::cas::backends::DiskBackend::with_max_size_bytes(temp.path().join("cache"), 100)
                .await
                .unwrap(),
        );
        let materializer = Materializer::new(
            cas.clone(),
            blob_cache.clone(),
            MaterializerConfig::default(),
        );
        let first = [b'a'; 60];
        let second = [b'b'; 60];
        let first_root = put_single_file(cas.as_ref(), "out", &first).await;
        let second_root = put_single_file(cas.as_ref(), "out", &second).await;

        materializer
            .materialize_directory_recursive(&first_root, &temp.path().join("exec-1"))
            .await
            .unwrap();
        materializer
            .materialize_directory_recursive(&second_root, &temp.path().join("exec-2"))
            .await
            .unwrap();

        assert!(blob_cache.size_bytes() <= 100);
        let first_blob = DigestInfo::from_bytes(&first);
        assert!(!blob_cache.contains(&first_blob).await.unwrap());
        assert_eq!(
            std::fs::read(temp.path().join("exec-1/out")).unwrap(),
            first
        );

        let again = materializer
            .materialize_directory_recursive(&first_root, &temp.path().join("exec-3"))
            .await
            .unwrap();
        assert_eq!(
            again.bytes_downloaded, 60,
            "an evicted blob is downloaded again"
        );
        assert_eq!(
            std::fs::read(temp.path().join("exec-3/out")).unwrap(),
            first
        );
    }

    /// A remote CAS that answers every call after `latency`, like the
    /// in-cluster rbe-cache seen from a worker.
    struct LatencyCas {
        inner: Arc<crate::cas::backends::DiskBackend>,
        latency: Duration,
    }

    #[async_trait::async_trait]
    impl CasBackend for LatencyCas {
        async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
            tokio::time::sleep(self.latency).await;
            self.inner.contains(digest).await
        }
        async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
            tokio::time::sleep(self.latency).await;
            self.inner.read(digest).await
        }
        async fn read_stream(
            &self,
            digest: &DigestInfo,
            offset: usize,
            limit: Option<usize>,
        ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
            tokio::time::sleep(self.latency).await;
            self.inner.read_stream(digest, offset, limit).await
        }
        async fn batch_read(
            &self,
            digests: &[DigestInfo],
        ) -> CasResult<Vec<(DigestInfo, Option<Bytes>)>> {
            tokio::time::sleep(self.latency).await;
            let mut results = Vec::with_capacity(digests.len());
            for digest in digests {
                results.push((*digest, self.inner.read(digest).await?));
            }
            Ok(results)
        }
        async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
            self.inner.write(digest, data).await
        }
        async fn write_stream(
            &self,
            digest: &DigestInfo,
            stream: BoxStream<'static, CasResult<Bytes>>,
        ) -> CasResult<()> {
            self.inner.write_stream(digest, stream).await
        }
        async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
            self.inner.delete(digest).await
        }
        async fn local_path(&self, _digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
            Ok(None)
        }
    }

    /// A root with `dirs` subdirectories of `files_per_dir` distinct small
    /// files each, stored in `cas`.
    async fn put_wide_tree(cas: &dyn CasBackend, dirs: usize, files_per_dir: usize) -> DigestInfo {
        use crate::proto::build::bazel::remote::execution::v2::{Digest, DirectoryNode};
        let proto = |d: &DigestInfo| Digest {
            hash: d.hash_to_string(),
            size_bytes: d.size,
        };
        let mut children = Vec::with_capacity(dirs);
        for d in 0..dirs {
            let mut files = Vec::with_capacity(files_per_dir);
            for f in 0..files_per_dir {
                let content = format!("dir {d} file {f}").into_bytes();
                let blob = DigestInfo::from_bytes(&content);
                cas.write(&blob, Bytes::from(content)).await.unwrap();
                files.push(FileNode {
                    name: format!("f{f}.js"),
                    digest: Some(proto(&blob)),
                    is_executable: false,
                    node_properties: None,
                });
            }
            let dir = Directory {
                files,
                ..Default::default()
            };
            children.push(DirectoryNode {
                name: format!("d{d:05}"),
                digest: Some(proto(&put_directory(cas, &dir).await)),
            });
        }
        let root = Directory {
            directories: children,
            ..Default::default()
        };
        put_directory(cas, &root).await
    }

    #[tokio::test]
    async fn with_reflinks_writing_over_an_input_leaves_the_cached_blob_intact() {
        let temp = TempDir::new().unwrap();
        let cas = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("store"))
                .await
                .unwrap(),
        );
        let blob_cache = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("cache"))
                .await
                .unwrap(),
        );
        let materializer = Materializer::new(
            cas.clone(),
            blob_cache.clone(),
            MaterializerConfig::default(),
        );
        let content = b"{\"name\": \"pkg\"}\n";
        let root = put_single_file(cas.as_ref(), "package.json", content).await;

        materializer
            .materialize_directory_recursive(&root, &temp.path().join("exec-1"))
            .await
            .unwrap();
        if materializer.reflink_unsupported.load(Ordering::Relaxed) {
            eprintln!("no reflinks on this filesystem; inputs share the cached inode");
            return;
        }
        std::fs::write(temp.path().join("exec-1/package.json"), b"rewritten").unwrap();

        let blob = DigestInfo::from_bytes(content);
        let cached = blob_cache.local_path(&blob).await.unwrap().unwrap();
        assert_eq!(std::fs::read(cached).unwrap(), content);
    }

    #[tokio::test]
    async fn a_truncated_blob_in_the_cache_is_downloaded_again() {
        let temp = TempDir::new().unwrap();
        let cas = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("store"))
                .await
                .unwrap(),
        );
        let blob_cache = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("cache"))
                .await
                .unwrap(),
        );
        let materializer = Materializer::new(
            cas.clone(),
            blob_cache.clone(),
            MaterializerConfig::default(),
        );
        let content = b"module.exports = 42;\n";
        let root = put_single_file(cas.as_ref(), "index.js", content).await;

        materializer
            .materialize_directory_recursive(&root, &temp.path().join("exec-1"))
            .await
            .unwrap();
        let blob = DigestInfo::from_bytes(content);
        let cached = blob_cache.local_path(&blob).await.unwrap().unwrap();
        std::fs::write(cached, b"").unwrap();

        let second = materializer
            .materialize_directory_recursive(&root, &temp.path().join("exec-2"))
            .await
            .unwrap();
        assert_eq!(second.bytes_downloaded, content.len() as u64);
        assert_eq!(
            std::fs::read(temp.path().join("exec-2/index.js")).unwrap(),
            content
        );
    }

    #[tokio::test]
    async fn a_wide_tree_is_materialized_completely_and_its_directories_are_cached() {
        let (temp, cas, materializer) = disk_materializer().await;
        let root = put_wide_tree(cas.as_ref(), 50, 6).await;

        let first = materializer
            .materialize_directory_recursive(&root, &temp.path().join("exec-1"))
            .await
            .unwrap();
        assert_eq!(
            (first.dir_count, first.file_count, first.cached_files),
            (51, 300, 0)
        );
        for d in [0, 17, 49] {
            for f in [0, 5] {
                let file = temp.path().join(format!("exec-1/d{d:05}/f{f}.js"));
                assert_eq!(
                    std::fs::read_to_string(file).unwrap(),
                    format!("dir {d} file {f}")
                );
            }
        }

        cas.delete(&root).await.unwrap();
        let second = materializer
            .materialize_directory_recursive(&root, &temp.path().join("exec-2"))
            .await
            .unwrap();
        assert_eq!((second.file_count, second.cached_files), (300, 300));
        assert_eq!(second.bytes_downloaded, 0);
    }

    #[tokio::test]
    async fn a_file_or_directory_named_dot_dot_is_refused() {
        use crate::proto::build::bazel::remote::execution::v2::{Digest, DirectoryNode};
        let (temp, cas, materializer) = disk_materializer().await;
        let escaping_file = put_single_file(cas.as_ref(), "..", b"x").await;
        assert!(materializer
            .materialize_directory_recursive(&escaping_file, &temp.path().join("exec-1"))
            .await
            .is_err());

        let empty = put_directory(cas.as_ref(), &Directory::default()).await;
        let escaping_dir = put_directory(
            cas.as_ref(),
            &Directory {
                directories: vec![DirectoryNode {
                    name: "..".into(),
                    digest: Some(Digest {
                        hash: empty.hash_to_string(),
                        size_bytes: empty.size,
                    }),
                }],
                ..Default::default()
            },
        )
        .await;
        assert!(materializer
            .materialize_directory_recursive(&escaping_dir, &temp.path().join("exec-2"))
            .await
            .is_err());
    }

    /// Materializes a node_modules-shaped tree (the worst action measured had
    /// 12,774 directories and 77,767 files, ~6 per directory) from a remote
    /// with 1 ms per call, cold and then with every file in the blob cache.
    /// `bazel test //:rbe_lib_test --test_arg=--ignored --test_arg=bench_
    /// --test_output=streamed`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn bench_materialize_a_wide_tree() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("store"))
                .await
                .unwrap(),
        );
        let root = put_wide_tree(store.as_ref(), 2_000, 6).await;
        let remote = Arc::new(LatencyCas {
            inner: store,
            latency: Duration::from_millis(1),
        });
        let blob_cache = Arc::new(
            crate::cas::backends::DiskBackend::new(temp.path().join("cache"))
                .await
                .unwrap()
                .without_fsync(),
        );
        let materializer = Materializer::new(remote, blob_cache, MaterializerConfig::default());
        for run in ["cold", "warm"] {
            let started = std::time::Instant::now();
            let stats = materializer
                .materialize_directory_recursive(&root, &temp.path().join(format!("exec-{run}")))
                .await
                .unwrap();
            eprintln!(
                "BENCH {run}: {:?} for {} files ({} cached) in {} directories",
                started.elapsed(),
                stats.file_count,
                stats.cached_files,
                stats.dir_count
            );
        }
    }

    async fn put_directory(cas: &dyn CasBackend, dir: &Directory) -> DigestInfo {
        let bytes = prost::Message::encode_to_vec(dir);
        let digest = DigestInfo::from_bytes(&bytes);
        cas.write(&digest, Bytes::from(bytes)).await.unwrap();
        digest
    }

    fn symlink(name: &str, target: &str) -> SymlinkNode {
        SymlinkNode {
            name: name.into(),
            target: target.into(),
            node_properties: None,
        }
    }

    #[tokio::test]
    async fn symlinks_are_created_with_their_target_as_sent_at_any_depth() {
        use crate::proto::build::bazel::remote::execution::v2::{Digest, DirectoryNode};
        let (temp, cas, materializer) = disk_materializer().await;
        let pkg = Directory {
            symlinks: vec![symlink("up", "../not/there/yet")],
            ..Default::default()
        };
        let pkg_digest = put_directory(cas.as_ref(), &pkg).await;
        let root = Directory {
            directories: vec![DirectoryNode {
                name: "pkg".into(),
                digest: Some(Digest {
                    hash: pkg_digest.hash_to_string(),
                    size_bytes: pkg_digest.size,
                }),
            }],
            symlinks: vec![symlink("absolute", "/nonexistent/abs")],
            ..Default::default()
        };
        let root_digest = put_directory(cas.as_ref(), &root).await;
        let execroot = temp.path().join("exec");

        let stats = materializer
            .materialize_directory_recursive(&root_digest, &execroot)
            .await
            .unwrap();

        assert_eq!(stats.symlink_count, 2);
        assert_eq!(
            std::fs::read_link(execroot.join("absolute")).unwrap(),
            PathBuf::from("/nonexistent/abs")
        );
        assert_eq!(
            std::fs::read_link(execroot.join("pkg/up")).unwrap(),
            PathBuf::from("../not/there/yet"),
            "a dangling relative target is kept, not resolved"
        );
    }

    #[tokio::test]
    async fn a_symlink_name_that_is_not_one_path_component_is_refused() {
        let (temp, cas, materializer) = disk_materializer().await;
        let root = Directory {
            symlinks: vec![symlink("../escape", "target")],
            ..Default::default()
        };
        let root_digest = put_directory(cas.as_ref(), &root).await;

        let result = materializer
            .materialize_directory_recursive(&root_digest, &temp.path().join("exec"))
            .await;

        assert!(result.is_err());
        assert!(!temp.path().join("escape").exists());
    }
}
