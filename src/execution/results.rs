use crate::execution::state_machine::OperationId;
use crate::worker::k8s::ExecutionResult;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};

/// Entry stored in the results map, including the time it was inserted so
/// old entries can be evicted.
struct ResultEntry {
    result: ExecutionResult,
    stored_at: Instant,
}

pub struct ResultsStore {
    results: Arc<DashMap<OperationId, ResultEntry>>,
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
        self.results.insert(
            operation_id,
            ResultEntry {
                result,
                stored_at: Instant::now(),
            },
        );
    }

    pub fn get(&self, operation_id: OperationId) -> Option<ExecutionResult> {
        self.results.get(&operation_id).map(|r| r.result.clone())
    }

    #[allow(dead_code)]
    pub fn remove(&self, operation_id: OperationId) -> Option<ExecutionResult> {
        debug!("Removing result for operation {}", operation_id.0);
        self.results.remove(&operation_id).map(|(_, r)| r.result)
    }

    #[allow(dead_code)]
    pub fn contains(&self, operation_id: OperationId) -> bool {
        self.results.contains_key(&operation_id)
    }

    /// Remove results older than `max_age_secs`.
    ///
    /// Returns the number of entries removed.
    pub fn cleanup_old(&self, max_age_secs: u64) -> usize {
        let cutoff = Instant::now() - std::time::Duration::from_secs(max_age_secs);
        let mut removed = 0;

        // Retain is not available on DashMap, so scan and remove expired entries.
        self.results.retain(|operation_id, entry| {
            let keep = entry.stored_at >= cutoff;
            if !keep {
                debug!("Cleaning up old result for operation {}", operation_id.0);
                removed += 1;
            }
            keep
        });

        if removed > 0 {
            info!("ResultsStore cleanup removed {} old entries", removed);
        }

        removed
    }

    pub fn len(&self) -> usize {
        self.results.len()
    }
}

impl Default for ResultsStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_results_store_ttl_cleanup() {
        let store = ResultsStore::new();
        let op_id = OperationId::generate();

        store.store(
            op_id,
            ExecutionResult {
                execution_id: op_id.0.to_string(),
                exit_code: 0,
                stdout: vec![],
                stderr: vec![],
                stdout_digest: None,
                stderr_digest: None,
                output_files: vec![],
                output_directories: vec![],
                execution_duration: std::time::Duration::ZERO,
            },
        );

        assert!(store.contains(op_id));
        assert_eq!(store.len(), 1);

        // Before TTL, cleanup should not remove anything.
        assert_eq!(store.cleanup_old(60), 0);
        assert!(store.contains(op_id));

        // Simulate old age by cleaning up with age 0.
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(store.cleanup_old(0), 1);
        assert!(!store.contains(op_id));
        assert_eq!(store.len(), 0);
    }
}
