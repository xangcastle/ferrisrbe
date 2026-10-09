//! Action Cache on an object store, the tier every server shares.
//!
//! An entry lives at `<prefix>ac/<sha256>` as the protobuf-encoded
//! `ActionResult` of the action digest it names. Expiration is the bucket's
//! business (a lifecycle rule): this store never evicts.

use std::sync::Arc;

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use prost::Message;
use tracing::error;

use crate::proto::build::bazel::remote::execution::v2::ActionResult as ProtoActionResult;
use crate::types::DigestInfo;

/// Action results of `store` under a directory already normalized by
/// [`crate::cas::backends::ObjectStoreBackend`].
pub struct ObjectActionCache {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl ObjectActionCache {
    pub(crate) fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }

    fn key(&self, digest: &DigestInfo) -> ObjectPath {
        ObjectPath::from(format!("{}ac/{}", self.prefix, digest.hash_to_string()))
    }

    /// The stored result for `digest`; an undecodable object reads as
    /// missing.
    pub async fn get(
        &self,
        digest: &DigestInfo,
    ) -> object_store::Result<Option<ProtoActionResult>> {
        let key = self.key(digest);
        let data = match self.store.get(&key).await {
            Ok(object) => object.bytes().await?,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        match ProtoActionResult::decode(data) {
            Ok(proto) => Ok(Some(proto)),
            Err(e) => {
                error!("Failed to decode action cache object {}: {}", key, e);
                Ok(None)
            }
        }
    }

    /// Whether an entry for `digest` exists.
    pub async fn contains(&self, digest: &DigestInfo) -> object_store::Result<bool> {
        match self.store.head(&self.key(digest)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Stores `result` for `digest`, replacing any previous entry.
    pub async fn put(
        &self,
        digest: &DigestInfo,
        result: &ProtoActionResult,
    ) -> object_store::Result<()> {
        let payload = PutPayload::from(result.encode_to_vec());
        self.store.put(&self.key(digest), payload).await.map(|_| ())
    }

    /// Removes the entry for `digest`, if any.
    pub async fn remove(&self, digest: &DigestInfo) -> object_store::Result<()> {
        match self.store.delete(&self.key(digest)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn result(exit_code: i32) -> ProtoActionResult {
        ProtoActionResult {
            exit_code,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_stored_result_reads_back_until_removed() {
        let store = Arc::new(InMemory::new());
        let cache = ObjectActionCache::new(store.clone(), "prod/".into());
        let digest = DigestInfo::from_bytes(b"action");
        assert!(cache.get(&digest).await.unwrap().is_none());
        assert!(!cache.contains(&digest).await.unwrap());

        cache.put(&digest, &result(3)).await.unwrap();
        assert_eq!(cache.get(&digest).await.unwrap().unwrap().exit_code, 3);
        assert!(cache.contains(&digest).await.unwrap());
        let key = ObjectPath::from(format!("prod/ac/{}", digest.hash_to_string()));
        assert!(store.head(&key).await.is_ok());

        cache.remove(&digest).await.unwrap();
        cache.remove(&digest).await.unwrap();
        assert!(cache.get(&digest).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_undecodable_object_is_a_miss() {
        let store = Arc::new(InMemory::new());
        let cache = ObjectActionCache::new(store.clone(), String::new());
        let digest = DigestInfo::from_bytes(b"corrupt");
        let key = ObjectPath::from(format!("ac/{}", digest.hash_to_string()));
        store
            .put(&key, PutPayload::from_static(&[0xff, 0xff, 0xff]))
            .await
            .unwrap();
        assert!(cache.get(&digest).await.unwrap().is_none());
    }
}
