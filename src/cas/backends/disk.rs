//! Disk-based CAS Backend
//!
//! Stores blobs in a local filesystem using content-addressed storage.
//! Blobs are organized in a two-level directory structure to avoid
//! having too many files in a single directory.
//!
//! Directory structure: `<root>/aa/bb/aabbccdd...`
//! where `aa` is the first 2 hex chars of the hash and `bb` is the next 2.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use sha2::{Digest as Sha2Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;

/// Disk-based CAS backend
pub struct DiskBackend {
    /// Root directory for blob storage
    root: PathBuf,
    /// Directory for temporary files during writes
    temp_dir: PathBuf,
}

impl DiskBackend {
    /// Create a new disk backend with the given root directory
    #[allow(dead_code)]
    pub async fn new(root: impl AsRef<Path>) -> CasResult<Self> {
        let root = root.as_ref().to_path_buf();
        let temp_dir = root.join(".tmp");
        
        fs::create_dir_all(&root).await?;
        fs::create_dir_all(&temp_dir).await?;
        
        info!("Initialized DiskBackend at {:?}", root);
        
        Ok(Self { root, temp_dir })
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
    
    /// Compute SHA256 digest from a stream
    #[allow(dead_code)]
    async fn compute_digest_from_stream(
        mut stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<(String, Vec<u8>)> {
        let mut hasher = Sha256::new();
        let mut all_data = Vec::new();
        
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            hasher.update(&chunk);
            all_data.extend_from_slice(&chunk);
        }
        
        Ok((hex::encode(hasher.finalize()), all_data))
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
                        digest.hash_to_string(), expected_size, metadata.len()
                    );
                    return Ok(false);
                }
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
                    return Ok(None);
                }
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
            debug!("Blob {} already exists, skipping write", digest.hash_to_string());
            return Ok(());
        }
        
        if let Err(e) = self.verify_digest(&data, digest) {
            error!("Digest verification failed for {}: {}", digest.hash_to_string(), e);
            return Err(e);
        }
        
        let final_path = self.blob_path(digest);
        let temp_path = self.temp_path();
        
        Self::ensure_parent(&final_path).await?;
        
        let mut file = File::create(&temp_path).await?;
        file.write_all(&data).await?;
        file.sync_all().await?;
        drop(file);
        
        match fs::rename(&temp_path, &final_path).await {
            Ok(()) => {
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
            debug!("Blob {} already exists, skipping stream write", digest.hash_to_string());
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
        
        let computed_hash = hex::encode(hasher.finalize());
        if computed_hash != digest.hash_to_string() {
            fs::remove_file(&temp_path).await.ok();
            return Err(CasError::digest_mismatch(&digest.hash_to_string(), &computed_hash));
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
                info!("Wrote blob {} ({} bytes, expected {} bytes) to {:?}", 
                    digest.hash_to_string(), total_size, digest.size, final_path);
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
        match fs::remove_file(&path).await {
            Ok(()) => {
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
            Ok(_) => Ok(Some(path)),
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
        
        backend.write(&digest, Bytes::from_static(data)).await.unwrap();
        
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
        
        let digest = DigestInfo::new("nonexistent", 0);
        
        assert!(!backend.contains(&digest).await.unwrap());
        assert!(backend.read(&digest).await.unwrap().is_none());
    }
    
    #[tokio::test]
    async fn test_disk_backend_digest_verification() {
        let temp_dir = TempDir::new().unwrap();
        let backend = DiskBackend::new(temp_dir.path()).await.unwrap();
        
        let data = b"test data";
        let wrong_digest = DigestInfo::new("wronghash", data.len() as i64);
        
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
}
