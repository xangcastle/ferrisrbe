//! A local disk CAS in front of a durable remote one.
//!
//! Writes reach both tiers before they are acknowledged, so the remote tier
//! holds every blob written since it was enabled and the server can move to
//! an empty disk (another node, another zone) without losing them. Reads
//! are served from disk; a blob only the remote tier holds, because the
//! disk evicted it or started empty, is copied to disk on first read, once
//! however many reads ask for it at the same time. Only the disk evicts:
//! the remote tier's retention is its own lifecycle.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use futures::stream::BoxStream;

use crate::cas::backends::object::SINGLE_PUT_MAX_BYTES;
use crate::cas::backends::DiskBackend;
use crate::cas::{CasBackend, CasResult, SharedCasBackend};
use crate::types::DigestInfo;

/// [`DiskBackend`] backed by a durable `remote` [`CasBackend`].
pub struct TieredBackend {
    local: Arc<DiskBackend>,
    remote: SharedCasBackend,
    /// One lock per blob being copied from the remote tier: the reads that
    /// arrive while it downloads wait for it instead of downloading again.
    filling: DashMap<DigestInfo, Arc<tokio::sync::Mutex<()>>>,
}

impl TieredBackend {
    pub fn new(local: Arc<DiskBackend>, remote: SharedCasBackend) -> Self {
        Self {
            local,
            remote,
            filling: DashMap::new(),
        }
    }

    /// Copies a blob only the remote tier holds to disk; whether it is now
    /// on disk.
    async fn fill_local(&self, digest: &DigestInfo) -> CasResult<bool> {
        if self.local.contains(digest).await? {
            return Ok(true);
        }
        let lock = self.filling.entry(*digest).or_default().clone();
        let filled = {
            let _guard = lock.lock().await;
            self.copy_from_remote(digest).await
        };
        self.filling.remove_if(digest, |_, held| {
            Arc::ptr_eq(held, &lock) && Arc::strong_count(held) <= 2
        });
        filled
    }

    async fn copy_from_remote(&self, digest: &DigestInfo) -> CasResult<bool> {
        if self.local.contains(digest).await? {
            return Ok(true);
        }
        let Some(stream) = self.remote.read_stream(digest, 0, None).await? else {
            return Ok(false);
        };
        self.local.write_stream(digest, stream).await?;
        Ok(true)
    }
}

#[async_trait]
impl CasBackend for TieredBackend {
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        if self.local.contains(digest).await? {
            return Ok(true);
        }
        self.remote.contains(digest).await
    }

    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        if let Some(data) = self.local.read(digest).await? {
            return Ok(Some(data));
        }
        if !self.fill_local(digest).await? {
            return Ok(None);
        }
        self.local.read(digest).await
    }

    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        if !self.fill_local(digest).await? {
            return Ok(None);
        }
        self.local.read_stream(digest, offset, limit).await
    }

    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        let (local, remote) = tokio::join!(
            self.local.write(digest, data.clone()),
            self.remote.write(digest, data)
        );
        local.and(remote)
    }

    /// Verified onto disk first, then uploaded from there: the client's
    /// stream is read once. A blob large enough for a multipart upload is
    /// not uploaded again if the remote tier already holds it.
    async fn write_stream(
        &self,
        digest: &DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        self.local.write_stream(digest, stream).await?;
        if digest.size > SINGLE_PUT_MAX_BYTES && self.remote.contains(digest).await? {
            return Ok(());
        }
        let Some(stored) = self.local.read_stream(digest, 0, None).await? else {
            return Err(crate::cas::CasError::NotFound(digest.hash_to_string()));
        };
        self.remote.write_stream(digest, stored).await
    }

    async fn find_missing(&self, digests: &[DigestInfo]) -> CasResult<Vec<DigestInfo>> {
        let not_local = self.local.find_missing(digests).await?;
        if not_local.is_empty() {
            return Ok(not_local);
        }
        self.remote.find_missing(&not_local).await
    }

    async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
        let (local, remote) = tokio::join!(self.local.delete(digest), self.remote.delete(digest));
        local.and(remote)
    }

    async fn count_entries(&self) -> u64 {
        self.local.count_entries().await
    }

    fn size_bytes(&self) -> u64 {
        self.local.size_bytes()
    }

    async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        if !self.fill_local(digest).await? {
            return Ok(None);
        }
        self.local.local_path(digest).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::backends::ObjectStoreBackend;
    use futures::{StreamExt, TryStreamExt};
    use object_store::memory::InMemory;

    /// The remote tier with its calls counted, and reads slowed down so
    /// that concurrent ones overlap.
    struct Counted {
        inner: ObjectStoreBackend,
        reads: std::sync::atomic::AtomicUsize,
        uploads: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl CasBackend for Counted {
        async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
            self.inner.contains(digest).await
        }
        async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
            self.inner.read(digest).await
        }
        async fn read_stream(
            &self,
            digest: &DigestInfo,
            offset: usize,
            limit: Option<usize>,
        ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.inner.read_stream(digest, offset, limit).await
        }
        async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
            self.inner.write(digest, data).await
        }
        async fn write_stream(
            &self,
            digest: &DigestInfo,
            stream: BoxStream<'static, CasResult<Bytes>>,
        ) -> CasResult<()> {
            self.uploads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.write_stream(digest, stream).await
        }
        async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
            self.inner.delete(digest).await
        }
        async fn local_path(&self, digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
            self.inner.local_path(digest).await
        }
    }

    async fn counted_tiers(dir: &std::path::Path) -> (Arc<Counted>, Arc<TieredBackend>) {
        let local = Arc::new(DiskBackend::new(dir).await.unwrap().without_fsync());
        local.warm_up().await.unwrap();
        let remote = Arc::new(Counted {
            inner: ObjectStoreBackend::new(Arc::new(InMemory::new()), ""),
            reads: Default::default(),
            uploads: Default::default(),
        });
        let tiered = Arc::new(TieredBackend::new(local, remote.clone()));
        (remote, tiered)
    }

    #[tokio::test]
    async fn concurrent_reads_of_a_remote_only_blob_download_it_once() {
        let temp = tempfile::TempDir::new().unwrap();
        let (remote, cas) = counted_tiers(temp.path()).await;
        let data = Bytes::from(vec![7u8; 64 * 1024]);
        let digest = DigestInfo::from_bytes(&data);
        remote.write(&digest, data.clone()).await.unwrap();

        let reads: Vec<_> = (0..16)
            .map(|_| {
                let cas = cas.clone();
                tokio::spawn(async move { cas.read(&digest).await })
            })
            .collect();
        for read in reads {
            assert_eq!(read.await.unwrap().unwrap().unwrap(), data);
        }
        assert_eq!(remote.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(cas.filling.is_empty());
    }

    #[tokio::test]
    async fn a_large_blob_the_remote_tier_holds_is_not_uploaded_again() {
        let temp = tempfile::TempDir::new().unwrap();
        let (remote, cas) = counted_tiers(temp.path()).await;
        let data = Bytes::from(vec![3u8; SINGLE_PUT_MAX_BYTES as usize + 1]);
        let digest = DigestInfo::from_bytes(&data);
        for _ in 0..2 {
            cas.write_stream(&digest, futures::stream::iter([Ok(data.clone())]).boxed())
                .await
                .unwrap();
        }
        assert_eq!(remote.uploads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    async fn tiers(dir: &std::path::Path) -> (Arc<DiskBackend>, SharedCasBackend, TieredBackend) {
        let local = Arc::new(DiskBackend::new(dir).await.unwrap().without_fsync());
        local.warm_up().await.unwrap();
        let remote: SharedCasBackend =
            Arc::new(ObjectStoreBackend::new(Arc::new(InMemory::new()), ""));
        let tiered = TieredBackend::new(local.clone(), remote.clone());
        (local, remote, tiered)
    }

    #[tokio::test]
    async fn a_write_lands_on_both_tiers() {
        let temp = tempfile::TempDir::new().unwrap();
        let (local, remote, cas) = tiers(temp.path()).await;
        let data = Bytes::from_static(b"durable blob");
        let digest = DigestInfo::from_bytes(&data);
        cas.write(&digest, data.clone()).await.unwrap();
        assert!(local.contains(&digest).await.unwrap());
        assert!(remote.contains(&digest).await.unwrap());

        let streamed = Bytes::from_static(b"streamed durable blob");
        let streamed_digest = DigestInfo::from_bytes(&streamed);
        cas.write_stream(
            &streamed_digest,
            futures::stream::iter([Ok(streamed.clone())]).boxed(),
        )
        .await
        .unwrap();
        assert_eq!(
            remote.read(&streamed_digest).await.unwrap().unwrap(),
            streamed
        );
    }

    #[tokio::test]
    async fn a_blob_only_the_remote_tier_holds_is_served_and_copied_to_disk() {
        let temp = tempfile::TempDir::new().unwrap();
        let (local, remote, cas) = tiers(temp.path()).await;
        let data = Bytes::from_static(b"written before the disk was replaced");
        let digest = DigestInfo::from_bytes(&data);
        remote.write(&digest, data.clone()).await.unwrap();

        assert!(cas.find_missing(&[digest]).await.unwrap().is_empty());
        let streamed: Vec<Bytes> = cas
            .read_stream(&digest, 0, None)
            .await
            .unwrap()
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(streamed.concat(), data);
        assert!(local.contains(&digest).await.unwrap());
        assert!(cas.local_path(&digest).await.unwrap().is_some());

        let absent = DigestInfo::from_bytes(b"nowhere");
        assert_eq!(cas.find_missing(&[absent]).await.unwrap(), vec![absent]);
        assert!(cas.read(&absent).await.unwrap().is_none());
        assert!(cas.local_path(&absent).await.unwrap().is_none());
    }
}
