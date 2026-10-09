//! Google `ByteStream` service over the CAS, for blobs too large for the
//! batch RPCs.
//!
//! Resource names follow REAPI: `blobs/<hash>/<size>` for reads and
//! `uploads/<uuid>/blobs/<hash>/<size>` for writes.

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use crate::proto::google::bytestream::{
    byte_stream_server::{ByteStream, ByteStreamServer},
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};

use crate::cas::{CasBackend, CasError};
use crate::metrics::Metrics;
use crate::types::DigestInfo;
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Instant;

/// `ByteStream` over the same [`CasBackend`] as the CAS service.
///
/// Uploads stream from the gRPC request into the backend through a bounded
/// channel and are never buffered whole in memory, so blob size is limited by
/// disk, not RAM. Downloads stream chunk by chunk in the same way.
pub struct ByteStreamService {
    backend: Arc<dyn CasBackend>,
    metrics: Arc<Metrics>,
}

/// Upload chunks buffered between the gRPC stream and the backend writer.
/// A full buffer applies backpressure to the client.
const UPLOAD_CHANNEL_DEPTH: usize = 64;

/// Download chunks buffered between the backend reader and the client.
const READ_CHANNEL_DEPTH: usize = 10;

/// An upload is aborted once it exceeds its declared size by this factor, so
/// a client cannot stream unbounded data into a temp file.
const MAX_UPLOAD_OVERRUN_FACTOR: i64 = 2;

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

/// The `(hash, size)` addressed by a `blobs/<hash>/<size>` or
/// `uploads/<uuid>/blobs/<hash>/<size>` resource name: its last two segments.
fn parse_resource_name(resource: &str) -> Option<(String, i64)> {
    let mut segments = resource.rsplit('/');
    let size = segments.next()?.parse::<i64>().ok()?;
    let hash = segments.next()?.to_string();
    Some((hash, size))
}

/// The digest a resource name addresses, or `INVALID_ARGUMENT`.
fn resource_digest(resource: &str) -> Result<DigestInfo, Status> {
    let (hash, size) = parse_resource_name(resource)
        .ok_or_else(|| Status::invalid_argument("Invalid resource name"))?;
    DigestInfo::new(&hash, size)
        .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))
}

impl ByteStreamService {
    pub fn new(backend: Arc<dyn CasBackend>, metrics: Arc<Metrics>) -> Self {
        Self { backend, metrics }
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> ByteStreamServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        ByteStreamServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// The gRPC status for a CAS failure: missing blobs are `NOT_FOUND`,
    /// digest problems `INVALID_ARGUMENT`, everything else `INTERNAL`.
    fn to_status(err: CasError) -> Status {
        match err {
            CasError::NotFound(_) => Status::not_found(err.to_string()),
            CasError::Io(e) => Status::internal(format!("IO error: {}", e)),
            CasError::DigestMismatch { expected, actual } => Status::invalid_argument(format!(
                "Digest mismatch: expected {}, got {}",
                expected, actual
            )),
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
        let start = Instant::now();
        let method = "Read";

        let result: Result<Response<Self::ReadStream>, Status> = async move {
            let req = request.into_inner();
            info!(
                "ByteStream::Read resource={} offset={} limit={}",
                req.resource_name, req.read_offset, req.read_limit
            );

            let digest_info = resource_digest(&req.resource_name)?;
            let (offset, limit) =
                read_window(req.read_offset, req.read_limit, digest_info.size).map_err(|e| *e)?;
            let backend = self.backend.clone();
            let (tx, rx) = tokio::sync::mpsc::channel(READ_CHANNEL_DEPTH);

            tokio::spawn(async move {
                match backend.read_stream(&digest_info, offset, limit).await {
                    Ok(Some(mut stream)) => {
                        while let Some(chunk_result) = stream.next().await {
                            match chunk_result {
                                Ok(data) => {
                                    if tx
                                        .send(Ok(ReadResponse {
                                            data: data.to_vec(),
                                        }))
                                        .await
                                        .is_err()
                                    {
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

                info!(
                    "ByteStream::Read completed resource={} elapsed_ms={}",
                    req.resource_name,
                    start.elapsed().as_millis()
                );
            });

            Ok(Response::new(ReceiverStream::new(rx)))
        }
        .await;

        let status_label = if result.is_ok() { "ok" } else { "error" };
        self.metrics
            .record_grpc_request("byte_stream", method, start.elapsed(), status_label);
        result
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let start = Instant::now();
        let method = "Write";

        let result: Result<Response<WriteResponse>, Status> = async move {
            let mut stream = request.into_inner();

            let first_chunk = match stream.message().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return Err(Status::invalid_argument("Empty stream")),
                Err(e) => return Err(Status::internal(format!("Stream error: {}", e))),
            };

            let WriteRequest {
                resource_name,
                write_offset,
                finish_write,
                data,
            } = first_chunk;
            if resource_name.is_empty() {
                return Err(Status::invalid_argument("Missing resource_name"));
            }

            let digest_info = resource_digest(&resource_name)?;
            let expected_size = digest_info.size;

            if write_offset != 0 {
                let stored = self
                    .backend
                    .contains(&digest_info)
                    .await
                    .map_err(Self::to_status)?;
                if stored && write_offset == expected_size {
                    return Ok(Response::new(WriteResponse {
                        committed_size: expected_size,
                    }));
                }
                return Err(Status::invalid_argument(format!(
                    "write_offset {write_offset} is not the committed size of {resource_name}: \
                     QueryWriteStatus reports 0 until the blob is complete"
                )));
            }

            info!(
                "ByteStream::Write started resource={} size={}",
                resource_name, expected_size
            );

            let (data_tx, data_rx) =
                tokio::sync::mpsc::channel::<Result<Bytes, CasError>>(UPLOAD_CHANNEL_DEPTH);
            let data_stream = tokio_stream::wrappers::ReceiverStream::new(data_rx);

            let backend = self.backend.clone();
            let write_task = tokio::spawn(async move {
                backend
                    .write_stream(&digest_info, Box::pin(data_stream))
                    .await
            });

            let mut total_received: i64 = 0;
            let mut chunk = Some((data, finish_write));
            while let Some((data, finish_write)) = chunk.take() {
                total_received += data.len() as i64;

                if total_received > expected_size * MAX_UPLOAD_OVERRUN_FACTOR {
                    warn!(
                        "Upload size {} exceeds expected size {} by >2x, aborting",
                        total_received, expected_size
                    );
                    return Err(Status::invalid_argument(format!(
                        "Upload size {} exceeds expected {}",
                        total_received, expected_size
                    )));
                }

                if data_tx.send(Ok(Bytes::from(data))).await.is_err() || finish_write {
                    break;
                }
                chunk = stream
                    .message()
                    .await?
                    .map(|next| (next.data, next.finish_write));
            }

            drop(data_tx);

            let write_result = write_task
                .await
                .map_err(|e| Status::internal(format!("Write task failed: {}", e)))?;

            match write_result {
                Ok(()) => {
                    info!(
                        "ByteStream::Write completed resource={} bytes_received={} elapsed_ms={}",
                        resource_name,
                        total_received,
                        start.elapsed().as_millis()
                    );
                    let size = self.backend.size_bytes();
                    let entries = self.backend.count_entries().await;
                    self.metrics.record_cas_stats(size, entries);
                    Ok(Response::new(WriteResponse {
                        committed_size: total_received,
                    }))
                }
                Err(e) => {
                    warn!(
                        "ByteStream::Write failed resource={} error={} elapsed_ms={}",
                        resource_name,
                        e,
                        start.elapsed().as_millis()
                    );
                    Err(Self::to_status(e))
                }
            }
        }
        .await;

        let status_label = if result.is_ok() { "ok" } else { "error" };
        self.metrics
            .record_grpc_request("byte_stream", method, start.elapsed(), status_label);
        result
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let start = Instant::now();
        let method = "QueryWriteStatus";

        let result: Result<Response<QueryWriteStatusResponse>, Status> = async move {
            let req = request.into_inner();
            let digest_info = resource_digest(&req.resource_name)?;
            let size = digest_info.size;

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
        .await;

        let status_label = if result.is_ok() { "ok" } else { "error" };
        self.metrics
            .record_grpc_request("byte_stream", method, start.elapsed(), status_label);
        result
    }
}

/// The offset and optional limit of a read of a `size`-byte blob, rejecting
/// what the ByteStream contract forbids: an offset that is negative or past
/// the end (`OUT_OF_RANGE`) and a negative limit. A zero limit means none.
fn read_window(offset: i64, limit: i64, size: i64) -> Result<(usize, Option<usize>), Box<Status>> {
    if offset < 0 || offset > size {
        return Err(Box::new(Status::out_of_range(format!(
            "read_offset {offset} is outside the {size}-byte blob"
        ))));
    }
    if limit < 0 {
        return Err(Box::new(Status::invalid_argument(format!(
            "read_limit {limit} is negative"
        ))));
    }
    Ok((offset as usize, (limit > 0).then_some(limit as usize)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_resource_name() {
        assert_eq!(
            parse_resource_name("blobs/abc123/100"),
            Some(("abc123".to_string(), 100))
        );
        assert_eq!(
            parse_resource_name("uploads/uuid-123/blobs/def456/200"),
            Some(("def456".to_string(), 200))
        );
        assert!(parse_resource_name("invalid").is_none());
        assert!(parse_resource_name("blobs/abc").is_none());
    }

    #[test]
    fn a_read_window_outside_the_blob_is_rejected() {
        assert_eq!(read_window(0, 0, 10).unwrap(), (0, None));
        assert_eq!(read_window(4, 3, 10).unwrap(), (4, Some(3)));
        assert_eq!(read_window(10, 0, 10).unwrap(), (10, None));
        assert_eq!(
            read_window(-1, 0, 10).unwrap_err().code(),
            tonic::Code::OutOfRange
        );
        assert_eq!(
            read_window(11, 0, 10).unwrap_err().code(),
            tonic::Code::OutOfRange
        );
        assert_eq!(
            read_window(0, -1, 10).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }
}
