//! Merkle Tree Materializer
//!
//! Reconstructs Bazel's execroot by downloading and assembling directories
//! from the Content Addressable Storage (CAS).
//!
//! Bazel uses a Merkle tree structure where:
//! - Each Directory contains files (with digests) and subdirectories (with digests)
//! - The Tree represents the complete input root for an action
//! - Files are content-addressed and stored in CAS
//!
//! This module efficiently materializes the tree using:
//! - Hardlinks/symlinks when files exist locally (zero-copy)
//! - Parallel downloads for missing files
//! - Digest validation on every file

use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest as Sha2Digest, Sha256};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, error, info, trace, warn};

use crate::cas::{CasBackend, CasError, CasResult};
use crate::proto::build::bazel::remote::execution::v2::{Directory, FileNode};
use crate::types::DigestInfo;

/// Environment variable name for streaming threshold (bytes)
pub const ENV_STREAMING_THRESHOLD: &str = "RBE_STREAMING_THRESHOLD";
/// Environment variable name for download timeout
pub const ENV_DOWNLOAD_TIMEOUT_SECS: &str = "RBE_DOWNLOAD_TIMEOUT_SECS";
/// Environment variable name for max concurrent downloads  
pub const ENV_MAX_CONCURRENT_DOWNLOADS: &str = "RBE_MAX_CONCURRENT_DOWNLOADS";
/// Environment variable name for download chunk size
pub const ENV_DOWNLOAD_CHUNK_SIZE: &str = "RBE_DOWNLOAD_CHUNK_SIZE";

/// Configuration for the materializer
#[derive(Debug, Clone)]
pub struct MaterializerConfig {
    /// Maximum concurrent downloads
    pub max_concurrent_downloads: usize,
    /// Use hardlinks when possible (if false, always copy)
    pub use_hardlinks: bool,
    /// Validate file digests after download
    pub validate_digests: bool,
    /// Chunk size for streaming downloads (64KB)
    pub download_chunk_size: usize,
    /// Timeout for each file download (seconds)
    pub download_timeout: Duration,
    /// Threshold for switching to streaming mode (bytes)
    pub streaming_threshold: i64,
}

impl Default for MaterializerConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl MaterializerConfig {
    /// Create configuration from environment variables (12-Factor App style)
    pub fn from_env() -> Self {
        let max_concurrent_downloads = std::env::var(ENV_MAX_CONCURRENT_DOWNLOADS)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        
        let download_chunk_size = std::env::var(ENV_DOWNLOAD_CHUNK_SIZE)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64 * 1024);
        
        let download_timeout_secs = std::env::var(ENV_DOWNLOAD_TIMEOUT_SECS)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);
        
        let streaming_threshold = std::env::var(ENV_STREAMING_THRESHOLD)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4 * 1024 * 1024);
        
        let use_hardlinks = std::env::var("RBE_USE_HARDLINKS")
            .map(|s| s != "false" && s != "0")
            .unwrap_or(true);
        
        let validate_digests = std::env::var("RBE_VALIDATE_DIGESTS")
            .map(|s| s != "false" && s != "0")
            .unwrap_or(true);
        
        Self {
            max_concurrent_downloads,
            use_hardlinks,
            validate_digests,
            download_chunk_size,
            download_timeout: Duration::from_secs(download_timeout_secs),
            streaming_threshold,
        }
    }
}

/// Tracks the state of file materialization
#[derive(Debug, Clone)]
pub enum FileState {
    /// File already exists locally and is valid
    Cached(PathBuf),
    /// File needs to be downloaded
    NeedDownload(DigestInfo),
    /// File is being downloaded by another task
    InProgress,
}

/// Result of materializing an input root
#[derive(Debug)]
pub struct MaterializedRoot {
    /// Path to the root directory (execroot)
    pub execroot: PathBuf,
    /// Total number of files materialized
    pub file_count: usize,
    /// Total number of directories created
    pub dir_count: usize,
    /// Total bytes downloaded
    pub bytes_downloaded: u64,
    /// Files that were already cached
    pub cached_files: usize,
}

/// Materializes Merkle trees from CAS to local filesystem
pub struct Materializer {
    cas_backend: Arc<dyn CasBackend>,
    config: MaterializerConfig,
    /// Local cache of CAS files (digest -> path)
    cas_cache_dir: PathBuf,
    /// Semaphore to limit concurrent downloads
    download_semaphore: Arc<Semaphore>,
    /// Flag to detect cross-device link failures and avoid spamming logs
    hardlink_failed_exdev: AtomicBool,
}

impl Materializer {
    pub fn new(
        cas_backend: Arc<dyn CasBackend>,
        cas_cache_dir: PathBuf,
        config: MaterializerConfig,
    ) -> Self {
        let download_semaphore = Arc::new(Semaphore::new(config.max_concurrent_downloads));
        
        Self {
            cas_backend,
            config,
            cas_cache_dir,
            download_semaphore,
            hardlink_failed_exdev: AtomicBool::new(false),
        }
    }

    /// Materialize an input root from a root directory digest
    ///
    /// Alternative entry point when we only have the root directory digest
    /// (not a full Tree). This requires recursive fetching.
    pub async fn materialize_directory_recursive(
        &self,
        root_digest: &DigestInfo,
        execroot: &Path,
    ) -> CasResult<MaterializedRoot> {
        info!("Materializing directory {} to {}", root_digest.hash_to_string(), execroot.display());

        fs::create_dir_all(execroot).await.map_err(CasError::Io)?;

        let root_dir = self.fetch_directory(root_digest).await?;

        let mut stats = MaterializationStats::default();
        self.materialize_directory_contents(
            &root_dir,
            root_digest,
            execroot,
            &mut stats,
        ).await?;

        info!(
            "Materialized {} files ({} cached) in {} directories, {} bytes downloaded",
            stats.file_count, stats.cached_files, stats.dir_count, stats.bytes_downloaded
        );

        Ok(MaterializedRoot {
            execroot: execroot.to_path_buf(),
            file_count: stats.file_count,
            dir_count: stats.dir_count,
            bytes_downloaded: stats.bytes_downloaded,
            cached_files: stats.cached_files,
        })
    }

    /// Fetch a Directory from CAS
    async fn fetch_directory(&self, digest: &DigestInfo) -> CasResult<Directory> {
        trace!("Fetching directory {}", digest.hash_to_string());
        
        let data = self.cas_backend
            .read(digest)
            .await?
            .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;

        if self.config.validate_digests {
            let computed = DigestInfo::from_bytes(&data);
            if computed.hash != digest.hash {
                return Err(CasError::digest_mismatch(
                    &digest.hash_to_string(),
                    &computed.hash_to_string(),
                ));
            }
        }

        let dir = Directory::decode(data).map_err(|e| {
            CasError::InvalidData(format!("Failed to decode Directory: {}", e))
        })?;

        Ok(dir)
    }

    /// Materialize a directory to the given path
    async fn materialize_directory(
        &self,
        dir: &Directory,
        path: &Path,
        stats: &mut MaterializationStats,
    ) -> CasResult<()> {
        trace!("Materializing directory to {}", path.display());

        fs::create_dir_all(path).await.map_err(CasError::Io)?;
        stats.dir_count += 1;

        for file_node in &dir.files {
            self.materialize_file(file_node, path, stats).await?;
        }

        for dir_node in &dir.directories {
            let child_path = path.join(&dir_node.name);
            let child_digest = digest_info_from_proto(dir_node.digest.as_ref().unwrap())?;
            let child_dir = self.fetch_directory(&child_digest).await?;
            
            self.materialize_directory_contents(
                &child_dir,
                &child_digest,
                &child_path,
                stats,
            ).await?;
        }

        Ok(())
    }

    /// Materialize directory contents (used for recursive calls)
    async fn materialize_directory_contents(
        &self,
        dir: &Directory,
        _dir_digest: &DigestInfo,
        path: &Path,
        stats: &mut MaterializationStats,
    ) -> CasResult<()> {
        trace!("Materializing directory contents to {}", path.display());

        fs::create_dir_all(path).await.map_err(CasError::Io)?;
        stats.dir_count += 1;

        for file_node in &dir.files {
            self.materialize_file(file_node, path, stats).await?;
        }

        for dir_node in &dir.directories {
            let child_path = path.join(&dir_node.name);
            let child_digest = digest_info_from_proto(dir_node.digest.as_ref().unwrap())?;
            let child_dir = self.fetch_directory(&child_digest).await?;
            
            Box::pin(self.materialize_directory_contents(
                &child_dir,
                &child_digest,
                &child_path,
                stats,
            )).await?;
        }

        Ok(())
    }

    /// Materialize a single file
    async fn materialize_file(
        &self,
        file_node: &FileNode,
        parent_path: &Path,
        stats: &mut MaterializationStats,
    ) -> CasResult<()> {
        let file_path = parent_path.join(&file_node.name);
        let digest = digest_info_from_proto(file_node.digest.as_ref().unwrap())?;
        
        trace!("Materializing file {} ({})", file_path.display(), digest.hash_to_string());

        if self.validate_existing_file(&file_path, &digest).await? {
            trace!("File {} already exists and is valid", file_path.display());
            stats.file_count += 1;
            stats.cached_files += 1;
            return Ok(());
        }

        let _permit = self.download_semaphore.acquire().await;

        let cas_path = self.cas_cache_path(&digest);
        if cas_path.exists() {
            self.link_file(&cas_path, &file_path, file_node.is_executable).await?;
            stats.file_count += 1;
            stats.cached_files += 1;
            return Ok(());
        }

        debug!("Downloading file {} from CAS (size: {} bytes)", digest.hash_to_string(), digest.size);
        
        if digest.size > self.config.streaming_threshold {
            self.materialize_file_streaming(&digest, &cas_path, &file_path, file_node.is_executable, stats).await?;
        } else {
            self.materialize_file_buffered(&digest, &cas_path, &file_path, file_node.is_executable, stats).await?;
        }

        Ok(())
    }

    /// Materialize a small file using in-memory buffering (faster for small files)
    async fn materialize_file_buffered(
        &self,
        digest: &DigestInfo,
        cas_path: &Path,
        file_path: &Path,
        is_executable: bool,
        stats: &mut MaterializationStats,
    ) -> CasResult<()> {
        let data = self.cas_backend
            .read(digest)
            .await?
            .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;

        if self.config.validate_digests {
            let computed = DigestInfo::from_bytes(&data);
            if computed.hash != digest.hash {
                return Err(CasError::digest_mismatch(
                    &digest.hash_to_string(),
                    &computed.hash_to_string(),
                ));
            }
        }

        fs::create_dir_all(cas_path.parent().unwrap()).await.map_err(CasError::Io)?;
        fs::write(cas_path, &data).await.map_err(CasError::Io)?;

        self.link_file(cas_path, file_path, is_executable).await?;

        stats.file_count += 1;
        stats.bytes_downloaded += data.len() as u64;

        Ok(())
    }

    /// Materialize a large file using streaming - O(1) memory regardless of file size
    /// Uses timeout per chunk to detect stuck connections
    async fn materialize_file_streaming(
        &self,
        digest: &DigestInfo,
        cas_path: &Path,
        file_path: &Path,
        is_executable: bool,
        stats: &mut MaterializationStats,
    ) -> CasResult<()> {
        let stream = self.cas_backend
            .read_stream(digest, 0, None)
            .await?
            .ok_or_else(|| CasError::NotFound(digest.hash_to_string()))?;

        fs::create_dir_all(cas_path.parent().unwrap()).await.map_err(CasError::Io)?;
        
        let temp_path = cas_path.with_extension(".tmp");
        let mut file = fs::File::create(&temp_path).await.map_err(CasError::Io)?;
        
        let mut stream = stream;
        let mut total_bytes: u64 = 0;
        let mut hasher = Sha256::new();
        let start_time = std::time::Instant::now();
        let timeout_duration = self.config.download_timeout;
        
        info!("Starting streaming download for {} (timeout: {:?})", 
              digest.hash_to_string(), timeout_duration);
        
        loop {
            if start_time.elapsed() > timeout_duration {
                fs::remove_file(&temp_path).await.ok();
                error!("Download timeout for {} after {:?}", digest.hash_to_string(), timeout_duration);
                return Err(CasError::Storage(format!(
                    "Download timeout after {:?}", timeout_duration
                )));
            }
            
            match timeout(Duration::from_secs(30), stream.next()).await {
                Ok(Some(chunk_result)) => {
                    let chunk = chunk_result?;
                    file.write_all(&chunk).await.map_err(CasError::Io)?;
                    hasher.update(&chunk);
                    total_bytes += chunk.len() as u64;
                }
                Ok(None) => break,
                Err(_) => {
                    fs::remove_file(&temp_path).await.ok();
                    error!("Chunk read timeout for {} (no data for 30s)", digest.hash_to_string());
                    return Err(CasError::Storage(
                        "Chunk read timeout - connection may be stuck".to_string()
                    ));
                }
            }
        }
        
        file.flush().await.map_err(CasError::Io)?;
        drop(file);
        
        info!("Downloaded {} ({} bytes) in {:?}", 
              digest.hash_to_string(), total_bytes, start_time.elapsed());
        
        if self.config.validate_digests {
            let computed_hash = hex::encode(hasher.finalize());
            if computed_hash != digest.hash_to_string() {
                fs::remove_file(&temp_path).await.ok();
                return Err(CasError::digest_mismatch(
                    &digest.hash_to_string(),
                    &computed_hash,
                ));
            }
        }
        
        fs::rename(&temp_path, cas_path).await.map_err(CasError::Io)?;
        
        self.link_file(cas_path, file_path, is_executable).await?;
        
        stats.file_count += 1;
        stats.bytes_downloaded += total_bytes;
        
        info!("Streamed large file {} ({} bytes) to CAS cache", digest.hash_to_string(), total_bytes);
        
        Ok(())
    }

    /// Check if an existing file is valid (exists and digest matches)
    /// Uses streaming digest computation for O(1) memory on large files
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
                let n = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await.map_err(CasError::Io)?;
                if n == 0 { break; }
                hasher.update(&buffer[..n]);
            }
            
            let computed_hash = hex::encode(hasher.finalize());
            Ok(computed_hash == expected_digest.hash_to_string())
        } else {
            Ok(true)
        }
    }

    /// Create a link (hardlink or copy) from source to destination
    /// Automatically detects cross-device link failures and falls back to copy silently.
    async fn link_file(
        &self,
        source: &Path,
        dest: &Path,
        is_executable: bool,
    ) -> CasResult<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await.map_err(CasError::Io)?;
        }

        if dest.exists() {
            fs::remove_file(dest).await.map_err(CasError::Io)?;
        }

        let use_hardlinks = self.config.use_hardlinks 
            && !self.hardlink_failed_exdev.load(Ordering::Relaxed);

        if use_hardlinks {
            match fs::hard_link(source, dest).await {
                Ok(()) => {}
                Err(e) => {
                    let is_exdev = e.kind() == ErrorKind::InvalidInput || 
                                   e.raw_os_error() == Some(18);
                    
                    if is_exdev {
                        if !self.hardlink_failed_exdev.swap(true, Ordering::Relaxed) {
                            info!("Cross-device link detected, switching to copy mode for remaining files (CAS and execroot on different filesystems)");
                        }
                    } else {
                        warn!("Hardlink failed ({}), falling back to copy", e);
                    }
                    
                    fs::copy(source, dest).await.map_err(CasError::Io)?;
                }
            }
        } else {
            fs::copy(source, dest).await.map_err(CasError::Io)?;
        }

        if is_executable {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(dest).await.map_err(CasError::Io)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                fs::set_permissions(dest, perms).await.map_err(CasError::Io)?;
            }
        }

        Ok(())
    }

    /// Get the local CAS cache path for a digest
    fn cas_cache_path(&self, digest: &DigestInfo) -> PathBuf {
        let hash_str = digest.hash_to_string();
        let prefix1 = &hash_str[0..2];
        let prefix2 = &hash_str[2..4];
        self.cas_cache_dir
            .join(prefix1)
            .join(prefix2)
            .join(&hash_str)
    }

    /// Clean up the execroot after execution
    pub async fn cleanup_execroot(&self, execroot: &Path) -> CasResult<()> {
        info!("Cleaning up execroot {}", execroot.display());
        
        if execroot.exists() {
            fs::remove_dir_all(execroot).await.map_err(CasError::Io)?;
        }

        Ok(())
    }
}

/// Statistics during materialization
#[derive(Debug, Default)]
struct MaterializationStats {
    file_count: usize,
    dir_count: usize,
    bytes_downloaded: u64,
    cached_files: usize,
}

/// Helper function to convert protobuf Digest to DigestInfo
fn digest_info_from_proto(
    digest: &crate::proto::build::bazel::remote::execution::v2::Digest
) -> CasResult<DigestInfo> {
    if digest.hash.is_empty() {
        return Err(CasError::InvalidData(
            format!("Digest hash is empty (size={})", digest.size_bytes)
        ));
    }
    trace!("Converting proto Digest to DigestInfo: hash={}, size={}", digest.hash, digest.size_bytes);
    Ok(DigestInfo::new(&digest.hash, digest.size_bytes))
}

/// Helper trait to decode protobuf messages
trait Decode: Sized {
    fn decode(data: Bytes) -> Result<Self, prost::DecodeError>;
}

impl Decode for Directory {
    fn decode(data: Bytes) -> Result<Self, prost::DecodeError> {
        prost::Message::decode(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_materializer_config_default() {
        let config = MaterializerConfig::default();
        assert_eq!(config.max_concurrent_downloads, 10);
        assert!(config.use_hardlinks);
        assert!(config.validate_digests);
        assert_eq!(config.download_chunk_size, 64 * 1024);
    }

    #[test]
    fn test_cas_cache_path() {
        let temp_dir = TempDir::new().unwrap();
        let config = MaterializerConfig::default();
        let materializer = Materializer::new(
            Arc::new(MockCasBackend),
            temp_dir.path().join("cas"),
            config,
        );

        let digest = DigestInfo::new("aabbccdd1122", 100);
        let path = materializer.cas_cache_path(&digest);
        
        assert!(path.to_string_lossy().contains("aa/bb/aabbccdd1122"));
    }

    struct MockCasBackend;
    
    #[async_trait::async_trait]
    impl CasBackend for MockCasBackend {
        async fn contains(&self, _digest: &DigestInfo) -> CasResult<bool> {
            Ok(false)
        }
        
        async fn read(&self, _digest: &DigestInfo) -> CasResult<Option<Bytes>> {
            Ok(None)
        }
        
        async fn read_stream(
            &self,
            _digest: &DigestInfo,
            _offset: usize,
            _limit: Option<usize>,
        ) -> CasResult<Option<futures::stream::BoxStream<'static, CasResult<Bytes>>>> {
            Ok(None)
        }
        
        async fn write(&self, _digest: &DigestInfo, _data: Bytes) -> CasResult<()> {
            Ok(())
        }
        
        async fn write_stream(
            &self,
            _digest: &DigestInfo,
            _stream: futures::stream::BoxStream<'static, CasResult<Bytes>>,
        ) -> CasResult<()> {
            Ok(())
        }
        
        async fn delete(&self, _digest: &DigestInfo) -> CasResult<()> {
            Ok(())
        }
        
        async fn local_path(&self, _digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
            Ok(None)
        }
    }
}
