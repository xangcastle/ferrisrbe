//! REAPI `Capabilities` service: the contract Bazel negotiates before using
//! the cache or remote execution.
//!
//! The server advertises REAPI 2.0 through 2.4, SHA-256 digests, uncompressed
//! transfers only, action cache updates from clients, and absolute symlinks
//! in outputs.

use std::sync::Arc;
use std::time::Instant;
use tonic::{Request, Response, Status};
use tracing::{debug, info};

use crate::metrics::Metrics;
use crate::proto::build::bazel::remote::execution::v2::{
    capabilities_server::{Capabilities, CapabilitiesServer},
    priority_capabilities::PriorityRange,
    ActionCacheUpdateCapabilities, CacheCapabilities, ExecutionCapabilities,
    GetCapabilitiesRequest, PriorityCapabilities, ServerCapabilities,
};
use crate::proto::build::bazel::semver::SemVer;

/// `DigestFunction.Value.SHA256`, the only digest function served.
const DIGEST_SHA256: i32 = 1;

/// `Compressor.Value.IDENTITY` only: ByteStream and the batch RPCs move blobs
/// uncompressed, so a client must not be offered zstd.
const COMPRESSORS: [i32; 1] = [0];

/// `SymlinkAbsolutePathStrategy.Value.ALLOWED`.
const SYMLINK_ABSOLUTE_PATHS_ALLOWED: i32 = 2;

/// Upper bound on the combined size of one `BatchUpdateBlobs` or
/// `BatchReadBlobs` request; larger transfers go through `ByteStream`.
const MAX_BATCH_TOTAL_SIZE_BYTES: i64 = 4 * 1024 * 1024;

/// Priority range accepted for cache and execution requests.
pub(crate) const PRIORITY_RANGE: PriorityRange = PriorityRange {
    min_priority: 0,
    max_priority: 1000,
};

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

fn api_version(major: i32, minor: i32) -> SemVer {
    SemVer {
        major,
        minor,
        patch: 0,
        prerelease: String::new(),
    }
}

/// `Capabilities` answering with a fixed [`ServerCapabilities`].
pub struct CapabilitiesService {
    exec_enabled: bool,
    metrics: Arc<Metrics>,
}

impl CapabilitiesService {
    /// `exec_enabled` controls whether remote execution is advertised; the
    /// cache capabilities are always advertised.
    pub fn new(exec_enabled: bool, metrics: Arc<Metrics>) -> Self {
        Self {
            exec_enabled,
            metrics,
        }
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> CapabilitiesServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        CapabilitiesServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    /// The capabilities advertised to every client.
    ///
    /// Priorities use the REAPI 2.4 repeated `priorities` field. Blob
    /// splitting and splicing are not supported, and `max_cas_blob_size_bytes`
    /// is `0`: no per-blob limit beyond the CAS disk budget.
    fn create_base_capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            cache_capabilities: Some(CacheCapabilities {
                digest_functions: vec![DIGEST_SHA256],
                action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                cache_priority_capabilities: Some(PriorityCapabilities {
                    priorities: vec![PRIORITY_RANGE],
                }),
                max_batch_total_size_bytes: MAX_BATCH_TOTAL_SIZE_BYTES,
                symlink_absolute_path_strategy: SYMLINK_ABSOLUTE_PATHS_ALLOWED,
                supported_compressors: COMPRESSORS.to_vec(),
                supported_batch_update_compressors: COMPRESSORS.to_vec(),
                max_cas_blob_size_bytes: 0,
                split_blob_support: false,
                rep_max_cdc_params: None,
                fast_cdc_2020_params: None,
                splice_blob_support: false,
            }),
            execution_capabilities: Some(ExecutionCapabilities {
                digest_function: DIGEST_SHA256,
                exec_enabled: self.exec_enabled,
                execution_priority_capabilities: Some(PriorityCapabilities {
                    priorities: vec![PRIORITY_RANGE],
                }),
                supported_node_properties: vec![],
                digest_functions: vec![DIGEST_SHA256],
            }),
            deprecated_api_version: Some(api_version(2, 0)),
            low_api_version: Some(api_version(2, 0)),
            high_api_version: Some(api_version(2, 4)),
        }
    }
}

impl Default for CapabilitiesService {
    fn default() -> Self {
        Self::new(true, Arc::new(Metrics::new()))
    }
}

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        let start = Instant::now();
        let method = "GetCapabilities";

        let addr = request.remote_addr();
        let req = request.into_inner();

        info!(
            "Capabilities from {:?} instance='{}'",
            addr, req.instance_name
        );

        let caps = self.create_base_capabilities();

        info!("Capabilities returned (REAPI v2.4 compatible - Bazel 8.3.0+ ready)");
        debug!(
            "deprecated={:?}, low={:?}, high={:?}, exec_enabled={}",
            caps.deprecated_api_version
                .as_ref()
                .map(|v| format!("{}.{}", v.major, v.minor)),
            caps.low_api_version
                .as_ref()
                .map(|v| format!("{}.{}", v.major, v.minor)),
            caps.high_api_version
                .as_ref()
                .map(|v| format!("{}.{}", v.major, v.minor)),
            caps.execution_capabilities
                .as_ref()
                .map(|e| e.exec_enabled)
                .unwrap_or(false),
        );

        self.metrics
            .record_grpc_request("capabilities", method, start.elapsed(), "ok");
        Ok(Response::new(caps))
    }
}
