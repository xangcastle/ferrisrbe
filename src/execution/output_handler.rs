//! Placement of an action's stdout and stderr, as REAPI prescribes.
//!
//! - Outputs below [`inline_output_threshold`] travel inline in the response.
//! - Larger outputs are written to the CAS and only their digest is returned,
//!   which keeps response size and server memory bounded regardless of how
//!   much an action prints.
//! - Outputs above [`max_capture_size`] are truncated to that size first.

use bytes::Bytes;
use tracing::{debug, info, warn};

use crate::cas::{CasBackend, CasError};
use crate::types::DigestInfo;
use std::sync::Arc;

/// Environment variable overriding [`inline_output_threshold`], in bytes.
pub const ENV_INLINE_OUTPUT_THRESHOLD: &str = "RBE_INLINE_OUTPUT_THRESHOLD";

/// Environment variable overriding [`max_capture_size`], in bytes.
pub const ENV_MAX_CAPTURE_SIZE: &str = "RBE_MAX_CAPTURE_SIZE";

/// Size in bytes below which an output is returned inline. Default 1 MiB.
pub fn inline_output_threshold() -> usize {
    std::env::var(ENV_INLINE_OUTPUT_THRESHOLD)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024 * 1024)
}

/// Size in bytes beyond which an output is truncated. Default 100 MiB.
pub fn max_capture_size() -> usize {
    std::env::var(ENV_MAX_CAPTURE_SIZE)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100 * 1024 * 1024)
}

/// Where an output ended up.
///
/// Inline outputs carry `raw` only; CAS outputs carry `digest` only; a
/// truncated output carries both the truncated bytes (with a trailing notice)
/// and the digest of those bytes without the notice.
#[derive(Debug, Clone)]
pub struct OutputResult {
    pub raw: Option<Vec<u8>>,
    pub digest: Option<DigestInfo>,
    #[allow(dead_code)]
    pub truncated: bool,
}

impl OutputResult {
    pub fn inline(data: Vec<u8>) -> Self {
        Self {
            raw: Some(data),
            digest: None,
            truncated: false,
        }
    }

    pub fn stored(digest: DigestInfo) -> Self {
        Self {
            raw: None,
            digest: Some(digest),
            truncated: false,
        }
    }

    pub fn truncated(raw: Vec<u8>, digest: Option<DigestInfo>) -> Self {
        Self {
            raw: Some(raw),
            digest,
            truncated: true,
        }
    }

    /// Whether the bytes are carried inline and nothing was stored.
    pub fn is_inline(&self) -> bool {
        self.raw.is_some() && self.digest.is_none()
    }

    /// Whether the output (or its truncated prefix) is in the CAS.
    pub fn is_stored(&self) -> bool {
        self.digest.is_some()
    }

    /// Length of the inline bytes; `0` for CAS-only outputs.
    #[allow(dead_code)]
    pub fn size(&self) -> usize {
        self.raw.as_ref().map(|r| r.len()).unwrap_or(0)
    }
}

/// Decides, per output, between inline delivery and CAS storage.
#[derive(Clone)]
pub struct OutputHandler {
    cas_backend: Arc<dyn CasBackend>,
}

impl OutputHandler {
    pub fn new(cas_backend: Arc<dyn CasBackend>) -> Self {
        Self { cas_backend }
    }

    /// Places `data`, labelled `name` (e.g. `"stdout"`) in logs, inline or in
    /// the CAS according to the thresholds above.
    ///
    /// # Errors
    ///
    /// Propagates CAS failures when the output has to be stored.
    pub async fn process_output(
        &self,
        name: &str,
        data: Vec<u8>,
    ) -> Result<OutputResult, CasError> {
        let size = data.len();

        if size == 0 {
            debug!("Output {} is empty, storing inline", name);
            return Ok(OutputResult::inline(Vec::new()));
        }

        let max_capture = max_capture_size();
        let inline_threshold = inline_output_threshold();

        if size > max_capture {
            warn!(
                "Output {} exceeds maximum capture size ({} > {}), truncating",
                name, size, max_capture
            );
            return self.truncate_and_store(name, data, max_capture).await;
        }

        if size < inline_threshold {
            debug!(
                "Output {} is small ({} < {}), storing inline",
                name, size, inline_threshold
            );
            return Ok(OutputResult::inline(data));
        }

        info!(
            "Output {} is large ({} >= {}), storing in CAS",
            name, size, inline_threshold
        );
        self.store_in_cas(name, data).await
    }

    /// Writes `data` to the CAS unless it is already there.
    async fn store_in_cas(&self, name: &str, data: Vec<u8>) -> Result<OutputResult, CasError> {
        let digest = DigestInfo::from_bytes(&data);
        let size = data.len();

        debug!(
            "Storing {} output in CAS: digest={}, size={}",
            name,
            digest.hash_to_string(),
            size
        );

        if self.cas_backend.contains(&digest).await? {
            debug!("Output {} already exists in CAS, skipping write", name);
            return Ok(OutputResult::stored(digest));
        }

        self.cas_backend.write(&digest, Bytes::from(data)).await?;

        info!(
            "Stored {} output in CAS: digest={}, size={}",
            name,
            digest.hash_to_string(),
            size
        );

        Ok(OutputResult::stored(digest))
    }

    /// Keeps the first `max_size` bytes, stores them in the CAS and returns
    /// them inline followed by a truncation notice.
    async fn truncate_and_store(
        &self,
        name: &str,
        data: Vec<u8>,
        max_size: usize,
    ) -> Result<OutputResult, CasError> {
        let truncated_data: Vec<u8> = data[..max_size].to_vec();
        let truncation_msg = format!(
            "\n[Output truncated: exceeded maximum size of {} bytes]\n",
            max_size
        );

        let digest = DigestInfo::from_bytes(&truncated_data);
        if !self.cas_backend.contains(&digest).await? {
            self.cas_backend
                .write(&digest, Bytes::from(truncated_data.clone()))
                .await?;
        }

        warn!(
            "Truncated {} output to {} bytes with CAS digest {}",
            name,
            truncated_data.len(),
            digest.hash_to_string()
        );

        let mut result = truncated_data;
        result.extend_from_slice(truncation_msg.as_bytes());

        Ok(OutputResult::truncated(result, Some(digest)))
    }

    /// The bytes of `result`: the inline data if present, otherwise the CAS
    /// blob. A blob missing from the CAS yields a placeholder message instead
    /// of an error, so a reader always gets something printable.
    #[allow(dead_code)]
    pub async fn retrieve_output(&self, result: &OutputResult) -> Result<Vec<u8>, CasError> {
        if let Some(ref data) = result.raw {
            return Ok(data.clone());
        }

        if let Some(ref digest) = result.digest {
            debug!(
                "Retrieving output from CAS: digest={}",
                digest.hash_to_string()
            );
            match self.cas_backend.read(digest).await? {
                Some(data) => Ok(data.to_vec()),
                None => {
                    warn!(
                        "Output not found in CAS: digest={}",
                        digest.hash_to_string()
                    );
                    Ok(
                        format!("[Output not found in CAS: {}]", digest.hash_to_string())
                            .into_bytes(),
                    )
                }
            }
        } else {
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::backends::DiskBackend;
    use tempfile::TempDir;

    #[tokio::test]
    async fn small_output_is_returned_inline() {
        let temp_dir = TempDir::new().unwrap();
        let backend = Arc::new(DiskBackend::new(temp_dir.path()).await.unwrap());
        let handler = OutputHandler::new(backend);

        let data = b"small output".to_vec();
        let result = handler
            .process_output("stdout", data.clone())
            .await
            .unwrap();

        assert!(result.is_inline());
        assert_eq!(result.raw, Some(data));
        assert!(result.digest.is_none());
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn empty_output_is_returned_inline() {
        let temp_dir = TempDir::new().unwrap();
        let backend = Arc::new(DiskBackend::new(temp_dir.path()).await.unwrap());
        let handler = OutputHandler::new(backend);

        let result = handler.process_output("stdout", Vec::new()).await.unwrap();

        assert!(result.is_inline());
        assert_eq!(result.raw, Some(Vec::new()));
        assert!(result.digest.is_none());
    }

    #[tokio::test]
    async fn inline_output_is_retrieved_as_is() {
        let temp_dir = TempDir::new().unwrap();
        let backend = Arc::new(DiskBackend::new(temp_dir.path()).await.unwrap());
        let handler = OutputHandler::new(backend);

        let data = b"test data".to_vec();
        let result = OutputResult::inline(data.clone());
        let retrieved = handler.retrieve_output(&result).await.unwrap();

        assert_eq!(retrieved, data);
    }
}
