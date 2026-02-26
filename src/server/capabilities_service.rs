use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{debug, info};

use crate::proto::build::bazel::remote::execution::v2::{
    capabilities_server::{Capabilities, CapabilitiesServer},
    priority_capabilities::PriorityRange,
    ActionCacheUpdateCapabilities, CacheCapabilities, ExecutionCapabilities,
    GetCapabilitiesRequest, PriorityCapabilities, ServerCapabilities,
};
use crate::proto::build::bazel::semver::SemVer;
use crate::version::VersionManager;

pub struct CapabilitiesService {
    #[allow(dead_code)]
    version_manager: Arc<VersionManager>,
}

impl CapabilitiesService {
    pub fn new() -> Self {
        Self {
            version_manager: Arc::new(VersionManager::new()),
        }
    }

    pub fn into_service(self) -> CapabilitiesServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        CapabilitiesServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    fn create_base_capabilities(&self) -> ServerCapabilities {
        // REAPI v2.4: PriorityCapabilities uses 'priorities' (repeated) instead of 'priority_range'
        let priority_range = PriorityRange {
            min_priority: 0,
            max_priority: 1000,
        };

        ServerCapabilities {
            cache_capabilities: Some(CacheCapabilities {
                digest_functions: vec![1],
                action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                // REAPI v2.4: cache_priority_capabilities uses 'priorities' (repeated)
                cache_priority_capabilities: Some(PriorityCapabilities {
                    priorities: vec![priority_range.clone()],
                }),
                max_batch_total_size_bytes: 4 * 1024 * 1024,
                symlink_absolute_path_strategy: 2,
                supported_compressors: vec![0, 1],
                supported_batch_update_compressors: vec![0, 1],
                max_cas_blob_size_bytes: 0,
                // REAPI v2.4: New required fields
                split_blob_support: false,
                rep_max_cdc_params: None,
                fast_cdc_2020_params: None,
                splice_blob_support: false,
            }),
            execution_capabilities: Some(ExecutionCapabilities {
                digest_function: 1,
                exec_enabled: true,
                // REAPI v2.4: execution_priority_capabilities uses 'priorities'
                execution_priority_capabilities: Some(PriorityCapabilities {
                    priorities: vec![priority_range],
                }),
                // REAPI v2.4: New fields
                supported_node_properties: vec![],
                digest_functions: vec![1],
            }),

            deprecated_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
            }),

            low_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
            }),

            high_api_version: Some(SemVer {
                major: 2,
                minor: 4,
                patch: 0,
                prerelease: String::new(),
            }),
        }
    }
}

impl Default for CapabilitiesService {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        let addr = request.remote_addr();
        let req = request.into_inner();

        info!(
            "📡 Capabilities from {:?} instance='{}'",
            addr, req.instance_name
        );

        let caps = self.create_base_capabilities();

        info!("✅ Capabilities returned (REAPI v2.4 compatible - Bazel 8.3.0+ ready)");
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

        Ok(Response::new(caps))
    }
}
