use crate::execution::state_machine::OperationId;
use crate::worker::k8s::ExecutionResult;
use dashmap::DashMap;
use std::sync::Arc;
use tracing::{debug, info};

pub struct ResultsStore {
    results: Arc<DashMap<OperationId, ExecutionResult>>,
}

impl ResultsStore {
    pub fn new() -> Self {
        Self {
            results: Arc::new(DashMap::new()),
        }
    }

    pub fn store(&self, operation_id: OperationId, result: ExecutionResult) {
        info!(
            "Storing result for operation {}: exit_code={}",
            operation_id.0, result.exit_code
        );
        self.results.insert(operation_id, result);
    }

    pub fn get(&self, operation_id: OperationId) -> Option<ExecutionResult> {
        self.results.get(&operation_id).map(|r| r.clone())
    }

    #[allow(dead_code)]
    pub fn remove(&self, operation_id: OperationId) -> Option<ExecutionResult> {
        debug!("Removing result for operation {}", operation_id.0);
        self.results.remove(&operation_id).map(|(_, r)| r)
    }

    #[allow(dead_code)]
    pub fn contains(&self, operation_id: OperationId) -> bool {
        self.results.contains_key(&operation_id)
    }

    #[allow(dead_code)]
    pub fn cleanup_old(&self, _max_age_secs: u64) -> usize {
        0
    }
}

impl Default for ResultsStore {
    fn default() -> Self {
        Self::new()
    }
}
