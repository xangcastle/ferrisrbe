//! Finished execution results, kept for a bounded time so `WaitExecution`
//! and late `GetOperation` callers can still read them.

use crate::execution::state_machine::OperationId;
use crate::proto::build::bazel::remote::execution::v2::ExecuteResponse;
use crate::worker::k8s::ExecutionResult;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};

/// A finished operation: what its worker reported and the `ExecuteResponse`
/// every client of the operation receives, `Execute` and `WaitExecution`
/// alike.
#[derive(Debug, Clone)]
pub struct FinishedOperation {
    pub result: ExecutionResult,
    pub response: ExecuteResponse,
}

/// A finished operation and when it was stored, which drives its expiry.
struct ResultEntry {
    finished: FinishedOperation,
    stored_at: Instant,
}

/// Concurrent map from operation to its finished result.
pub struct ResultsStore {
    results: Arc<DashMap<OperationId, ResultEntry>>,
}

impl ResultsStore {
    pub fn new() -> Self {
        Self {
            results: Arc::new(DashMap::new()),
        }
    }

    /// Stores `finished`, replacing any earlier one for the operation.
    pub fn store(&self, operation_id: OperationId, finished: FinishedOperation) {
        info!(
            "Storing result for operation {}: exit_code={}",
            operation_id.0, finished.result.exit_code
        );
        self.results.insert(
            operation_id,
            ResultEntry {
                finished,
                stored_at: Instant::now(),
            },
        );
    }

    pub fn get(&self, operation_id: OperationId) -> Option<FinishedOperation> {
        self.results.get(&operation_id).map(|r| r.finished.clone())
    }

    #[allow(dead_code)]
    pub fn remove(&self, operation_id: OperationId) -> Option<FinishedOperation> {
        debug!("Removing result for operation {}", operation_id.0);
        self.results.remove(&operation_id).map(|(_, r)| r.finished)
    }

    #[allow(dead_code)]
    pub fn contains(&self, operation_id: OperationId) -> bool {
        self.results.contains_key(&operation_id)
    }

    /// Removes results stored more than `max_age_secs` ago and returns how
    /// many were removed.
    pub fn cleanup_old(&self, max_age_secs: u64) -> usize {
        let cutoff = Instant::now() - std::time::Duration::from_secs(max_age_secs);
        let mut removed = 0;

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

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    use crate::worker::k8s::PhaseTimestamps;
    use std::time::Duration;

    #[tokio::test]
    async fn cleanup_keeps_fresh_results_and_drops_expired_ones() {
        let store = ResultsStore::new();
        let op_id = OperationId::generate();

        let result = ExecutionResult {
            execution_id: op_id.0.to_string(),
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            stdout_digest: None,
            stderr_digest: None,
            output_files: vec![],
            output_directories: vec![],
            output_symlinks: vec![],
            execution_duration: std::time::Duration::ZERO,
            error: None,
            timed_out: false,
            worker_id: String::new(),
            timestamps: PhaseTimestamps::default(),
            input_stats: None,
        };
        store.store(
            op_id,
            FinishedOperation {
                result,
                response: ExecuteResponse::default(),
            },
        );

        assert!(store.contains(op_id));
        assert_eq!(store.len(), 1);

        assert_eq!(store.cleanup_old(60), 0, "a fresh result must survive");
        assert!(store.contains(op_id));

        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(store.cleanup_old(0), 1);
        assert!(!store.contains(op_id));
        assert_eq!(store.len(), 0);
    }
}
