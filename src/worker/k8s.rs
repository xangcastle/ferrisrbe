//! Worker-side types shared with the server, the server's registry of
//! connected workers, and a minimal in-process worker that runs commands
//! directly.

use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Identity and resource envelope a worker advertises to the server.
#[derive(Debug, Clone)]
pub struct K8sWorkerConfig {
    pub server_endpoint: String,
    /// Scheduling class; part of the worker id and matched by
    /// [`WorkerRequirements::worker_type`].
    pub worker_type: String,
    /// `key=value` platform labels matched by [`WorkerRequirements::labels`].
    pub labels: Vec<String>,
    pub max_concurrent: usize,
    pub memory_limit_mb: usize,
    pub cpu_limit_millicores: usize,
}

impl Default for K8sWorkerConfig {
    fn default() -> Self {
        Self {
            server_endpoint: "http://rbe-server:9092".to_string(),
            worker_type: "default".to_string(),
            labels: vec!["os=linux".to_string(), "arch=arm64".to_string()],
            max_concurrent: 4,
            memory_limit_mb: 8192,
            cpu_limit_millicores: 4000,
        }
    }
}

/// A worker that executes each assignment as a local child process, one at a
/// time, and reports status on a channel.
pub struct K8sWorker {
    config: K8sWorkerConfig,
    worker_id: String,
    assignment_rx: mpsc::Receiver<WorkAssignment>,
    status_tx: mpsc::Sender<WorkerStatus>,
}

/// One action handed to a worker.
#[derive(Debug, Clone)]
pub struct WorkAssignment {
    pub execution_id: String,
    pub action_digest: Option<String>,
    pub input_root_digest: Option<String>,
    pub command: Vec<String>,
    pub timeout: Duration,
    /// Output files the action must produce, relative to the working directory.
    pub output_files: Vec<String>,
    /// Output directories the action must produce, relative to the working directory.
    pub output_directories: Vec<String>,
    /// Working directory relative to the execroot; the execroot itself when `None`.
    pub working_directory: Option<String>,
    /// `Command.environment_variables`, in the order the client sent them.
    pub environment_variables: Vec<(String, String)>,
    pub network: NetworkPolicy,
}

/// Network access an action asked for with the `Network` platform property.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No `Network` property: the worker's `RBE_WORKER_NETWORK` decides.
    #[default]
    WorkerDefault,
    /// `Network=off`: loopback only.
    Off,
    /// `Network=on`.
    On,
}

impl NetworkPolicy {
    /// The policy of a REAPI platform's `Network` property.
    ///
    /// # Errors
    ///
    /// For a `Network` value other than `on` or `off`.
    pub fn from_platform(properties: &[(String, String)]) -> Result<Self, String> {
        let mut policy = Self::WorkerDefault;
        for (name, value) in properties {
            if name == "Network" {
                policy = Self::parse(value)?;
            }
        }
        Ok(policy)
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            _ => Err(format!("unsupported Network {value:?}; supported: on, off")),
        }
    }
}

/// A worker's report of its state, and of the result when it finishes.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub worker_id: String,
    pub state: WorkerState,
    pub execution_id: Option<String>,
    pub progress: Option<ProgressUpdate>,
    pub result: Option<ExecutionResult>,
}

/// Availability of a worker for new assignments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Idle,
    Busy,
    Unhealthy,
    /// Finishing what it runs before shutting down; gets no new assignments.
    Draining,
}

/// Progress of a running execution.
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub stage: String,
    pub percent_complete: f32,
    pub message: String,
}

/// Outcome of one execution. Stdout and stderr travel either inline or as
/// CAS digests, never both.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub execution_id: String,
    /// Process exit code; `-1` when the process could not be started or was
    /// terminated by a signal.
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Where the worker stored stdout or stderr in the CAS when it did not
    /// fit inline; the bytes are then empty.
    pub stdout_digest: Option<crate::types::DigestInfo>,
    pub stderr_digest: Option<crate::types::DigestInfo>,
    pub output_files: Vec<OutputFile>,
    pub output_directories: Vec<OutputDirectory>,
    pub output_symlinks: Vec<OutputSymlink>,
    pub execution_duration: Duration,
    /// Why the action could not run at all; `exit_code` is meaningless then.
    pub error: Option<String>,
    /// The worker killed the action when its timeout expired.
    pub timed_out: bool,
    /// The worker that ran the action; empty when unknown.
    pub worker_id: String,
    pub timestamps: PhaseTimestamps,
    /// What the worker materialized as the action's inputs.
    pub input_stats: Option<InputStats>,
}

/// Input files and bytes of one execution, with its heaviest groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputStats {
    pub files: u64,
    pub bytes: u64,
    pub groups: Vec<crate::worker::materializer::InputGroup>,
}

/// When each phase of an execution happened on the worker, in Unix
/// milliseconds; `0` for a phase that did not happen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseTimestamps {
    pub worker_start_ms: i64,
    pub input_fetch_start_ms: i64,
    pub input_fetch_completed_ms: i64,
    pub execution_start_ms: i64,
    pub execution_completed_ms: i64,
    pub output_upload_start_ms: i64,
    pub output_upload_completed_ms: i64,
    pub worker_completed_ms: i64,
}

/// A produced file, already uploaded to the CAS.
#[derive(Debug, Clone)]
pub struct OutputFile {
    pub path: String,
    pub digest: String,
    pub size_bytes: i64,
    pub is_executable: bool,
}

/// A produced directory, uploaded to the CAS as a `Tree`.
#[derive(Debug, Clone)]
pub struct OutputDirectory {
    pub path: String,
    pub digest: String,
    pub size_bytes: i64,
}

/// A produced output that is a symlink, reported as such rather than followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSymlink {
    pub path: String,
    /// The link's content exactly as `readlink` returns it.
    pub target: String,
}

#[derive(Clone)]
pub struct AssignmentSender {
    pub sender: mpsc::Sender<WorkAssignment>,
}

/// Status heartbeat interval of an idle [`K8sWorker`].
const IDLE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

impl K8sWorker {
    /// Creates a worker together with the sender that feeds it assignments
    /// and the receiver of its status reports.
    pub fn new(
        config: K8sWorkerConfig,
    ) -> (
        Self,
        mpsc::Sender<WorkAssignment>,
        mpsc::Receiver<WorkerStatus>,
    ) {
        let worker_id = format!("{}-{}", config.worker_type, uuid::Uuid::new_v4());

        let (assignment_tx, assignment_rx) = mpsc::channel(10);
        let (status_tx, status_rx) = mpsc::channel(100);

        let worker = Self {
            config,
            worker_id,
            assignment_rx,
            status_tx,
        };

        (worker, assignment_tx, status_rx)
    }

    /// Executes assignments until the assignment channel closes, reporting
    /// `Busy` before and `Idle` with the result after each one, plus an
    /// `Idle` heartbeat every 30 seconds.
    pub async fn run(mut self) -> anyhow::Result<()> {
        info!("K8s Worker {} starting...", self.worker_id);
        info!("Connecting to server: {}", self.config.server_endpoint);

        let status_tx = self.status_tx.clone();
        let worker_id = self.worker_id.clone();
        tokio::spawn(async move {
            loop {
                let status = WorkerStatus {
                    worker_id: worker_id.clone(),
                    state: WorkerState::Idle,
                    execution_id: None,
                    progress: None,
                    result: None,
                };

                if status_tx.send(status).await.is_err() {
                    break;
                }

                tokio::time::sleep(IDLE_HEARTBEAT_INTERVAL).await;
            }
        });

        loop {
            match self.assignment_rx.recv().await {
                Some(assignment) => {
                    info!("Received assignment: {}", assignment.execution_id);

                    let _ = self
                        .status_tx
                        .send(WorkerStatus {
                            worker_id: self.worker_id.clone(),
                            state: WorkerState::Busy,
                            execution_id: Some(assignment.execution_id.clone()),
                            progress: Some(ProgressUpdate {
                                stage: "Queued".to_string(),
                                percent_complete: 0.0,
                                message: "Starting execution".to_string(),
                            }),
                            result: None,
                        })
                        .await;

                    let result = self.execute_action(&assignment).await;

                    let _ = self
                        .status_tx
                        .send(WorkerStatus {
                            worker_id: self.worker_id.clone(),
                            state: WorkerState::Idle,
                            execution_id: Some(assignment.execution_id),
                            progress: None,
                            result: Some(result),
                        })
                        .await;
                }
                None => {
                    warn!("Assignment channel closed, shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Runs the assignment's command, or echoes its id when the command is
    /// empty. A spawn failure is reported as exit code `-1` with the error in
    /// stderr, never as an `Err`.
    async fn execute_action(&self, assignment: &WorkAssignment) -> ExecutionResult {
        let start = Instant::now();

        let output = if assignment.command.is_empty() {
            Command::new("echo")
                .arg(format!("Executed: {}", assignment.execution_id))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        } else {
            let mut cmd = Command::new(&assignment.command[0]);
            if assignment.command.len() > 1 {
                cmd.args(&assignment.command[1..]);
            }
            cmd.stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        };

        match output {
            Ok(output) => ExecutionResult {
                execution_id: assignment.execution_id.clone(),
                exit_code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
                stdout_digest: None,
                stderr_digest: None,
                output_files: vec![],
                output_directories: vec![],
                output_symlinks: vec![],
                execution_duration: start.elapsed(),
                error: None,
                timed_out: false,
                worker_id: String::new(),
                timestamps: PhaseTimestamps::default(),
                input_stats: None,
            },
            Err(e) => {
                error!("Failed to execute: {}", e);
                ExecutionResult {
                    execution_id: assignment.execution_id.clone(),
                    exit_code: -1,
                    stdout: vec![],
                    stderr: e.to_string().into_bytes(),
                    stdout_digest: None,
                    stderr_digest: None,
                    output_files: vec![],
                    output_directories: vec![],
                    output_symlinks: vec![],
                    execution_duration: start.elapsed(),
                    error: Some(e.to_string()),
                    timed_out: false,
                    worker_id: String::new(),
                    timestamps: PhaseTimestamps::default(),
                    input_stats: None,
                }
            }
        }
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }
}

/// Server-side registry of connected workers, keyed by worker id.
pub struct WorkerRegistry {
    workers: dashmap::DashMap<String, WorkerInfo>,
    /// Signalled when a slot may have opened: a worker registered, freed a
    /// slot or reported a heartbeat. Wakes [`WorkerRegistry::capacity_changed`].
    capacity: tokio::sync::Notify,
}

/// What the server knows about one connected worker.
///
/// A worker runs up to `capacity` executions at once; `running` holds the
/// ones assigned to it and not yet reported. `state` is `Busy` while any is
/// running and `Idle` otherwise, unless the worker is `Unhealthy` or
/// `Draining`.
#[derive(Debug, Clone)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub worker_type: String,
    pub labels: Vec<String>,
    pub state: WorkerState,
    /// Executions the worker accepts at once, at least 1.
    pub capacity: usize,
    /// Execution ids assigned to the worker and not yet reported.
    pub running: Vec<String>,
    /// Time of the last status update received from the worker.
    pub last_heartbeat: Instant,
    /// Delivers assignments to the worker's stream.
    pub assignment_tx: mpsc::Sender<WorkAssignment>,
}

impl WorkerInfo {
    fn accepts_work(&self) -> bool {
        !matches!(self.state, WorkerState::Unhealthy | WorkerState::Draining)
    }

    fn has_free_slot(&self) -> bool {
        self.accepts_work() && self.running.len() < self.capacity.max(1)
    }

    fn refresh_state(&mut self) {
        if self.accepts_work() {
            self.state = if self.running.is_empty() {
                WorkerState::Idle
            } else {
                WorkerState::Busy
            };
        }
    }
}

/// JSON view of a worker for the API and the UI. `state` is the lowercase
/// [`WorkerState`] name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkerView {
    pub worker_id: String,
    pub worker_type: String,
    pub labels: Vec<String>,
    pub state: String,
    /// The oldest running execution, if any.
    pub current_execution: Option<String>,
    pub running: usize,
    pub capacity: usize,
    pub heartbeat_age_secs: u64,
}

/// A worker chosen for an assignment and the channel to reach it.
#[derive(Clone)]
pub struct SelectedWorker {
    pub worker_id: String,
    pub assignment_tx: mpsc::Sender<WorkAssignment>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self {
            workers: dashmap::DashMap::new(),
            capacity: tokio::sync::Notify::new(),
        }
    }

    /// Returns once a slot may have opened since the last call; a change
    /// while nobody waits is kept for the next one.
    pub async fn capacity_changed(&self) {
        self.capacity.notified().await;
    }

    pub fn register(&self, info: WorkerInfo) {
        info!("Registering worker: {}", info.worker_id);
        self.workers.insert(info.worker_id.clone(), info);
        self.capacity.notify_one();
    }

    /// Workers whose last heartbeat is older than `timeout`.
    pub fn silent_workers(&self, timeout: Duration) -> Vec<String> {
        self.workers
            .iter()
            .filter(|w| w.last_heartbeat.elapsed() > timeout)
            .map(|w| w.worker_id.clone())
            .collect()
    }

    /// Moves `worker_id`'s last heartbeat `by` into the past.
    #[cfg(test)]
    pub fn age_heartbeat(&self, worker_id: &str, by: Duration) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.last_heartbeat = std::time::Instant::now() - by;
        }
    }

    pub fn unregister(&self, worker_id: &str) {
        info!("Unregistering worker: {}", worker_id);
        self.workers.remove(worker_id);
    }

    /// Serializable view of every registered worker, sorted by id.
    pub fn snapshot(&self) -> Vec<WorkerView> {
        let mut workers: Vec<WorkerView> = self
            .workers
            .iter()
            .map(|w| WorkerView {
                worker_id: w.worker_id.clone(),
                worker_type: w.worker_type.clone(),
                labels: w.labels.clone(),
                state: format!("{:?}", w.state).to_lowercase(),
                current_execution: w.running.first().cloned(),
                running: w.running.len(),
                capacity: w.capacity,
                heartbeat_age_secs: w.last_heartbeat.elapsed().as_secs(),
            })
            .collect();
        workers.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        workers
    }

    /// The worker matching `requirements` with a free slot and the fewest
    /// running executions, the one that heard from the server least recently
    /// among equals, spreading load across the fleet.
    pub fn select_worker(&self, requirements: &WorkerRequirements) -> Option<SelectedWorker> {
        self.workers
            .iter()
            .filter(|entry| entry.value().has_free_slot() && requirements.matches(entry.value()))
            .min_by_key(|entry| (entry.value().running.len(), entry.value().last_heartbeat))
            .map(|entry| {
                let info = entry.value();
                debug!("Selected worker {} for assignment", info.worker_id);
                SelectedWorker {
                    worker_id: info.worker_id.clone(),
                    assignment_tx: info.assignment_tx.clone(),
                }
            })
    }

    /// Takes a slot of `worker_id` for `execution_id`.
    pub fn reserve(&self, worker_id: &str, execution_id: &str) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.running.push(execution_id.to_string());
            entry.last_heartbeat = Instant::now();
            entry.refresh_state();
        }
    }

    /// Frees the slot `execution_id` held on `worker_id`.
    pub fn release(&self, worker_id: &str, execution_id: &str) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.running.retain(|id| id != execution_id);
            entry.last_heartbeat = Instant::now();
            entry.refresh_state();
        }
        self.capacity.notify_one();
    }

    /// Stops assigning to `worker_id`, whose stream no longer accepts work.
    pub fn mark_unhealthy(&self, worker_id: &str) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.state = WorkerState::Unhealthy;
        }
    }

    /// Stops assigning to `worker_id`, which finishes what it runs and then
    /// disconnects. Only a new registration undoes it.
    pub fn mark_draining(&self, worker_id: &str) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.last_heartbeat = Instant::now();
            entry.state = WorkerState::Draining;
        }
    }

    /// Records a heartbeat: the worker is alive, and healthy unless it says
    /// otherwise. Its slots are the server's own accounting and do not change.
    /// A draining worker stays draining. Returns whether the worker is
    /// registered; one the reaper dropped is not.
    pub fn heartbeat(&self, worker_id: &str, healthy: bool) -> bool {
        let Some(mut entry) = self.workers.get_mut(worker_id) else {
            return false;
        };
        entry.last_heartbeat = Instant::now();
        if entry.state != WorkerState::Draining {
            entry.state = if healthy {
                WorkerState::Idle
            } else {
                WorkerState::Unhealthy
            };
            entry.refresh_state();
        }
        drop(entry);
        self.capacity.notify_one();
        true
    }

    /// Whether `worker_id` is registered.
    pub fn contains(&self, worker_id: &str) -> bool {
        self.workers.contains_key(worker_id)
    }

    /// Delivers `assignment` without waiting.
    ///
    /// # Errors
    ///
    /// When the worker is unknown or its channel is full or closed.
    pub fn send_assignment_to_worker(
        &self,
        worker_id: &str,
        assignment: WorkAssignment,
    ) -> Result<(), String> {
        if let Some(entry) = self.workers.get(worker_id) {
            let info = entry.value();
            info.assignment_tx
                .try_send(assignment)
                .map_err(|e| format!("Failed to send assignment: {}", e))
        } else {
            Err(format!("Worker {} not found", worker_id))
        }
    }

    pub fn list_workers(&self) -> Vec<WorkerInfo> {
        self.workers
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// The fleet's capacity and load right now. Unhealthy and draining
    /// workers get no new assignments, so they add no slots, but what they
    /// still run counts.
    pub fn load(&self) -> FleetLoad {
        let mut load = FleetLoad::default();
        for entry in self.workers.iter() {
            let worker = entry.value();
            load.running += worker.running.len();
            if worker.accepts_work() {
                load.workers += 1;
                load.slots += worker.capacity;
                load.free_slots += worker.capacity.saturating_sub(worker.running.len());
            }
        }
        load
    }

    pub fn stats(&self) -> WorkerStats {
        let mut idle = 0;
        let mut busy = 0;
        let mut unhealthy = 0;
        let mut draining = 0;

        for entry in self.workers.iter() {
            match entry.value().state {
                WorkerState::Idle => idle += 1,
                WorkerState::Busy => busy += 1,
                WorkerState::Unhealthy => unhealthy += 1,
                WorkerState::Draining => draining += 1,
            }
        }

        WorkerStats {
            total: idle + busy + unhealthy + draining,
            idle,
            busy,
            unhealthy,
            draining,
        }
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Constraints a worker must satisfy to receive an action. Empty
/// requirements match every worker.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkerRequirements {
    pub worker_type: Option<String>,
    pub labels: Vec<String>,
}

/// The worker `arch` label of a REAPI platform-lexicon `ISA` value.
const ISA_ARCH_LABELS: [(&str, &str); 2] = [("arm-a64", "arm64"), ("x86-64", "amd64")];

impl WorkerRequirements {
    /// The labels a worker needs to run an action with these REAPI platform
    /// properties: `OSFamily=<os>` requires `os=<os>`, each `ISA` value
    /// requires the matching `arch` label (`arm-a64` is `arm64`, `x86-64` is
    /// `amd64`), and `Pool=<pool>` requires `pool=<pool>`. `Priority` only
    /// orders the queue (see `QueuePriority::from_platform`) and `Network`
    /// only reaches the worker (see [`NetworkPolicy`]); neither requires
    /// anything. No properties match every worker.
    ///
    /// # Errors
    ///
    /// For any other property or `ISA` value: no worker could satisfy it, so
    /// the action is refused rather than left waiting forever.
    pub fn from_platform(properties: &[(String, String)]) -> Result<Self, String> {
        Self::from_platform_with(properties, &[])
    }

    /// [`Self::from_platform`], also accepting each property named in
    /// `extra`, which requires the worker label `<name>=<value>` verbatim
    /// (`CHROOT=false` requires `CHROOT=false`), the way a Buildbarn worker
    /// declares its platform.
    ///
    /// # Errors
    ///
    /// For any property neither known nor in `extra`, and for an unknown
    /// `ISA` value.
    pub fn from_platform_with(
        properties: &[(String, String)],
        extra: &[String],
    ) -> Result<Self, String> {
        let mut labels = Vec::new();
        for (name, value) in properties {
            let label = match name.as_str() {
                "OSFamily" => format!("os={value}"),
                "ISA" => ISA_ARCH_LABELS
                    .iter()
                    .find(|(isa, _)| isa == value)
                    .map(|(_, arch)| format!("arch={arch}"))
                    .ok_or_else(|| {
                        format!("unsupported ISA {value:?}; supported: arm-a64, x86-64")
                    })?,
                "Pool" => format!("pool={value}"),
                "Priority" => continue,
                "Network" => {
                    NetworkPolicy::parse(value)?;
                    continue;
                }
                other if extra.iter().any(|e| e == other) => format!("{name}={value}"),
                _ => {
                    let supported = ["OSFamily", "ISA", "Pool", "Priority", "Network"]
                        .into_iter()
                        .chain(extra.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "unsupported platform property {name:?}; supported: {supported}"
                    ));
                }
            };
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        Ok(Self {
            worker_type: None,
            labels,
        })
    }

    /// Whether `info` has the required type (if any) and every required label.
    pub fn matches(&self, info: &WorkerInfo) -> bool {
        self.worker_type
            .as_ref()
            .is_none_or(|required| &info.worker_type == required)
            && self.labels.iter().all(|label| info.labels.contains(label))
    }
}

/// Capacity and load of the worker fleet at one instant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FleetLoad {
    /// Workers not marked unhealthy.
    pub workers: usize,
    /// Execution slots across those workers.
    pub slots: usize,
    /// Slots of those workers with nothing assigned.
    pub free_slots: usize,
    /// Executions assigned to any worker and not yet reported.
    pub running: usize,
}

/// Worker counts by state.
#[derive(Debug, Clone)]
pub struct WorkerStats {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub unhealthy: usize,
    pub draining: usize,
}

impl WorkerStats {
    /// Busy workers in excess of idle ones; `0` when capacity is available.
    pub fn queue_depth(&self) -> usize {
        self.busy.saturating_sub(self.idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    fn registered(registry: &WorkerRegistry, worker_id: &str, capacity: usize) {
        let (assignment_tx, _) = mpsc::channel(1);
        registry.register(WorkerInfo {
            worker_id: worker_id.to_string(),
            worker_type: "default".to_string(),
            labels: vec![],
            state: WorkerState::Idle,
            capacity,
            running: vec![],
            last_heartbeat: Instant::now(),
            assignment_tx,
        });
    }

    fn selected(registry: &WorkerRegistry) -> Option<String> {
        registry
            .select_worker(&WorkerRequirements::default())
            .map(|w| w.worker_id)
    }

    #[test]
    fn pool_requires_its_label_and_priority_requires_nothing() {
        let requirements = WorkerRequirements::from_platform(&props(&[
            ("OSFamily", "linux"),
            ("Pool", "fast"),
            ("Priority", "critical"),
        ]))
        .unwrap();
        assert_eq!(requirements.labels, vec!["os=linux", "pool=fast"]);
    }

    #[test]
    fn the_load_counts_slots_of_healthy_workers_and_everything_running() {
        let registry = WorkerRegistry::new();
        registered(&registry, "a", 16);
        registered(&registry, "b", 16);
        registered(&registry, "sick", 8);
        registry.reserve("a", "exec-1");
        registry.reserve("a", "exec-2");
        registry.reserve("sick", "exec-3");
        registry.mark_unhealthy("sick");

        assert_eq!(
            registry.load(),
            FleetLoad {
                workers: 2,
                slots: 32,
                free_slots: 30,
                running: 3,
            }
        );
    }

    #[test]
    fn a_heartbeat_tells_whether_the_worker_is_still_registered() {
        let registry = WorkerRegistry::new();
        registered(&registry, "w", 1);
        assert!(registry.heartbeat("w", true));

        registry.unregister("w");
        assert!(!registry.heartbeat("w", true), "a reaped worker is unknown");
        assert!(
            registry.snapshot().is_empty(),
            "and the heartbeat does not bring it back"
        );
    }

    #[test]
    fn a_draining_worker_gets_no_work_and_a_heartbeat_does_not_undo_it() {
        let registry = WorkerRegistry::new();
        registered(&registry, "w", 4);
        registry.reserve("w", "exec-1");
        registry.mark_draining("w");
        registry.heartbeat("w", true);

        assert_eq!(selected(&registry), None);
        assert_eq!(registry.load().slots, 0);
        assert_eq!(registry.load().running, 1);
        assert_eq!(registry.stats().draining, 1);
        registry.release("w", "exec-1");
        assert_eq!(selected(&registry), None, "still draining once idle");
    }

    #[test]
    fn a_worker_takes_as_many_executions_as_its_capacity() {
        let registry = WorkerRegistry::new();
        registered(&registry, "w", 2);
        registry.reserve("w", "exec-1");
        assert_eq!(selected(&registry).as_deref(), Some("w"));
        registry.reserve("w", "exec-2");
        assert_eq!(selected(&registry), None, "both slots are taken");
        registry.release("w", "exec-1");
        assert_eq!(selected(&registry).as_deref(), Some("w"));
    }

    #[test]
    fn the_least_loaded_worker_is_chosen() {
        let registry = WorkerRegistry::new();
        registered(&registry, "busy", 4);
        registered(&registry, "free", 4);
        registry.reserve("busy", "exec-1");
        assert_eq!(selected(&registry).as_deref(), Some("free"));
    }

    #[test]
    fn an_unhealthy_worker_gets_nothing_until_a_healthy_heartbeat() {
        let registry = WorkerRegistry::new();
        registered(&registry, "w", 2);
        registry.mark_unhealthy("w");
        assert_eq!(selected(&registry), None);
        registry.heartbeat("w", true);
        assert_eq!(selected(&registry).as_deref(), Some("w"));
    }

    #[test]
    fn a_heartbeat_keeps_the_running_executions() {
        let registry = WorkerRegistry::new();
        registered(&registry, "w", 1);
        registry.reserve("w", "exec-1");
        registry.heartbeat("w", true);
        assert_eq!(selected(&registry), None, "the slot is still taken");
        assert_eq!(registry.snapshot()[0].state, "busy");
    }

    #[test]
    fn the_rbe_platform_requires_a_linux_arm64_worker() {
        let requirements =
            WorkerRequirements::from_platform(&props(&[("OSFamily", "linux"), ("ISA", "arm-a64")]))
                .unwrap();
        assert_eq!(requirements.labels, vec!["os=linux", "arch=arm64"]);
    }

    #[test]
    fn no_platform_matches_every_worker() {
        assert_eq!(
            WorkerRequirements::from_platform(&[]).unwrap(),
            WorkerRequirements::default()
        );
    }

    #[test]
    fn unknown_properties_and_isa_values_are_refused() {
        assert!(WorkerRequirements::from_platform(&props(&[("container-image", "x")])).is_err());
        assert!(WorkerRequirements::from_platform(&props(&[("ISA", "rv64g")])).is_err());
        assert!(WorkerRequirements::from_platform(&props(&[("Network", "maybe")])).is_err());
    }

    #[test]
    fn an_extra_property_requires_its_verbatim_label() {
        let matrix = props(&[("OSFamily", "linux"), ("CHROOT", "false")]);
        let extra = ["CHROOT".to_string()];
        let requirements = WorkerRequirements::from_platform_with(&matrix, &extra).unwrap();
        assert_eq!(requirements.labels, vec!["os=linux", "CHROOT=false"]);

        let error = WorkerRequirements::from_platform(&matrix).unwrap_err();
        assert!(error.contains("\"CHROOT\""), "{error}");
        let error =
            WorkerRequirements::from_platform_with(&props(&[("dockerNetwork", "off")]), &extra)
                .unwrap_err();
        assert!(error.ends_with("Network, CHROOT"), "{error}");
    }

    #[test]
    fn the_network_property_reaches_the_worker_without_requiring_a_label() {
        let off = props(&[("OSFamily", "linux"), ("Network", "off")]);
        assert_eq!(
            WorkerRequirements::from_platform(&off).unwrap().labels,
            vec!["os=linux"]
        );
        assert_eq!(NetworkPolicy::from_platform(&off), Ok(NetworkPolicy::Off));
        assert_eq!(
            NetworkPolicy::from_platform(&props(&[("Network", "on")])),
            Ok(NetworkPolicy::On)
        );
        assert_eq!(
            NetworkPolicy::from_platform(&[]),
            Ok(NetworkPolicy::WorkerDefault)
        );
    }
}
