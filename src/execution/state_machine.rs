//! Lifecycle of a remote execution operation.
//!
//! Every operation owns an [`ExecutionStateMachine`] that only moves along
//! the transitions [`ExecutionStage::can_transition_to`] allows and records a
//! bounded transition history. [`StateMachineManager::cleanup_finished_machines`]
//! retires machines some time after they reach a terminal stage; an
//! operation in progress is never retired, however long it runs.

use crate::types::{AtomicInstant, DigestInfo, RbeError, Result};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tracing::debug;

/// Process-unique identifier of an execution operation, displayed as `op-<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u64);

/// Longest transition history kept per operation; older entries are dropped.
const MAX_HISTORY_LEN: usize = 100;

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

/// Stage of an operation. Finer-grained than REAPI's `ExecutionStage`; see
/// [`ExecutionStage::to_reapi_stage`] for the mapping reported to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStage {
    /// Reported as REAPI `UNKNOWN`.
    #[allow(dead_code)]
    Unknown,
    /// Looking the action up in the action cache. REAPI `CACHE_CHECK`.
    CacheCheck,
    /// Waiting for a worker. REAPI `QUEUED`.
    Queued,
    /// Handed to a worker. REAPI `EXECUTING`.
    Assigned,
    /// Worker is materializing inputs. REAPI `EXECUTING`.
    #[allow(dead_code)]
    Downloading,
    /// Worker is running the command. REAPI `EXECUTING`.
    #[allow(dead_code)]
    Executing,
    /// Worker is uploading outputs. REAPI `EXECUTING`.
    #[allow(dead_code)]
    Uploading,
    /// Finished successfully. Terminal; REAPI `COMPLETED`.
    Completed,
    /// Finished with an error. Terminal; REAPI `COMPLETED`.
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

    /// Whether an operation in this stage may move to `next`.
    ///
    /// Stages advance, every non-terminal stage may fail, and every stage
    /// from `CacheCheck` on may complete directly (a cache hit, or a worker
    /// that reports its result without intermediate progress). The worker
    /// stages may also return to `Queued`, when the assignment could not be
    /// delivered or the worker was lost, so the action runs elsewhere.
    /// `Completed` and `Failed` are terminal.
    pub fn can_transition_to(&self, next: ExecutionStage) -> bool {
        use ExecutionStage::*;

        matches!(
            (*self, next),
            (Unknown, CacheCheck | Failed)
                | (CacheCheck, Queued | Completed | Failed)
                | (Queued, Assigned | Failed)
                | (Assigned, Queued | Downloading | Completed | Failed)
                | (Downloading, Queued | Executing | Completed | Failed)
                | (Executing, Queued | Uploading | Completed | Failed)
                | (Uploading, Queued | Completed | Failed)
        )
    }

    /// The REAPI v2 `ExecutionStage.Value` reported to clients:
    /// `UNKNOWN = 0`, `CACHE_CHECK = 1`, `QUEUED = 2`, `EXECUTING = 3`,
    /// `COMPLETED = 4`. Every worker-side stage reports as `EXECUTING` and
    /// both terminal stages as `COMPLETED`.
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

    /// The REAPI name of [`ExecutionStage::to_reapi_stage`], e.g. `"EXECUTING"`.
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

/// The stage of one operation plus the bookkeeping that decides whether it
/// is still alive. Starts in [`ExecutionStage::CacheCheck`].
pub struct ExecutionStateMachine {
    pub operation_id: OperationId,
    pub action_digest: DigestInfo,
    state: Arc<RwLock<ExecutionStage>>,
    /// Refreshed on every transition and every explicit heartbeat.
    last_heartbeat: Arc<AtomicInstant>,
    started_at: std::time::Instant,
    /// The most recent [`MAX_HISTORY_LEN`] transitions, oldest first.
    transition_history: Arc<RwLock<Vec<(ExecutionStage, std::time::Instant)>>>,
    /// Flips to `true` once, when the operation reaches a terminal stage.
    terminal: watch::Sender<bool>,
    /// Clients streaming the operation: `Execute` and `WaitExecution` calls.
    watchers: AtomicUsize,
    /// When the last watcher went away, or the operation was created.
    unwatched_since: AtomicInstant,
}

/// A client following an operation; dropping it ends the watch.
pub struct Watch<'a>(&'a ExecutionStateMachine);

impl Drop for Watch<'_> {
    fn drop(&mut self) {
        if self.0.watchers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.unwatched_since.refresh();
        }
    }
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
            terminal: watch::Sender::new(false),
            watchers: AtomicUsize::new(0),
            unwatched_since: AtomicInstant::now(),
        }
    }

    #[allow(dead_code)]
    pub async fn current_state(&self) -> ExecutionStage {
        *self.state.read().await
    }

    /// Moves to `new_state`, refreshing the heartbeat.
    ///
    /// # Errors
    ///
    /// [`RbeError::InvalidStateTransition`] if the current stage cannot move
    /// to `new_state`; the stage is left unchanged.
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
        if matches!(
            new_state,
            ExecutionStage::Completed | ExecutionStage::Failed
        ) {
            self.terminal.send_replace(true);
        }

        self.last_heartbeat.refresh();

        let mut history = self.transition_history.write().await;
        history.push((new_state, std::time::Instant::now()));
        if history.len() > MAX_HISTORY_LEN {
            let excess = history.len() - MAX_HISTORY_LEN;
            history.drain(0..excess);
        }

        Ok(())
    }

    /// Counts a client as following the operation until the guard drops.
    pub fn watch(&self) -> Watch<'_> {
        self.watchers.fetch_add(1, Ordering::SeqCst);
        Watch(self)
    }

    /// Whether no client has followed the operation for longer than `grace`.
    pub fn unwatched_for(&self, grace: std::time::Duration) -> bool {
        self.watchers.load(Ordering::SeqCst) == 0
            && self.unwatched_since.elapsed_millis() > grace.as_millis() as u64
    }

    /// Returns once the operation is in a terminal stage, right away if it
    /// already is.
    pub async fn wait_terminal(&self) {
        let mut terminal = self.terminal.subscribe();
        let _ = terminal.wait_for(|reached| *reached).await;
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

/// Registry of the state machines of every in-flight operation.
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

    /// Drops every machine that reached a terminal stage at least
    /// `retention_millis` ago, and returns how many were dropped. Machines in
    /// any other stage are kept: their operation is still queued or running.
    pub async fn cleanup_finished_machines(&self, retention_millis: u64) -> usize {
        let candidates: Vec<(OperationId, Arc<ExecutionStateMachine>)> = self
            .machines
            .iter()
            .filter(|m| m.value().last_heartbeat.elapsed_millis() >= retention_millis)
            .map(|m| (*m.key(), m.value().clone()))
            .collect();

        let mut removed = 0;
        for (op_id, machine) in candidates {
            if machine.is_terminal().await {
                self.machines.remove(&op_id);
                removed += 1;
            }
        }
        removed
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

        assert!(
            Assigned.can_transition_to(Completed),
            "Assigned -> Completed should be allowed"
        );
        assert!(
            Assigned.can_transition_to(Failed),
            "Assigned -> Failed should be allowed"
        );
        assert!(
            Downloading.can_transition_to(Completed),
            "Downloading -> Completed should be allowed"
        );
        assert!(
            Executing.can_transition_to(Completed),
            "Executing -> Completed should be allowed"
        );
    }

    #[tokio::test]
    async fn cleanup_retires_finished_machines_and_keeps_running_ones() {
        let manager = StateMachineManager::new();
        let digest = DigestInfo::new(&"a".repeat(64), 1).unwrap();
        let running = manager.create_machine(digest);
        let finished = manager.create_machine(digest);
        running.transition_to(ExecutionStage::Queued).await.unwrap();
        running
            .transition_to(ExecutionStage::Assigned)
            .await
            .unwrap();
        finished
            .transition_to(ExecutionStage::Completed)
            .await
            .unwrap();

        assert_eq!(manager.cleanup_finished_machines(0).await, 1);
        assert!(manager.get_machine(running.operation_id).is_some());
        assert!(manager.get_machine(finished.operation_id).is_none());
    }

    #[test]
    fn worker_stages_can_return_to_the_queue_but_terminal_ones_cannot() {
        use ExecutionStage::*;

        for stage in [Assigned, Downloading, Executing, Uploading] {
            assert!(stage.can_transition_to(Queued), "{stage} -> Queued");
        }
        assert!(!Queued.can_transition_to(Queued));
        assert!(!CacheCheck.can_transition_to(Assigned));
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
        let digest = DigestInfo::new(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            1024,
        )
        .unwrap();
        let machine = ExecutionStateMachine::new(OperationId::generate(), digest);

        assert_eq!(machine.current_state().await, ExecutionStage::CacheCheck);

        machine.transition_to(ExecutionStage::Queued).await.unwrap();
        assert_eq!(machine.current_state().await, ExecutionStage::Queued);

        let result = machine.transition_to(ExecutionStage::Uploading).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn an_operation_is_unwatched_once_its_last_client_leaves_and_the_grace_passes() {
        let digest = DigestInfo::new(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            1024,
        )
        .unwrap();
        let machine = ExecutionStateMachine::new(OperationId::generate(), digest);
        let grace = std::time::Duration::from_millis(20);

        let first = machine.watch();
        let second = machine.watch();
        drop(first);
        tokio::time::sleep(grace * 2).await;
        assert!(!machine.unwatched_for(grace), "a client still follows it");

        drop(second);
        assert!(
            !machine.unwatched_for(grace),
            "the grace starts when the last one leaves"
        );
        tokio::time::sleep(grace * 2).await;
        assert!(machine.unwatched_for(grace));
    }

    #[tokio::test]
    async fn waiters_wake_when_the_operation_finishes_and_later_ones_return_at_once() {
        let digest = DigestInfo::new(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            1024,
        )
        .unwrap();
        let machine = Arc::new(ExecutionStateMachine::new(OperationId::generate(), digest));
        machine.transition_to(ExecutionStage::Queued).await.unwrap();

        let early = tokio::spawn({
            let machine = machine.clone();
            async move { machine.wait_terminal().await }
        });
        tokio::task::yield_now().await;
        assert!(!early.is_finished());

        machine.transition_to(ExecutionStage::Failed).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), early)
            .await
            .expect("an early waiter wakes")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), machine.wait_terminal())
            .await
            .expect("a later waiter returns at once");
    }

    #[tokio::test]
    async fn test_terminal_states() {
        let digest = DigestInfo::new(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            1024,
        )
        .unwrap();
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
