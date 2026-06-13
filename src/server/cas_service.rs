use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::info;

use crate::proto::build::bazel::remote::execution::v2::{
    batch_read_blobs_response, batch_update_blobs_response,
    content_addressable_storage_server::{
        ContentAddressableStorage, ContentAddressableStorageServer,
    },
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, Digest, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetTreeRequest, GetTreeResponse, SpliceBlobRequest, SpliceBlobResponse, SplitBlobRequest,
    SplitBlobResponse,
};
use crate::proto::google::rpc::Status as RpcStatus;

use crate::cas::{CasBackend, CasError, CasResult};
use crate::types::DigestInfo;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// CAS (Content Addressable Storage) Service
///
/// Implements the REAPI v2 ContentAddressableStorage interface.
/// All blobs are stored via the shared CasBackend, ensuring consistency
/// with ByteStreamService.
pub struct CasService {
    backend: Arc<dyn CasBackend>,
}

impl CasService {
    pub fn new(backend: Arc<dyn CasBackend>) -> Self {
        Self { backend }
    }

    pub fn into_service(self) -> ContentAddressableStorageServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        ContentAddressableStorageServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// Convert REAPI Digest to internal DigestInfo
    fn to_digest_info(digest: &Digest) -> Result<DigestInfo, Status> {
        DigestInfo::new(&digest.hash, digest.size_bytes)
            .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))
    }

    /// Convert CasError to tonic Status
    fn to_status(err: CasError) -> Status {
        match err {
            CasError::NotFound(_) => Status::not_found(err.to_string()),
            CasError::Io(e) => Status::internal(format!("IO error: {}", e)),
            CasError::DigestMismatch { .. } => Status::invalid_argument(err.to_string()),
            CasError::InvalidDigest(_) => Status::invalid_argument(err.to_string()),
            _ => Status::internal(err.to_string()),
        }
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for CasService {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let start = Instant::now();
        let req = request.into_inner();
        let requested_count = req.blob_digests.len();

        let digest_infos: Vec<DigestInfo> = req
            .blob_digests
            .iter()
            .map(Self::to_digest_info)
            .collect::<Result<Vec<_>, _>>()?;
        let missing_infos = match self.backend.find_missing(&digest_infos).await {
            Ok(missing) => missing,
            Err(e) => return Err(Self::to_status(e)),
        };

        // Map back to proto digests preserving the original order among missing ones.
        let missing_hashes: std::collections::HashSet<String> = missing_infos
            .iter()
            .map(|d| d.hash_to_string())
            .collect();
        let missing: Vec<Digest> = req
            .blob_digests
            .into_iter()
            .filter(|d| missing_hashes.contains(&d.hash))
            .collect();

        info!(
            "📡 CAS::FindMissingBlobs - requested: {}, missing: {}, elapsed_ms: {}",
            requested_count,
            missing.len(),
            start.elapsed().as_millis()
        );

        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let start = Instant::now();
        let req = request.into_inner();

        // Preserve original order and map each digest to its data.
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

        let batch_results = match self.backend.batch_write(&items).await {
            Ok(results) => results,
            Err(e) => {
                // Backend-level failure: mark every item as failed so we still
                // return a valid REAPI response.
                tracing::error!("BatchUpdateBlobs backend failure: {}", e);
                let mut responses = Vec::with_capacity(ordered_digests.len());
                for (digest, _hash) in ordered_digests {
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 2,
                            message: e.to_string(),
                            details: vec![],
                        }),
                    });
                }
                return Ok(Response::new(BatchUpdateBlobsResponse { responses }));
            }
        };

        let result_by_hash: HashMap<String, CasResult<()>> = batch_results
            .into_iter()
            .map(|(digest, result)| (digest.hash_to_string(), result))
            .collect();

        let mut responses = Vec::with_capacity(ordered_digests.len());
        for (digest, hash) in ordered_digests {
            match result_by_hash.get(&hash) {
                Some(Ok(())) => {
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 0,
                            message: String::new(),
                            details: vec![],
                        }),
                    });
                }
                Some(Err(e)) => {
                    tracing::error!("Failed to write blob {}: {}", hash, e);
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 3,
                            message: e.to_string(),
                            details: vec![],
                        }),
                    });
                }
                None => {
                    tracing::error!("Missing result for blob {} in batch response", hash);
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 2,
                            message: "Missing result in batch response".to_string(),
                            details: vec![],
                        }),
                    });
                }
            }
        }

        info!(
            "✅ CAS::BatchUpdateBlobs updated={}, elapsed_ms={}",
            responses.len(),
            start.elapsed().as_millis()
        );

        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let start = Instant::now();
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

        let digest_infos: Vec<DigestInfo> = ordered_digests
            .iter()
            .map(|(_, info, _)| *info)
            .collect();

        let batch_results = match self.backend.batch_read(&digest_infos).await {
            Ok(results) => results,
            Err(e) => {
                tracing::error!("BatchReadBlobs backend failure: {}", e);
                let mut responses = Vec::with_capacity(ordered_digests.len());
                for (digest, _info, _hash) in ordered_digests {
                    responses.push(batch_read_blobs_response::Response {
                        digest: Some(digest),
                        data: Vec::new(),
                        compressor: 0,
                        status: Some(RpcStatus {
                            code: 2,
                            message: e.to_string(),
                            details: vec![],
                        }),
                    });
                }
                return Ok(Response::new(BatchReadBlobsResponse { responses }));
            }
        };

        let data_by_hash: HashMap<String, Option<Bytes>> = batch_results
            .into_iter()
            .map(|(digest, data)| (digest.hash_to_string(), data))
            .collect();

        let mut responses = Vec::with_capacity(ordered_digests.len());
        for (digest, _info, hash) in ordered_digests {
            match data_by_hash.get(&hash) {
                Some(Some(data)) => {
                    responses.push(batch_read_blobs_response::Response {
                        digest: Some(digest),
                        data: data.to_vec(),
                        compressor: 0,
                        status: Some(RpcStatus {
                            code: 0,
                            message: String::new(),
                            details: vec![],
                        }),
                    });
                }
                Some(None) => {
                    responses.push(batch_read_blobs_response::Response {
                        digest: Some(digest),
                        data: Vec::new(),
                        compressor: 0,
                        status: Some(RpcStatus {
                            code: 5,
                            message: format!("Blob not found: {}", hash),
                            details: vec![],
                        }),
                    });
                }
                None => {
                    tracing::error!("Missing result for blob {} in batch response", hash);
                    responses.push(batch_read_blobs_response::Response {
                        digest: Some(digest),
                        data: Vec::new(),
                        compressor: 0,
                        status: Some(RpcStatus {
                            code: 2,
                            message: "Missing result in batch response".to_string(),
                            details: vec![],
                        }),
                    });
                }
            }
        }

        info!(
            "📡 CAS::BatchReadBlobs read={}, elapsed_ms={}",
            responses.len(),
            start.elapsed().as_millis()
        );

        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = ReceiverStream<Result<GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        _request: Request<GetTreeRequest>,
    ) -> Result<Response<ReceiverStream<Result<GetTreeResponse, Status>>>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(10);

        let _ = tx
            .send(Ok(GetTreeResponse {
                directories: Vec::new(),
                next_page_token: String::new(),
            }))
            .await;

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    // REAPI v2.4: New optional methods - basic implementation
    async fn split_blob(
        &self,
        _request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        // Not implemented - returns error
        Err(Status::unimplemented("SplitBlob not implemented"))
    }

    async fn splice_blob(
        &self,
        _request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        // Not implemented - returns error
        Err(Status::unimplemented("SpliceBlob not implemented"))
    }
}
