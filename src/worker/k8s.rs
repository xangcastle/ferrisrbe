

use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone)]
pub struct K8sWorkerConfig {

    pub server_endpoint: String,

    pub worker_type: String,

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

pub struct K8sWorker {
    config: K8sWorkerConfig,
    worker_id: String,

    assignment_rx: mpsc::Receiver<WorkAssignment>,

    status_tx: mpsc::Sender<WorkerStatus>,
}

#[derive(Debug, Clone)]
pub struct WorkAssignment {
    pub execution_id: String,
    pub action_digest: Option<String>,
    pub input_root_digest: Option<String>,
    pub command: Vec<String>,
    pub timeout: Duration,
    /// Expected output files (relative paths)
    pub output_files: Vec<String>,
    /// Expected output directories (relative paths)
    pub output_directories: Vec<String>,
    /// Working directory relative to execroot
    pub working_directory: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub worker_id: String,
    pub state: WorkerState,
    pub execution_id: Option<String>,
    pub progress: Option<ProgressUpdate>,
    pub result: Option<ExecutionResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Idle,
    Busy,
    Unhealthy,
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub stage: String,
    pub percent_complete: f32,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub execution_id: String,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_digest: Option<String>,
    pub stderr_digest: Option<String>,
    pub output_files: Vec<OutputFile>,
    pub output_directories: Vec<OutputDirectory>,
    pub execution_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct OutputFile {
    pub path: String,
    pub digest: String,
    pub size_bytes: i64,
    pub is_executable: bool,
}

#[derive(Debug, Clone)]
pub struct OutputDirectory {
    pub path: String,
    pub digest: String,
    pub size_bytes: i64,
}

#[derive(Clone)]
pub struct AssignmentSender {
    pub sender: mpsc::Sender<WorkAssignment>,
}

impl K8sWorker {

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

                tokio::time::sleep(Duration::from_secs(30)).await;
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
                execution_duration: start.elapsed(),
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
                    execution_duration: start.elapsed(),
                }
            }
        }
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }
}

pub struct WorkerRegistry {
    workers: dashmap::DashMap<String, WorkerInfo>,
}

#[derive(Debug, Clone)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub worker_type: String,
    pub labels: Vec<String>,
    pub state: WorkerState,
    pub current_execution: Option<String>,
    pub last_heartbeat: Instant,

    pub assignment_tx: mpsc::Sender<WorkAssignment>,
}

#[derive(Clone)]
pub struct SelectedWorker {
    pub worker_id: String,
    pub assignment_tx: mpsc::Sender<WorkAssignment>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self {
            workers: dashmap::DashMap::new(),
        }
    }

    pub fn register(&self, info: WorkerInfo) {
        info!("Registering worker: {}", info.worker_id);
        self.workers.insert(info.worker_id.clone(), info);
    }

    pub fn unregister(&self, worker_id: &str) {
        info!("Unregistering worker: {}", worker_id);
        self.workers.remove(worker_id);
    }

    pub fn select_worker(&self, requirements: &WorkerRequirements) -> Option<SelectedWorker> {
        let total_workers = self.workers.len();
        debug!("Selecting worker from {} total workers", total_workers);

        let idle_workers: Vec<_> = self
            .workers
            .iter()
            .filter(|entry| {
                let info = entry.value();
                let is_match = info.state == WorkerState::Idle && requirements.matches(info);
                debug!(
                    "Worker {}: state={:?}, matches={}",
                    info.worker_id, info.state, is_match
                );
                is_match
            })
            .collect();

        debug!("Found {} idle matching workers", idle_workers.len());

        idle_workers
            .into_iter()
            .min_by_key(|entry| entry.value().last_heartbeat)
            .map(|entry| {
                let info = entry.value();
                info!("Selected worker {} for assignment", info.worker_id);
                SelectedWorker {
                    worker_id: info.worker_id.clone(),
                    assignment_tx: info.assignment_tx.clone(),
                }
            })
    }

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

    pub fn update_status(&self, worker_id: &str, status: WorkerStatus) {
        if let Some(mut entry) = self.workers.get_mut(worker_id) {
            entry.state = status.state;
            entry.current_execution = status.execution_id;
            entry.last_heartbeat = Instant::now();
        }
    }

    pub fn list_workers(&self) -> Vec<WorkerInfo> {
        self.workers
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn stats(&self) -> WorkerStats {
        let mut idle = 0;
        let mut busy = 0;
        let mut unhealthy = 0;

        for entry in self.workers.iter() {
            match entry.value().state {
                WorkerState::Idle => idle += 1,
                WorkerState::Busy => busy += 1,
                WorkerState::Unhealthy => unhealthy += 1,
            }
        }

        WorkerStats {
            total: idle + busy + unhealthy,
            idle,
            busy,
            unhealthy,
        }
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct WorkerRequirements {
    pub worker_type: Option<String>,
    pub labels: Vec<String>,
}

impl WorkerRequirements {
    pub fn matches(&self, info: &WorkerInfo) -> bool {

        if let Some(ref req_type) = self.worker_type {
            if &info.worker_type != req_type {
                return false;
            }
        }

        for label in &self.labels {
            if !info.labels.contains(label) {
                return false;
            }
        }

        true
    }
}

#[derive(Debug, Clone)]
pub struct WorkerStats {
    pub total: usize,
    pub idle: usize,
    pub busy: usize,
    pub unhealthy: usize,
}

impl WorkerStats {

    pub fn queue_depth(&self) -> usize {

        self.busy.saturating_sub(self.idle)
    }
}
