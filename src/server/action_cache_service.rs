

use tonic::{Request, Response, Status};
use tracing::info;

use crate::cache::action_cache::{ActionResult as CacheActionResult, L1ActionCache};
use crate::proto::build::bazel::remote::execution::v2::{
    action_cache_server::{ActionCache, ActionCacheServer},
    ActionResult, ExecutedActionMetadata, GetActionResultRequest,
    UpdateActionResultRequest,
};
use crate::types::DigestInfo;

use std::sync::Arc;

pub struct ActionCacheService {
    l1_cache: Arc<L1ActionCache>,
}

impl ActionCacheService {
    pub fn new(l1_cache: Arc<L1ActionCache>) -> Self {
        Self { l1_cache }
    }

    pub fn into_service(self) -> ActionCacheServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        ActionCacheServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    fn convert_to_proto_result(&self, result: CacheActionResult) -> ActionResult {
        // REAPI v2.4: ActionResult uses ExecutedActionMetadata (not ExecutionMetadata)
        // and timestamps are prost_types::Timestamp
        ActionResult {
            exit_code: result.exit_code,
            stdout_raw: Vec::new(),
            stdout_digest: None,
            stderr_raw: Vec::new(),
            stderr_digest: None,
            output_files: Vec::new(),
            output_directories: Vec::new(),
            // REAPI v2.4: New required fields
            #[allow(deprecated)]
            output_file_symlinks: vec![],
            output_symlinks: vec![],
            #[allow(deprecated)]
            output_directory_symlinks: vec![],
            execution_metadata: Some(ExecutedActionMetadata {
                worker: String::new(),
                queued_timestamp: None,
                worker_start_timestamp: None,
                worker_completed_timestamp: None,
                input_fetch_start_timestamp: None,
                input_fetch_completed_timestamp: None,
                execution_start_timestamp: None,
                execution_completed_timestamp: None,
                // Additional optional fields from v2.3/v2.4
                virtual_execution_duration: None,
                auxiliary_metadata: vec![],
                output_upload_start_timestamp: None,
                output_upload_completed_timestamp: None,
            }),
        }
    }
}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("Missing action_digest"))?;

        let digest_info = DigestInfo::new(&action_digest.hash, action_digest.size_bytes);

        info!("📡 ActionCache::GetActionResult digest={}", digest_info);

        match self.l1_cache.get(&digest_info) {
            Some(result) => {
                info!("✅ ActionCache HIT for digest={}", digest_info);
                let proto_result = self.convert_to_proto_result(result);
                Ok(Response::new(proto_result))
            }
            None => {
                info!("❌ ActionCache MISS for digest={}", digest_info);
                Err(Status::not_found("Action result not found in cache"))
            }
        }
    }

    async fn update_action_result(
        &self,
        request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = request.into_inner();
        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("Missing action_digest"))?;

        let action_result = req
            .action_result
            .ok_or_else(|| Status::invalid_argument("Missing action_result"))?;

        let digest_info = DigestInfo::new(&action_digest.hash, action_digest.size_bytes);

        info!("📡 ActionCache::UpdateActionResult digest={}", digest_info);

        let cache_result = CacheActionResult {
            digest: digest_info,
            exit_code: action_result.exit_code,
            stdout_digest: None,
            stderr_digest: None,
            output_files: Vec::new(),
            output_directories: Vec::new(),
            execution_metadata: Default::default(),
        };

        self.l1_cache.put(digest_info, cache_result.clone());

        info!("✅ ActionCache updated for digest={}", digest_info);

        Ok(Response::new(self.convert_to_proto_result(cache_result)))
    }
}
