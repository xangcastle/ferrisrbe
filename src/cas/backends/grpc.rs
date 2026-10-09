//! CAS backend over a remote REAPI `ContentAddressableStorage` and
//! `ByteStream` service, such as `bazel-remote`.
//!
//! Lookups degrade instead of failing: a failed existence check or read is
//! reported as a missing blob, which callers resolve by uploading or
//! executing again. Writes surface their errors.

use crate::grpc_credentials::{AuthChannel, BearerAuth};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::cas::{CasBackend, CasError, CasResult};
use crate::proto::build::bazel::remote::execution::v2::batch_update_blobs_request::Request as UpdateBlobRequest;
use crate::proto::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use crate::proto::build::bazel::remote::execution::v2::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest, FindMissingBlobsRequest,
};
use crate::proto::google::bytestream::byte_stream_client::ByteStreamClient;
use crate::proto::google::bytestream::{ReadRequest, WriteRequest};
use crate::types::DigestInfo;

/// `DigestFunction.Value.SHA256` in the REAPI.
const SHA256_DIGEST_FUNCTION: i32 = 1;

/// Connection attempts made by [`GrpcCasBackend::new`] before giving up.
const CONNECT_ATTEMPTS: u32 = 30;
const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Environment variable overriding the gRPC message size limit, in bytes.
const ENV_MAX_GRPC_MSG_SIZE: &str = "RBE_MAX_GRPC_MSG_SIZE";
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

/// Largest blob that [`CasBackend::read_stream`] fetches with a unary batch
/// read when the ByteStream read cannot be started.
const UNARY_READ_FALLBACK_LIMIT: i64 = 4 * 1024 * 1024;

/// Depth of the channel feeding a ByteStream upload.
const UPLOAD_CHANNEL_DEPTH: usize = 64;

/// CAS client for a remote REAPI endpoint.
pub struct GrpcCasBackend {
    client: ContentAddressableStorageClient<AuthChannel>,
    byte_stream_client: ByteStreamClient<AuthChannel>,
    instance_name: String,
}

fn proto_digest(digest: &DigestInfo) -> Digest {
    Digest {
        hash: digest.hash_to_string(),
        size_bytes: digest.size,
    }
}

fn digest_from_proto(digest: Digest) -> CasResult<DigestInfo> {
    DigestInfo::new(&digest.hash, digest.size_bytes)
        .map_err(|e| CasError::Storage(format!("Invalid digest: {}", e)))
}

/// The digest a batch response entry refers to; `operation` names the batch
/// in the error for a response that omits it.
fn response_digest(digest: Option<Digest>, operation: &str) -> CasResult<DigestInfo> {
    digest
        .map(digest_from_proto)
        .transpose()?
        .ok_or_else(|| CasError::Storage(format!("Missing digest in batch {operation} response")))
}

impl GrpcCasBackend {
    /// Connects to `endpoint` (`host:port`, or a full `http(s)://` URL),
    /// retrying every two seconds for up to a minute so the backend can start
    /// alongside a CAS that is still coming up. Every request carries the
    /// credentials of `auth`.
    ///
    /// Message sizes are limited to [`ENV_MAX_GRPC_MSG_SIZE`] bytes, 100 MiB
    /// by default.
    pub async fn new(endpoint: &str, auth: BearerAuth) -> CasResult<Self> {
        let url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            endpoint.to_string()
        } else {
            format!("http://{}", endpoint)
        };
        let max_msg_size = std::env::var(ENV_MAX_GRPC_MSG_SIZE)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);

        let mut last_error = None;
        for attempt in 1..=CONNECT_ATTEMPTS {
            let connection = Channel::from_shared(url.clone())
                .map_err(|e| CasError::Storage(format!("Invalid endpoint: {}", e)))?
                .connect()
                .await;
            match connection {
                Ok(channel) => {
                    info!(
                        "Connected to gRPC CAS backend at: {} (attempt {})",
                        endpoint, attempt
                    );
                    return Ok(Self {
                        client: ContentAddressableStorageClient::with_interceptor(
                            channel.clone(),
                            auth.clone(),
                        )
                        .max_decoding_message_size(max_msg_size)
                        .max_encoding_message_size(max_msg_size),
                        byte_stream_client: ByteStreamClient::with_interceptor(channel, auth)
                            .max_decoding_message_size(max_msg_size)
                            .max_encoding_message_size(max_msg_size),
                        instance_name: String::new(),
                    });
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < CONNECT_ATTEMPTS {
                        info!(
                            "Waiting for CAS at {} (attempt {}/{})...",
                            endpoint, attempt, CONNECT_ATTEMPTS
                        );
                        tokio::time::sleep(CONNECT_RETRY_DELAY).await;
                    }
                }
            }
        }

        Err(CasError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!(
                "Failed to connect to CAS at {} after {} attempts: {:?}",
                endpoint, CONNECT_ATTEMPTS, last_error
            ),
        )))
    }

    fn find_missing_request(&self, digests: &[DigestInfo]) -> FindMissingBlobsRequest {
        FindMissingBlobsRequest {
            instance_name: self.instance_name.clone(),
            blob_digests: digests.iter().map(proto_digest).collect(),
            digest_function: SHA256_DIGEST_FUNCTION,
        }
    }

    fn batch_read_request(&self, digests: &[DigestInfo]) -> BatchReadBlobsRequest {
        BatchReadBlobsRequest {
            instance_name: self.instance_name.clone(),
            digests: digests.iter().map(proto_digest).collect(),
            acceptable_compressors: vec![],
            digest_function: SHA256_DIGEST_FUNCTION,
        }
    }

    fn batch_update_request<'a>(
        &self,
        items: impl Iterator<Item = (&'a DigestInfo, &'a Bytes)>,
    ) -> BatchUpdateBlobsRequest {
        BatchUpdateBlobsRequest {
            instance_name: self.instance_name.clone(),
            requests: items
                .map(|(digest, data)| UpdateBlobRequest {
                    digest: Some(proto_digest(digest)),
                    data: data.to_vec(),
                    compressor: 0,
                })
                .collect(),
            digest_function: SHA256_DIGEST_FUNCTION,
        }
    }
}

/// The start of a ByteStream resource name: `<instance>/`, or nothing for
/// the default instance, which REAPI names without a leading slash.
fn instance_prefix(instance_name: &str) -> String {
    if instance_name.is_empty() {
        String::new()
    } else {
        format!("{instance_name}/")
    }
}

/// Feeds `stream` into a ByteStream write as `WriteRequest`s. The resource
/// name travels only on the first request, as the ByteStream protocol
/// requires; the last request carries `finish_write`.
async fn feed_upload(
    resource_name: String,
    mut stream: BoxStream<'static, CasResult<Bytes>>,
    tx: tokio::sync::mpsc::Sender<WriteRequest>,
) -> CasResult<()> {
    let mut offset = 0i64;
    let mut resource_name = Some(resource_name);

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.inspect_err(|e| {
            error!("Error reading local stream for CAS upload: {}", e);
        })?;
        let chunk_len = chunk.len() as i64;
        let request = WriteRequest {
            resource_name: resource_name.take().unwrap_or_default(),
            write_offset: offset,
            finish_write: false,
            data: chunk.to_vec(),
        };
        if tx.send(request).await.is_err() {
            error!("ByteStream receiver dropped");
            return Err(CasError::Storage("ByteStream receiver dropped".to_string()));
        }
        offset += chunk_len;
    }

    let final_request = WriteRequest {
        resource_name: resource_name.unwrap_or_default(),
        write_offset: offset,
        finish_write: true,
        data: vec![],
    };
    let _ = tx.send(final_request).await;
    Ok(())
}

#[async_trait]
impl CasBackend for GrpcCasBackend {
    /// A failed RPC is logged and reported as not present.
    async fn contains(&self, digest: &DigestInfo) -> CasResult<bool> {
        trace!(
            "GrpcCasBackend::contains() called for digest: {} (size={})",
            digest.hash_to_string(),
            digest.size
        );

        let request = self.find_missing_request(std::slice::from_ref(digest));
        match self.client.clone().find_missing_blobs(request).await {
            Ok(response) => Ok(response.into_inner().missing_blob_digests.is_empty()),
            Err(e) => {
                warn!("Failed to check blob existence: {}", e);
                Ok(false)
            }
        }
    }

    /// A failed RPC is logged and every digest is reported missing, so the
    /// caller uploads them again rather than trusting an unknown state.
    async fn find_missing(&self, digests: &[DigestInfo]) -> CasResult<Vec<DigestInfo>> {
        trace!(
            "GrpcCasBackend::find_missing() called for {} digests",
            digests.len()
        );

        if digests.is_empty() {
            return Ok(Vec::new());
        }

        let request = self.find_missing_request(digests);
        match self.client.clone().find_missing_blobs(request).await {
            Ok(response) => response
                .into_inner()
                .missing_blob_digests
                .into_iter()
                .map(digest_from_proto)
                .collect(),
            Err(e) => {
                warn!("Failed to find missing blobs: {}", e);
                Ok(digests.to_vec())
            }
        }
    }

    /// A failed RPC or a non-OK status for the blob is reported as not
    /// present.
    async fn read(&self, digest: &DigestInfo) -> CasResult<Option<Bytes>> {
        trace!(
            "GrpcCasBackend::read() called for digest: {} (size={})",
            digest.hash_to_string(),
            digest.size
        );

        let request = self.batch_read_request(std::slice::from_ref(digest));
        match self.client.clone().batch_read_blobs(request).await {
            Ok(response) => Ok(response
                .into_inner()
                .responses
                .into_iter()
                .next()
                .filter(|response| response.status.as_ref().map(|s| s.code).unwrap_or(0) == 0)
                .map(|response| response.data.into())),
            Err(e) => {
                warn!("Failed to read blob {}: {}", digest.hash_to_string(), e);
                Ok(None)
            }
        }
    }

    /// Blobs the remote reports with a non-OK status come back as `None`; a
    /// failed RPC fails the whole batch.
    async fn batch_read(
        &self,
        digests: &[DigestInfo],
    ) -> CasResult<Vec<(DigestInfo, Option<Bytes>)>> {
        trace!(
            "GrpcCasBackend::batch_read() called for {} digests",
            digests.len()
        );

        if digests.is_empty() {
            return Ok(Vec::new());
        }

        let request = self.batch_read_request(digests);
        match self.client.clone().batch_read_blobs(request).await {
            Ok(response) => response
                .into_inner()
                .responses
                .into_iter()
                .map(|response| {
                    let digest = response_digest(response.digest, "read")?;
                    let ok = response.status.as_ref().map(|s| s.code).unwrap_or(0) == 0;
                    Ok((digest, ok.then(|| response.data.into())))
                })
                .collect(),
            Err(e) => {
                warn!("Failed to batch read {} blobs: {}", digests.len(), e);
                Err(CasError::Storage(format!("gRPC batch read failed: {}", e)))
            }
        }
    }

    /// Streams the blob over ByteStream. When the stream cannot be started,
    /// blobs under 4 MiB are fetched with a unary read instead.
    async fn read_stream(
        &self,
        digest: &DigestInfo,
        offset: usize,
        limit: Option<usize>,
    ) -> CasResult<Option<BoxStream<'static, CasResult<Bytes>>>> {
        let request = ReadRequest {
            resource_name: format!(
                "{}blobs/{}/{}",
                instance_prefix(&self.instance_name),
                digest.hash_to_string(),
                digest.size
            ),
            read_offset: offset as i64,
            read_limit: limit.map(|l| l as i64).unwrap_or(0),
        };

        match self.byte_stream_client.clone().read(request).await {
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
                if digest.size >= UNARY_READ_FALLBACK_LIMIT {
                    return Err(CasError::Storage(format!("ByteStream read failed: {}", e)));
                }
                Ok(self.read(digest).await?.map(|data| {
                    Box::pin(futures::stream::once(async move { Ok(data) }))
                        as BoxStream<'static, CasResult<Bytes>>
                }))
            }
        }
    }

    /// Uploads with a unary batch update. Only a failed RPC is an error; the
    /// per-blob status in the response is not inspected.
    async fn write(&self, digest: &DigestInfo, data: Bytes) -> CasResult<()> {
        let request = self.batch_update_request(std::iter::once((digest, &data)));
        match self.client.clone().batch_update_blobs(request).await {
            Ok(_) => Ok(()),
            Err(e) => {
                error!("Failed to write blob {}: {}", digest.hash_to_string(), e);
                Err(CasError::Storage(format!("gRPC error: {}", e)))
            }
        }
    }

    /// Blobs the remote rejects come back with an error carrying its status;
    /// a failed RPC fails the whole batch.
    async fn batch_write(
        &self,
        items: &[(DigestInfo, Bytes)],
    ) -> CasResult<Vec<(DigestInfo, CasResult<()>)>> {
        trace!(
            "GrpcCasBackend::batch_write() called for {} blobs",
            items.len()
        );

        if items.is_empty() {
            return Ok(Vec::new());
        }

        let request = self.batch_update_request(items.iter().map(|(d, data)| (d, data)));
        match self.client.clone().batch_update_blobs(request).await {
            Ok(response) => response
                .into_inner()
                .responses
                .into_iter()
                .map(|response| {
                    let digest = response_digest(response.digest, "write")?;
                    let status_code = response.status.as_ref().map(|s| s.code).unwrap_or(0);
                    let result = if status_code == 0 {
                        Ok(())
                    } else {
                        let message = response
                            .status
                            .as_ref()
                            .map(|s| s.message.clone())
                            .unwrap_or_default();
                        Err(CasError::Storage(format!(
                            "CAS rejected blob: {} (code {})",
                            message, status_code
                        )))
                    };
                    Ok((digest, result))
                })
                .collect(),
            Err(e) => {
                error!("Failed to batch write {} blobs: {}", items.len(), e);
                Err(CasError::Storage(format!("gRPC batch write failed: {}", e)))
            }
        }
    }

    /// Uploads over ByteStream. The write succeeds only if the local stream
    /// was consumed without error and the remote committed exactly
    /// `digest.size` bytes.
    async fn write_stream(
        &self,
        digest: &DigestInfo,
        stream: BoxStream<'static, CasResult<Bytes>>,
    ) -> CasResult<()> {
        let resource_name = format!(
            "{}uploads/{}/blobs/{}/{}",
            instance_prefix(&self.instance_name),
            Uuid::new_v4(),
            digest.hash_to_string(),
            digest.size
        );

        let (tx, rx) = tokio::sync::mpsc::channel::<WriteRequest>(UPLOAD_CHANNEL_DEPTH);
        let feeder = tokio::spawn(feed_upload(resource_name, stream, tx));

        let response = match self
            .byte_stream_client
            .clone()
            .write(tonic::Request::new(ReceiverStream::new(rx)))
            .await
        {
            Ok(response) => response,
            Err(e) => {
                feeder.abort();
                return Err(CasError::Storage(format!(
                    "gRPC ByteStream write failed: {}",
                    e
                )));
            }
        };

        match feeder.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(CasError::Storage(format!(
                    "Local stream error during ByteStream upload: {}",
                    e
                )))
            }
            Err(e) => return Err(CasError::Storage(format!("Stream task failed: {}", e))),
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

    /// The remote CAS manages its own retention; deletion is a no-op.
    async fn delete(&self, _digest: &DigestInfo) -> CasResult<()> {
        debug!("Delete not supported by gRPC CAS backend");
        Ok(())
    }

    async fn local_path(&self, _digest: &DigestInfo) -> CasResult<Option<PathBuf>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_instance_has_no_leading_slash() {
        assert_eq!(instance_prefix(""), "");
        assert_eq!(instance_prefix("main"), "main/");
    }
}
