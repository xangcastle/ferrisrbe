

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::proto::ferris::rbe::worker::worker_service_server::{
    WorkerService, WorkerServiceServer,
};
use crate::proto::ferris::rbe::worker::{
    server_message, worker_message, Digest, RegistrationAck, ServerMessage,
    WorkAssignment as ProtoWorkAssignment, WorkerMessage,
};

use crate::execution::engine::ExecutionEngine;
use crate::worker::k8s::{
    ExecutionResult, WorkAssignment, WorkerInfo, WorkerRegistry, WorkerState,
    WorkerStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub struct WorkerServiceImpl {
    worker_registry: Arc<WorkerRegistry>,
    execution_engine: Arc<ExecutionEngine>,

    active_connections: Arc<RwLock<HashMap<String, mpsc::Sender<Result<ServerMessage, Status>>>>>,
}

impl WorkerServiceImpl {
    pub fn new(
        worker_registry: Arc<WorkerRegistry>,
        execution_engine: Arc<ExecutionEngine>,
    ) -> Self {
        Self {
            worker_registry,
            execution_engine,
            active_connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn into_service(self) -> WorkerServiceServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100 * 1024 * 1024);
        WorkerServiceServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }

    #[allow(dead_code)]
    async fn send_assignment(
        &self,
        worker_id: &str,
        assignment: WorkAssignment,
    ) -> Result<(), String> {
        let connections = self.active_connections.read().await;
        if let Some(sender) = connections.get(worker_id) {
            let proto_assignment = convert_assignment(assignment);
            let msg = ServerMessage {
                payload: Some(
                    crate::proto::ferris::rbe::worker::server_message::Payload::Assignment(
                        proto_assignment,
                    ),
                ),
            };
            sender
                .send(Ok(msg))
                .await
                .map_err(|e| format!("Failed to send: {}", e))
        } else {
            Err(format!("Worker {} not connected", worker_id))
        }
    }

    #[allow(dead_code)]
    async fn register_connection(
        &self,
        worker_id: String,
        sender: mpsc::Sender<Result<ServerMessage, Status>>,
    ) {
        let mut connections = self.active_connections.write().await;
        connections.insert(worker_id.clone(), sender);
        info!(
            "Worker {} connected, active connections: {}",
            worker_id,
            connections.len()
        );
    }

    #[allow(dead_code)]
    async fn unregister_connection(&self, worker_id: &str) {
        let mut connections = self.active_connections.write().await;
        connections.remove(worker_id);
        info!(
            "Worker {} disconnected, active connections: {}",
            worker_id,
            connections.len()
        );
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    type StreamWorkStream = ReceiverStream<Result<ServerMessage, Status>>;

    async fn stream_work(
        &self,
        request: Request<Streaming<WorkerMessage>>,
    ) -> Result<Response<Self::StreamWorkStream>, Status> {
        let mut inbound = request.into_inner();

        let (tx, rx) = mpsc::channel::<Result<ServerMessage, Status>>(1024);
        let response_stream = ReceiverStream::new(rx);

        let registry = self.worker_registry.clone();
        let engine = self.execution_engine.clone();
        let connections = self.active_connections.clone();
        let _engine_assignment_tx = engine.assignment_sender();
        let engine_result_tx = engine.result_sender();

        tokio::spawn(async move {
            let mut worker_id: Option<String> = None;

            info!("Starting worker message loop for new connection");
            
            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(worker_message::Payload::Registration(reg)) => {
                        info!(
                            "Worker registration: {} (type: {:?})",
                            reg.worker_id, reg.worker_type
                        );
                        let wid = reg.worker_id.clone();
                        if wid.is_empty() {
                            error!("Rejecting registration: empty worker_id");
                            break;
                        }
                        worker_id = Some(wid.clone());

                        let (assign_tx, mut assign_rx) = mpsc::channel::<WorkAssignment>(100);

                        let worker_info = WorkerInfo {
                            worker_id: wid.clone(),
                            worker_type: reg.worker_type.clone(),
                            labels: reg.labels.clone(),
                            state: WorkerState::Idle,
                            current_execution: None,
                            last_heartbeat: std::time::Instant::now(),
                            assignment_tx: assign_tx.clone(),
                        };
                        registry.register(worker_info);

                        {
                            let mut conns = connections.write().await;
                            conns.insert(wid.clone(), tx.clone());
                        }

                        let tx_clone = tx.clone();
                        tokio::spawn(async move {
                            while let Some(assignment) = assign_rx.recv().await {
                                let proto_assignment = convert_assignment(assignment);
                                let msg = ServerMessage {
                                    payload: Some(server_message::Payload::Assignment(
                                        proto_assignment,
                                    )),
                                };
                                if tx_clone.send(Ok(msg)).await.is_err() {
                                    break;
                                }
                            }
                        });

                        let ack = ServerMessage {
                            payload: Some(server_message::Payload::Ack(RegistrationAck {
                                accepted: true,
                                message: "Worker registered successfully".to_string(),
                                config: Some(crate::proto::ferris::rbe::worker::ServerConfig {
                                    cas_endpoint: std::env::var("CAS_ENDPOINT")
                                        .unwrap_or_else(|_| "bazel-remote:9094".to_string()),
                                    heartbeat_interval_sec: 30,
                                }),
                            })),
                        };
                        if tx.send(Ok(ack)).await.is_err() {
                            error!("Failed to send ACK to worker {}", reg.worker_id);
                            break;
                        }
                    }

                    Some(worker_message::Payload::Heartbeat(hb)) => {
                        debug!(
                            "Heartbeat from worker {}: state={:?}",
                            hb.worker_id, hb.state
                        );

                        let state = match hb.state {
                            0 => WorkerState::Idle,
                            1 => WorkerState::Busy,
                            _ => WorkerState::Unhealthy,
                        };

                        registry.update_status(
                            &hb.worker_id,
                            WorkerStatus {
                                worker_id: hb.worker_id.clone(),
                                state,
                                execution_id: hb.active_executions.first().cloned(),
                                progress: None,
                                result: None,
                            },
                        );
                    }

                    Some(worker_message::Payload::ExecutionUpdate(update)) => {
                        debug!(
                            "Execution update from worker {}: exec={} state={:?} progress={}%",
                            update.worker_id,
                            update.execution_id,
                            update.state(),
                            update.progress_percent
                        );
                    }

                    Some(worker_message::Payload::Result(result)) => {
                        info!(
                            "Execution result from worker {}: exec={} exit_code={} output_files={} output_dirs={}",
                            result.worker_id, result.execution_id, result.exit_code,
                            result.output_files.len(), result.output_directories.len()
                        );

                        let output_files: Vec<crate::worker::k8s::OutputFile> = result
                            .output_files
                            .iter()
                            .map(|f| crate::worker::k8s::OutputFile {
                                path: f.path.clone(),
                                digest: f
                                    .digest
                                    .as_ref()
                                    .map(|d| d.hash.clone())
                                    .unwrap_or_default(),
                                size_bytes: f.digest.as_ref().map(|d| d.size_bytes).unwrap_or(0),
                                is_executable: f.is_executable,
                            })
                            .collect();

                        let internal_result = ExecutionResult {
                            execution_id: result.execution_id.clone(),
                            exit_code: result.exit_code,
                            stdout: result.stdout,
                            stderr: result.stderr,
                            stdout_digest: None,
                            stderr_digest: None,
                            output_files,
                            output_directories: vec![],
                            execution_duration: Duration::from_millis(
                                result.execution_duration_ms as u64,
                            ),
                        };

                        if let Err(e) = engine_result_tx
                            .send((result.worker_id.clone(), internal_result))
                            .await
                        {
                            error!("Failed to send result to engine: {}", e);
                        }
                    }

                    None => {
                        warn!("Received WorkerMessage with no payload");
                    }
                }
            }

            if let Some(wid) = worker_id {
                registry.unregister(&wid);
                {
                    let mut conns = connections.write().await;
                    conns.remove(&wid);
                }
                info!("Worker {} disconnected and unregistered", wid);
            }
        });

        Ok(Response::new(response_stream))
    }
}

fn convert_assignment(internal: WorkAssignment) -> ProtoWorkAssignment {
    let input_root_digest = internal.input_root_digest.and_then(|s| {
        if let Some(pos) = s.rfind('-') {
            let hash = &s[..pos];
            let size_str = &s[pos + 1..];
            if let Ok(size) = size_str.parse::<i64>() {
                return Some(Digest {
                    hash: hash.to_string(),
                    size_bytes: size,
                });
            }
        }
        Some(Digest {
            hash: s,
            size_bytes: 0,
        })
    });
    
    ProtoWorkAssignment {
        execution_id: internal.execution_id,
        action_digest: internal.action_digest.map(|h| Digest {
            hash: h,
            size_bytes: 0,
        }),
        input_root_digest,
        timeout_sec: internal.timeout.as_secs() as i32,
        command: internal.command,
        environment_variables: vec![],
        output_files: internal.output_files,
        output_directories: internal.output_directories,
        working_directory: internal.working_directory.unwrap_or_default(),
    }
}
