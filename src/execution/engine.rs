use crate::cache::action_cache::L1ActionCache;
use crate::execution::results::ResultsStore;
use crate::execution::scheduler::{ExecutableAction, MultiLevelScheduler};
use crate::execution::state_machine::{
    ExecutionStage, ExecutionStateMachine, OperationId, StateMachineManager,
};
use crate::worker::k8s::{
    ExecutionResult, WorkAssignment, WorkerRegistry, WorkerRequirements, WorkerState,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

pub struct ExecutionEngine {
    scheduler: Arc<MultiLevelScheduler>,
    worker_registry: Arc<WorkerRegistry>,
    state_manager: Arc<StateMachineManager>,
    #[allow(dead_code)]
    l1_cache: Arc<L1ActionCache>,
    results_store: Arc<ResultsStore>,

    assignment_tx: mpsc::Sender<(String, WorkAssignment)>,
    #[allow(dead_code)]
    assignment_rx: RwLock<mpsc::Receiver<(String, WorkAssignment)>>,

    result_tx: mpsc::Sender<(String, ExecutionResult)>,
    result_rx: RwLock<mpsc::Receiver<(String, ExecutionResult)>>,

    active_executions: Arc<RwLock<HashMap<OperationId, String>>>,
}

impl ExecutionEngine {
    pub fn new(
        scheduler: Arc<MultiLevelScheduler>,
        worker_registry: Arc<WorkerRegistry>,
        state_manager: Arc<StateMachineManager>,
        l1_cache: Arc<L1ActionCache>,
        results_store: Arc<ResultsStore>,
    ) -> Self {
        let (assignment_tx, assignment_rx) = mpsc::channel(1000);
        let (result_tx, result_rx) = mpsc::channel(1000);

        Self {
            scheduler,
            worker_registry,
            state_manager,
            l1_cache,
            results_store,
            assignment_tx,
            assignment_rx: RwLock::new(assignment_rx),
            result_tx,
            result_rx: RwLock::new(result_rx),
            active_executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn assignment_sender(&self) -> mpsc::Sender<(String, WorkAssignment)> {
        self.assignment_tx.clone()
    }

    pub fn result_sender(&self) -> mpsc::Sender<(String, ExecutionResult)> {
        self.result_tx.clone()
    }

    #[allow(dead_code)]
    pub fn results_store(&self) -> Arc<ResultsStore> {
        self.results_store.clone()
    }

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
        let mut interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            interval.tick().await;

            if let Some((action, state_machine)) = self.scheduler.dequeue() {
                let operation_id = action.operation_id;
                let digest = action.action_digest;

                debug!("Dispatcher: processing action {}", operation_id);

                if let Err(e) = state_machine.transition_to(ExecutionStage::Queued).await {
                    warn!("Failed to transition to Queued: {}", e);
                    self.scheduler.complete_action(&digest);
                    continue;
                }

                if let Err(e) = state_machine.transition_to(ExecutionStage::Assigned).await {
                    warn!("Failed to transition to Assigned: {}", e);
                    self.scheduler.complete_action(&digest);
                    continue;
                }

                let requirements = WorkerRequirements::default();
                match self.worker_registry.select_worker(&requirements) {
                    Some(selected) => {
                        let assignment = WorkAssignment {
                            execution_id: format!("exec-{}", operation_id.0),
                            action_digest: Some(action.action_digest.to_string()),
                            input_root_digest: action
                                .input_root_digest
                                .as_ref()
                                .map(|d| d.to_string()),
                            command: action.command.clone(),
                            timeout: action.timeout,
                            output_files: action.output_files.clone(),
                            output_directories: action.output_directories.clone(),
                            working_directory: action.working_directory.clone(),
                        };

                        let worker_id: String = selected.worker_id.clone();
                        if let Err(e) = selected.assignment_tx.send(assignment).await {
                            warn!("Failed to send assignment to worker {}: {}", worker_id, e);

                            self.requeue_action(action, state_machine).await;
                            continue;
                        }

                        {
                            let mut active = self.active_executions.write().await;
                            active.insert(operation_id, worker_id.clone());
                        }

                        let status = crate::worker::k8s::WorkerStatus {
                            worker_id: worker_id.clone(),
                            state: WorkerState::Busy,
                            execution_id: Some(format!("exec-{}", operation_id.0)),
                            progress: None,
                            result: None,
                        };
                        self.worker_registry.update_status(&worker_id, status);

                        info!(
                            "Assigned operation {} to worker {}",
                            operation_id, worker_id
                        );
                    }
                    None => {
                        warn!(
                            "No workers available for operation {}, completing with error",
                            operation_id
                        );
                        if let Err(e) = state_machine.transition_to(ExecutionStage::Failed).await {
                            warn!("Failed to transition to Failed: {}", e);
                        }
                        self.scheduler.complete_action(&digest);
                    }
                }
            }
        }
    }

    async fn result_processor_loop(&self) {
        let mut result_rx: tokio::sync::RwLockWriteGuard<
            '_,
            mpsc::Receiver<(String, ExecutionResult)>,
        > = self.result_rx.write().await;

        while let Some((worker_id, result)) = result_rx.recv().await {
            let worker_id: String = worker_id;
            let execution_id: String = result.execution_id.clone();
            info!(
                "Processing result for execution {} from worker {}",
                execution_id, worker_id
            );

            let op_id_str = execution_id.trim_start_matches("exec-");
            if let Ok(op_id_num) = op_id_str.parse::<u64>() {
                let operation_id = OperationId(op_id_num);

                if let Some(sm) = self.state_manager.get_machine(operation_id) {
                    let final_stage = if result.exit_code == 0 {
                        ExecutionStage::Completed
                    } else {
                        ExecutionStage::Failed
                    };

                    if let Err(e) = sm.transition_to(final_stage).await {
                        warn!("Failed to transition to final state: {}", e);
                    }

                    let digest = sm.action_digest;

                    {
                        let mut active = self.active_executions.write().await;
                        active.remove(&operation_id);
                    }

                    self.scheduler.complete_action(&digest);

                    self.results_store.store(operation_id, result.clone());

                    info!(
                        "Execution {} completed with exit code {}",
                        execution_id, result.exit_code
                    );
                } else {
                    warn!("State machine not found for operation {}", operation_id);
                }
            }

            let wid: String = worker_id.clone();
            let status = crate::worker::k8s::WorkerStatus {
                worker_id: wid.clone(),
                state: WorkerState::Idle,
                execution_id: None,
                progress: None,
                result: None,
            };
            self.worker_registry.update_status(&wid, status);
        }
    }

    async fn cleanup_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));

        loop {
            interval.tick().await;

            let cleaned = self.state_manager.cleanup_dead_machines(300_000).await;
            if cleaned > 0 {
                warn!("Cleaned up {} dead state machines", cleaned);
            }

            let stale = self
                .scheduler
                .cleanup_stale_actions(Duration::from_secs(3600));
            debug!("Scheduler has {} in-flight actions", stale);
        }
    }

    async fn requeue_action(
        &self,
        action: ExecutableAction,
        state_machine: Arc<ExecutionStateMachine>,
    ) {
        tokio::time::sleep(Duration::from_millis(500)).await;

        match self.scheduler.enqueue(action, state_machine) {
            Ok(_) => debug!("Re-queued action successfully"),
            Err(e) => warn!("Failed to re-queue action: {}", e),
        }
    }

    #[allow(dead_code)]
    pub async fn stats(&self) -> EngineStats {
        let active = self.active_executions.read().await;
        EngineStats {
            active_executions: active.len(),
            scheduler_stats: self.scheduler.stats(),
            worker_stats: self.worker_registry.stats(),
        }
    }

    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        info!("ExecutionEngine shutdown signal sent");
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EngineStats {
    pub active_executions: usize,
    pub scheduler_stats: crate::execution::scheduler::SchedulerQueueStats,
    pub worker_stats: crate::worker::k8s::WorkerStats,
}
