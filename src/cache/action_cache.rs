

use crate::types::{AtomicInstant, DigestInfo, Result, DASHMAP_SHARD_COUNT};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

/// Environment variable for L1 cache capacity (number of entries)
pub const ENV_L1_CACHE_CAPACITY: &str = "RBE_L1_CACHE_CAPACITY";
/// Environment variable for L1 cache TTL in seconds
pub const ENV_L1_CACHE_TTL_SECS: &str = "RBE_L1_CACHE_TTL_SECS";

/// Entry for the expiration heap - allows O(1) access to next expiring item
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpirationEntry {
    /// Expiration time (created_at + ttl)
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

#[derive(Debug, Clone)]
pub struct CacheEntry<T> {
    pub data: T,
    pub created_at: Instant,
    pub ttl: Duration,
    #[allow(dead_code)]
    pub access_count: u64,
}

impl<T> CacheEntry<T> {
    pub fn new(data: T, ttl: Duration) -> Self {
        Self {
            data,
            created_at: Instant::now(),
            ttl,
            access_count: 1,
        }
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }

    #[allow(dead_code)]
    pub fn touch(&mut self) {
        self.access_count += 1;
    }
}

#[derive(Debug, Clone)]
pub struct ActionResult {
    #[allow(dead_code)]
    pub digest: DigestInfo,
    pub exit_code: i32,
    pub stdout_digest: Option<DigestInfo>,
    pub stderr_digest: Option<DigestInfo>,
    pub output_files: Vec<OutputFile>,
    pub output_directories: Vec<OutputDirectory>,
    #[allow(dead_code)]
    pub execution_metadata: ExecutionMetadata,
}

#[derive(Debug, Clone)]
pub struct OutputFile {
    pub path: String,
    pub digest: DigestInfo,
    pub is_executable: bool,
}

#[derive(Debug, Clone)]
pub struct OutputDirectory {
    pub path: String,
    pub tree_digest: DigestInfo,
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionMetadata {
    #[allow(dead_code)]
    pub worker: String,
    #[allow(dead_code)]
    pub queued_duration: Duration,
    #[allow(dead_code)]
    pub execution_duration: Duration,
    #[allow(dead_code)]
    pub input_fetch_duration: Duration,
    #[allow(dead_code)]
    pub output_upload_duration: Duration,
}

pub struct L1ActionCache {
    cache: DashMap<DigestInfo, CacheEntry<ActionResult>, ahash::RandomState>,
    max_capacity: usize,
    default_ttl: Duration,
    lru_queue: Mutex<VecDeque<DigestInfo>>,
    /// Min-heap ordered by expiration time for O(1) cleanup of expired entries
    expiration_heap: Mutex<BinaryHeap<Reverse<ExpirationEntry>>>,
}

impl L1ActionCache {
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
            lru_queue: Mutex::new(VecDeque::with_capacity(max_capacity)),
            expiration_heap: Mutex::new(BinaryHeap::with_capacity(max_capacity)),
        }
    }

    pub fn get(&self, digest: &DigestInfo) -> Option<ActionResult> {
        let entry = self.cache.get(digest)?;

        if entry.is_expired() {
            drop(entry);
            self.cache.remove(digest);
            return None;
        }

        let result = entry.data.clone();
        drop(entry);

        debug!("L1 cache hit for digest: {:?}", digest);
        Some(result)
    }

    pub fn put(&self, digest: DigestInfo, result: ActionResult) {
        if self.cache.len() >= self.max_capacity {
            self.evict_oldest();
        }

        let entry = CacheEntry::new(result, self.default_ttl);
        let expires_at = entry.created_at + entry.ttl;
        self.cache.insert(digest, entry);

        let mut queue = self.lru_queue.lock();
        queue.push_back(digest);
        drop(queue);

        let mut heap = self.expiration_heap.lock();
        heap.push(Reverse(ExpirationEntry { expires_at, digest }));
        
        let heap_len = heap.len();
        let should_prune = heap_len > self.max_capacity * 2;
        drop(heap);
        
        if should_prune {
            debug!("Expiration heap has {} entries (capacity {}), pruning ghosts", 
                   heap_len, self.max_capacity);
            self.cleanup_expired();
            
            self.prune_ghost_entries();
        }

        debug!("L1 cache insert for digest: {:?}", digest);
    }
    
    /// Prune "ghost" entries from expiration heap (entries not in cache anymore)
    /// This prevents memory leak when LRU evicts but heap still references them.
    /// Prune "ghost" entries from expiration heap (entries not in cache anymore).
    fn prune_ghost_entries(&self) {
        let mut heap = self.expiration_heap.lock();
        
        let mut vec = std::mem::take(&mut *heap).into_vec();
        let original_len = vec.len();
        
        vec.retain(|Reverse(entry)| self.cache.contains_key(&entry.digest));
        
        let pruned = original_len - vec.len();
        
        *heap = BinaryHeap::from(vec);
        
        if pruned > 0 {
            warn!("Pruned {} ghost entries from expiration heap in O(N)", pruned);
        }
    }

    #[allow(dead_code)]
    pub fn invalidate(&self, digest: &DigestInfo) {
        self.cache.remove(digest);
        debug!("L1 cache invalidate for digest: {:?}", digest);
    }

    /// Cleanup expired entries using the expiration heap - O(k) where k = expired entries
    /// instead of O(n) scanning the entire cache.
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
                    self.cache.remove(&entry.digest);
                    expired_count += 1;
                }
            }
        }

        if expired_count > 0 {
            debug!("Cleaned up {} expired cache entries", expired_count);
        }
    }

    fn evict_oldest(&self) {
        let mut queue = self.lru_queue.lock();
        while let Some(digest) = queue.pop_front() {
            if self.cache.remove(&digest).is_some() {
                debug!("Evicted oldest cache entry: {:?}", digest);
                break;
            }
        }
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    #[allow(dead_code)]
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
    /// Create configuration from environment variables (12-Factor App style)
    pub fn from_env() -> Self {
        let capacity = std::env::var(ENV_L1_CACHE_CAPACITY)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        
        let ttl_secs = std::env::var(ENV_L1_CACHE_TTL_SECS)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        
        Self::new(capacity, Duration::from_secs(ttl_secs))
    }
}

#[allow(dead_code)]
#[async_trait::async_trait]
pub trait L2Store: Send + Sync {
    async fn get(&self, digest: &DigestInfo) -> Result<Option<ActionResult>>;
    async fn put(&self, digest: &DigestInfo, result: &ActionResult) -> Result<()>;
}

#[allow(dead_code)]
#[async_trait::async_trait]
pub trait CasStore: Send + Sync {

    async fn exists(&self, digest: &DigestInfo) -> Result<bool>;

    async fn validate_references(&self, result: &ActionResult) -> Result<bool>;
}

#[allow(dead_code)]
pub struct ActionCacheServer<L2, CAS>
where
    L2: L2Store,
    CAS: CasStore,
{

    l1_cache: Arc<L1ActionCache>,

    l2_store: Arc<L2>,

    cas_store: Arc<CAS>,

    validation_queue: Arc<Mutex<VecDeque<DigestInfo>>>,

    last_heartbeat: Arc<AtomicInstant>,
}

#[allow(dead_code)]
impl<L2, CAS> ActionCacheServer<L2, CAS>
where
    L2: L2Store + 'static,
    CAS: CasStore + 'static,
{
    pub fn new(
        l1_capacity: usize,
        l1_ttl: Duration,
        l2_store: Arc<L2>,
        cas_store: Arc<CAS>,
    ) -> Self {
        Self {
            l1_cache: Arc::new(L1ActionCache::new(l1_capacity, l1_ttl)),
            l2_store,
            cas_store,
            validation_queue: Arc::new(Mutex::new(VecDeque::new())),
            last_heartbeat: Arc::new(AtomicInstant::now()),
        }
    }

    #[allow(dead_code)]
    pub async fn get(&self, digest: &DigestInfo) -> Result<Option<ActionResult>> {

        if let Some(result) = self.l1_cache.get(digest) {

            self.enqueue_validation(*digest);
            return Ok(Some(result));
        }

        match self.l2_store.get(digest).await? {
            Some(result) => {

                self.l1_cache.put(*digest, result.clone());
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    #[allow(dead_code)]
    pub async fn put(&self, digest: &DigestInfo, result: &ActionResult) -> Result<()> {

        self.l2_store.put(digest, result).await?;

        self.l1_cache.put(*digest, result.clone());

        Ok(())
    }

    #[allow(dead_code)]
    fn enqueue_validation(&self, digest: DigestInfo) {
        let mut queue = self.validation_queue.lock();
        queue.push_back(digest);

        if queue.len() > 1000 {
            queue.pop_front();
        }
    }

    #[allow(dead_code)]
    pub async fn process_validations(&self, batch_size: usize) -> Result<usize> {
        let mut processed = 0;
        let cas = self.cas_store.clone();
        let l1 = self.l1_cache.clone();

        for _ in 0..batch_size {
            let digest = {
                let mut queue = self.validation_queue.lock();
                queue.pop_front()
            };

            if let Some(digest) = digest {

                if let Some(result) = l1.get(&digest) {

                    match cas.validate_references(&result).await {
                        Ok(true) => {
                            debug!("Validation passed for {:?}", digest);
                        }
                        Ok(false) => {
                            warn!("Validation failed for {:?}, invalidating", digest);
                            l1.invalidate(&digest);
                        }
                        Err(e) => {
                            warn!("Validation error for {:?}: {}", digest, e);
                        }
                    }
                }
                processed += 1;
            } else {
                break;
            }
        }

        Ok(processed)
    }

    #[allow(dead_code)]
    pub fn spawn_validation_task(self: Arc<Self>, interval: Duration) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(interval);
            loop {
                interval.tick().await;
                if let Err(e) = self.process_validations(100).await {
                    warn!("Background validation error: {}", e);
                }

                self.l1_cache.cleanup_expired();
            }
        });
    }

    #[allow(dead_code)]
    pub fn heartbeat(&self) {
        self.last_heartbeat.refresh();
    }

    #[allow(dead_code)]
    pub fn is_healthy(&self) -> bool {
        self.last_heartbeat.elapsed_millis() < 30_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_l1_cache_basic() {
        let cache = L1ActionCache::new(100, Duration::from_secs(60));
        let digest = DigestInfo::new("test123", 1024);
        let result = ActionResult {
            digest,
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
            output_files: vec![],
            output_directories: vec![],
            execution_metadata: ExecutionMetadata::default(),
        };

        cache.put(digest, result.clone());
        assert_eq!(cache.len(), 1);

        let retrieved = cache.get(&digest).unwrap();
        assert_eq!(retrieved.exit_code, 0);
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
            let digest = DigestInfo::new(&format!("test{}", i), 1024);
            let result = ActionResult {
                digest,
                exit_code: i,
                stdout_digest: None,
                stderr_digest: None,
                output_files: vec![],
                output_directories: vec![],
                execution_metadata: ExecutionMetadata::default(),
            };
            cache.put(digest, result);
        }

        assert!(cache.len() <= 2);
    }
}
