//! Core value types shared across the server: content digests, a lock-free
//! timestamp, and the crate error type.

use once_cell::sync::OnceCell;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Shard count of the `DashMap`s on hot request paths, sized for many
/// concurrent writers.
pub const DASHMAP_SHARD_COUNT: usize = 64;

/// Process-wide origin of every [`AtomicInstant`].
static GLOBAL_BASE_INSTANT: OnceCell<Instant> = OnceCell::new();

/// Fixes the origin of [`AtomicInstant`] values. Call once at startup; the
/// first use of an `AtomicInstant` fixes it otherwise.
pub fn init_global_base_instant() {
    let _ = GLOBAL_BASE_INSTANT.get_or_init(Instant::now);
}

fn elapsed_since_base() -> u64 {
    GLOBAL_BASE_INSTANT
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// A monotonic timestamp with millisecond resolution that can be read and
/// refreshed through a shared reference without locking.
#[derive(Debug)]
pub struct AtomicInstant {
    /// Milliseconds since [`GLOBAL_BASE_INSTANT`].
    millis: AtomicU64,
}

impl AtomicInstant {
    /// The current time.
    pub fn now() -> Self {
        Self {
            millis: AtomicU64::new(elapsed_since_base()),
        }
    }

    /// Moves the timestamp to the current time.
    pub fn refresh(&self) {
        self.millis.store(elapsed_since_base(), Ordering::Relaxed);
    }

    /// Milliseconds since the timestamp was created or last refreshed.
    pub fn elapsed_millis(&self) -> u64 {
        let stored = self.millis.load(Ordering::Relaxed);
        elapsed_since_base().saturating_sub(stored)
    }
}

impl Default for AtomicInstant {
    fn default() -> Self {
        Self::now()
    }
}

/// A REAPI digest: the SHA-256 of a blob and its size in bytes.
///
/// Equality, ordering and hashing cover both fields, so the same hash
/// declared with different sizes yields distinct digests.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DigestInfo {
    pub hash: [u8; 32],
    pub size: i64,
}

impl DigestInfo {
    /// A digest from a 64-character hex SHA-256 (either case) and a size.
    ///
    /// # Errors
    ///
    /// [`RbeError::InvalidDigest`] if `hash_str` is not exactly 64 hex
    /// characters.
    pub fn new(hash_str: &str, size: i64) -> Result<Self> {
        let hash = Self::parse_sha256(hash_str)?;
        Ok(Self { hash, size })
    }

    fn parse_sha256(hash_str: &str) -> Result<[u8; 32]> {
        let bytes = hash_str.as_bytes();
        if bytes.len() != 64 {
            return Err(RbeError::InvalidDigest(format!(
                "expected 64 hex characters, got {}",
                bytes.len()
            )));
        }

        let nibble = |position: usize| {
            let c = bytes[position];
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(RbeError::InvalidDigest(format!(
                    "invalid hex character '{}' at position {}",
                    c as char, position
                ))),
            }
        };

        let mut result = [0u8; 32];
        for (i, byte) in result.iter_mut().enumerate() {
            *byte = (nibble(i * 2)? << 4) | nibble(i * 2 + 1)?;
        }
        Ok(result)
    }

    /// The digest of `data`.
    pub fn from_bytes(data: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            hash: Sha256::digest(data).into(),
            size: data.len() as i64,
        }
    }

    /// A random RFC 4122 version 4 UUID as a 128-bit value.
    pub fn generate_uuid() -> u128 {
        u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes())
    }

    /// The hash as 64 lowercase hex characters.
    pub fn hash_to_string(&self) -> String {
        hex::encode(self.hash)
    }
}

/// Abbreviated to the first 16 hex characters of the hash.
impl fmt::Debug for DigestInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DigestInfo({}...)", &self.hash_to_string()[..16])
    }
}

/// `<hash>-<size>`, the form used in REAPI resource names and logs.
impl fmt::Display for DigestInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.hash_to_string(), self.size)
    }
}

/// Crate-wide error for execution, scheduling and cache bookkeeping.
#[derive(Debug, thiserror::Error)]
pub enum RbeError {
    #[error("Invalid digest string: {0}")]
    InvalidDigest(String),

    #[error("Invalid state transition: {from} -> {to}")]
    InvalidStateTransition { from: String, to: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type using [`RbeError`].
pub type Result<T> = std::result::Result<T, RbeError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn digest_parses_a_hex_hash_and_keeps_the_size() {
        let digest = DigestInfo::new(
            "ecd71870d1963316a97e3ac3408c9835ad8cf0f3c1bc703527c30265534f75ae",
            1024,
        )
        .unwrap();
        assert_eq!(digest.size, 1024);
        assert_eq!(
            digest.hash_to_string(),
            "ecd71870d1963316a97e3ac3408c9835ad8cf0f3c1bc703527c30265534f75ae"
        );
    }

    #[test]
    fn digest_rejects_short_or_non_hex_hashes() {
        assert!(DigestInfo::new("abc123", 1024).is_err());
        assert!(DigestInfo::new(
            "0000000000000000000000000000000000000000000000000000000061626331323g",
            1024
        )
        .is_err());
    }

    #[test]
    fn digest_accepts_uppercase_hex() {
        let lower = DigestInfo::new(&"ab".repeat(32), 1).unwrap();
        let upper = DigestInfo::new(&"AB".repeat(32), 1).unwrap();
        assert_eq!(lower, upper);
    }

    #[test]
    fn digest_from_bytes_hashes_the_content() {
        let digest = DigestInfo::from_bytes(b"hello world");
        assert_eq!(digest.size, 11);
        assert_eq!(
            digest.hash_to_string(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn generated_uuids_are_distinct() {
        assert_ne!(DigestInfo::generate_uuid(), DigestInfo::generate_uuid());
    }

    #[test]
    fn atomic_instant_measures_and_refreshes() {
        init_global_base_instant();

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
