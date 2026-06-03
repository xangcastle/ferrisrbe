//! gRPC-based CAS Backend
//!
//! Connects to an external CAS service (like bazel-remote) via gRPC.
//! Uses the REAPI ContentAddressableStorage and ByteStream interfaces.

use async_trait::async_trait;
use bytes::Bytes;
#[allow(unused_imports)]
use bytes::BytesMut;
use futures::stream::BoxStream;
use std::path::PathBuf;
use tracing::{debug, error, info, trace, warn};

use crate::cas::{CasBackend, CasError, CasResult};
use crate::proto::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use crate::proto::build::bazel::remote::execution::v2::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
};
use crate::proto::google::bytestream::byte_stream_client::ByteStreamClient;
use crate::proto::google::bytestream::{ReadRequest, WriteRequest};
use crate::types::DigestInfo;
use futures::StreamExt;
#[allow(unused_imports)]
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use uuid::Uuid;

/// gRPC-based CAS backend that connects to an external CAS service
pub struct GrpcCasBackend {
    /// gRPC client for ContentAddressableStorage
    client: ContentAddressableStorageClient<Channel>,
    /// gRPC client for ByteStream (streaming uploads/downloads)
    byte_stream_client: ByteStreamClient<Channel>,
    /// Instance name for the CAS service
    instance_name: String,
    /// Channel for reconnection
    #[allow(dead_code)]
    channel: Channel,
}

impl GrpcCasBackend {
    /// Create a new gRPC CAS backend with retries
    pub async fn new(endpoint: &str) -> CasResult<Self> {
        let endpoint_owned: String =
            if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
                endpoint.to_string()
            } else {
                format!("http://{}", endpoint)
            };

        let mut last_error = None;
        for attempt in 1..=30 {
            match Channel::from_shared(endpoint_owned.clone())
                .map_err(|e| CasError::Storage(format!("Invalid endpoint: {}", e)))?
                .connect()
                .await
            {
                Ok(channel) => {
                    let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(100 * 1024 * 1024);
                    let client = ContentAddressableStorageClient::new(channel.clone())
                        .max_decoding_message_size(max_msg_size)
                        .max_encoding_message_size(max_msg_size);
                    let byte_stream_client = ByteStreamClient::new(channel.clone())
                        .max_decoding_message_size(max_msg_size)
                        .max_encoding_message_size(max_msg_size);
                    info!(
                        "Connected to gRPC CAS backend at: {} (attempt {})",
                        endpoint, attempt
                    );

                    return Ok(Self {
                        client,
                        byte_stream_client,
                        instance_name: String::new(),
                        channel,
                    });
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < 30 {
                        info!(
                            "Waiting for CAS at {} (attempt {}/30)...",
                            endpoint, attempt
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }

        Err(CasError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!(
                "Failed to connect to CAS at {} after 30 attempts: {:?}",
                endpoint, last_error
            ),
        )))
    }

    /// Convert DigestInfo to proto Digest
    fn to_proto_digest(&self, digest: &DigestInfo) -> Digest {
        let hash_str = digest.hash_to_string();
        trace!(
            "Converting DigestInfo to proto Digest: hash={}, size={}",
            hash_str,
            digest.size
        );
        Digest {
            hash: hash_str,
            size_bytes: digest.size,
        }
    }
}

#[async_trait]
impl CasBackend for GrpcCasBackend {
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        trace!(
            "GrpcCasBackend::contains() called for digest: {} (size={})",
            digest.hash_to_string(),
            digest.size
        );

        let request = crate::proto::build::bazel::remote::execution::v2::FindMissingBlobsRequest {
            instance_name: self.instance_name.clone(),
            blob_digests: vec![self.to_proto_digest(digest)],
            digest_function: 1,
        };

        let mut client = self.client.clone();
        match client.find_missing_blobs(request).await {
            Ok(response) => {
                let missing = response.into_inner().missing_blob_digests;
                Ok(missing.is_empty())
            }
            Err(e) => {
                warn!("Failed to check blob existence: {}", e);
                Ok(false)
            }
        }
    }

    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        trace!(
            "GrpcCasBackend::read() called for digest: {} (size={})",
            digest.hash_to_string(),
            digest.size
        );

        let request = BatchReadBlobsRequest {
            instance_name: self.instance_name.clone(),
            digests: vec![self.to_proto_digest(digest)],
            acceptable_compressors: vec![],
            digest_function: 1,
        };

        let mut client = self.client.clone();
        match client.batch_read_blobs(request).await {
            Ok(response) => {
                let inner = response.into_inner();
                if let Some(response) = inner.responses.into_iter().next() {
                    return Ok(Some(response.data.into()));
                }
                Ok(None)
            }
            Err(e) => {
                warn!("Failed to read blob {}: {}", digest.hash_to_string(), e);
                Ok(None)
            }
        }
    }

    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        let resource_name = format!(
            "{}/blobs/{}/{}",
            self.instance_name,
            digest.hash_to_string(),
            digest.size
        );

        let read_limit = limit.map(|l| l as i64).unwrap_or(0);
        let request = ReadRequest {
            resource_name,
            read_offset: offset as i64,
            read_limit,
        };

        let mut client = self.byte_stream_client.clone();
        match client.read(request).await {
            Ok(response) => {
                let stream = response.into_inner().map(|result| {
                    result
                        .map(|r| Bytes::from(r.data))
                        .map_err(|e| CasError::Storage(format!("ByteStream read error: {}", e)))
                });
                Ok(Some(Box::pin(stream)))
            }
            Err(e) => {
                warn!(
                    "Failed to start ByteStream read for {}: {}",
                    digest.hash_to_string(),
                    e
                );
                if digest.size < 4 * 1024 * 1024 {
                    match self.read(digest).await? {
                        Some(data) => {
                            let stream = futures::stream::once(async move { Ok(data) });
                            Ok(Some(Box::pin(stream)))
                        }
                        None => Ok(None),
                    }
                } else {
                    Err(CasError::Storage(format!("ByteStream read failed: {}", e)))
                }
            }
        }
    }

    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        let request = BatchUpdateBlobsRequest {
            instance_name: self.instance_name.clone(),
            requests: vec![crate::proto::build::bazel::remote::execution::v2::batch_update_blobs_request::Request {
                digest: Some(self.to_proto_digest(digest)),
                data: data.to_vec(),
                compressor: 0,
            }],
            digest_function: 1,
        };

        let mut client = self.client.clone();
        match client.batch_update_blobs(request).await {
            Ok(response) => {
                let inner = response.into_inner();
                if let Some(_response) = inner.responses.into_iter().next() {
                    return Ok(());
                }
                Ok(())
            }
            Err(e) => {
                error!("Failed to write blob {}: {}", digest.hash_to_string(), e);
                Err(CasError::Storage(format!("gRPC error: {}", e)))
            }
        }
    }

    async fn write_stream(
        &self,
        digest: &DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        let resource_name = format!(
            "{}/uploads/{}/blobs/{}/{}",
            self.instance_name,
            Uuid::new_v4(),
            digest.hash_to_string(),
            digest.size
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<WriteRequest>(4);
        let resource_name_clone = resource_name.clone();

        let stream_task = tokio::spawn(async move {
            let mut offset = 0i64;
            let mut is_first = true;
            let mut stream = stream;

            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        let chunk_len = chunk.len() as i64;
                        let request = WriteRequest {
                            resource_name: if is_first {
                                resource_name_clone.clone()
                            } else {
                                String::new()
                            },
                            write_offset: offset,
                            finish_write: false,
                            data: chunk.to_vec(),
                        };

                        if tx.send(request).await.is_err() {
                            error!("ByteStream receiver dropped");
                            return Err(CasError::Storage(
                                "ByteStream receiver dropped".to_string(),
                            ));
                        }

                        offset += chunk_len;
                        is_first = false;
                    }
                    Err(e) => {
                        error!("Error reading local stream for CAS upload: {}", e);
                        return Err(e);
                    }
                }
            }

            let final_request = WriteRequest {
                resource_name: if is_first {
                    resource_name_clone
                } else {
                    String::new()
                },
                write_offset: offset,
                finish_write: true,
                data: vec![],
            };

            let _ = tx.send(final_request).await;
            Ok(())
        });

        let request_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut client = self.byte_stream_client.clone();

        match client.write(tonic::Request::new(request_stream)).await {
            Ok(response) => {
                match stream_task.await {
                    Ok(Ok(())) => {
                        // Local stream completed successfully
                    }
                    Ok(Err(e)) => {
                        // Local stream failed - abort the upload even if gRPC returned OK
                        return Err(CasError::Storage(format!(
                            "Local stream error during ByteStream upload: {}",
                            e
                        )));
                    }
                    Err(e) => {
                        // Task panicked or was cancelled
                        return Err(CasError::Storage(format!("Stream task failed: {}", e)));
                    }
                }

                let committed = response.into_inner().committed_size;
                if committed != digest.size {
                    return Err(CasError::Storage(format!(
                        "Partial ByteStream upload: expected {} bytes, committed {}",
                        digest.size, committed
                    )));
                }
                debug!(
                    "ByteStream upload successful for {}",
                    digest.hash_to_string()
                );
                Ok(())
            }
            Err(e) => {
                stream_task.abort();
                Err(CasError::Storage(format!(
                    "gRPC ByteStream write failed: {}",
                    e
                )))
            }
        }
    }

    async fn delete(&self, _digest: &DigestInfo) -> CasResult<()> {
        debug!("Delete not supported by gRPC CAS backend");
        Ok(())
    }

    async fn local_path(&self, _digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        Ok(None)
    }
}
