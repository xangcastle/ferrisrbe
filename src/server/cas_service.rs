//! REAPI `ContentAddressableStorage` service: existence checks and batch
//! transfers of small blobs. Large blobs go through `ByteStream`, which
//! shares the same [`CasBackend`].
//!
//! Batch responses always list every requested digest in request order,
//! each with its own status; a backend failure fails the items, not the RPC.

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Request, Response, Status};
use tracing::info;

use crate::proto::build::bazel::remote::execution::v2::{
    batch_read_blobs_response, batch_update_blobs_response,
    content_addressable_storage_server::{
        ContentAddressableStorage, ContentAddressableStorageServer,
    },
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, Digest, Directory, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetTreeRequest, GetTreeResponse, SpliceBlobRequest, SpliceBlobResponse, SplitBlobRequest,
    SplitBlobResponse,
};
use crate::proto::google::rpc::Status as RpcStatus;

use crate::cas::{CasBackend, CasError, CasResult};
use crate::metrics::Metrics;
use crate::types::DigestInfo;
use bytes::Bytes;
use prost::Message;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

const MISSING_BATCH_RESULT: &str = "Missing result in batch response";

/// Directories per `GetTree` page when the client sets no `page_size`.
const DEFAULT_TREE_PAGE_SIZE: usize = 1000;

/// Per-item status of a batch response.
fn rpc_status(code: Code, message: impl Into<String>) -> Option<RpcStatus> {
    Some(RpcStatus {
        code: code as i32,
        message: message.into(),
        details: vec![],
    })
}

/// `ContentAddressableStorage` over the shared [`CasBackend`].
pub struct CasService {
    backend: Arc<dyn CasBackend>,
    metrics: Arc<Metrics>,
}

impl CasService {
    pub fn new(backend: Arc<dyn CasBackend>, metrics: Arc<Metrics>) -> Self {
        Self { backend, metrics }
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> ContentAddressableStorageServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        ContentAddressableStorageServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// The internal form of a REAPI digest, or `INVALID_ARGUMENT`. Returning
    /// `tonic::Status` directly is what lets handlers use `?` on it.
    #[allow(clippy::result_large_err)]
    fn to_digest_info(digest: &Digest) -> Result<DigestInfo, Status> {
        DigestInfo::new(&digest.hash, digest.size_bytes)
            .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))
    }

    /// The gRPC status for a CAS failure: missing blobs are `NOT_FOUND`,
    /// digest problems `INVALID_ARGUMENT`, everything else `INTERNAL`.
    fn to_status(err: CasError) -> Status {
        match err {
            CasError::NotFound(_) => Status::not_found(err.to_string()),
            CasError::Io(e) => Status::internal(format!("IO error: {}", e)),
            CasError::DigestMismatch { .. } => Status::invalid_argument(err.to_string()),
            CasError::InvalidDigest(_) => Status::invalid_argument(err.to_string()),
            _ => Status::internal(err.to_string()),
        }
    }

    /// Update CAS size and entry-count gauges using backend state.
    /// Every `Directory` reachable from `root`, breadth-first, each once.
    /// Children missing from the CAS are left out, as REAPI allows; a missing
    /// root is `NOT_FOUND`.
    async fn collect_tree(&self, root: DigestInfo) -> Result<Vec<Directory>, Status> {
        let mut directories = Vec::new();
        let mut seen = HashSet::from([root]);
        let mut pending = VecDeque::from([root]);
        while let Some(digest) = pending.pop_front() {
            let Some(bytes) = self.backend.read(&digest).await.map_err(Self::to_status)? else {
                if digest == root {
                    return Err(Status::not_found(format!(
                        "root directory {digest} not found"
                    )));
                }
                continue;
            };
            let directory = Directory::decode(bytes).map_err(|e| {
                Status::invalid_argument(format!("blob {digest} is not a Directory: {e}"))
            })?;
            for child in &directory.directories {
                let Some(child) = child.digest.as_ref() else {
                    continue;
                };
                let child = Self::to_digest_info(child)?;
                if seen.insert(child) {
                    pending.push_back(child);
                }
            }
            directories.push(directory);
        }
        Ok(directories)
    }

    async fn update_cas_stats(&self) {
        let size = self.backend.size_bytes();
        let entries = self.backend.count_entries().await;
        self.metrics.record_cas_stats(size, entries);
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for CasService {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let start = Instant::now();
        let method = "FindMissingBlobs";
        let req = request.into_inner();
        let requested_count = req.blob_digests.len();

        let result: Result<Response<FindMissingBlobsResponse>, Status> = async move {
            let digest_infos: Vec<DigestInfo> = req
                .blob_digests
                .iter()
                .map(Self::to_digest_info)
                .collect::<Result<Vec<_>, _>>()?;
            let missing_infos = self
                .backend
                .find_missing(&digest_infos)
                .await
                .map_err(Self::to_status)?;

            let missing_hashes: std::collections::HashSet<String> =
                missing_infos.iter().map(|d| d.hash_to_string()).collect();
            let missing: Vec<Digest> = req
                .blob_digests
                .into_iter()
                .filter(|d| missing_hashes.contains(&d.hash))
                .collect();

            info!(
                "CAS::FindMissingBlobs - requested: {}, missing: {}, elapsed_ms: {}",
                requested_count,
                missing.len(),
                start.elapsed().as_millis()
            );

            Ok(Response::new(FindMissingBlobsResponse {
                missing_blob_digests: missing,
            }))
        }
        .await;

        let status_label = if result.is_ok() { "ok" } else { "error" };
        self.metrics
            .record_grpc_request("cas", method, start.elapsed(), status_label);
        result
    }

    /// Writes every blob and reports a status per item: `OK`,
    /// `INVALID_ARGUMENT` when the data does not match its digest, or
    /// `UNKNOWN` when the backend failed. A request without a digest fails
    /// the whole RPC with `INVALID_ARGUMENT`.
    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let start = Instant::now();
        let method = "BatchUpdateBlobs";
        let req = request.into_inner();

        let mut ordered_digests = Vec::with_capacity(req.requests.len());
        let mut items: Vec<(DigestInfo, Bytes)> = Vec::with_capacity(req.requests.len());
        for blob_req in req.requests {
            let digest = blob_req
                .digest
                .clone()
                .ok_or_else(|| Status::invalid_argument("Missing digest"))?;
            let digest_info = Self::to_digest_info(&digest)?;
            let data = Bytes::from(blob_req.data);
            ordered_digests.push((digest, digest_info.hash_to_string()));
            items.push((digest_info, data));
        }

        let responses: Vec<batch_update_blobs_response::Response> =
            match self.backend.batch_write(&items).await {
                Ok(batch_results) => {
                    let result_by_hash: HashMap<String, CasResult<()>> = batch_results
                        .into_iter()
                        .map(|(digest, result)| (digest.hash_to_string(), result))
                        .collect();
                    let responses: Vec<_> = ordered_digests
                        .into_iter()
                        .map(|(digest, hash)| {
                            let status = match result_by_hash.get(&hash) {
                                Some(Ok(())) => rpc_status(Code::Ok, String::new()),
                                Some(Err(e)) => {
                                    tracing::error!("Failed to write blob {}: {}", hash, e);
                                    rpc_status(Code::InvalidArgument, e.to_string())
                                }
                                None => {
                                    tracing::error!(
                                        "Missing result for blob {} in batch response",
                                        hash
                                    );
                                    rpc_status(Code::Unknown, MISSING_BATCH_RESULT)
                                }
                            };
                            batch_update_blobs_response::Response {
                                digest: Some(digest),
                                status,
                            }
                        })
                        .collect();
                    info!(
                        "CAS::BatchUpdateBlobs updated={}, elapsed_ms={}",
                        responses.len(),
                        start.elapsed().as_millis()
                    );
                    responses
                }
                Err(e) => {
                    tracing::error!("BatchUpdateBlobs backend failure: {}", e);
                    ordered_digests
                        .into_iter()
                        .map(|(digest, _)| batch_update_blobs_response::Response {
                            digest: Some(digest),
                            status: rpc_status(Code::Unknown, e.to_string()),
                        })
                        .collect()
                }
            };

        self.metrics
            .record_grpc_request("cas", method, start.elapsed(), "ok");
        self.update_cas_stats().await;
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    /// Reads every blob and reports a status per item: `OK` with the data,
    /// `NOT_FOUND`, or `UNKNOWN` when the backend failed. Data is returned
    /// uncompressed.
    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let start = Instant::now();
        let method = "BatchReadBlobs";
        let req = request.into_inner();

        let ordered_digests: Vec<(Digest, DigestInfo, String)> = req
            .digests
            .into_iter()
            .map(|digest| -> Result<(Digest, DigestInfo, String), Status> {
                let digest_info = Self::to_digest_info(&digest)?;
                let hash = digest.hash.clone();
                Ok((digest, digest_info, hash))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let digest_infos: Vec<DigestInfo> =
            ordered_digests.iter().map(|(_, info, _)| *info).collect();

        let read_response = |digest: Digest, data: Vec<u8>, status: Option<RpcStatus>| {
            batch_read_blobs_response::Response {
                digest: Some(digest),
                data,
                compressor: 0,
                status,
            }
        };

        let responses: Vec<batch_read_blobs_response::Response> = match self
            .backend
            .batch_read(&digest_infos)
            .await
        {
            Ok(batch_results) => {
                let data_by_hash: HashMap<String, Option<Bytes>> = batch_results
                    .into_iter()
                    .map(|(digest, data)| (digest.hash_to_string(), data))
                    .collect();
                let responses: Vec<_> = ordered_digests
                    .into_iter()
                    .map(|(digest, _, hash)| match data_by_hash.get(&hash) {
                        Some(Some(data)) => read_response(
                            digest,
                            data.to_vec(),
                            rpc_status(Code::Ok, String::new()),
                        ),
                        Some(None) => read_response(
                            digest,
                            Vec::new(),
                            rpc_status(Code::NotFound, format!("Blob not found: {}", hash)),
                        ),
                        None => {
                            tracing::error!("Missing result for blob {} in batch response", hash);
                            read_response(
                                digest,
                                Vec::new(),
                                rpc_status(Code::Unknown, MISSING_BATCH_RESULT),
                            )
                        }
                    })
                    .collect();
                info!(
                    "CAS::BatchReadBlobs read={}, elapsed_ms={}",
                    responses.len(),
                    start.elapsed().as_millis()
                );
                responses
            }
            Err(e) => {
                tracing::error!("BatchReadBlobs backend failure: {}", e);
                ordered_digests
                    .into_iter()
                    .map(|(digest, _, _)| {
                        read_response(digest, Vec::new(), rpc_status(Code::Unknown, e.to_string()))
                    })
                    .collect()
            }
        };

        self.metrics
            .record_grpc_request("cas", method, start.elapsed(), "ok");
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = ReceiverStream<Result<GetTreeResponse, Status>>;

    /// Streams the directories under `root_digest`, breadth-first, in pages
    /// of `page_size` (default [`DEFAULT_TREE_PAGE_SIZE`]). Each page's
    /// `next_page_token` is the offset of the next one, so a client can
    /// resume a broken stream by passing it back as `page_token`.
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<ReceiverStream<Result<GetTreeResponse, Status>>>, Status> {
        let start = Instant::now();
        let method = "GetTree";
        let req = request.into_inner();

        let root = req
            .root_digest
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("root_digest is required"))?;
        let root = Self::to_digest_info(root)?;
        let offset = match req.page_token.as_str() {
            "" => 0,
            token => token
                .parse::<usize>()
                .map_err(|_| Status::invalid_argument(format!("invalid page_token {token:?}")))?,
        };
        let page_size = usize::try_from(req.page_size)
            .ok()
            .filter(|&size| size > 0)
            .unwrap_or(DEFAULT_TREE_PAGE_SIZE);

        let directories = match self.collect_tree(root).await {
            Ok(directories) => directories,
            Err(status) => {
                self.metrics
                    .record_grpc_request("cas", method, start.elapsed(), "error");
                return Err(status);
            }
        };

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            let total = directories.len();
            let mut remaining = directories.into_iter().skip(offset);
            let mut next = offset.min(total);
            loop {
                let page: Vec<Directory> = remaining.by_ref().take(page_size).collect();
                next += page.len();
                let next_page_token = if next < total {
                    next.to_string()
                } else {
                    String::new()
                };
                let last = next_page_token.is_empty();
                let sent = tx
                    .send(Ok(GetTreeResponse {
                        directories: page,
                        next_page_token,
                    }))
                    .await;
                if last || sent.is_err() {
                    break;
                }
            }
        });

        self.metrics
            .record_grpc_request("cas", method, start.elapsed(), "ok");
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// `UNIMPLEMENTED`: blob splitting is not advertised in the capabilities.
    async fn split_blob(
        &self,
        _request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        let start = Instant::now();
        self.metrics
            .record_grpc_request("cas", "SplitBlob", start.elapsed(), "error");
        Err(Status::unimplemented("SplitBlob not implemented"))
    }

    /// `UNIMPLEMENTED`: blob splicing is not advertised in the capabilities.
    async fn splice_blob(
        &self,
        _request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        let start = Instant::now();
        self.metrics
            .record_grpc_request("cas", "SpliceBlob", start.elapsed(), "error");
        Err(Status::unimplemented("SpliceBlob not implemented"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::backends::DiskBackend;
    use crate::proto::build::bazel::remote::execution::v2::{DirectoryNode, FileNode};
    use sha2::{Digest as _, Sha256};
    use tokio_stream::StreamExt;

    async fn service() -> (tempfile::TempDir, CasService) {
        let temp = tempfile::TempDir::new().unwrap();
        let backend: Arc<dyn CasBackend> = Arc::new(DiskBackend::new(temp.path()).await.unwrap());
        (temp, CasService::new(backend, Arc::new(Metrics::new())))
    }

    async fn store(service: &CasService, directory: &Directory) -> Digest {
        let bytes = directory.encode_to_vec();
        let digest = Digest {
            hash: hex::encode(Sha256::digest(&bytes)),
            size_bytes: bytes.len() as i64,
        };
        let info = CasService::to_digest_info(&digest).unwrap();
        service
            .backend
            .write(&info, Bytes::from(bytes))
            .await
            .unwrap();
        digest
    }

    fn file(name: &str) -> FileNode {
        FileNode {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn dir(name: &str, digest: &Digest) -> DirectoryNode {
        DirectoryNode {
            name: name.to_string(),
            digest: Some(digest.clone()),
        }
    }

    async fn tree(
        service: &CasService,
        root: &Digest,
        page_size: i32,
        token: &str,
    ) -> Vec<GetTreeResponse> {
        let request = Request::new(GetTreeRequest {
            root_digest: Some(root.clone()),
            page_size,
            page_token: token.to_string(),
            ..Default::default()
        });
        let stream = service.get_tree(request).await.unwrap().into_inner();
        stream.map(|page| page.unwrap()).collect().await
    }

    #[tokio::test]
    async fn get_tree_walks_every_directory_once_in_pages() {
        let (_temp, service) = service().await;
        let shared = store(
            &service,
            &Directory {
                files: vec![file("shared.txt")],
                ..Default::default()
            },
        )
        .await;
        let a = store(
            &service,
            &Directory {
                directories: vec![dir("lib", &shared)],
                ..Default::default()
            },
        )
        .await;
        let b = store(
            &service,
            &Directory {
                directories: vec![dir("vendor", &shared)],
                ..Default::default()
            },
        )
        .await;
        let root = store(
            &service,
            &Directory {
                directories: vec![dir("a", &a), dir("b", &b)],
                ..Default::default()
            },
        )
        .await;

        let pages = tree(&service, &root, 2, "").await;
        let sizes: Vec<usize> = pages.iter().map(|p| p.directories.len()).collect();
        assert_eq!(sizes, vec![2, 2]);
        assert_eq!(pages[0].next_page_token, "2");
        assert_eq!(pages[1].next_page_token, "");

        let resumed = tree(&service, &root, 0, "2").await;
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].directories.len(), 2);
        assert_eq!(resumed[0].directories[1].files[0].name, "shared.txt");
    }

    #[tokio::test]
    async fn get_tree_leaves_out_missing_children_but_not_a_missing_root() {
        let (_temp, service) = service().await;
        let absent = Digest {
            hash: "ab".repeat(32),
            size_bytes: 10,
        };
        let root = store(
            &service,
            &Directory {
                directories: vec![dir("gone", &absent)],
                ..Default::default()
            },
        )
        .await;

        let pages = tree(&service, &root, 0, "").await;
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].directories.len(), 1);

        let missing = service
            .get_tree(Request::new(GetTreeRequest {
                root_digest: Some(absent),
                ..Default::default()
            }))
            .await;
        assert_eq!(missing.err().unwrap().code(), Code::NotFound);
    }
}
