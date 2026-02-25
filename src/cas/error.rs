//! CAS (Content Addressable Storage) errors

use thiserror::Error;

/// Errors that can occur when interacting with the CAS backend
#[derive(Error, Debug)]
pub enum CasError {
    #[error("Blob not found: {0}")]
    #[allow(dead_code)]
    NotFound(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    
    #[error("Invalid digest: {0}")]
    InvalidDigest(String),
    
    #[error("Invalid data: {0}")]
    InvalidData(String),
    
    #[error("Storage error: {0}")]
    Storage(String),
    
    #[error("Temp file error: {0}")]
    #[allow(dead_code)]
    TempFile(String),
}

impl CasError {
    #[allow(dead_code)]
    pub fn not_found(digest: &str) -> Self {
        CasError::NotFound(digest.to_string())
    }
    
    pub fn digest_mismatch(expected: &str, actual: &str) -> Self {
        CasError::DigestMismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}
