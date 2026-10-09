//! REAPI `ActionCache` service.
//!
//! A cached `ActionResult` is served only while every blob it references is
//! still in the CAS, including the files listed by its output directory
//! trees. An entry whose outputs were evicted is dropped and reported as a
//! miss, so Bazel re-runs the action instead of failing to download outputs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moka::future::Cache;
use prost::Message;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::cache::action_cache::{ActionCacheGetResult, ActionCacheStore, CacheActionResult};
use crate::cache::action_cache_log::ActionCacheLog;
use crate::cas::{CasBackend, CasError, CasResult};
use crate::metrics::Metrics;
use crate::proto::build::bazel::remote::execution::v2::{
    action_cache_server::{ActionCache, ActionCacheServer},
    ActionResult, Digest, GetActionResultRequest, Tree, UpdateActionResultRequest,
};
use crate::server::request_metadata::request_metadata;
use crate::types::DigestInfo;

/// How long a tree whose files were all found in the CAS is trusted without
/// checking again. Trees are content-addressed, so a verified tree can only
/// become stale through eviction of its files, which this bounds.
const VERIFIED_TREE_TTL: Duration = Duration::from_secs(15 * 60);

/// Upper bound on the verified-tree memo.
const VERIFIED_TREE_CAPACITY: u64 = 100_000;

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

/// Checks that a cached `ActionResult` can still be served: every blob it
/// references is in the CAS, the files of its output trees included. Shared
/// by `GetActionResult` and the cache lookup of `Execute`, so both trust the
/// same verified trees.
pub struct ResultVerifier {
    cas: Arc<dyn CasBackend>,
    verified_trees: Cache<String, ()>,
}

impl ResultVerifier {
    pub fn new(cas: Arc<dyn CasBackend>) -> Self {
        Self {
            cas,
            verified_trees: Cache::builder()
                .max_capacity(VERIFIED_TREE_CAPACITY)
                .time_to_live(VERIFIED_TREE_TTL)
                .build(),
        }
    }

    /// Count the blobs an `ActionResult` needs that the CAS no longer has,
    /// descending into output directory trees.
    pub async fn missing_blobs(&self, result: &ActionResult) -> CasResult<usize> {
        let mut missing = self
            .cas
            .find_missing(&referenced_blobs(result))
            .await?
            .len();

        for tree_digest in tree_digests(result) {
            let key = tree_digest.hash_to_string();
            if self.verified_trees.get(&key).await.is_some() {
                continue;
            }
            let Some(bytes) = self.cas.read(&tree_digest).await? else {
                missing += 1;
                continue;
            };
            let files = tree_file_digests(&bytes)?;
            let lost = self.cas.find_missing(&files).await?;
            if lost.is_empty() {
                self.verified_trees.insert(key, ()).await;
            } else {
                missing += lost.len();
            }
        }

        Ok(missing)
    }
}

/// `ActionCache` over an [`ActionCacheStore`], validating hits against the CAS.
pub struct ActionCacheService {
    store: Arc<dyn ActionCacheStore>,
    verifier: Arc<ResultVerifier>,
    metrics: Arc<Metrics>,
    log: Option<Arc<ActionCacheLog>>,
}

impl ActionCacheService {
    pub fn new(
        store: Arc<dyn ActionCacheStore>,
        cas: Arc<dyn CasBackend>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self::with_verifier(store, Arc::new(ResultVerifier::new(cas)), metrics)
    }

    /// The service over a verifier shared with the execution service.
    pub fn with_verifier(
        store: Arc<dyn ActionCacheStore>,
        verifier: Arc<ResultVerifier>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            store,
            verifier,
            metrics,
            log: None,
        }
    }

    /// Record every lookup per invocation and target for the BES.
    pub fn with_log(mut self, log: Arc<ActionCacheLog>) -> Self {
        self.log = Some(log);
        self
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> ActionCacheServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        ActionCacheServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }
}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let start = Instant::now();
        let caller = request_metadata(&request);
        let digest_info = action_digest_of(request.into_inner().action_digest)?;

        info!("ActionCache::GetActionResult digest={}", digest_info);

        let ActionCacheGetResult { result, tier } = self.store.get_with_tier(&digest_info).await;
        self.metrics.record_action_cache_get(tier);

        let mut status_label = match &result {
            Some(_) => "ok",
            None => "not_found",
        };

        let response = match result {
            Some(result) => match self.verifier.missing_blobs(&result.proto).await {
                Ok(0) => {
                    info!("ActionCache HIT for digest={}", digest_info);
                    Ok(Response::new(result.proto))
                }
                Ok(missing) => {
                    warn!(
                        "ActionCache entry {} references {} blob(s) no longer in the CAS; dropping it",
                        digest_info, missing
                    );
                    self.metrics.record_action_cache_stale_hit();
                    self.store.remove(&digest_info).await;
                    status_label = "stale";
                    Err(Status::not_found(
                        "Action result outputs were evicted from the CAS",
                    ))
                }
                Err(e) => {
                    warn!(
                        "Could not verify CAS blobs for ActionCache entry {}: {}",
                        digest_info, e
                    );
                    status_label = "cas_error";
                    Err(Status::not_found(
                        "Action result could not be verified against the CAS",
                    ))
                }
            },
            None => {
                info!("ActionCache MISS for digest={}", digest_info);
                Err(Status::not_found("Action result not found in cache"))
            }
        };

        self.metrics.record_grpc_request(
            "action_cache",
            "GetActionResult",
            start.elapsed(),
            status_label,
        );
        if let (Some(log), Some(caller)) = (&self.log, &caller) {
            if !caller.tool_invocation_id.is_empty() && !caller.target_id.is_empty() {
                log.record(
                    &caller.tool_invocation_id,
                    &caller.target_id,
                    &caller.action_mnemonic,
                    status_label == "ok",
                )
                .await;
            }
        }

        response
    }

    async fn update_action_result(
        &self,
        request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let start = Instant::now();
        let req = request.into_inner();
        let action_result = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("Missing action_result"))?;
        let digest_info = action_digest_of(req.action_digest)?;

        info!("ActionCache::UpdateActionResult digest={}", digest_info);

        self.metrics.record_action_cache_update(None);

        let cache_result = CacheActionResult::new(digest_info, action_result);
        let proto_result = cache_result.proto.clone();

        self.store.put(digest_info, cache_result).await;

        info!("ActionCache updated for digest={}", digest_info);

        self.metrics.record_grpc_request(
            "action_cache",
            "UpdateActionResult",
            start.elapsed(),
            "ok",
        );

        Ok(Response::new(proto_result))
    }
}

/// The action digest of a request, or `INVALID_ARGUMENT` when it is absent
/// or malformed.
fn action_digest_of(digest: Option<Digest>) -> Result<DigestInfo, Status> {
    let digest = digest.ok_or_else(|| Status::invalid_argument("Missing action_digest"))?;
    DigestInfo::new(&digest.hash, digest.size_bytes)
        .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))
}

/// The non-empty `Tree` digests of the result's output directories.
fn tree_digests(result: &ActionResult) -> Vec<DigestInfo> {
    result
        .output_directories
        .iter()
        .filter_map(|d| d.tree_digest.as_ref())
        .filter(|d| d.size_bytes > 0)
        .filter_map(|d| DigestInfo::new(&d.hash, d.size_bytes).ok())
        .collect()
}

/// The non-empty file digests listed anywhere in an encoded `Tree`.
///
/// Bazel materialises an output directory from its `Tree` blob alone, so
/// these files are the only further blobs a directory output requires.
pub fn tree_file_digests(tree: &[u8]) -> CasResult<Vec<DigestInfo>> {
    let tree = Tree::decode(tree)
        .map_err(|e| CasError::InvalidData(format!("Failed to decode Tree: {}", e)))?;
    Ok(tree
        .root
        .iter()
        .chain(tree.children.iter())
        .flat_map(|dir| dir.files.iter())
        .filter_map(|f| f.digest.as_ref())
        .filter(|d| d.size_bytes > 0)
        .filter_map(|d| DigestInfo::new(&d.hash, d.size_bytes).ok())
        .collect())
}

/// Every blob an `ActionResult` references directly: output files, output
/// directory trees, stdout and stderr. Size-zero digests are omitted because
/// the empty blob is never fetched from the CAS.
pub fn referenced_blobs(result: &ActionResult) -> Vec<DigestInfo> {
    result
        .output_files
        .iter()
        .filter_map(|f| f.digest.as_ref())
        .chain(
            result
                .output_directories
                .iter()
                .filter_map(|d| d.tree_digest.as_ref()),
        )
        .chain(result.stdout_digest.iter())
        .chain(result.stderr_digest.iter())
        .filter(|d| d.size_bytes > 0)
        .filter_map(|d| DigestInfo::new(&d.hash, d.size_bytes).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::action_cache::L1ActionCache;
    use crate::cas::backends::DiskBackend;
    use crate::proto::build::bazel::remote::execution::v2::{
        Digest, Directory, FileNode, OutputDirectory, OutputFile,
    };
    use bytes::Bytes;
    use tempfile::TempDir;

    fn digest_of(data: &[u8]) -> (DigestInfo, Digest) {
        use sha2::{Digest as _, Sha256};
        let hash = hex::encode(Sha256::digest(data));
        let info = DigestInfo::new(&hash, data.len() as i64).unwrap();
        let proto = Digest {
            hash,
            size_bytes: data.len() as i64,
        };
        (info, proto)
    }

    fn result_with_output(digest: Digest) -> ActionResult {
        ActionResult {
            output_files: vec![OutputFile {
                path: "out".into(),
                digest: Some(digest),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    async fn fixture() -> (
        TempDir,
        Arc<L1ActionCache>,
        Arc<DiskBackend>,
        ActionCacheService,
    ) {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(L1ActionCache::default());
        let cas = Arc::new(DiskBackend::new(temp.path()).await.unwrap());
        let service = ActionCacheService::new(store.clone(), cas.clone(), Arc::new(Metrics::new()));
        (temp, store, cas, service)
    }

    fn lookup(action: &Digest) -> Request<GetActionResultRequest> {
        Request::new(GetActionResultRequest {
            action_digest: Some(action.clone()),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn hit_is_served_when_every_output_blob_is_present() {
        let (_temp, store, cas, service) = fixture().await;
        let data = b"output bytes";
        let (blob_info, blob) = digest_of(data);
        cas.write(&blob_info, Bytes::from_static(data))
            .await
            .unwrap();
        let (action_info, action) = digest_of(b"action");
        store.put(
            action_info,
            CacheActionResult::new(action_info, result_with_output(blob)),
        );

        let response = service.get_action_result(lookup(&action)).await.unwrap();
        assert_eq!(response.into_inner().output_files.len(), 1);
    }

    #[tokio::test]
    async fn hit_becomes_a_miss_and_is_dropped_when_an_output_blob_is_gone() {
        let (_temp, store, _cas, service) = fixture().await;
        let (_, blob) = digest_of(b"never written to the cas");
        let (action_info, action) = digest_of(b"action");
        store.put(
            action_info,
            CacheActionResult::new(action_info, result_with_output(blob)),
        );

        let status = service
            .get_action_result(lookup(&action))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert!(
            store.get(&action_info).is_none(),
            "stale entry must be dropped"
        );
    }

    fn tree_with_file(file: Digest) -> Vec<u8> {
        Tree {
            root: Some(Directory {
                files: vec![FileNode {
                    name: "lib.a".into(),
                    digest: Some(file),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            children: vec![],
        }
        .encode_to_vec()
    }

    fn result_with_tree(tree: Digest) -> ActionResult {
        ActionResult {
            output_directories: vec![OutputDirectory {
                path: "pkg".into(),
                tree_digest: Some(tree),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn tree_hit_is_dropped_when_a_listed_file_is_gone() {
        let (_temp, store, cas, service) = fixture().await;
        let (_, lost_file) = digest_of(b"evicted stdlib archive");
        let tree_bytes = tree_with_file(lost_file);
        let (tree_info, tree) = digest_of(&tree_bytes);
        cas.write(&tree_info, Bytes::from(tree_bytes))
            .await
            .unwrap();
        let (action_info, action) = digest_of(b"go stdlib");
        store.put(
            action_info,
            CacheActionResult::new(action_info, result_with_tree(tree)),
        );

        let status = service
            .get_action_result(lookup(&action))
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert!(store.get(&action_info).is_none());
    }

    #[tokio::test]
    async fn tree_hit_is_served_when_every_listed_file_is_present() {
        let (_temp, store, cas, service) = fixture().await;
        let data = b"present stdlib archive";
        let (file_info, file) = digest_of(data);
        cas.write(&file_info, Bytes::from_static(data))
            .await
            .unwrap();
        let tree_bytes = tree_with_file(file);
        let (tree_info, tree) = digest_of(&tree_bytes);
        cas.write(&tree_info, Bytes::from(tree_bytes))
            .await
            .unwrap();
        let (action_info, action) = digest_of(b"go stdlib");
        store.put(
            action_info,
            CacheActionResult::new(action_info, result_with_tree(tree)),
        );

        let response = service.get_action_result(lookup(&action)).await.unwrap();
        assert_eq!(response.into_inner().output_directories.len(), 1);
    }

    #[test]
    fn referenced_blobs_covers_files_trees_and_std_streams_but_skips_empty() {
        let (_, file) = digest_of(b"file");
        let (_, tree) = digest_of(b"tree");
        let (_, stdout) = digest_of(b"stdout");
        let (_, empty) = digest_of(b"");
        let result = ActionResult {
            output_files: vec![OutputFile {
                path: "f".into(),
                digest: Some(file.clone()),
                ..Default::default()
            }],
            output_directories: vec![OutputDirectory {
                path: "d".into(),
                tree_digest: Some(tree.clone()),
                ..Default::default()
            }],
            stdout_digest: Some(stdout.clone()),
            stderr_digest: Some(empty),
            ..Default::default()
        };
        let hashes: Vec<String> = referenced_blobs(&result)
            .iter()
            .map(|d| d.hash_to_string())
            .collect();
        assert_eq!(hashes, vec![file.hash, tree.hash, stdout.hash]);
    }
}
