use tonic::{Request, Response, Status};
use tracing::info;

use crate::cache::action_cache::{ActionCacheStore, CacheActionResult};
use crate::proto::build::bazel::remote::execution::v2::{
    action_cache_server::{ActionCache, ActionCacheServer},
    ActionResult, GetActionResultRequest, UpdateActionResultRequest,
};
use crate::types::DigestInfo;

use std::sync::Arc;

pub struct ActionCacheService {
    store: Arc<dyn ActionCacheStore>,
}

impl ActionCacheService {
    pub fn new(store: Arc<dyn ActionCacheStore>) -> Self {
        Self { store }
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

        let digest_info = DigestInfo::new(&action_digest.hash, action_digest.size_bytes)
            .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))?;

        info!("📡 ActionCache::GetActionResult digest={}", digest_info);

        match self.store.get(&digest_info).await {
            Some(result) => {
                info!("✅ ActionCache HIT for digest={}", digest_info);
                Ok(Response::new(result.proto))
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

        let digest_info = DigestInfo::new(&action_digest.hash, action_digest.size_bytes)
            .map_err(|e| Status::invalid_argument(format!("Invalid digest: {}", e)))?;

        info!("📡 ActionCache::UpdateActionResult digest={}", digest_info);

        let cache_result = CacheActionResult::new(digest_info, action_result);
        let proto_result = cache_result.proto.clone();

        self.store.put(digest_info, cache_result).await;

        info!("✅ ActionCache updated for digest={}", digest_info);

        Ok(Response::new(proto_result))
    }
}
