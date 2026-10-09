//! The disk Action Cache in front of the shared one on an object store.
//!
//! Writes reach both tiers before they are acknowledged, so a result any
//! server stored is visible to every other one. Reads are served from disk;
//! a result only the remote tier holds is copied to disk when read. An entry
//! the disk had before the remote tier existed is uploaded in the
//! background when read from disk, so the hot part of the cache moves to the
//! remote tier without a migration.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::warn;

use crate::cache::action_cache::{
    ActionCacheGetResult, ActionCacheHitTier, ActionCacheStore, CacheActionResult,
};
use crate::cache::disk_action_cache::DiskActionCache;
use crate::cache::object_action_cache::ObjectActionCache;
use crate::types::DigestInfo;

/// Uploads of disk entries to the remote tier in flight at once. A disk hit
/// that finds none free skips its upload; the next one retries it.
const PROMOTIONS_IN_FLIGHT: usize = 64;

/// [`DiskActionCache`] backed by a shared [`ObjectActionCache`].
pub struct TieredActionCache {
    local: Arc<DiskActionCache>,
    remote: Arc<ObjectActionCache>,
    promotions: Arc<Semaphore>,
}

impl TieredActionCache {
    pub fn new(local: Arc<DiskActionCache>, remote: Arc<ObjectActionCache>) -> Self {
        Self {
            local,
            remote,
            promotions: Arc::new(Semaphore::new(PROMOTIONS_IN_FLIGHT)),
        }
    }

    /// Uploads a disk entry the remote tier lacks, off the request path.
    fn promote(&self, result: &CacheActionResult) {
        let Ok(permit) = self.promotions.clone().try_acquire_owned() else {
            return;
        };
        let remote = self.remote.clone();
        let result = result.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let uploaded = match remote.contains(&result.digest).await {
                Ok(true) => Ok(()),
                Ok(false) => remote.put(&result.digest, &result.proto).await,
                Err(e) => Err(e),
            };
            if let Err(e) = uploaded {
                warn!("Failed to promote action result {:?}: {}", result.digest, e);
            }
        });
    }
}

#[async_trait::async_trait]
impl ActionCacheStore for TieredActionCache {
    async fn get(&self, digest: &DigestInfo) -> Option<CacheActionResult> {
        self.get_with_tier(digest).await.result
    }

    async fn get_with_tier(&self, digest: &DigestInfo) -> ActionCacheGetResult {
        let local = self.local.get_with_tier(digest).await;
        match local.tier {
            ActionCacheHitTier::L1 => return local,
            ActionCacheHitTier::L2 => {
                if let Some(result) = &local.result {
                    self.promote(result);
                }
                return local;
            }
            ActionCacheHitTier::Miss | ActionCacheHitTier::Remote => {}
        }
        match self.remote.get(digest).await {
            Ok(Some(proto)) => {
                let result = CacheActionResult::new(*digest, proto);
                self.local.put(*digest, result.clone()).await;
                ActionCacheGetResult {
                    result: Some(result),
                    tier: ActionCacheHitTier::Remote,
                }
            }
            Ok(None) => local,
            Err(e) => {
                warn!(
                    "Failed to read action result {:?} from the remote tier: {}",
                    digest, e
                );
                local
            }
        }
    }

    /// A remote failure is logged, not returned: a cache write must not fail
    /// the build that produced it.
    async fn put(&self, digest: DigestInfo, result: CacheActionResult) {
        let remote = self.remote.put(&digest, &result.proto);
        let ((), stored) = tokio::join!(self.local.put(digest, result.clone()), remote);
        if let Err(e) = stored {
            warn!(
                "Failed to store action result {:?} in the remote tier: {}",
                digest, e
            );
        }
    }

    async fn remove(&self, digest: &DigestInfo) {
        self.local.remove(digest).await;
        if let Err(e) = self.remote.remove(digest).await {
            warn!(
                "Failed to remove action result {:?} from the remote tier: {}",
                digest, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::build::bazel::remote::execution::v2::ActionResult as ProtoActionResult;
    use object_store::memory::InMemory;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    const MINUTE: Duration = Duration::from_secs(60);

    fn result(digest: DigestInfo, exit_code: i32) -> CacheActionResult {
        CacheActionResult::new(
            digest,
            ProtoActionResult {
                exit_code,
                ..Default::default()
            },
        )
    }

    async fn disk(root: &Path) -> Arc<DiskActionCache> {
        Arc::new(
            DiskActionCache::new(root, 1, MINUTE, 100, MINUTE)
                .await
                .unwrap(),
        )
    }

    fn shared() -> Arc<ObjectActionCache> {
        Arc::new(ObjectActionCache::new(
            Arc::new(InMemory::new()),
            String::new(),
        ))
    }

    #[tokio::test]
    async fn a_result_stored_by_one_server_is_read_by_another_from_an_empty_disk() {
        let remote = shared();
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let first = TieredActionCache::new(disk(a.path()).await, remote.clone());
        let second_disk = disk(b.path()).await;
        let second = TieredActionCache::new(second_disk.clone(), remote.clone());
        let digest = DigestInfo::from_bytes(b"action");

        first.put(digest, result(digest, 5)).await;
        assert!(remote.contains(&digest).await.unwrap());

        let read = second.get_with_tier(&digest).await;
        assert_eq!(read.tier, ActionCacheHitTier::Remote);
        assert_eq!(read.result.unwrap().proto.exit_code, 5);
        assert_eq!(
            second_disk.get_with_tier(&digest).await.tier,
            ActionCacheHitTier::L1,
            "a remote hit is copied to the disk tier"
        );
    }

    #[tokio::test]
    async fn a_disk_entry_written_before_the_remote_tier_is_promoted_when_read() {
        let dir = TempDir::new().unwrap();
        let digest = DigestInfo::from_bytes(b"legacy");
        disk(dir.path()).await.put(digest, result(digest, 1)).await;

        let remote = shared();
        let tiered = TieredActionCache::new(disk(dir.path()).await, remote.clone());
        assert_eq!(
            tiered.get_with_tier(&digest).await.tier,
            ActionCacheHitTier::L2
        );
        for _ in 0..100 {
            if remote.contains(&digest).await.unwrap() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the disk entry never reached the remote tier");
    }

    #[tokio::test]
    async fn a_removed_result_is_gone_from_both_tiers() {
        let dir = TempDir::new().unwrap();
        let remote = shared();
        let tiered = TieredActionCache::new(disk(dir.path()).await, remote.clone());
        let digest = DigestInfo::from_bytes(b"stale");

        tiered.put(digest, result(digest, 0)).await;
        tiered.remove(&digest).await;

        assert!(!remote.contains(&digest).await.unwrap());
        assert_eq!(
            tiered.get_with_tier(&digest).await.tier,
            ActionCacheHitTier::Miss
        );
    }
}
