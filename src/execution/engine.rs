//! Remote execution dispatch loop.
//!
//! [`ExecutionEngine`] pulls actions from the [`MultiLevelScheduler`], drives
//! each one through its [`ExecutionStateMachine`], hands it to an idle worker
//! from the [`WorkerRegistry`], and records the result reported back. An
//! action waits in the queue until a worker is idle, and returns to it when
//! its assignment cannot be delivered or its worker is lost, up to
//! [`DEFAULT_MAX_WORKER_LOSSES`] times. A worker silent for longer than
//! [`DEFAULT_HEARTBEAT_TIMEOUT`] counts as lost even if its stream is open.

use crate::execution::results::{FinishedOperation, ResultsStore};
use crate::execution::scheduler::{ExecutableAction, MultiLevelScheduler};
use crate::execution::state_machine::{
    ExecutionStage, ExecutionStateMachine, OperationId, StateMachineManager,
};
use crate::metrics::Metrics;
use crate::proto::build::bazel::remote::execution::v2::ExecuteResponse;
use crate::worker::k8s::{
    ExecutionResult, PhaseTimestamps, SelectedWorker, WorkAssignment, WorkerRegistry,
    WorkerRequirements,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

/// Capacity of the result channel.
const CHANNEL_CAPACITY: usize = 1000;

/// Longest the dispatcher waits between passes when nothing wakes it: a
/// new or requeued action, or a slot that may have opened, wakes it at once.
const DISPATCH_FALLBACK_INTERVAL: Duration = Duration::from_secs(1);

/// How often finished state machines, stale actions and expired results are swept.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

/// An in-flight action older than this is dropped from the scheduler.
const STALE_ACTION_AGE: Duration = Duration::from_secs(3600);

/// Default retention of execution results and of finished state machines;
/// overridden by `RBE_RESULTS_TTL_SECS`.
const DEFAULT_RESULTS_TTL_SECS: u64 = 3600;

/// Workers an action may lose before it fails instead of returning to the
/// queue; `RBE_MAX_WORKER_LOSSES` overrides it. Bounds an action that
/// crashes every worker that runs it.
const DEFAULT_MAX_WORKER_LOSSES: u32 = 3;

/// Silence after which a connected worker counts as lost;
/// `RBE_WORKER_HEARTBEAT_TIMEOUT_SECS` overrides it. Workers heartbeat at
/// most every 60 s, busy or not, so this is five missed beats.
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a queued operation may go without any client following it
/// before it is cancelled; `RBE_UNWATCHED_OPERATION_TIMEOUT_SECS` overrides
/// it. Long enough for Bazel to come back with `WaitExecution` after its
/// `Execute` stream drops.
const DEFAULT_UNWATCHED_TIMEOUT: Duration = Duration::from_secs(60);

/// Prefix of the execution id that identifies an operation on the wire.
const EXECUTION_ID_PREFIX: &str = "exec-";

fn execution_id(operation_id: OperationId) -> String {
    format!("{EXECUTION_ID_PREFIX}{}", operation_id.0)
}

fn operation_id_of(execution_id: &str) -> Option<OperationId> {
    execution_id
        .trim_start_matches(EXECUTION_ID_PREFIX)
        .parse()
        .ok()
        .map(OperationId)
}

/// What a worker's stream reports to the engine, in stream order: a loss
/// queued behind a result is handled after it, so a worker that reports and
/// then disconnects never has that operation re-queued.
#[derive(Debug)]
pub enum WorkerReport {
    /// `(worker_id, result)` of a finished execution.
    Result(String, Box<ExecutionResult>),
    /// The worker's stream ended; whatever it still holds is re-queued.
    Lost(String),
}

/// Turns a finished execution into the `ExecuteResponse` its clients
/// receive, and records what has to outlive them: the Action Cache entry,
/// the execution metrics. The engine calls it before it publishes the
/// operation as finished and releases its digest, so neither depends on a
/// client still listening, and an identical `Execute` arriving meanwhile
/// joins the operation instead of missing the cache.
#[async_trait::async_trait]
pub trait ResultRecorder: Send + Sync {
    /// `action` is `None` when the engine no longer holds it, and then
    /// nothing is cached.
    async fn record(
        &self,
        action: Option<&ExecutableAction>,
        state_machine: &ExecutionStateMachine,
        result: &ExecutionResult,
    ) -> ExecuteResponse;
}

/// An operation handed to a worker, kept so it can be re-queued if the
/// worker is lost before it reports.
struct ActiveExecution {
    worker_id: String,
    action: ExecutableAction,
}

/// Coordinates scheduling, worker assignment and result collection.
///
/// Three background tasks run once [`ExecutionEngine::spawn`] is called: the
/// dispatcher, the result processor and the periodic cleanup.
pub struct ExecutionEngine {
    scheduler: Arc<MultiLevelScheduler>,
    worker_registry: Arc<WorkerRegistry>,
    state_manager: Arc<StateMachineManager>,
    results_store: Arc<ResultsStore>,

    result_tx: mpsc::Sender<WorkerReport>,
    result_rx: RwLock<mpsc::Receiver<WorkerReport>>,

    active_executions: Arc<RwLock<HashMap<OperationId, ActiveExecution>>>,
    max_worker_losses: u32,
    heartbeat_timeout: Duration,
    unwatched_timeout: Duration,
    recorder: Arc<dyn ResultRecorder>,
    metrics: Option<Arc<Metrics>>,
}

impl ExecutionEngine {
    pub fn new(
        scheduler: Arc<MultiLevelScheduler>,
        worker_registry: Arc<WorkerRegistry>,
        state_manager: Arc<StateMachineManager>,
        results_store: Arc<ResultsStore>,
        recorder: Arc<dyn ResultRecorder>,
    ) -> Self {
        let (result_tx, result_rx) = mpsc::channel(CHANNEL_CAPACITY);

        Self {
            scheduler,
            worker_registry,
            state_manager,
            results_store,
            result_tx,
            result_rx: RwLock::new(result_rx),
            active_executions: Arc::new(RwLock::new(HashMap::new())),
            max_worker_losses: std::env::var("RBE_MAX_WORKER_LOSSES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_MAX_WORKER_LOSSES),
            heartbeat_timeout: std::env::var("RBE_WORKER_HEARTBEAT_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&secs| secs > 0)
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_HEARTBEAT_TIMEOUT),
            unwatched_timeout: std::env::var("RBE_UNWATCHED_OPERATION_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&secs| secs > 0)
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_UNWATCHED_TIMEOUT),
            recorder,
            metrics: None,
        }
    }

    /// Counts the operations cancelled for lack of watchers in `metrics`.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Channel on which worker streams report results and their own loss.
    pub fn result_sender(&self) -> mpsc::Sender<WorkerReport> {
        self.result_tx.clone()
    }

    /// Starts the dispatcher, result processor and cleanup tasks.
    pub fn spawn(self: Arc<Self>) {
        let dispatcher_engine = self.clone();
        tokio::spawn(async move {
            dispatcher_engine.dispatcher_loop().await;
        });

        let result_engine = self.clone();
        tokio::spawn(async move {
            result_engine.result_processor_loop().await;
        });

        let cleanup_engine = self.clone();
        tokio::spawn(async move {
            cleanup_engine.cleanup_loop().await;
        });

        info!("ExecutionEngine started with dispatcher, result processor, and cleanup tasks");
    }

    async fn dispatcher_loop(&self) {
        loop {
            self.dispatch_ready().await;
            tokio::select! {
                () = self.scheduler.work_queued() => {}
                () = self.worker_registry.capacity_changed() => {}
                () = tokio::time::sleep(DISPATCH_FALLBACK_INTERVAL) => {}
            }
        }
    }

    /// Assigns queued actions to idle workers that satisfy their requirements,
    /// and returns how many were dispatched. A pass looks at each action
    /// queued when it starts at most once: one that no idle worker matches
    /// goes back to the front of its queue, in order, without holding up the
    /// actions behind it. Actions stay queued while no worker is idle.
    async fn dispatch_ready(&self) -> usize {
        let mut dispatched = 0;
        let mut unmatched = Vec::new();
        for _ in 0..self.scheduler.queued_len() {
            if self
                .worker_registry
                .select_worker(&WorkerRequirements::default())
                .is_none()
            {
                break;
            }
            let Some((action, state_machine)) = self.scheduler.dequeue() else {
                break;
            };
            match self.worker_registry.select_worker(&action.requirements) {
                Some(worker) => {
                    if self.dispatch(worker, action, state_machine).await {
                        dispatched += 1;
                    }
                }
                None => unmatched.push((action, state_machine)),
            }
        }
        for (action, state_machine) in unmatched.into_iter().rev() {
            self.scheduler.put_back(action, state_machine);
        }
        dispatched
    }

    /// Moves one dequeued action to `Assigned` and delivers it to `worker`,
    /// returning whether it was delivered.
    ///
    /// An action that cannot reach `Assigned` is released from the scheduler.
    /// If the worker's channel is closed, the worker is marked unhealthy and
    /// the action goes back to the queue.
    async fn dispatch(
        &self,
        worker: SelectedWorker,
        action: ExecutableAction,
        state_machine: Arc<ExecutionStateMachine>,
    ) -> bool {
        let operation_id = action.operation_id;
        if let Err(e) = state_machine.transition_to(ExecutionStage::Assigned).await {
            warn!("Operation {} cannot be assigned: {}", operation_id, e);
            self.scheduler.complete_action(&action.action_digest);
            return false;
        }

        let assignment = WorkAssignment {
            execution_id: execution_id(operation_id),
            action_digest: Some(action.action_digest.to_string()),
            input_root_digest: action.input_root_digest.as_ref().map(|d| d.to_string()),
            command: action.command.clone(),
            timeout: action.timeout,
            output_files: action.output_files.clone(),
            output_directories: action.output_directories.clone(),
            working_directory: action.working_directory.clone(),
            environment_variables: action.environment_variables.clone(),
            network: action.network,
        };

        let worker_id = worker.worker_id;
        if let Err(e) = worker.assignment_tx.send(assignment).await {
            warn!("Failed to send assignment to worker {}: {}", worker_id, e);
            self.worker_registry.mark_unhealthy(&worker_id);
            self.requeue(action, state_machine).await;
            return false;
        }

        self.active_executions.write().await.insert(
            operation_id,
            ActiveExecution {
                worker_id: worker_id.clone(),
                action,
            },
        );
        self.worker_registry
            .reserve(&worker_id, &execution_id(operation_id));
        info!(
            "Assigned operation {} to worker {}",
            operation_id, worker_id
        );
        true
    }

    /// Returns an action that never ran to the queue.
    async fn requeue(&self, action: ExecutableAction, state_machine: Arc<ExecutionStateMachine>) {
        match state_machine.transition_to(ExecutionStage::Queued).await {
            Ok(()) => self.scheduler.requeue(action, state_machine),
            Err(e) => warn!(
                "Operation {} cannot be re-queued: {}",
                action.operation_id, e
            ),
        }
    }

    /// Re-queues every operation assigned to `worker_id`, which is gone and
    /// will never report their results; an operation that has now lost more
    /// than `max_worker_losses` workers fails instead.
    pub async fn worker_lost(&self, worker_id: &str) {
        let lost: Vec<(OperationId, ActiveExecution)> = {
            let mut active = self.active_executions.write().await;
            let ids: Vec<OperationId> = active
                .iter()
                .filter(|(_, execution)| execution.worker_id == worker_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| active.remove(&id).map(|execution| (id, execution)))
                .collect()
        };

        for (operation_id, mut execution) in lost {
            let Some(state_machine) = self.state_manager.get_machine(operation_id) else {
                warn!("State machine not found for operation {}", operation_id);
                continue;
            };
            execution.action.worker_losses += 1;
            let losses = execution.action.worker_losses;
            if losses > self.max_worker_losses {
                warn!(
                    "Worker {} lost; operation {} has lost {} workers, failing it",
                    worker_id, operation_id, losses
                );
                self.finalize(
                    operation_id,
                    worker_losses_result(operation_id, losses),
                    Some(execution.action),
                )
                .await;
            } else {
                warn!(
                    "Worker {} lost; re-queueing operation {} ({} of {} losses)",
                    worker_id, operation_id, losses, self.max_worker_losses
                );
                self.requeue(execution.action, state_machine).await;
            }
        }
    }

    /// Unregisters every worker silent for longer than `heartbeat_timeout`
    /// and returns their ids. Unregistering drops the assignment channel,
    /// which ends the worker's stream; the caller reports each as lost.
    fn reap_silent_workers(&self) -> Vec<String> {
        let silent = self.worker_registry.silent_workers(self.heartbeat_timeout);
        for worker_id in &silent {
            warn!(
                "Worker {} sent no heartbeat for {:?}; treating it as lost",
                worker_id, self.heartbeat_timeout
            );
            self.worker_registry.unregister(worker_id);
        }
        silent
    }

    /// Finalizes each reported result and frees the slot it held, and
    /// re-queues what a lost worker still held, in the order reported.
    async fn result_processor_loop(&self) {
        let mut result_rx = self.result_rx.write().await;

        while let Some(report) = result_rx.recv().await {
            let (worker_id, result) = match report {
                WorkerReport::Result(worker_id, result) => (worker_id, *result),
                WorkerReport::Lost(worker_id) => {
                    self.worker_lost(&worker_id).await;
                    continue;
                }
            };
            debug!(
                "Processing result for execution {} from worker {}",
                result.execution_id, worker_id
            );
            self.worker_registry
                .release(&worker_id, &result.execution_id);
            if let Some(operation_id) = operation_id_of(&result.execution_id) {
                self.finalize(operation_id, result, None).await;
            }
        }
    }

    /// Records a reported result. An action that ran completes, whatever its
    /// exit code; only one that could not run at all (`result.error`) fails.
    ///
    /// The [`ResultRecorder`] runs first; then the result and its response
    /// go to the [`ResultsStore`] for `Execute` and `WaitExecution` callers,
    /// the operation turns terminal, and its digest is released. Only the
    /// result processor calls this, one result at a time, so the stage
    /// checked here cannot change while the recorder runs. `action` is the
    /// one a lost worker held, already out of `active_executions`.
    async fn finalize(
        &self,
        operation_id: OperationId,
        result: ExecutionResult,
        action: Option<ExecutableAction>,
    ) {
        let Some(state_machine) = self.state_manager.get_machine(operation_id) else {
            self.active_executions.write().await.remove(&operation_id);
            warn!("State machine not found for operation {}", operation_id);
            return;
        };

        let final_stage = if result.error.is_some() {
            ExecutionStage::Failed
        } else {
            ExecutionStage::Completed
        };
        let stage = state_machine.current_state().await;
        if !stage.can_transition_to(final_stage) {
            warn!(
                "Dropping a late result for operation {} from {}: it is {} now",
                operation_id, result.worker_id, stage
            );
            return;
        }
        let action = self
            .active_executions
            .write()
            .await
            .remove(&operation_id)
            .map(|execution| execution.action)
            .or(action);
        let response = self
            .recorder
            .record(action.as_ref(), &state_machine, &result)
            .await;
        self.results_store
            .store(operation_id, FinishedOperation { result, response });
        if let Err(e) = state_machine.transition_to(final_stage).await {
            warn!("Failed to transition to final state: {}", e);
        }
        self.scheduler.complete_action(&state_machine.action_digest);
        info!("Operation {} finished as {}", operation_id, final_stage);
    }

    /// Cancels every queued operation no client has followed for
    /// `unwatched_timeout` (a build that was cancelled or whose client went
    /// away), so its actions never take a worker slot, and returns how many
    /// it cancelled. An operation that already runs finishes, and its result
    /// is cached.
    async fn cancel_unwatched(&self) -> usize {
        let abandoned = self.scheduler.remove_unwatched(self.unwatched_timeout);
        for (action, state_machine) in &abandoned {
            let operation_id = action.operation_id;
            let result = engine_failure(
                operation_id,
                format!(
                    "cancelled: no client followed the operation for {:?} while it was queued",
                    self.unwatched_timeout
                ),
            );
            let response = self.recorder.record(None, state_machine, &result).await;
            self.results_store
                .store(operation_id, FinishedOperation { result, response });
            if let Err(e) = state_machine.transition_to(ExecutionStage::Failed).await {
                warn!("Failed to cancel operation {}: {}", operation_id, e);
            }
            self.scheduler.complete_action(&action.action_digest);
        }
        if !abandoned.is_empty() {
            warn!(
                "Cancelled {} queued operations no client followed for {:?}",
                abandoned.len(),
                self.unwatched_timeout
            );
            if let Some(metrics) = &self.metrics {
                metrics.record_execution_abandoned(abandoned.len());
            }
        }
        abandoned.len()
    }

    async fn cleanup_loop(&self) {
        let mut interval = tokio::time::interval(CLEANUP_INTERVAL);
        let results_ttl_secs: u64 = std::env::var("RBE_RESULTS_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RESULTS_TTL_SECS);

        loop {
            interval.tick().await;

            for worker_id in self.reap_silent_workers() {
                if self
                    .result_tx
                    .send(WorkerReport::Lost(worker_id))
                    .await
                    .is_err()
                {
                    break;
                }
            }

            self.cancel_unwatched().await;

            let retired = self
                .state_manager
                .cleanup_finished_machines(results_ttl_secs * 1000)
                .await;
            if retired > 0 {
                debug!("Retired {} finished state machines", retired);
            }

            let in_flight = self.scheduler.cleanup_stale_actions(STALE_ACTION_AGE);
            debug!("Scheduler has {} in-flight actions", in_flight);

            let results_cleaned = self.results_store.cleanup_old(results_ttl_secs);
            if results_cleaned > 0 {
                info!(
                    "Cleaned up {} old results (ttl={}s)",
                    results_cleaned, results_ttl_secs
                );
            }
        }
    }
}

/// The failure reported for an operation that lost `losses` workers.
fn worker_losses_result(operation_id: OperationId, losses: u32) -> ExecutionResult {
    engine_failure(
        operation_id,
        format!(
            "the action lost {losses} workers before finishing; it may crash or hang the worker that runs it"
        ),
    )
}

/// The result of an operation the engine ended without a worker result.
fn engine_failure(operation_id: OperationId, error: String) -> ExecutionResult {
    ExecutionResult {
        execution_id: execution_id(operation_id),
        exit_code: -1,
        stdout: vec![],
        stderr: vec![],
        stdout_digest: None,
        stderr_digest: None,
        output_files: vec![],
        output_directories: vec![],
        output_symlinks: vec![],
        execution_duration: Duration::ZERO,
        error: Some(error),
        timed_out: false,
        worker_id: String::new(),
        timestamps: PhaseTimestamps::default(),
        input_stats: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::scheduler::QueuePriority;
    use crate::types::DigestInfo;
    use crate::worker::k8s::{NetworkPolicy, PhaseTimestamps, WorkerInfo};
    use std::time::Instant;

    /// Notes, for each result it records, whether the operation was already
    /// terminal and how many digests were still in flight at that moment.
    struct ObservingRecorder {
        scheduler: Arc<MultiLevelScheduler>,
        seen: std::sync::Mutex<Vec<(OperationId, bool, usize)>>,
    }

    #[async_trait::async_trait]
    impl ResultRecorder for ObservingRecorder {
        async fn record(
            &self,
            _action: Option<&ExecutableAction>,
            state_machine: &ExecutionStateMachine,
            result: &ExecutionResult,
        ) -> ExecuteResponse {
            let terminal = state_machine.is_terminal().await;
            self.seen.lock().unwrap().push((
                state_machine.operation_id,
                terminal,
                self.scheduler.stats().in_flight,
            ));
            ExecuteResponse {
                message: result.worker_id.clone(),
                ..Default::default()
            }
        }
    }

    fn engine_with_recorder() -> (ExecutionEngine, Arc<ObservingRecorder>) {
        let scheduler = Arc::new(MultiLevelScheduler::new());
        let recorder = Arc::new(ObservingRecorder {
            scheduler: scheduler.clone(),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let engine = ExecutionEngine::new(
            scheduler,
            Arc::new(WorkerRegistry::new()),
            Arc::new(StateMachineManager::new()),
            Arc::new(ResultsStore::new()),
            recorder.clone(),
        );
        (engine, recorder)
    }

    fn engine() -> ExecutionEngine {
        engine_with_recorder().0
    }

    fn register(engine: &ExecutionEngine, worker_id: &str) -> mpsc::Receiver<WorkAssignment> {
        register_with_labels(engine, worker_id, &[])
    }

    fn register_with_labels(
        engine: &ExecutionEngine,
        worker_id: &str,
        labels: &[&str],
    ) -> mpsc::Receiver<WorkAssignment> {
        register_with(engine, worker_id, labels, 1)
    }

    fn register_with(
        engine: &ExecutionEngine,
        worker_id: &str,
        labels: &[&str],
        capacity: usize,
    ) -> mpsc::Receiver<WorkAssignment> {
        let (assignment_tx, assignment_rx) = mpsc::channel(8);
        engine.worker_registry.register(WorkerInfo {
            worker_id: worker_id.to_string(),
            worker_type: "default".to_string(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            state: crate::worker::k8s::WorkerState::Idle,
            capacity,
            running: vec![],
            last_heartbeat: Instant::now(),
            assignment_tx,
        });
        assignment_rx
    }

    /// Enqueues an action as `Execute` does and returns its state machine.
    async fn submit(engine: &ExecutionEngine, hash_char: char) -> Arc<ExecutionStateMachine> {
        submit_requiring(engine, hash_char, &[]).await
    }

    async fn submit_requiring(
        engine: &ExecutionEngine,
        hash_char: char,
        labels: &[&str],
    ) -> Arc<ExecutionStateMachine> {
        let digest = DigestInfo::new(&hash_char.to_string().repeat(64), 10).unwrap();
        let operation_id = OperationId::generate();
        let state_machine = engine
            .state_manager
            .create_machine_with_id(operation_id, digest);
        state_machine
            .transition_to(ExecutionStage::Queued)
            .await
            .unwrap();
        let action = ExecutableAction {
            operation_id,
            action_digest: digest,
            command: vec!["/bin/true".to_string()],
            timeout: Duration::from_secs(60),
            priority: QueuePriority::Medium,
            output_files: vec![],
            output_directories: vec![],
            working_directory: Some("pkg".to_string()),
            input_root_digest: None,
            environment_variables: vec![("LANG".to_string(), "C.UTF-8".to_string())],
            do_not_cache: false,
            requirements: WorkerRequirements {
                worker_type: None,
                labels: labels.iter().map(|l| l.to_string()).collect(),
            },
            network: NetworkPolicy::default(),
            worker_losses: 0,
            metadata: None,
            execution_priority: 0,
        };
        engine
            .scheduler
            .enqueue(action, state_machine.clone())
            .unwrap();
        state_machine
    }

    fn result(
        state_machine: &ExecutionStateMachine,
        exit_code: i32,
        error: Option<&str>,
    ) -> ExecutionResult {
        ExecutionResult {
            execution_id: execution_id(state_machine.operation_id),
            exit_code,
            stdout: vec![],
            stderr: vec![],
            stdout_digest: None,
            stderr_digest: None,
            output_files: vec![],
            output_directories: vec![],
            output_symlinks: vec![],
            execution_duration: Duration::ZERO,
            error: error.map(str::to_string),
            timed_out: false,
            worker_id: String::new(),
            timestamps: PhaseTimestamps::default(),
            input_stats: None,
        }
    }

    #[tokio::test]
    async fn actions_wait_for_a_worker_instead_of_failing() {
        let engine = engine();
        let state_machine = submit(&engine, 'a').await;

        assert_eq!(engine.dispatch_ready().await, 0);
        assert_eq!(state_machine.current_state().await, ExecutionStage::Queued);

        let mut assignments = register(&engine, "w1");
        assert_eq!(engine.dispatch_ready().await, 1);
        let assignment = assignments.recv().await.unwrap();
        assert_eq!(assignment.working_directory.as_deref(), Some("pkg"));
        assert_eq!(
            assignment.environment_variables,
            vec![("LANG".to_string(), "C.UTF-8".to_string())]
        );
        assert_eq!(
            state_machine.current_state().await,
            ExecutionStage::Assigned
        );
    }

    #[tokio::test]
    async fn one_pass_dispatches_to_every_idle_worker() {
        let engine = engine();
        let _w1 = register(&engine, "w1");
        let _w2 = register(&engine, "w2");
        for hash_char in ['a', 'b', 'c'] {
            submit(&engine, hash_char).await;
        }

        assert_eq!(engine.dispatch_ready().await, 2, "one per idle worker");
        assert_eq!(engine.dispatch_ready().await, 0, "both workers are busy");
    }

    #[tokio::test]
    async fn the_dispatcher_wakes_on_new_work_and_on_a_freed_slot() {
        let engine = Arc::new(engine());
        let mut assignments = register(&engine, "w1");
        let dispatcher = tokio::spawn({
            let engine = engine.clone();
            async move { engine.dispatcher_loop().await }
        });
        let well_under_the_fallback = DISPATCH_FALLBACK_INTERVAL / 4;

        let first = submit(&engine, 'a').await;
        let second = submit(&engine, 'b').await;
        tokio::time::timeout(well_under_the_fallback, assignments.recv())
            .await
            .expect("new work is dispatched at once");
        assert_eq!(second.current_state().await, ExecutionStage::Queued);

        engine
            .worker_registry
            .release("w1", &execution_id(first.operation_id));
        tokio::time::timeout(well_under_the_fallback, assignments.recv())
            .await
            .expect("a freed slot takes the next action at once");
        assert_eq!(second.current_state().await, ExecutionStage::Assigned);
        dispatcher.abort();
    }

    #[tokio::test]
    async fn an_action_waits_for_a_worker_of_its_platform_without_blocking_others() {
        let engine = engine();
        let mut arm = register_with_labels(&engine, "arm", &["os=linux", "arch=arm64"]);
        let amd64_only = submit_requiring(&engine, 'a', &["arch=amd64"]).await;
        let anywhere = submit(&engine, 'b').await;

        assert_eq!(engine.dispatch_ready().await, 1);
        assert_eq!(
            arm.recv().await.unwrap().execution_id,
            execution_id(anywhere.operation_id)
        );
        assert_eq!(amd64_only.current_state().await, ExecutionStage::Queued);

        let mut amd = register_with_labels(&engine, "amd", &["os=linux", "arch=amd64"]);
        assert_eq!(engine.dispatch_ready().await, 1);
        assert_eq!(
            amd.recv().await.unwrap().execution_id,
            execution_id(amd64_only.operation_id)
        );
    }

    #[tokio::test]
    async fn a_worker_with_capacity_takes_several_actions_and_frees_each_slot_on_its_result() {
        let engine = engine();
        let mut assignments = register_with(&engine, "w", &[], 2);
        let first = submit(&engine, 'a').await;
        let second = submit(&engine, 'b').await;
        let third = submit(&engine, 'c').await;

        assert_eq!(engine.dispatch_ready().await, 2, "two slots");
        assignments.recv().await.unwrap();
        assignments.recv().await.unwrap();
        assert_eq!(third.current_state().await, ExecutionStage::Queued);

        engine
            .worker_registry
            .release("w", &execution_id(first.operation_id));
        engine
            .finalize(first.operation_id, result(&first, 0, None), None)
            .await;
        assert_eq!(
            engine.dispatch_ready().await,
            1,
            "the freed slot takes the third"
        );
        assert_eq!(second.current_state().await, ExecutionStage::Assigned);
        assert_eq!(third.current_state().await, ExecutionStage::Assigned);
    }

    #[tokio::test]
    async fn a_lost_worker_hands_its_action_to_another() {
        let engine = engine();
        let mut first = register(&engine, "w1");
        let state_machine = submit(&engine, 'a').await;
        assert_eq!(engine.dispatch_ready().await, 1);
        first.recv().await.unwrap();

        engine.worker_registry.unregister("w1");
        engine.worker_lost("w1").await;
        assert_eq!(state_machine.current_state().await, ExecutionStage::Queued);

        let mut second = register(&engine, "w2");
        assert_eq!(engine.dispatch_ready().await, 1);
        let assignment = second.recv().await.unwrap();
        assert_eq!(
            assignment.execution_id,
            execution_id(state_machine.operation_id)
        );
    }

    #[tokio::test]
    async fn a_nonzero_exit_completes_and_only_an_infrastructure_error_fails() {
        let engine = engine();
        let _worker = register(&engine, "w1");
        let exited = submit(&engine, 'a').await;
        let broken = submit(&engine, 'b').await;
        engine.dispatch_ready().await;
        let _worker2 = register(&engine, "w2");
        engine.dispatch_ready().await;

        engine
            .finalize(exited.operation_id, result(&exited, 1, None), None)
            .await;
        engine
            .finalize(
                broken.operation_id,
                result(&broken, -1, Some("inputs missing")),
                None,
            )
            .await;

        assert_eq!(exited.current_state().await, ExecutionStage::Completed);
        assert_eq!(broken.current_state().await, ExecutionStage::Failed);
        assert_eq!(
            engine
                .results_store
                .get(exited.operation_id)
                .unwrap()
                .result
                .exit_code,
            1
        );
    }

    #[tokio::test]
    async fn an_action_that_loses_too_many_workers_fails() {
        let mut engine = engine();
        engine.max_worker_losses = 2;
        let state_machine = submit(&engine, 'a').await;
        for round in 0..3 {
            let worker = format!("w{round}");
            let mut assignments = register(&engine, &worker);
            assert_eq!(engine.dispatch_ready().await, 1, "round {round}");
            assignments.recv().await.unwrap();
            engine.worker_registry.unregister(&worker);
            engine.worker_lost(&worker).await;
        }
        assert_eq!(state_machine.current_state().await, ExecutionStage::Failed);
        let failure = engine
            .results_store
            .get(state_machine.operation_id)
            .unwrap();
        assert!(failure.result.error.unwrap().contains("lost 3 workers"));
    }

    #[tokio::test]
    async fn a_silent_worker_is_reaped_and_its_action_requeued() {
        let mut engine = engine();
        engine.heartbeat_timeout = Duration::from_secs(60);
        let mut assignments = register(&engine, "silent");
        let state_machine = submit(&engine, 'a').await;
        assert_eq!(engine.dispatch_ready().await, 1);
        assignments.recv().await.unwrap();
        assert!(engine.reap_silent_workers().is_empty());

        engine
            .worker_registry
            .age_heartbeat("silent", Duration::from_secs(120));
        assert_eq!(engine.reap_silent_workers(), vec!["silent".to_string()]);
        assert!(engine.worker_registry.snapshot().is_empty());

        engine.worker_lost("silent").await;
        assert_eq!(state_machine.current_state().await, ExecutionStage::Queued);
    }

    #[tokio::test]
    async fn a_late_result_does_not_finish_a_requeued_action() {
        let engine = engine();
        let mut assignments = register(&engine, "w1");
        let state_machine = submit(&engine, 'a').await;
        assert_eq!(engine.dispatch_ready().await, 1);
        assignments.recv().await.unwrap();
        engine.worker_registry.unregister("w1");
        engine.worker_lost("w1").await;

        engine
            .finalize(
                state_machine.operation_id,
                result(&state_machine, 0, None),
                None,
            )
            .await;

        assert_eq!(state_machine.current_state().await, ExecutionStage::Queued);
        assert!(engine
            .results_store
            .get(state_machine.operation_id)
            .is_none());
    }

    #[tokio::test]
    async fn a_result_is_recorded_before_it_is_published_and_its_digest_released() {
        let (engine, recorder) = engine_with_recorder();
        let mut assignments = register(&engine, "w1");
        let state_machine = submit(&engine, 'a').await;
        assert_eq!(engine.dispatch_ready().await, 1);
        assignments.recv().await.unwrap();

        let mut reported = result(&state_machine, 0, None);
        reported.worker_id = "w1".to_string();
        engine
            .finalize(state_machine.operation_id, reported, None)
            .await;

        assert_eq!(
            *recorder.seen.lock().unwrap(),
            vec![(state_machine.operation_id, false, 1)],
            "recorded while still running and in flight"
        );
        assert_eq!(
            state_machine.current_state().await,
            ExecutionStage::Completed
        );
        assert_eq!(engine.scheduler.stats().in_flight, 0);
        let finished = engine
            .results_store
            .get(state_machine.operation_id)
            .unwrap();
        assert_eq!(finished.response.message, "w1");
    }

    #[tokio::test]
    async fn a_queued_operation_nobody_follows_is_cancelled_and_one_followed_is_kept() {
        let mut engine = engine();
        engine.unwatched_timeout = Duration::ZERO;
        let abandoned = submit(&engine, 'a').await;
        let followed = submit(&engine, 'b').await;
        let _client = followed.watch();
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert_eq!(engine.cancel_unwatched().await, 1);

        assert_eq!(abandoned.current_state().await, ExecutionStage::Failed);
        let finished = engine.results_store.get(abandoned.operation_id).unwrap();
        assert!(finished
            .result
            .error
            .unwrap()
            .contains("no client followed"));
        assert_eq!(followed.current_state().await, ExecutionStage::Queued);
        assert_eq!(engine.scheduler.queued_len(), 1);
        assert_eq!(
            engine.scheduler.stats().in_flight,
            1,
            "the cancelled digest is released"
        );
    }
}
