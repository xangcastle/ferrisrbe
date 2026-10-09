//! In-memory (L1) Action Cache and the store interface shared with the
//! disk-backed L2 cache.
//!
//! The L1 cache bounds both entry count and entry age: a full cache evicts
//! its least recently used entry, and every entry expires `ttl` after it was
//! written, however often it is read.

use crate::cache::lru::LruState;
use crate::proto::build::bazel::remote::execution::v2::ActionResult as ProtoActionResult;
use crate::types::{DigestInfo, DASHMAP_SHARD_COUNT};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// Environment variable for the L1 cache capacity, in entries.
pub const ENV_L1_CACHE_CAPACITY: &str = "RBE_L1_CACHE_CAPACITY";
/// Environment variable for the L1 cache entry TTL, in seconds.
pub const ENV_L1_CACHE_TTL_SECS: &str = "RBE_L1_CACHE_TTL_SECS";

const DEFAULT_L1_CAPACITY: usize = 100_000;
const DEFAULT_L1_TTL_SECS: u64 = 3600;

/// Tier at which an Action Cache lookup was satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCacheHitTier {
    /// Hit in the in-memory L1 cache.
    L1,
    /// Hit in the persistent L2 cache.
    L2,
    /// Hit in the object store shared by every server.
    Remote,
    /// Miss in both caches.
    Miss,
}

/// Result of an Action Cache lookup and the tier that answered it.
#[derive(Debug, Clone)]
pub struct ActionCacheGetResult {
    pub result: Option<CacheActionResult>,
    pub tier: ActionCacheHitTier,
}

/// Expiration record ordered by deadline, so the soonest-expiring entry is
/// always at the top of the heap.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpirationEntry {
    /// `created_at + ttl` of the cache entry this record tracks.
    expires_at: Instant,
    digest: DigestInfo,
}

impl Ord for ExpirationEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expires_at.cmp(&other.expires_at)
    }
}

impl PartialOrd for ExpirationEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A cached value with the write time and lifetime that bound its validity.
#[derive(Debug, Clone)]
struct CacheEntry<T> {
    data: T,
    created_at: Instant,
    ttl: Duration,
}

impl<T> CacheEntry<T> {
    fn new(data: T, ttl: Duration) -> Self {
        Self {
            data,
            created_at: Instant::now(),
            ttl,
        }
    }

    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}

/// In-memory representation of an action cache entry.
///
/// Wraps the REAPI `ActionResult` proto so that all fields (outputs, stdout/stderr
/// digests, execution metadata, etc.) are preserved exactly as Bazel expects.
#[derive(Debug, Clone)]
pub struct CacheActionResult {
    pub digest: DigestInfo,
    pub proto: ProtoActionResult,
}

impl CacheActionResult {
    /// Pairs an `ActionResult` with the action digest it is cached under.
    pub fn new(digest: DigestInfo, proto: ProtoActionResult) -> Self {
        Self { digest, proto }
    }
}

/// Bounded, TTL-limited in-memory Action Cache.
///
/// The value map is a sharded `DashMap`; the recency list and the expiration
/// heap sit behind their own mutexes. No value-map guard is ever held while
/// one of those mutexes is taken, so the locks cannot form a cycle.
pub struct L1ActionCache {
    cache: DashMap<DigestInfo, CacheEntry<CacheActionResult>, ahash::RandomState>,
    max_capacity: usize,
    default_ttl: Duration,
    /// Recency of `cache` entries, updated on hits, inserts and removals so
    /// that eviction follows use rather than insertion order.
    lru: Mutex<LruState>,
    /// Expiration records ordered by deadline. A record can outlive its entry
    /// (evicted or rewritten); such records are skipped when popped, and the
    /// heap is compacted once it grows past twice the cache capacity.
    expiration_heap: Mutex<BinaryHeap<Reverse<ExpirationEntry>>>,
}

impl L1ActionCache {
    /// An empty cache holding at most `max_capacity` entries, each valid for
    /// `default_ttl` after it is written.
    pub fn new(max_capacity: usize, default_ttl: Duration) -> Self {
        let cache = DashMap::with_capacity_and_hasher_and_shard_amount(
            max_capacity,
            ahash::RandomState::new(),
            DASHMAP_SHARD_COUNT,
        );

        Self {
            cache,
            max_capacity,
            default_ttl,
            lru: Mutex::new(LruState::new()),
            expiration_heap: Mutex::new(BinaryHeap::with_capacity(max_capacity)),
        }
    }

    /// The live entry for `digest`, marked most recently used. An expired
    /// entry is removed and reported as a miss.
    pub fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        let entry = self.cache.get(digest)?;

        if entry.is_expired() {
            drop(entry);
            self.remove_entry(digest);
            return None;
        }

        let result = entry.data.clone();
        drop(entry);

        self.lru.lock().touch(*digest, 0);

        debug!("L1 cache hit for digest: {:?}", digest);
        Some(result)
    }

    /// Stores `result` for `digest` with a fresh TTL, first evicting the least
    /// recently used entry when a new key would exceed capacity.
    pub fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        if !self.cache.contains_key(&digest) && self.cache.len() >= self.max_capacity {
            self.evict_lru();
        }

        let entry = CacheEntry::new(result, self.default_ttl);
        let expires_at = entry.created_at + entry.ttl;
        self.cache.insert(digest, entry);

        self.lru.lock().touch(digest, 0);

        let mut heap = self.expiration_heap.lock();
        heap.push(Reverse(ExpirationEntry { expires_at, digest }));

        let heap_len = heap.len();
        let should_prune = heap_len > self.max_capacity * 2;
        drop(heap);

        if should_prune {
            debug!(
                "Expiration heap has {} entries (capacity {}), pruning ghosts",
                heap_len, self.max_capacity
            );
            self.cleanup_expired();

            self.prune_ghost_entries();
        }

        debug!("L1 cache insert for digest: {:?}", digest);
    }

    /// Drops expiration records whose entry is no longer cached, bounding the
    /// heap by the live entries. Linear in the heap size.
    fn prune_ghost_entries(&self) {
        let mut heap = self.expiration_heap.lock();

        let mut vec = std::mem::take(&mut *heap).into_vec();
        let original_len = vec.len();

        vec.retain(|Reverse(entry)| self.cache.contains_key(&entry.digest));

        let pruned = original_len - vec.len();

        *heap = BinaryHeap::from(vec);

        if pruned > 0 {
            warn!(
                "Pruned {} ghost entries from expiration heap in O(N)",
                pruned
            );
        }
    }

    /// Removes the entry for `digest`, if any.
    pub fn invalidate(&self, digest: &DigestInfo) {
        self.remove_entry(digest);
        debug!("L1 cache invalidate for digest: {:?}", digest);
    }

    /// Removes every expired entry. Only the expired prefix of the expiration
    /// heap is walked, so the cost follows what expired, not the cache size.
    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        let mut expired_count = 0;
        let mut heap = self.expiration_heap.lock();

        while let Some(Reverse(entry)) = heap.peek() {
            if entry.expires_at > now {
                break;
            }

            let entry = heap.pop().unwrap().0;

            if let Some(cache_entry) = self.cache.get(&entry.digest) {
                if cache_entry.is_expired() {
                    drop(cache_entry);
                    self.remove_entry(&entry.digest);
                    expired_count += 1;
                }
            }
        }

        if expired_count > 0 {
            debug!("Cleaned up {} expired cache entries", expired_count);
        }
    }

    /// Removes an entry from both the value map and the recency list.
    fn remove_entry(&self, digest: &DigestInfo) {
        self.cache.remove(digest);
        self.lru.lock().remove(*digest);
    }

    fn evict_lru(&self) {
        let mut lru = self.lru.lock();
        while let Some((digest, _)) = lru.pop_lru() {
            if self.cache.remove(&digest).is_some() {
                debug!("Evicted LRU cache entry: {:?}", digest);
                break;
            }
        }
    }

    /// Number of cached entries, including expired ones not yet collected.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Default for L1ActionCache {
    fn default() -> Self {
        Self::from_env()
    }
}

impl L1ActionCache {
    /// A cache sized by [`ENV_L1_CACHE_CAPACITY`] and [`ENV_L1_CACHE_TTL_SECS`],
    /// defaulting to 100 000 entries valid for one hour.
    pub fn from_env() -> Self {
        let capacity = std::env::var(ENV_L1_CACHE_CAPACITY)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_L1_CAPACITY);

        let ttl_secs = std::env::var(ENV_L1_CACHE_TTL_SECS)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_L1_TTL_SECS);

        Self::new(capacity, Duration::from_secs(ttl_secs))
    }
}

/// Store interface behind `ActionCacheService`, implemented by the in-memory
/// L1 cache and by the disk-backed two-tier cache.
#[async_trait::async_trait]
pub trait ActionCacheStore: Send + Sync {
    /// The cached result for `digest`, if any.
    async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult>;

    /// The cached result for `digest` and the tier that answered. Single-tier
    /// stores report every hit as [`ActionCacheHitTier::L1`].
    async fn get_with_tier(&self, digest: &DigestInfo) -> ActionCacheGetResult {
        let result = self.get(digest).await;
        let tier = if result.is_some() {
            ActionCacheHitTier::L1
        } else {
            ActionCacheHitTier::Miss
        };
        ActionCacheGetResult { result, tier }
    }

    /// Stores `result` for `digest`, replacing any previous entry.
    async fn put(&self, digest: DigestInfo, result: CacheActionResult);

    /// Removes the entry for `digest`; stores without removal ignore the call.
    async fn remove(&self, _digest: &DigestInfo) {}
}

#[async_trait::async_trait]
impl ActionCacheStore for L1ActionCache {
    async fn remove(&self, digest: &DigestInfo) {
        self.invalidate(digest);
    }

    async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        L1ActionCache::get(self, digest)
    }

    async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        L1ActionCache::put(self, digest, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result(digest: DigestInfo, exit_code: i32) -> CacheActionResult {
        CacheActionResult::new(
            digest,
            ProtoActionResult {
                exit_code,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_l1_cache_basic() {
        let cache = L1ActionCache::new(100, Duration::from_secs(60));
        let digest = DigestInfo::new(
            "ecd71870d1963316a97e3ac3408c9835ad8cf0f3c1bc703527c30265534f75ae",
            1024,
        )
        .unwrap();
        let result = sample_result(digest, 0);

        cache.put(digest, result.clone());
        assert_eq!(cache.len(), 1);

        let retrieved = cache.get(&digest).unwrap();
        assert_eq!(retrieved.proto.exit_code, 0);
    }

    #[test]
    fn test_cache_entry_expiry() {
        let entry = CacheEntry::new(42, Duration::from_millis(10));
        assert!(!entry.is_expired());
        std::thread::sleep(Duration::from_millis(15));
        assert!(entry.is_expired());
    }

    #[test]
    fn test_l1_cache_eviction() {
        let cache = L1ActionCache::new(2, Duration::from_secs(60));

        for i in 0..5 {
            let digest = DigestInfo::new(&format!("{:064x}", i), 1024).unwrap();
            let result = sample_result(digest, i);
            cache.put(digest, result);
        }

        assert!(cache.len() <= 2);
    }

    #[test]
    fn test_l1_cache_evicts_least_recently_used_not_first_inserted() {
        let cache = L1ActionCache::new(2, Duration::from_secs(60));
        let a = DigestInfo::new(&format!("{:064x}", 1), 8).unwrap();
        let b = DigestInfo::new(&format!("{:064x}", 2), 8).unwrap();
        let c = DigestInfo::new(&format!("{:064x}", 3), 8).unwrap();

        cache.put(a, sample_result(a, 1));
        cache.put(b, sample_result(b, 2));
        cache.get(&a);

        cache.put(c, sample_result(c, 3));

        assert!(cache.get(&a).is_some(), "recently used entry must survive");
        assert!(
            cache.get(&b).is_none(),
            "least recently used must be evicted"
        );
        assert!(cache.get(&c).is_some());
    }

    #[test]
    fn test_l1_cache_reput_moves_to_mru_without_duplicating_slot() {
        let cache = L1ActionCache::new(2, Duration::from_secs(60));
        let a = DigestInfo::new(&format!("{:064x}", 1), 8).unwrap();
        let b = DigestInfo::new(&format!("{:064x}", 2), 8).unwrap();
        let c = DigestInfo::new(&format!("{:064x}", 3), 8).unwrap();

        cache.put(a, sample_result(a, 1));
        cache.put(b, sample_result(b, 2));
        cache.put(a, sample_result(a, 1));

        cache.put(c, sample_result(c, 3));

        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_l1_cache_invalidate_frees_recency_slot() {
        let cache = L1ActionCache::new(2, Duration::from_secs(60));
        let a = DigestInfo::new(&format!("{:064x}", 1), 8).unwrap();
        let b = DigestInfo::new(&format!("{:064x}", 2), 8).unwrap();
        let c = DigestInfo::new(&format!("{:064x}", 3), 8).unwrap();
        let d = DigestInfo::new(&format!("{:064x}", 4), 8).unwrap();

        cache.put(a, sample_result(a, 1));
        cache.put(b, sample_result(b, 2));
        cache.invalidate(&a);
        cache.put(c, sample_result(c, 3));
        cache.put(d, sample_result(d, 4));

        assert!(cache.get(&a).is_none());
        assert!(cache.get(&b).is_none(), "b was LRU when d was inserted");
        assert!(cache.get(&c).is_some());
        assert!(cache.get(&d).is_some());
        assert_eq!(cache.len(), 2);
    }
}
