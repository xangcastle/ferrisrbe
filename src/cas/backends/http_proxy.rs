//! HTTP Proxy CAS Backend
//!
//! Wraps a local disk backend and falls back to HTTP requests to bazel-remote
//! when blobs are not found locally.
//!
//! NOTE: Currently HTTP functionality is disabled because reqwest
//! is not available in the Bazel environment. Only the local backend is used.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::path::PathBuf;
#[allow(unused_imports)]
use tracing::{debug, info, warn};

use crate::cas::backends::DiskBackend;
#[allow(unused_imports)]
use crate::cas::CasError;
use crate::cas::{CasBackend, CasResult};
use crate::types::DigestInfo;

/// HTTP Proxy CAS backend that wraps a local disk backend
/// and falls back to HTTP requests for missing blobs
pub struct HttpProxyBackend {
    /// Local disk backend
    local: DiskBackend,
    /// HTTP endpoint for bazel-remote (not currently used)
    _http_endpoint: String,
}

impl HttpProxyBackend {
    /// Create a new HTTP proxy backend
    #[allow(dead_code)]
    pub async fn new(local_path: &str, http_endpoint: &str) -> CasResult<Self> {
        let local = DiskBackend::new(local_path).await?;

        info!(
            "HTTP proxy CAS backend: local={} (HTTP disabled)",
            local_path
        );

        Ok(Self {
            local,
            _http_endpoint: http_endpoint.to_string(),
        })
    }

    /// Fetch a blob from bazel-remote via HTTP
    ///
    /// NOTE: This function is disabled because reqwest is not available.
    /// It always returns None so that the local backend is used.
    async fn fetch_from_remote(&self, _digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        warn!("HTTP fetch disabled - reqwest not available");
        Ok(None)
    }
}

#[async_trait]
impl CasBackend for HttpProxyBackend {
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        if self.local.contains(digest).await? {
            return Ok(true);
        }

        match self.fetch_from_remote(digest).await {
            Ok(Some(_)) => Ok(true),
            _ => Ok(false),
        }
    }

    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        match self.local.read(digest).await? {
            Some(data) => Ok(Some(data)),
            None => self.fetch_from_remote(digest).await,
        }
    }

    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        self.local.read_stream(digest, offset, limit).await
    }

    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        self.local.write(digest, data).await
    }

    async fn write_stream(
        &self,
        digest: &DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        self.local.write_stream(digest, stream).await
    }

    async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
        self.local.delete(digest).await
    }

    async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        self.local.local_path(digest).await
    }
}
