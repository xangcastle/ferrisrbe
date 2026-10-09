//! Least-recently-used bookkeeping shared by the disk CAS, the disk Action
//! Cache and the in-memory L1 Action Cache.
//!
//! Recency is a monotonic counter: `order` maps counter to digest and
//! `positions` maps digest to counter, so a touch is an `O(log n)` re-insert
//! at the most recently used end and the eviction victim is always the
//! smallest counter. Counters never repeat, so a digest occupies exactly one
//! slot however often it is touched.

use std::collections::{BTreeMap, HashMap};

use crate::types::DigestInfo;

/// Recency order and recorded size of every tracked digest.
pub struct LruState {
    /// Counter to digest; the smallest counter is the least recently used.
    order: BTreeMap<u64, DigestInfo>,
    positions: HashMap<DigestInfo, u64>,
    sizes: HashMap<DigestInfo, u64>,
    /// Content hash to the digest key currently tracking it.
    ///
    /// A digest read back from the on-disk layout carries the hash but not
    /// the size the entry was written under (for the Action Cache that size
    /// is the declared size of the Action proto, unrelated to the file). The
    /// same entry can therefore be known under two keys; indexing presence
    /// by hash lets [`LruState::insert_at`] recognise it and never count one
    /// entry twice.
    hash_index: HashMap<[u8; 32], DigestInfo>,
    next_counter: u64,
}

impl LruState {
    pub fn new() -> Self {
        Self {
            order: BTreeMap::new(),
            positions: HashMap::new(),
            sizes: HashMap::new(),
            hash_index: HashMap::new(),
            next_counter: 1,
        }
    }

    /// Guarantees that every counter handed out from now on sorts after
    /// `stamp`, so live touches rank above entries restored from mtimes.
    pub fn floor_counter(&mut self, stamp: u64) {
        self.next_counter = self.next_counter.max(stamp + 1);
    }

    /// Marks `digest` most recently used and records `size` for it.
    pub fn touch(&mut self, digest: DigestInfo, size: u64) {
        if let Some(old_counter) = self.positions.remove(&digest) {
            self.order.remove(&old_counter);
        }
        let counter = self.next_counter;
        self.next_counter += 1;
        self.track(digest, size, counter);
    }

    pub fn contains(&self, digest: &DigestInfo) -> bool {
        self.positions.contains_key(digest)
    }

    /// Tracks `digest` at the recency rank of `stamp` (an mtime in
    /// milliseconds) rather than at the most recently used end, restoring the
    /// order of entries found on disk without touching each one.
    ///
    /// Returns `false` without changes when the content hash is already
    /// tracked under any key, i.e. a live write accounted for it first.
    pub fn insert_at(&mut self, digest: DigestInfo, size: u64, stamp: u64) -> bool {
        if self.hash_index.contains_key(&digest.hash) {
            return false;
        }
        let mut counter = stamp.max(1);
        while self.order.contains_key(&counter) {
            counter += 1;
        }
        self.track(digest, size, counter);
        self.next_counter = self.next_counter.max(counter + 1);
        true
    }

    /// Stops tracking `digest`; returns its recorded size.
    pub fn remove(&mut self, digest: DigestInfo) -> Option<u64> {
        let counter = self.positions.remove(&digest)?;
        self.order.remove(&counter);
        self.forget(&digest)
    }

    /// Removes and returns the least recently used digest and its size.
    pub fn pop_lru(&mut self) -> Option<(DigestInfo, u64)> {
        let (_, digest) = self.order.pop_first()?;
        self.positions.remove(&digest);
        let size = self.forget(&digest).unwrap_or(0);
        Some((digest, size))
    }

    /// Removes digests, least recently used first, until their recorded
    /// sizes add up to at least `free_bytes` or nothing is left. Returns the
    /// removed digests with their sizes, in eviction order.
    pub fn evict(&mut self, mut free_bytes: u64) -> Vec<(DigestInfo, u64)> {
        let mut removed = Vec::new();
        while free_bytes > 0 {
            let Some((digest, size)) = self.pop_lru() else {
                break;
            };
            removed.push((digest, size));
            free_bytes = free_bytes.saturating_sub(size);
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn track(&mut self, digest: DigestInfo, size: u64, counter: u64) {
        self.order.insert(counter, digest);
        self.positions.insert(digest, counter);
        self.sizes.insert(digest, size);
        self.hash_index.insert(digest.hash, digest);
    }

    /// Drops the size and hash-index records of a digest already removed
    /// from the recency order. The hash index keeps pointing at another key
    /// for the same content, if that key is the one it currently tracks.
    fn forget(&mut self, digest: &DigestInfo) -> Option<u64> {
        if self.hash_index.get(&digest.hash) == Some(digest) {
            self.hash_index.remove(&digest.hash);
        }
        self.sizes.remove(digest)
    }
}

impl Default for LruState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: u8) -> DigestInfo {
        DigestInfo::new(&format!("{:064x}", seed), 8).unwrap()
    }

    #[test]
    fn insert_at_ranks_by_stamp_below_live_touches() {
        let mut lru = LruState::new();
        let live = digest(1);
        let old = digest(2);
        let older = digest(3);
        lru.floor_counter(1_000);
        lru.touch(live, 5);
        assert!(lru.insert_at(old, 6, 500));
        assert!(lru.insert_at(older, 7, 500));
        assert!(lru.contains(&old));
        assert_eq!(lru.pop_lru(), Some((old, 6)));
        assert_eq!(lru.pop_lru(), Some((older, 7)));
        assert_eq!(lru.pop_lru(), Some((live, 5)));
    }

    #[test]
    fn insert_at_skips_a_hash_already_tracked_under_a_different_size() {
        let mut lru = LruState::new();
        let hash = digest(1).hash;
        let live = DigestInfo { hash, size: 42 };
        let scanned = DigestInfo { hash, size: 0 };
        lru.touch(live, 5);
        assert!(!lru.insert_at(scanned, 5, 1));
        assert!(!lru.contains(&scanned));
        assert_eq!(lru.pop_lru(), Some((live, 5)));
    }

    #[test]
    fn pop_lru_returns_oldest_touched_entry() {
        let mut lru = LruState::new();
        let a = digest(1);
        let b = digest(2);
        let c = digest(3);
        lru.touch(a, 10);
        lru.touch(b, 20);
        lru.touch(c, 30);

        assert_eq!(lru.pop_lru(), Some((a, 10)));
        assert_eq!(lru.pop_lru(), Some((b, 20)));
        assert_eq!(lru.pop_lru(), Some((c, 30)));
        assert!(lru.pop_lru().is_none());
    }

    #[test]
    fn touch_moves_entry_to_mru_and_updates_size() {
        let mut lru = LruState::new();
        let a = digest(1);
        let b = digest(2);
        lru.touch(a, 10);
        lru.touch(b, 20);
        lru.touch(a, 11);

        assert_eq!(lru.len(), 2);
        assert_eq!(lru.pop_lru(), Some((b, 20)));
        assert_eq!(lru.pop_lru(), Some((a, 11)));
    }

    #[test]
    fn remove_returns_recorded_size() {
        let mut lru = LruState::new();
        let a = digest(1);
        lru.touch(a, 42);

        assert_eq!(lru.remove(a), Some(42));
        assert_eq!(lru.remove(a), None);
        assert!(lru.is_empty());
    }

    #[test]
    fn evict_frees_bytes_oldest_first() {
        let mut lru = LruState::new();
        let a = digest(1);
        let b = digest(2);
        let c = digest(3);
        lru.touch(a, 10);
        lru.touch(b, 20);
        lru.touch(c, 30);

        assert_eq!(lru.evict(30), vec![(a, 10), (b, 20)]);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.pop_lru(), Some((c, 30)));
    }

    #[test]
    fn evicted_hash_can_be_reinserted_from_a_scan() {
        let mut lru = LruState::new();
        let a = digest(1);
        lru.touch(a, 10);
        assert_eq!(lru.evict(10), vec![(a, 10)]);
        assert!(lru.insert_at(
            DigestInfo {
                hash: a.hash,
                size: 0
            },
            10,
            1
        ));
    }

    #[test]
    fn floor_counter_seeds_recency_above_stamp() {
        let mut lru = LruState::new();
        let a = digest(1);
        let b = digest(2);
        lru.touch(a, 10);
        lru.floor_counter(1_000);
        lru.touch(b, 20);

        assert_eq!(lru.pop_lru(), Some((a, 10)));
        assert_eq!(lru.pop_lru(), Some((b, 20)));
    }
}
