//! Content Addressable Storage (CAS) Backend
//!
//! Provides a unified interface for storing and retrieving blobs by their content digest.
//! All CAS operations are async and support both complete blobs and streaming access.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::sync::Arc;

pub mod backends;
pub mod error;

pub use error::CasError;

/// Result type for CAS operations
pub type CasResult<T> = Result<T, CasError>;

/// Content Addressable Storage backend trait
///
/// Implementations must be thread-safe (Send + Sync) and support concurrent access.
/// All operations are async and may involve network or disk I/O.
#[async_trait]
pub trait CasBackend: Send + Sync + 'static {
    /// Check if a blob with the given digest exists in storage
    async fn contains(&self, digest: &crate::types::DigestInfo) -> CasResult<bool>;

    /// Read a complete blob from storage
    ///
    /// Returns `Ok(None)` if the blob doesn't exist.
    /// For large blobs, consider using `read_stream` instead.
    async fn read(&self, digest: &crate::types::DigestInfo) -> CasResult<Option<Bytes>>;

    /// Read a blob as a stream of chunks
    ///
    /// This is the preferred method for reading large blobs as it allows
    /// streaming without loading the entire blob into memory.
    ///
    /// # Arguments
    /// * `digest` - The digest of the blob to read
    /// * `offset` - Byte offset to start reading from
    /// * `limit` - Optional maximum number of bytes to read
    async fn read_stream(
        &self,
        digest: &crate::types::DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>>;

    /// Write a complete blob to storage
    ///
    /// The implementation must verify that the data matches the provided digest.
    /// If the digest doesn't match, the operation fails with `CasError::DigestMismatch`.
    async fn write(&self, digest: &crate::types::DigestInfo, data: Bytes) -> CasResult<()>;

    /// Write a blob from a stream of chunks
    ///
    /// This is the preferred method for writing large blobs as it allows
    /// streaming without loading the entire blob into memory.
    ///
    /// The implementation must:
    /// 1. Verify the digest matches the written data
    /// 2. Store atomically (either complete successfully or not at all)
    /// 3. Support concurrent writes of different blobs
    async fn write_stream(
        &self,
        digest: &crate::types::DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()>;

    /// Delete a blob from storage
    ///
    /// Returns `Ok(())` even if the blob doesn't exist (idempotent).
    async fn delete(&self, digest: &crate::types::DigestInfo) -> CasResult<()>;

    /// Get the local filesystem path for a blob (if available)
    ///
    /// This is an optimization for local disk backends that allows
    /// efficient hardlinking without copying data.
    ///
    /// Returns `Ok(None)` if the blob doesn't exist or if the backend
    /// doesn't support local paths (e.g., S3).
    async fn local_path(
        &self,
        digest: &crate::types::DigestInfo,
    ) -> CasResult<Option<std::path::PathBuf>>;
}

/// Type alias for a shared CAS backend reference
pub type SharedCasBackend = Arc<dyn CasBackend>;
