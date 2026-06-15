use std::fmt;
use std::hash::Hash;
#[allow(unused_imports)]
use std::hash::Hasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub const CHANNEL_CHUNK_SIZE: usize = 64 * 1024;

pub const MAX_INLINE_SIZE: usize = 4 * 1024 * 1024;

pub const DASHMAP_SHARD_COUNT: usize = 64;

/// Global base instant initialized at process start.
/// All AtomicInstant values are offsets from this base.
static GLOBAL_BASE_INSTANT: OnceCell<Instant> = OnceCell::new();

/// Initialize the global base instant. Must be called once at startup.
pub fn init_global_base_instant() {
    let _ = GLOBAL_BASE_INSTANT.get_or_init(Instant::now);
}

/// Get elapsed millis since the global base instant.
/// Automatically initializes if not already done.
fn elapsed_since_base() -> u64 {
    GLOBAL_BASE_INSTANT
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

#[derive(Debug)]
pub struct AtomicInstant {
    /// Offset in millis from GLOBAL_BASE_INSTANT
    millis: AtomicU64,
}

impl AtomicInstant {
    /// Create an AtomicInstant representing the current time.
    pub fn now() -> Self {
        Self {
            millis: AtomicU64::new(elapsed_since_base()),
        }
    }

    /// Refresh the timestamp to the current time.
    pub fn refresh(&self) {
        self.millis.store(elapsed_since_base(), Ordering::Relaxed);
    }

    /// Get elapsed millis since this instant was created/refreshed.
    pub fn elapsed_millis(&self) -> u64 {
        let stored = self.millis.load(Ordering::Relaxed);
        elapsed_since_base().saturating_sub(stored)
    }

    /// Get the raw offset value (for testing/comparison).
    pub fn as_millis(&self) -> u64 {
        self.millis.load(Ordering::Relaxed)
    }
}

impl Default for AtomicInstant {
    fn default() -> Self {
        Self::now()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DigestInfo {
    /// SHA256 hash as 32 bytes (256 bits)
    pub hash: [u8; 32],
    pub size: i64,
}

impl DigestInfo {
    /// Create a new DigestInfo from a 64-character SHA256 hex string and size.
    ///
    /// Returns an error if the string is not exactly 64 hex characters.
    pub fn new(hash_str: &str, size: i64) -> Result<Self> {
        let hash = Self::parse_sha256(hash_str)?;
        Ok(Self { hash, size })
    }

    /// Parse a SHA256 hex string into [u8; 32].
    ///
    /// Rejects strings that are not exactly 64 hex characters.
    fn parse_sha256(hash_str: &str) -> Result<[u8; 32]> {
        let bytes = hash_str.as_bytes();
        if bytes.len() != 64 {
            return Err(RbeError::InvalidDigest(format!(
                "expected 64 hex characters, got {}",
                bytes.len()
            )));
        }

        let mut result = [0u8; 32];
        for i in 0..32 {
            let high = Self::hex_char_to_nibble(bytes[i * 2]).map_err(|_| {
                RbeError::InvalidDigest(format!(
                    "invalid hex character '{}' at position {}",
                    bytes[i * 2] as char,
                    i * 2
                ))
            })?;
            let low = Self::hex_char_to_nibble(bytes[i * 2 + 1]).map_err(|_| {
                RbeError::InvalidDigest(format!(
                    "invalid hex character '{}' at position {}",
                    bytes[i * 2 + 1] as char,
                    i * 2 + 1
                ))
            })?;
            result[i] = (high << 4) | low;
        }
        Ok(result)
    }

    fn hex_char_to_nibble(c: u8) -> std::result::Result<u8, ()> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(()),
        }
    }

    pub fn from_bytes(data: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        let hash = hasher.finalize();

        Self {
            hash: hash.into(),
            size: data.len() as i64,
        }
    }

    /// Generate a random RFC 4122 UUID as a 128-bit value.
    pub fn generate_uuid() -> u128 {
        let uuid = uuid::Uuid::new_v4();
        u128::from_be_bytes(*uuid.as_bytes())
    }

    /// Convert the hash to a 64-character hex string
    pub fn hash_to_string(&self) -> String {
        self.hash.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

impl fmt::Debug for DigestInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DigestInfo({}...)", &self.hash_to_string()[..16])
    }
}

impl fmt::Display for DigestInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.hash_to_string(), self.size)
    }
}

use once_cell::sync::OnceCell;

pub mod channels {
    use super::CHANNEL_CHUNK_SIZE;
    use tokio::sync::mpsc;

    pub fn chunk_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
        mpsc::channel(CHANNEL_CHUNK_SIZE)
    }

    pub fn bounded_channel<T>(capacity: usize) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
        mpsc::channel(capacity)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RbeError {
    #[error("Digest not found: {0}")]
    DigestNotFound(DigestInfo),

    #[error("Invalid digest string: {0}")]
    InvalidDigest(String),

    #[error("CAS storage error: {0}")]
    CasError(String),

    #[error("Action cache miss")]
    CacheMiss,

    #[error("Invalid state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    #[error("Execution timeout")]
    ExecutionTimeout,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, RbeError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_digest_info_creation() {
        let digest = DigestInfo::new(
            "ecd71870d1963316a97e3ac3408c9835ad8cf0f3c1bc703527c30265534f75ae",
            1024,
        )
        .unwrap();
        assert_eq!(digest.size, 1024);
        assert!(!digest.hash.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_digest_info_rejects_invalid_hex() {
        assert!(DigestInfo::new("abc123", 1024).is_err());
        assert!(DigestInfo::new(
            "0000000000000000000000000000000000000000000000000000000061626331323g",
            1024
        )
        .is_err());
    }

    #[test]
    fn test_digest_from_bytes() {
        let data = b"hello world";
        let digest = DigestInfo::from_bytes(data);
        assert_eq!(digest.size, 11);
    }

    #[test]
    fn test_uuid_generation() {
        let uuid1 = DigestInfo::generate_uuid();
        let uuid2 = DigestInfo::generate_uuid();
        assert_ne!(uuid1, uuid2);
    }

    #[test]
    fn test_atomic_instant() {
        let _ = GLOBAL_BASE_INSTANT.get_or_init(Instant::now);

        let instant = AtomicInstant::now();
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = instant.elapsed_millis();
        assert!(elapsed >= 10, "Expected at least 10ms, got {}ms", elapsed);

        instant.refresh();
        let elapsed_after_refresh = instant.elapsed_millis();
        assert!(
            elapsed_after_refresh < 5,
            "Expected <5ms after refresh, got {}ms",
            elapsed_after_refresh
        );
    }
}
