//! Errors of CAS operations.

use thiserror::Error;

/// Failure of a CAS operation.
#[derive(Error, Debug)]
pub enum CasError {
    /// A blob required by the operation is not stored.
    #[error("Blob not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Content does not hash to the digest it was presented under.
    #[error("Digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },

    /// A digest is malformed or its declared size does not match the content.
    #[error("Invalid digest: {0}")]
    InvalidDigest(String),

    /// A stored or received message cannot be decoded or encoded.
    #[error("Invalid data: {0}")]
    InvalidData(String),

    /// The backend refused or failed the operation for a storage reason,
    /// such as a size limit or a remote error.
    #[error("Storage error: {0}")]
    Storage(String),
}

impl CasError {
    /// A [`CasError::DigestMismatch`] between two hex digests.
    pub fn digest_mismatch(expected: &str, actual: &str) -> Self {
        CasError::DigestMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}
