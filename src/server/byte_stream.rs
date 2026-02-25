
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use crate::proto::google::bytestream::{
    byte_stream_server::{ByteStream, ByteStreamServer},
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};

use crate::cas::{CasBackend, CasError};
use crate::types::DigestInfo;
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;

/// ByteStream Service
/// 
/// Implements the Google ByteStream API for streaming blob upload/download.
/// Uses the same CasBackend as CasService for unified storage.
/// 
/// # Streaming Architecture
/// 
/// For writes, data flows directly from the gRPC stream to disk without
/// accumulating in memory. This allows handling blobs of any size (GBs)
/// without OOM errors.
/// 
/// ```
/// gRPC Stream → Channel → DiskBackend → Temp File → Atomic Rename
/// ```
pub struct ByteStreamService {
    backend: Arc<dyn CasBackend>,
}

/// Maximum size of a single chunk from gRPC
#[allow(dead_code)]
const MAX_CHUNK_SIZE: usize = 64 * 1024;

/// Channel buffer size for backpressure
/// Larger values = more memory usage but better throughput
/// Smaller values = less memory but more context switches
const CHANNEL_BUFFER_SIZE: usize = 4;

impl ByteStreamService {
    pub fn new(backend: Arc<dyn CasBackend>) -> Self {
        Self { backend }
    }

    pub fn into_service(self) -> ByteStreamServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        ByteStreamServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// Parse resource name in format: "uploads/<uuid>/blobs/<hash>/<size>"
    /// or "blobs/<hash>/<size>"
    fn parse_resource_name(&self, resource: &str) -> Option<(String, i64)> {
        let parts: Vec<&str> = resource.split('/').collect();
        if parts.len() >= 2 {
            let hash = parts[parts.len() - 2].to_string();
            let size = parts.last()?.parse::<i64>().ok()?;
            Some((hash, size))
        } else {
            None
        }
    }
    
    /// Convert CasError to tonic Status
    fn to_status(err: CasError) -> Status {
        match err {
            CasError::NotFound(_) => Status::not_found(err.to_string()),
            CasError::Io(e) => Status::internal(format!("IO error: {}", e)),
            CasError::DigestMismatch { expected, actual } => {
                Status::invalid_argument(format!(
                    "Digest mismatch: expected {}, got {}", expected, actual
                ))
            }
            CasError::InvalidDigest(msg) => Status::invalid_argument(msg),
            _ => Status::internal(err.to_string()),
        }
    }
}

#[tonic::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ReceiverStream<Result<ReadResponse, Status>>;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<ReceiverStream<Result<ReadResponse, Status>>>, Status> {
        let req = request.into_inner();
        info!(
            "📡 ByteStream::Read resource={} offset={} limit={}",
            req.resource_name, req.read_offset, req.read_limit
        );

        let (hash, _size) = self
            .parse_resource_name(&req.resource_name)
            .ok_or_else(|| Status::invalid_argument("Invalid resource name"))?;
        
        let digest_info = DigestInfo::new(&hash, _size);
        let backend = self.backend.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(10);

        tokio::spawn(async move {
            let limit = if req.read_limit > 0 {
                Some(req.read_limit as usize)
            } else {
                None
            };
            
            match backend.read_stream(
                &digest_info,
                req.read_offset as usize,
                limit
            ).await {
                Ok(Some(mut stream)) => {
                    while let Some(chunk_result) = stream.next().await {
                        match chunk_result {
                            Ok(data) => {
                                if tx.send(Ok(ReadResponse {
                                    data: data.to_vec(),
                                })).await.is_err() {
                                    debug!("Client disconnected during read");
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(Self::to_status(e))).await;
                                break;
                            }
                        }
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Err(Status::not_found("Blob not found"))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(Self::to_status(e))).await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();
        
        let first_chunk = match stream.message().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return Err(Status::invalid_argument("Empty stream")),
            Err(e) => return Err(Status::internal(format!("Stream error: {}", e))),
        };
        
        let resource_name = first_chunk.resource_name;
        if resource_name.is_empty() {
            return Err(Status::invalid_argument("Missing resource_name"));
        }
        
        let (hash, expected_size) = self
            .parse_resource_name(&resource_name)
            .ok_or_else(|| Status::invalid_argument("Invalid resource name"))?;
        
        let digest_info = DigestInfo::new(&hash, expected_size);
        
        info!(
            "📡 ByteStream::Write started resource={} size={}",
            resource_name, expected_size
        );
        
        let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Result<Bytes, CasError>>(CHANNEL_BUFFER_SIZE);
        let data_stream = tokio_stream::wrappers::ReceiverStream::new(data_rx);
        
        let backend = self.backend.clone();
        let write_task = tokio::spawn(async move {
            backend.write_stream(&digest_info, Box::pin(data_stream)).await
        });
        
        let mut total_received: i64 = first_chunk.data.len() as i64;
        if data_tx.send(Ok(Bytes::from(first_chunk.data))).await.is_err() {
            return Err(Status::internal("Backend channel closed"));
        }
        
        while let Some(chunk_result) = stream.message().await? {
            let chunk_size = chunk_result.data.len() as i64;
            total_received += chunk_size;
            
            if total_received > expected_size * 2 {
                warn!(
                    "Upload size {} exceeds expected size {} by >2x, aborting",
                    total_received, expected_size
                );
                return Err(Status::invalid_argument(
                    format!("Upload size {} exceeds expected {}", total_received, expected_size)
                ));
            }
            
            if data_tx.send(Ok(Bytes::from(chunk_result.data))).await.is_err() {
                break;
            }
            
            if chunk_result.finish_write {
                break;
            }
        }
        
        drop(data_tx);
        
        let write_result = write_task.await
            .map_err(|e| Status::internal(format!("Write task failed: {}", e)))?;
        
        match write_result {
            Ok(()) => {
                info!(
                    "✅ ByteStream::Write completed resource={} bytes_received={}",
                    resource_name, total_received
                );
                Ok(Response::new(WriteResponse { 
                    committed_size: total_received 
                }))
            }
            Err(e) => {
                warn!(
                    "❌ ByteStream::Write failed resource={} error={}",
                    resource_name, e
                );
                Err(Self::to_status(e))
            }
        }
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let req = request.into_inner();
        let (hash, size) = self
            .parse_resource_name(&req.resource_name)
            .ok_or_else(|| Status::invalid_argument("Invalid resource name"))?;
        
        let digest_info = DigestInfo::new(&hash, size);

        let committed_size = match self.backend.contains(&digest_info).await {
            Ok(true) => size,
            Ok(false) => 0,
            Err(e) => return Err(Self::to_status(e)),
        };

        Ok(Response::new(QueryWriteStatusResponse {
            committed_size,
            complete: committed_size == size,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::backends::DiskBackend;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_parse_resource_name() {
        let temp_dir = TempDir::new().unwrap();
        let backend = Arc::new(DiskBackend::new(temp_dir.path()).await.unwrap());
        let service = ByteStreamService::new(backend);
        
        let result = service.parse_resource_name("blobs/abc123/100");
        assert_eq!(result, Some(("abc123".to_string(), 100)));
        
        let result = service.parse_resource_name("uploads/uuid-123/blobs/def456/200");
        assert_eq!(result, Some(("def456".to_string(), 200)));
        
        assert!(service.parse_resource_name("invalid").is_none());
        assert!(service.parse_resource_name("blobs/abc").is_none());
    }

    #[tokio::test]
    async fn test_write_and_read_streaming() {
        let temp_dir = TempDir::new().unwrap();
        let backend = Arc::new(DiskBackend::new(temp_dir.path()).await.unwrap());
        let service = ByteStreamService::new(backend);
        
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let data = b"Hello, World! test data";
        
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(data);
        let actual_hash = hex::encode(hasher.finalize());
        let size = data.len() as i64;
        
        let chunks: Vec<WriteRequest> = vec![
            WriteRequest {
                resource_name: format!("blobs/{}/{}", actual_hash, size),
                write_offset: 0,
                data: data[..5].to_vec(),
                finish_write: false,
            },
            WriteRequest {
                resource_name: String::new(),
                write_offset: 5,
                data: data[5..10].to_vec(),
                finish_write: false,
            },
            WriteRequest {
                resource_name: String::new(),
                write_offset: 10,
                data: data[10..].to_vec(),
                finish_write: true,
            },
        ];
        
    }
}
