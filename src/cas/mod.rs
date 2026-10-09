//! Content Addressable Storage.
//!
//! [`CasBackend`] is the storage contract behind the REAPI CAS and ByteStream
//! services: blobs are addressed by the SHA-256 of their content and are
//! immutable once stored.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::path::PathBuf;
use std::sync::Arc;

use crate::types::DigestInfo;

pub mod backends;
pub mod error;

pub use error::CasError;

/// Result type of CAS operations.
pub type CasResult<T> = Result<T, CasError>;

/// Content-addressed blob store.
///
/// Implementations are shared across request handlers and must support
/// concurrent calls on any mix of digests. The batch operations have
/// sequential default implementations; backends with a native batch API
/// override them to use a single round trip.
#[async_trait]
pub trait CasBackend: Send + Sync + 'static {
    /// Whether a blob with this digest is stored.
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool>;

    /// The whole blob, or `None` if it is not stored. Large blobs should be
    /// read with [`CasBackend::read_stream`].
    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>>;

    /// The blob as a stream of chunks, starting at byte `offset` and yielding
    /// at most `limit` bytes; `None` if it is not stored.
    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>>;

    /// Stores `data` under `digest`.
    ///
    /// Fails with [`CasError::DigestMismatch`] when `data` does not hash to
    /// `digest`; nothing is stored in that case.
    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()>;

    /// Stores the concatenated chunks of `stream` under `digest`.
    ///
    /// The blob becomes visible only once complete and verified against the
    /// digest; a failed or mismatched write leaves nothing behind.
    async fn write_stream(
        &self,
        digest: &DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()>;

    /// The subset of `digests` that is not stored, in input order.
    async fn find_missing(&self, digests: &[DigestInfo]) -> CasResult<Vec<DigestInfo>> {
        let mut missing = Vec::new();
        for digest in digests {
            if !self.contains(digest).await? {
                missing.push(*digest);
            }
        }
        Ok(missing)
    }

    /// Each digest paired with its blob, or `None` where it is not stored.
    async fn batch_read(
        &self,
        digests: &[DigestInfo],
    ) -> CasResult<Vec<(DigestInfo, Option<Bytes>)>> {
        let mut results = Vec::with_capacity(digests.len());
        for digest in digests {
            let data = self.read(digest).await?;
            results.push((*digest, data));
        }
        Ok(results)
    }

    /// Stores every item and reports the outcome of each one separately.
    async fn batch_write(
        &self,
        items: &[(DigestInfo, Bytes)],
    ) -> CasResult<Vec<(DigestInfo, CasResult<()>)>> {
        let mut results = Vec::with_capacity(items.len());
        for (digest, data) in items {
            let result = self.write(digest, data.clone()).await;
            results.push((*digest, result));
        }
        Ok(results)
    }

    /// Removes the blob. Removing a blob that is not stored succeeds.
    async fn delete(&self, digest: &DigestInfo) -> CasResult<()>;

    /// Number of stored blobs, or `0` for backends that do not track it.
    async fn count_entries(&self) -> u64 {
        0
    }

    /// Total bytes stored, or `0` for backends that do not track it.
    fn size_bytes(&self) -> u64 {
        0
    }

    /// The file holding the blob, for backends that store blobs as local
    /// files; lets callers hardlink instead of copying. `None` when the blob
    /// is not stored or the backend is not local.
    async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>>;
}

/// Shared handle to a CAS backend.
pub type SharedCasBackend = Arc<dyn CasBackend>;
