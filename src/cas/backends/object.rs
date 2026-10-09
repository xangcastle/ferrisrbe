//! CAS on an object store: S3, or anything speaking its API (MinIO, R2).
//!
//! A blob lives at `<prefix>cas/<sha256>`. Objects are immutable and
//! written only after their content is verified, so a stored object is
//! always the blob its key names; a multipart upload whose content does not
//! match its digest is aborted before it becomes visible. Retention is the
//! bucket's business (a lifecycle rule): this backend never evicts.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use object_store::path::Path as ObjectPath;
use object_store::{GetOptions, GetRange, ObjectStore, PutPayload, WriteMultipart};
use sha2::{Digest as _, Sha256};

use crate::cache::object_action_cache::ObjectActionCache;
use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;

/// Blobs up to this size are uploaded with a single PUT; larger ones as a
/// multipart upload, so memory stays bounded by the part size.
pub(crate) const SINGLE_PUT_MAX_BYTES: i64 = 8 << 20;

/// HEAD requests in flight for one `find_missing` call.
const FIND_MISSING_CONCURRENCY: usize = 64;

/// Upload parts in flight for one multipart write.
const MULTIPART_CONCURRENCY: usize = 4;

/// A [`CasBackend`] on an [`ObjectStore`].
pub struct ObjectStoreBackend {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl ObjectStoreBackend {
    /// Blobs of `store` under the directory `prefix`; `ci`, `/ci` and
    /// `ci/` name the same one, and an empty prefix is the bucket root.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        let prefix = prefix.into();
        let prefix = match prefix.trim_matches('/') {
            "" => String::new(),
            dir => format!("{dir}/"),
        };
        Self { store, prefix }
    }

    /// The S3 bucket `bucket`, configured from the standard `AWS_*`
    /// environment: region, credentials or IRSA web identity, and
    /// `AWS_ENDPOINT` with `AWS_ALLOW_HTTP` for an S3-compatible store.
    pub fn s3_from_env(bucket: &str, prefix: impl Into<String>) -> CasResult<Self> {
        let store = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .build()
            .map_err(storage)?;
        Ok(Self::new(Arc::new(store), prefix))
    }

    /// Writes, reads back and deletes an object under the prefix: fails
    /// when the credentials, the bucket or the permissions are wrong, which
    /// otherwise only shows on the first blob a build uploads.
    pub async fn check_access(&self) -> CasResult<()> {
        let key = ObjectPath::from(format!("{}probe/{}", self.prefix, uuid::Uuid::new_v4()));
        let payload = Bytes::from_static(b"ferrisrbe access probe");
        self.store
            .put(&key, PutPayload::from(payload.clone()))
            .await
            .map_err(storage)?;
        let read = self.store.get(&key).await.map_err(storage)?.bytes().await;
        let deleted = self.store.delete(&key).await;
        if read.map_err(storage)? != payload {
            return Err(CasError::Storage(format!(
                "{key} read back different content"
            )));
        }
        deleted.map_err(storage)
    }

    /// The Action Cache under the same prefix and client.
    pub fn action_cache(&self) -> ObjectActionCache {
        ObjectActionCache::new(self.store.clone(), self.prefix.clone())
    }

    fn key(&self, digest: &DigestInfo) -> ObjectPath {
        ObjectPath::from(format!("{}cas/{}", self.prefix, digest.hash_to_string()))
    }
}

/// An object store failure as a [`CasError::Storage`].
fn storage(error: object_store::Error) -> CasError {
    CasError::Storage(error.to_string())
}

/// Whether `key` holds an object of `size` bytes; one of another size reads
/// as missing.
async fn has_object(store: Arc<dyn ObjectStore>, key: ObjectPath, size: i64) -> CasResult<bool> {
    match store.head(&key).await {
        Ok(meta) => Ok(meta.size == size as u64),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(storage(e)),
    }
}

/// Fails unless `data` is the blob `digest` names.
fn verify(digest: &DigestInfo, data: &[u8]) -> CasResult<()> {
    if data.len() as i64 != digest.size {
        return Err(CasError::InvalidDigest(format!(
            "{} declares {} bytes, got {}",
            digest.hash_to_string(),
            digest.size,
            data.len()
        )));
    }
    verify_hash(digest, Sha256::digest(data).into())
}

fn verify_hash(digest: &DigestInfo, computed: [u8; 32]) -> CasResult<()> {
    if computed == digest.hash {
        Ok(())
    } else {
        Err(CasError::digest_mismatch(
            &digest.hash_to_string(),
            &hex::encode(computed),
        ))
    }
}

#[async_trait]
impl CasBackend for ObjectStoreBackend {
    /// An object whose size differs from the digest's reads as missing.
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        has_object(self.store.clone(), self.key(digest), digest.size).await
    }

    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        match self.store.get(&self.key(digest)).await {
            Ok(result) => Ok(Some(result.bytes().await.map_err(storage)?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }

    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        let size = digest.size.max(0) as u64;
        let start = offset as u64;
        let end = limit.map_or(size, |limit| (start + limit as u64).min(size));
        if start >= end {
            return Ok(if self.contains(digest).await? {
                Some(futures::stream::empty().boxed())
            } else {
                None
            });
        }
        let options = GetOptions {
            range: Some(GetRange::Bounded(start..end)),
            ..Default::default()
        };
        match self.store.get_opts(&self.key(digest), options).await {
            Ok(result) => Ok(Some(result.into_stream().map_err(storage).boxed())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }

    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        verify(digest, &data)?;
        self.store
            .put(&self.key(digest), PutPayload::from(data))
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn write_stream(
        &self,
        digest: &DigestInfo,
        mut stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        if digest.size <= SINGLE_PUT_MAX_BYTES {
            let mut data = BytesMut::with_capacity(digest.size.max(0) as usize);
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                if (data.len() + chunk.len()) as i64 > digest.size {
                    return Err(CasError::InvalidDigest(format!(
                        "{} declares {} bytes and the stream sends more",
                        digest.hash_to_string(),
                        digest.size
                    )));
                }
                data.extend_from_slice(&chunk);
            }
            return self.write(digest, data.freeze()).await;
        }

        let upload = self
            .store
            .put_multipart(&self.key(digest))
            .await
            .map_err(storage)?;
        let mut writer = WriteMultipart::new(upload);
        let mut hasher = Sha256::new();
        let mut written: i64 = 0;
        let outcome: CasResult<()> = async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                hasher.update(&chunk);
                written += chunk.len() as i64;
                writer
                    .wait_for_capacity(MULTIPART_CONCURRENCY)
                    .await
                    .map_err(storage)?;
                writer.write(&chunk);
            }
            if written != digest.size {
                return Err(CasError::InvalidDigest(format!(
                    "{} declares {} bytes, got {written}",
                    digest.hash_to_string(),
                    digest.size
                )));
            }
            verify_hash(digest, hasher.finalize().into())
        }
        .await;
        match outcome {
            Ok(()) => {
                writer.finish().await.map_err(storage)?;
                Ok(())
            }
            Err(e) => {
                writer.abort().await.ok();
                Err(e)
            }
        }
    }

    async fn find_missing(&self, digests: &[DigestInfo]) -> CasResult<Vec<DigestInfo>> {
        let checks: Vec<_> = digests
            .iter()
            .map(|digest| has_object(self.store.clone(), self.key(digest), digest.size))
            .collect();
        let present: Vec<bool> = futures::stream::iter(checks)
            .buffered(FIND_MISSING_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(digests
            .iter()
            .zip(present)
            .filter(|(_, present)| !present)
            .map(|(digest, _)| *digest)
            .collect())
    }

    async fn delete(&self, digest: &DigestInfo) -> CasResult<()> {
        match self.store.delete(&self.key(digest)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(storage(e)),
        }
    }

    async fn local_path(&self, _digest: &DigestInfo) -> CasResult<Option<std::path::PathBuf>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn backend() -> ObjectStoreBackend {
        ObjectStoreBackend::new(Arc::new(InMemory::new()), "test/")
    }

    fn chunks(data: &[u8], size: usize) -> BoxStream<'static, CasResult<Bytes>> {
        let parts: Vec<CasResult<Bytes>> = data
            .chunks(size)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        futures::stream::iter(parts).boxed()
    }

    #[tokio::test]
    async fn a_written_blob_is_found_read_and_ranged() {
        let cas = backend();
        let data = Bytes::from_static(b"hello object store");
        let digest = DigestInfo::from_bytes(&data);
        assert_eq!(cas.find_missing(&[digest]).await.unwrap(), vec![digest]);

        cas.write(&digest, data.clone()).await.unwrap();
        assert!(cas.contains(&digest).await.unwrap());
        assert_eq!(cas.read(&digest).await.unwrap().unwrap(), data);
        let range: Vec<Bytes> = cas
            .read_stream(&digest, 6, Some(6))
            .await
            .unwrap()
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(range.concat(), b"object");
        assert!(cas.find_missing(&[digest]).await.unwrap().is_empty());

        cas.delete(&digest).await.unwrap();
        assert!(!cas.contains(&digest).await.unwrap());
        assert!(cas.read(&digest).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_access_probe_leaves_nothing_behind() {
        let store = Arc::new(InMemory::new());
        ObjectStoreBackend::new(store.clone(), "ci")
            .check_access()
            .await
            .unwrap();
        let left: Vec<_> = store.list(None).try_collect().await.unwrap();
        assert!(left.is_empty());
    }

    #[tokio::test]
    async fn content_that_does_not_match_its_digest_is_not_stored() {
        let cas = backend();
        let digest = DigestInfo::from_bytes(b"expected");
        assert!(cas
            .write(&digest, Bytes::from_static(b"somethin"))
            .await
            .is_err());
        assert!(!cas.contains(&digest).await.unwrap());
    }

    #[tokio::test]
    async fn a_large_stream_goes_up_in_parts_and_only_if_it_matches() {
        let cas = backend();
        let data: Vec<u8> = (0..(SINGLE_PUT_MAX_BYTES as usize + 3 * 1024 * 1024))
            .map(|i| (i % 251) as u8)
            .collect();
        let digest = DigestInfo::from_bytes(&data);
        cas.write_stream(&digest, chunks(&data, 1 << 20))
            .await
            .unwrap();
        assert_eq!(cas.read(&digest).await.unwrap().unwrap(), data);

        let mut corrupted = data.clone();
        corrupted[10] ^= 1;
        let other = DigestInfo::from_bytes(&data[1..]);
        assert!(cas
            .write_stream(&other, chunks(&corrupted[1..], 1 << 20))
            .await
            .is_err());
        assert!(!cas.contains(&other).await.unwrap());
    }

    #[tokio::test]
    async fn the_prefix_is_a_directory_however_it_is_written() {
        let data = Bytes::from_static(b"prefixed");
        let digest = DigestInfo::from_bytes(&data);
        for (prefix, key) in [
            ("ci", "ci/cas/"),
            ("/ci/", "ci/cas/"),
            ("ci/", "ci/cas/"),
            ("", "cas/"),
            ("/", "cas/"),
        ] {
            let store = Arc::new(InMemory::new());
            ObjectStoreBackend::new(store.clone(), prefix)
                .write(&digest, data.clone())
                .await
                .unwrap();
            let path = ObjectPath::from(format!("{key}{}", digest.hash_to_string()));
            assert!(store.head(&path).await.is_ok(), "{prefix:?} -> {key}");
        }
    }

    #[tokio::test]
    async fn an_empty_range_is_an_empty_stream() {
        let cas = backend();
        let data = Bytes::from_static(b"0123456789");
        let digest = DigestInfo::from_bytes(&data);
        cas.write(&digest, data).await.unwrap();
        let read: Vec<Bytes> = cas
            .read_stream(&digest, 4, Some(0))
            .await
            .unwrap()
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert!(read.is_empty());
    }

    #[tokio::test]
    async fn a_stream_longer_than_its_digest_is_refused_while_buffering() {
        let cas = backend();
        let digest = DigestInfo::from_bytes(b"short");
        let longer = vec![0u8; 4096];
        assert!(cas
            .write_stream(&digest, chunks(&longer, 64))
            .await
            .is_err());
        assert!(!cas.contains(&digest).await.unwrap());
    }

    #[tokio::test]
    async fn find_missing_keeps_the_input_order() {
        let cas = backend();
        let blobs: Vec<Bytes> = (0..10).map(|i| Bytes::from(vec![i as u8; i + 1])).collect();
        let digests: Vec<DigestInfo> = blobs.iter().map(|b| DigestInfo::from_bytes(b)).collect();
        for (digest, blob) in digests.iter().zip(&blobs).step_by(2) {
            cas.write(digest, blob.clone()).await.unwrap();
        }
        let missing = cas.find_missing(&digests).await.unwrap();
        assert_eq!(
            missing,
            digests
                .iter()
                .skip(1)
                .step_by(2)
                .copied()
                .collect::<Vec<_>>()
        );
    }
}
