
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
    GetTreeRequest, GetTreeResponse,
    SplitBlobRequest, SplitBlobResponse, SpliceBlobRequest, SpliceBlobResponse,
};
use crate::proto::google::rpc::Status as RpcStatus;

use crate::cas::{CasBackend, CasError};
use crate::types::DigestInfo;
use std::sync::Arc;

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
    fn to_digest_info(digest: &Digest) -> DigestInfo {
        DigestInfo::new(&digest.hash, digest.size_bytes)
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
        let req = request.into_inner();
        let requested_count = req.blob_digests.len();

        let mut missing = Vec::new();
        for digest in req.blob_digests {
            let digest_info = Self::to_digest_info(&digest);
            match self.backend.contains(&digest_info).await {
                Ok(false) => missing.push(digest),
                Ok(true) => {}
                Err(e) => return Err(Self::to_status(e)),
            }
        }

        info!(
            "📡 CAS::FindMissingBlobs - requested: {}, missing: {}",
            requested_count,
            missing.len()
        );

        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let req = request.into_inner();
        let mut responses = Vec::new();

        for blob_req in req.requests {
            let digest = blob_req
                .digest
                .clone()
                .ok_or_else(|| Status::invalid_argument("Missing digest"))?;
            
            let digest_info = Self::to_digest_info(&digest);
            let data = bytes::Bytes::from(blob_req.data);
            let hash = digest.hash.clone();

            match self.backend.write(&digest_info, data).await {
                Ok(()) => {
                    // REAPI v2.4: Use RpcStatus instead of status_code
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 0,
                            message: String::new(),
                            details: vec![],
                        }),
                    });
                }
                Err(e) => {
                    responses.push(batch_update_blobs_response::Response {
                        digest: Some(digest),
                        status: Some(RpcStatus {
                            code: 3,
                            message: e.to_string(),
                            details: vec![],
                        }),
                    });
                    tracing::error!("Failed to write blob {}: {}", hash, e);
                }
            }
        }

        info!("✅ CAS::BatchUpdateBlobs updated={}", responses.len());

        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let req = request.into_inner();
        let mut responses = Vec::new();

        for digest in req.digests {
            let digest_info = Self::to_digest_info(&digest);
            let hash = digest.hash.clone();
            
            match self.backend.read(&digest_info).await {
                Ok(Some(data)) => {
                    // REAPI v2.4: Use RpcStatus instead of status_code
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
                Ok(None) => {
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
                Err(e) => {
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
                    tracing::error!("Failed to read blob {}: {}", hash, e);
                }
            }
        }

        info!("📡 CAS::BatchReadBlobs read={}", responses.len());

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
