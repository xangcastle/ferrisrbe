

use crate::types::{AtomicInstant, DigestInfo, RbeError, Result};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u64);

impl OperationId {
    pub fn generate() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStage {
    /// (UNKNOWN)
    #[allow(dead_code)]
    Unknown,
    /// (CACHE_CHECK)
    CacheCheck,
    /// (QUEUED)
    Queued,
    /// (EXECUTING)
    Assigned,
    /// (EXECUTING)
    #[allow(dead_code)]
    Downloading,
    /// (EXECUTING)
    #[allow(dead_code)]
    Executing,
    /// (EXECUTING)
    #[allow(dead_code)]
    Uploading,
    /// (COMPLETED)
    Completed,
    /// (COMPLETED)
    Failed,
}

impl ExecutionStage {

    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionStage::Unknown => "Unknown",
            ExecutionStage::CacheCheck => "CacheCheck",
            ExecutionStage::Queued => "Queued",
            ExecutionStage::Assigned => "Assigned",
            ExecutionStage::Downloading => "Downloading",
            ExecutionStage::Executing => "Executing",
            ExecutionStage::Uploading => "Uploading",
            ExecutionStage::Completed => "Completed",
            ExecutionStage::Failed => "Failed",
        }
    }

    pub fn can_transition_to(&self, next: ExecutionStage) -> bool {
        use ExecutionStage::*;

        match (*self, next) {
            (Unknown, CacheCheck) => true,
            (Unknown, Failed) => true,

            (CacheCheck, Queued) => true,
            (CacheCheck, Completed) => true,
            (CacheCheck, Failed) => true,

            (Queued, Assigned) => true,
            (Queued, Failed) => true,

            (Assigned, Downloading) => true,
            (Assigned, Completed) => true,
            (Assigned, Failed) => true,

            (Downloading, Executing) => true,
            (Downloading, Completed) => true,
            (Downloading, Failed) => true,

            (Executing, Uploading) => true,
            (Executing, Completed) => true,
            (Executing, Failed) => true,

            (Uploading, Completed) => true,
            (Uploading, Failed) => true,

            (Completed, _) => false,
            (Failed, _) => false,

            _ => false,
        }
    }

    /// Maps FerrisRBE internal state to standard REAPI v2 state.
    /// 
    /// REAPI v2 defines the following states:
    /// - UNKNOWN = 0
    /// - CACHE_CHECK = 1
    /// - QUEUED = 2
    /// - EXECUTING = 3
    /// - COMPLETED = 4
    /// 
    /// FerrisRBE has more granular states for internal tracking,
    /// but uses the standard mapping to report to Bazel.
    #[allow(dead_code)]
    pub fn to_reapi_stage(&self) -> i32 {
        use ExecutionStage::*;
        
        match self {
            Unknown => 0,
            CacheCheck => 1,
            Queued => 2,
            Assigned => 3,
            Downloading => 3,
            Executing => 3,
            Uploading => 3,
            Completed => 4,
            Failed => 4,
        }
    }

    /// Returns the REAPI state name for logging/debugging.
    #[allow(dead_code)]
    pub fn to_reapi_stage_name(&self) -> &'static str {
        use ExecutionStage::*;
        
        match self {
            Unknown => "UNKNOWN",
            CacheCheck => "CACHE_CHECK",
            Queued => "QUEUED",
            Assigned => "EXECUTING",
            Downloading => "EXECUTING",
            Executing => "EXECUTING",
            Uploading => "EXECUTING",
            Completed => "COMPLETED",
            Failed => "COMPLETED",
        }
    }
}

impl fmt::Display for ExecutionStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

pub struct ExecutionStateMachine {

    pub operation_id: OperationId,

    pub action_digest: DigestInfo,

    state: Arc<RwLock<ExecutionStage>>,

    last_heartbeat: Arc<AtomicInstant>,

    started_at: std::time::Instant,

    transition_history: Arc<RwLock<Vec<(ExecutionStage, std::time::Instant)>>>,
}

impl ExecutionStateMachine {
    pub fn new(operation_id: OperationId, action_digest: DigestInfo) -> Self {
        let state = Arc::new(RwLock::new(ExecutionStage::CacheCheck));
        let now = std::time::Instant::now();

        Self {
            operation_id,
            action_digest,
            state,
            last_heartbeat: Arc::new(AtomicInstant::now()),
            started_at: now,
            transition_history: Arc::new(RwLock::new(vec![(ExecutionStage::CacheCheck, now)])),
        }
    }

    #[allow(dead_code)]
    pub async fn current_state(&self) -> ExecutionStage {
        *self.state.read().await
    }

    pub async fn transition_to(&self, new_state: ExecutionStage) -> Result<()> {
        let mut state = self.state.write().await;

        if !state.can_transition_to(new_state) {
            return Err(RbeError::InvalidStateTransition {
                from: state.to_string(),
                to: new_state.to_string(),
            });
        }

        debug!(
            "Operation {}: {} -> {}",
            self.operation_id, *state, new_state
        );

        *state = new_state;
        drop(state);

        self.last_heartbeat.refresh();

        let mut history = self.transition_history.write().await;
        history.push((new_state, std::time::Instant::now()));

        Ok(())
    }

    pub async fn is_terminal(&self) -> bool {
        let state = self.state.read().await;
        matches!(*state, ExecutionStage::Completed | ExecutionStage::Failed)
    }

    #[allow(dead_code)]
    pub fn heartbeat(&self) {
        self.last_heartbeat.refresh();
        debug!("Heartbeat for operation {}", self.operation_id);
    }

    pub fn is_alive(&self) -> bool {
        self.last_heartbeat.elapsed_millis() < 60_000
    }

    #[allow(dead_code)]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    #[allow(dead_code)]
    pub async fn transition_history(&self) -> Vec<(ExecutionStage, std::time::Instant)> {
        self.transition_history.read().await.clone()
    }
}

impl fmt::Debug for ExecutionStateMachine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionStateMachine")
            .field("operation_id", &self.operation_id)
            .field("action_digest", &self.action_digest)
            .field("started_at", &self.started_at)
            .finish()
    }
}

pub struct StateMachineManager {

    machines: Arc<dashmap::DashMap<OperationId, Arc<ExecutionStateMachine>>>,
}

impl StateMachineManager {
    pub fn new() -> Self {
        Self {
            machines: Arc::new(dashmap::DashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub fn create_machine(&self, action_digest: DigestInfo) -> Arc<ExecutionStateMachine> {
        let operation_id = OperationId::generate();
        self.create_machine_with_id(operation_id, action_digest)
    }

    pub fn create_machine_with_id(
        &self,
        operation_id: OperationId,
        action_digest: DigestInfo,
    ) -> Arc<ExecutionStateMachine> {
        let machine = Arc::new(ExecutionStateMachine::new(operation_id, action_digest));

        self.machines.insert(operation_id, machine.clone());
        debug!("Created state machine for operation {}", operation_id);

        machine
    }

    pub fn get_machine(&self, operation_id: OperationId) -> Option<Arc<ExecutionStateMachine>> {
        self.machines.get(&operation_id).map(|m| m.clone())
    }

    #[allow(dead_code)]
    pub fn remove_machine(&self, operation_id: OperationId) {
        self.machines.remove(&operation_id);
        debug!("Removed state machine for operation {}", operation_id);
    }

    pub async fn cleanup_dead_machines(&self, timeout_millis: u64) -> usize {
        let dead: Vec<OperationId> = self
            .machines
            .iter()
            .filter(|m| {
                !m.value().is_alive() || m.value().last_heartbeat.elapsed_millis() > timeout_millis
            })
            .map(|m| *m.key())
            .collect();

        for op_id in &dead {
            error!("Removing dead machine for operation {}", op_id);
            self.machines.remove(op_id);
        }

        dead.len()
    }

    #[allow(dead_code)]
    pub fn active_operations(&self) -> Vec<OperationId> {
        self.machines.iter().map(|m| *m.key()).collect()
    }
}

impl Default for StateMachineManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_transitions() {
        use ExecutionStage::*;

        assert!(CacheCheck.can_transition_to(Queued));
        assert!(CacheCheck.can_transition_to(Completed));
        assert!(Queued.can_transition_to(Assigned));
        assert!(Assigned.can_transition_to(Downloading));
        assert!(Downloading.can_transition_to(Executing));
        assert!(Executing.can_transition_to(Uploading));
        assert!(Uploading.can_transition_to(Completed));
        
        assert!(Assigned.can_transition_to(Completed), "Assigned -> Completed should be allowed");
        assert!(Assigned.can_transition_to(Failed), "Assigned -> Failed should be allowed");
        assert!(Downloading.can_transition_to(Completed), "Downloading -> Completed should be allowed");
        assert!(Executing.can_transition_to(Completed), "Executing -> Completed should be allowed");
    }

    #[test]
    fn test_invalid_transitions() {
        use ExecutionStage::*;

        assert!(!CacheCheck.can_transition_to(Uploading));
        assert!(!CacheCheck.can_transition_to(Executing));
        assert!(!Queued.can_transition_to(Uploading));
        
        assert!(!Completed.can_transition_to(Failed));
        assert!(!Completed.can_transition_to(Queued));
        assert!(!Failed.can_transition_to(Queued));
        assert!(!Failed.can_transition_to(Completed));
    }
    
    #[test]
    fn test_reapi_stage_mapping() {
        use ExecutionStage::*;
        
        assert_eq!(Unknown.to_reapi_stage(), 0);
        assert_eq!(CacheCheck.to_reapi_stage(), 1);
        assert_eq!(Queued.to_reapi_stage(), 2);
        
        assert_eq!(Assigned.to_reapi_stage(), 3);
        assert_eq!(Downloading.to_reapi_stage(), 3);
        assert_eq!(Executing.to_reapi_stage(), 3);
        assert_eq!(Uploading.to_reapi_stage(), 3);
        
        assert_eq!(Completed.to_reapi_stage(), 4);
        assert_eq!(Failed.to_reapi_stage(), 4);
        
        assert_eq!(Assigned.to_reapi_stage_name(), "EXECUTING");
        assert_eq!(Executing.to_reapi_stage_name(), "EXECUTING");
        assert_eq!(Completed.to_reapi_stage_name(), "COMPLETED");
    }

    #[tokio::test]
    async fn test_state_machine_transitions() {
        let digest = DigestInfo::new("test", 1024);
        let machine = ExecutionStateMachine::new(OperationId::generate(), digest);

        assert_eq!(machine.current_state().await, ExecutionStage::CacheCheck);

        machine.transition_to(ExecutionStage::Queued).await.unwrap();
        assert_eq!(machine.current_state().await, ExecutionStage::Queued);

        let result = machine.transition_to(ExecutionStage::Uploading).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_terminal_states() {
        let digest = DigestInfo::new("test", 1024);
        let machine = ExecutionStateMachine::new(OperationId::generate(), digest);

        machine.transition_to(ExecutionStage::Queued).await.unwrap();
        machine
            .transition_to(ExecutionStage::Assigned)
            .await
            .unwrap();
        machine
            .transition_to(ExecutionStage::Downloading)
            .await
            .unwrap();
        machine
            .transition_to(ExecutionStage::Executing)
            .await
            .unwrap();
        machine
            .transition_to(ExecutionStage::Uploading)
            .await
            .unwrap();
        machine
            .transition_to(ExecutionStage::Completed)
            .await
            .unwrap();

        assert!(machine.is_terminal().await);
    }

    #[test]
    fn test_operation_id_generation() {
        let id1 = OperationId::generate();
        let id2 = OperationId::generate();
        assert_ne!(id1, id2);
    }
}
