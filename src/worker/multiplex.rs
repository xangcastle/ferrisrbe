//! Bazel multiplex persistent workers over the JSON worker protocol.
//!
//! A [`PersistentWorker`] keeps one worker process alive and multiplexes
//! many concurrent [`WorkRequest`]s over its stdin and stdout, one JSON
//! message per line, matching each [`WorkResponse`] to its request by
//! `request_id`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
#[allow(unused_imports)]
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

/// Deadline for a worker to answer one request.
const RESPONSE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(60);

/// Requests buffered for the worker task before callers wait.
const REQUEST_QUEUE_DEPTH: usize = 100;

/// A request in Bazel's JSON worker protocol. `request_id` is assigned by
/// [`PersistentWorker::execute`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRequest {
    pub arguments: Vec<String>,
    #[serde(with = "serde_bytes", default)]
    pub stdin: Vec<u8>,
    pub request_id: u32,
    #[serde(default)]
    pub verbosity: String,
    #[serde(default)]
    pub sandbox_dir: Vec<String>,
}

/// A response in Bazel's JSON worker protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResponse {
    pub exit_code: i32,
    pub output: String,
    pub request_id: u32,
}

/// A request paired with the channel its response is delivered on.
struct InternalRequest {
    request: WorkRequest,
    response_tx: oneshot::Sender<WorkResponse>,
}

/// How to launch workers of one type.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub command: Vec<String>,
    pub max_concurrent_requests: usize,
    pub worker_type: String,
}

/// One live worker process, owned by a single task.
///
/// Callers never touch the process: requests reach the owning task over a
/// channel, and the task alternates between writing requests to stdin and
/// reading responses from stdout, routing each response to the waiting
/// caller by `request_id`. No lock is held across I/O, so a slow response
/// never blocks other requests from being sent. When stdout closes, every
/// pending request is answered with exit code `1` and the process is killed.
pub struct PersistentWorker {
    request_tx: mpsc::Sender<InternalRequest>,
    request_counter: AtomicU32,
    _process_handle: tokio::task::JoinHandle<()>,
}

/// Writes `json` and a newline to the worker and flushes.
async fn write_request_line(stdin: &mut ChildStdin, json: &str) -> std::io::Result<()> {
    stdin.write_all(json.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

impl PersistentWorker {
    /// Launches `config.command` and starts the task that owns it.
    ///
    /// # Errors
    ///
    /// When the process cannot be spawned or its pipes are unavailable.
    pub async fn spawn(config: WorkerConfig) -> anyhow::Result<Self> {
        let mut cmd = Command::new(&config.command[0]);
        cmd.args(&config.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        info!("Spawning multiplex worker: {:?}", config.command);

        let mut process = cmd.spawn()?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stdin"))?;

        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stdout"))?;

        let (request_tx, mut request_rx) = mpsc::channel::<InternalRequest>(REQUEST_QUEUE_DEPTH);

        let worker_type = config.worker_type.clone();

        let handle = tokio::spawn(async move {
            let mut stdin = stdin;
            let mut stdout = BufReader::new(stdout);
            let mut pending_responses: HashMap<u32, oneshot::Sender<WorkResponse>> = HashMap::new();
            let mut read_buf = String::new();

            loop {
                tokio::select! {
                    Some(internal_req) = request_rx.recv() => {
                        let json = match serde_json::to_string(&internal_req.request) {
                            Ok(j) => j,
                            Err(e) => {
                                error!("Failed to serialize request: {}", e);
                                let _ = internal_req.response_tx.send(WorkResponse {
                                    exit_code: 1,
                                    output: format!("Serialization error: {}", e),
                                    request_id: internal_req.request.request_id,
                                });
                                continue;
                            }
                        };

                        pending_responses.insert(internal_req.request.request_id, internal_req.response_tx);

                        if let Err(e) = write_request_line(&mut stdin, &json).await {
                            error!("Failed to write request to worker stdin: {}", e);
                            pending_responses.remove(&internal_req.request.request_id);
                            continue;
                        }

                        debug!("Sent request {} to {} worker", internal_req.request.request_id, worker_type);
                    }

                    result = stdout.read_line(&mut read_buf) => {
                        match result {
                            Ok(0) => {
                                info!("Worker {} stdout closed", worker_type);
                                break;
                            }
                            Ok(_) => {
                                let line = std::mem::take(&mut read_buf);
                                match serde_json::from_str::<WorkResponse>(&line) {
                                    Ok(response) => {
                                        debug!("Worker {} response: {:?}", worker_type, response);
                                        if let Some(tx) = pending_responses.remove(&response.request_id) {
                                            let _ = tx.send(response);
                                        } else {
                                            warn!("Received response for unknown request_id: {}", response.request_id);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to parse worker response: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Error reading from worker stdout: {}", e);
                                break;
                            }
                        }
                    }
                }
            }

            for (request_id, tx) in pending_responses {
                let _ = tx.send(WorkResponse {
                    exit_code: 1,
                    output: "Worker process died".to_string(),
                    request_id,
                });
            }

            let _ = process.kill().await;
            info!("Worker {} task terminated", worker_type);
        });

        Ok(Self {
            request_tx,
            request_counter: AtomicU32::new(0),
            _process_handle: handle,
        })
    }

    /// Sends `request` under a fresh `request_id` and waits for its response.
    ///
    /// # Errors
    ///
    /// When the worker task has exited, or no response arrives within 60
    /// seconds.
    pub async fn execute(&self, request: WorkRequest) -> anyhow::Result<WorkResponse> {
        let request_id = self.request_counter.fetch_add(1, Ordering::Relaxed);
        let request = WorkRequest {
            request_id,
            ..request
        };

        let (response_tx, response_rx) = oneshot::channel();

        let internal_req = InternalRequest {
            request,
            response_tx,
        };

        self.request_tx
            .send(internal_req)
            .await
            .map_err(|_| anyhow::anyhow!("Worker task died"))?;

        let response = tokio::time::timeout(RESPONSE_TIMEOUT, response_rx)
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for response"))?
            .map_err(|_| anyhow::anyhow!("Response channel closed"))?;

        Ok(response)
    }
}

/// Pools of [`PersistentWorker`]s by worker type.
pub struct MultiplexWorkerManager {
    workers: Arc<Mutex<HashMap<String, Vec<Arc<PersistentWorker>>>>>,
    configs: Arc<Mutex<HashMap<String, WorkerConfig>>>,
}

impl Default for MultiplexWorkerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiplexWorkerManager {
    pub fn new() -> Self {
        Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
            configs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn register_worker_type(&self, worker_type: &str, config: WorkerConfig) {
        let mut configs = self.configs.lock().await;
        configs.insert(worker_type.to_string(), config);
        info!("Registered multiplex worker type: {}", worker_type);
    }

    /// Starts `count` more workers of a registered type.
    ///
    /// # Errors
    ///
    /// When the type is not registered or a worker fails to spawn.
    pub async fn spawn_workers(&self, worker_type: &str, count: usize) -> anyhow::Result<()> {
        let configs = self.configs.lock().await;
        let config = configs
            .get(worker_type)
            .ok_or_else(|| anyhow::anyhow!("Unknown worker type: {}", worker_type))?
            .clone();
        drop(configs);

        let mut workers = self.workers.lock().await;
        let entry = workers
            .entry(worker_type.to_string())
            .or_insert_with(Vec::new);

        for i in 0..count {
            let worker = PersistentWorker::spawn(config.clone()).await?;
            entry.push(Arc::new(worker));
            info!("Spawned {} worker {}/{}", worker_type, i + 1, count);
        }

        Ok(())
    }

    /// Runs `request` on a worker of `worker_type`. Every request of a type
    /// goes to its first worker, which multiplexes them.
    pub async fn execute(
        &self,
        worker_type: &str,
        request: WorkRequest,
    ) -> anyhow::Result<WorkResponse> {
        let workers = self.workers.lock().await;
        let worker_list = workers
            .get(worker_type)
            .ok_or_else(|| anyhow::anyhow!("No workers for type: {}", worker_type))?;

        let worker = worker_list
            .first()
            .ok_or_else(|| anyhow::anyhow!("No available workers"))?
            .clone();
        drop(workers);

        worker.execute(request).await
    }

    /// Number of workers per type.
    pub async fn stats(&self) -> HashMap<String, usize> {
        let workers = self.workers.lock().await;
        workers.iter().map(|(k, v)| (k.clone(), v.len())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_manager_has_no_workers() {
        let manager = MultiplexWorkerManager::new();
        assert!(manager.stats().await.is_empty());
    }
}
