//! Worker control plane: the bidirectional `StreamWork` RPC that remote
//! workers hold open to receive assignments and report back.
//!
//! A worker registers first; from then on the server pushes work assignments
//! down the stream while the worker sends heartbeats, progress and results
//! up it. When the stream ends the worker is unregistered and the actions it
//! was running go back to the queue.

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::proto::ferris::rbe::worker::worker_service_server::{
    WorkerService, WorkerServiceServer,
};
use crate::proto::ferris::rbe::worker::{
    server_message, worker_heartbeat, worker_message, Digest, EnvironmentVariable,
    ExecutionResult as ProtoExecutionResult, NetworkAccess, RegistrationAck, ServerConfig,
    ServerMessage, WorkAssignment as ProtoWorkAssignment, WorkerHeartbeat, WorkerMessage,
    WorkerRegistration,
};

use crate::execution::engine::{ExecutionEngine, WorkerReport};
use crate::types::DigestInfo;
use crate::worker::k8s::{
    ExecutionResult, InputStats, NetworkPolicy, OutputDirectory, OutputFile, OutputSymlink,
    PhaseTimestamps, WorkAssignment, WorkerInfo, WorkerRegistry, WorkerState,
};
use crate::worker::materializer::InputGroup;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Server messages buffered per worker stream.
const OUTBOUND_CHANNEL_DEPTH: usize = 1024;

/// Assignments buffered per worker before the scheduler blocks on it.
const ASSIGNMENT_CHANNEL_DEPTH: usize = 100;

/// Heartbeat interval requested from workers at registration.
const HEARTBEAT_INTERVAL_SECS: i32 = 30;

/// CAS address handed to workers when `CAS_ENDPOINT` is unset.
const DEFAULT_CAS_ENDPOINT: &str = "rbe-cache:9094";

/// Default gRPC message limit in both directions, overridable with
/// `RBE_MAX_GRPC_MSG_SIZE` (bytes).
const DEFAULT_MAX_GRPC_MSG_SIZE: usize = 100 * 1024 * 1024;

type Outbound = mpsc::Sender<Result<ServerMessage, Status>>;

/// `WorkerService` feeding the [`WorkerRegistry`] and the [`ExecutionEngine`].
pub struct WorkerServiceImpl {
    worker_registry: Arc<WorkerRegistry>,
    execution_engine: Arc<ExecutionEngine>,
}

impl WorkerServiceImpl {
    pub fn new(
        worker_registry: Arc<WorkerRegistry>,
        execution_engine: Arc<ExecutionEngine>,
    ) -> Self {
        Self {
            worker_registry,
            execution_engine,
        }
    }

    /// The tonic server for this service, with message limits of
    /// [`DEFAULT_MAX_GRPC_MSG_SIZE`] unless `RBE_MAX_GRPC_MSG_SIZE` is set.
    pub fn into_service(self) -> WorkerServiceServer<Self> {
        let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_GRPC_MSG_SIZE);
        WorkerServiceServer::new(self)
            .max_decoding_message_size(max_msg_size)
            .max_encoding_message_size(max_msg_size)
    }
}

/// Registers the worker, starts forwarding its assignments down `outbound`
/// and acknowledges the registration. Returns the worker id, or `None` when
/// the registration is rejected or the stream is already closed.
async fn register(
    registration: WorkerRegistration,
    registry: &WorkerRegistry,
    outbound: &Outbound,
) -> Option<String> {
    info!(
        "Worker registration: {} (type: {:?})",
        registration.worker_id, registration.worker_type
    );
    let worker_id = registration.worker_id.clone();
    if worker_id.is_empty() {
        error!("Rejecting registration: empty worker_id");
        return None;
    }

    let (assignment_tx, mut assignment_rx) =
        mpsc::channel::<WorkAssignment>(ASSIGNMENT_CHANNEL_DEPTH);
    registry.register(WorkerInfo {
        worker_id: worker_id.clone(),
        worker_type: registration.worker_type.clone(),
        labels: registration.labels.clone(),
        state: WorkerState::Idle,
        capacity: registration
            .capabilities
            .as_ref()
            .and_then(|c| usize::try_from(c.max_concurrent).ok())
            .filter(|&n| n > 0)
            .unwrap_or(1),
        running: vec![],
        last_heartbeat: Instant::now(),
        assignment_tx,
    });

    let forward = outbound.clone();
    tokio::spawn(async move {
        while let Some(assignment) = assignment_rx.recv().await {
            let msg = ServerMessage {
                payload: Some(server_message::Payload::Assignment(convert_assignment(
                    assignment,
                ))),
            };
            if forward.send(Ok(msg)).await.is_err() {
                break;
            }
        }
    });

    let ack = ServerMessage {
        payload: Some(server_message::Payload::Ack(RegistrationAck {
            accepted: true,
            message: "Worker registered successfully".to_string(),
            config: Some(ServerConfig {
                cas_endpoint: std::env::var("CAS_ENDPOINT")
                    .unwrap_or_else(|_| DEFAULT_CAS_ENDPOINT.to_string()),
                heartbeat_interval_sec: HEARTBEAT_INTERVAL_SECS,
            }),
        })),
    };
    if outbound.send(Ok(ack)).await.is_err() {
        error!("Failed to send ACK to worker {}", worker_id);
        return None;
    }
    Some(worker_id)
}

/// Records a heartbeat and returns whether its worker is still registered.
fn record_heartbeat(heartbeat: WorkerHeartbeat, registry: &WorkerRegistry) -> bool {
    debug!(
        "Heartbeat from worker {}: state={:?}",
        heartbeat.worker_id, heartbeat.state
    );
    if heartbeat.state == worker_heartbeat::State::Draining as i32 {
        info!("Worker {} is draining", heartbeat.worker_id);
        registry.mark_draining(&heartbeat.worker_id);
        return registry.contains(&heartbeat.worker_id);
    }
    let healthy = heartbeat.state != worker_heartbeat::State::Unhealthy as i32;
    registry.heartbeat(&heartbeat.worker_id, healthy)
}

/// The engine's form of a worker-reported result; stdout and stderr arrive
/// inline or, when large, as the digest the worker stored them under. A
/// non-empty `error` marks an action that could not run.
fn internal_result(result: ProtoExecutionResult) -> ExecutionResult {
    let stored = |digest: &Option<Digest>| {
        digest
            .as_ref()
            .and_then(|d| DigestInfo::new(&d.hash, d.size_bytes).ok())
    };
    let stdout_digest = stored(&result.stdout_digest);
    let stderr_digest = stored(&result.stderr_digest);
    let output_files = result
        .output_files
        .iter()
        .map(|f| OutputFile {
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
    let output_directories = result
        .output_directories
        .iter()
        .map(|d| OutputDirectory {
            path: d.path.clone(),
            digest: d
                .tree_digest
                .as_ref()
                .map(|t| t.hash.clone())
                .unwrap_or_default(),
            size_bytes: d.tree_digest.as_ref().map(|t| t.size_bytes).unwrap_or(0),
        })
        .collect();
    let output_symlinks = result
        .output_symlinks
        .iter()
        .map(|s| OutputSymlink {
            path: s.path.clone(),
            target: s.target.clone(),
        })
        .collect();

    ExecutionResult {
        execution_id: result.execution_id,
        exit_code: result.exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
        stdout_digest,
        stderr_digest,
        output_files,
        output_directories,
        output_symlinks,
        execution_duration: Duration::from_millis(result.execution_duration_ms as u64),
        error: Some(result.error).filter(|e| !e.is_empty()),
        timed_out: result.timed_out,
        worker_id: result.worker_id,
        timestamps: result
            .timestamps
            .map(|t| PhaseTimestamps {
                worker_start_ms: t.worker_start_ms,
                input_fetch_start_ms: t.input_fetch_start_ms,
                input_fetch_completed_ms: t.input_fetch_completed_ms,
                execution_start_ms: t.execution_start_ms,
                execution_completed_ms: t.execution_completed_ms,
                output_upload_start_ms: t.output_upload_start_ms,
                output_upload_completed_ms: t.output_upload_completed_ms,
                worker_completed_ms: t.worker_completed_ms,
            })
            .unwrap_or_default(),
        input_stats: result.input_stats.map(|s| InputStats {
            files: s.files,
            bytes: s.bytes,
            groups: s
                .groups
                .into_iter()
                .map(|g| InputGroup {
                    name: g.name,
                    files: g.files,
                    bytes: g.bytes,
                })
                .collect(),
        }),
    }
}

#[tonic::async_trait]
impl WorkerService for WorkerServiceImpl {
    type StreamWorkStream = ReceiverStream<Result<ServerMessage, Status>>;

    /// Serves one worker for the lifetime of its stream. Messages before
    /// registration are processed, but the worker is only tracked (and
    /// unregistered on disconnect) once it has registered successfully.
    ///
    /// A registered worker the server dropped meanwhile (the heartbeat
    /// reaper does, after a silence) is told `UNAVAILABLE` on its next
    /// heartbeat and its stream ends, so it reconnects and registers again
    /// instead of staying connected without ever getting work.
    async fn stream_work(
        &self,
        request: Request<Streaming<WorkerMessage>>,
    ) -> Result<Response<Self::StreamWorkStream>, Status> {
        let mut inbound = request.into_inner();
        let (outbound, rx) = mpsc::channel::<Result<ServerMessage, Status>>(OUTBOUND_CHANNEL_DEPTH);

        let registry = self.worker_registry.clone();
        let engine = self.execution_engine.clone();
        let result_tx = engine.result_sender();

        tokio::spawn(async move {
            let mut worker_id: Option<String> = None;
            info!("Starting worker message loop for new connection");

            while let Ok(Some(msg)) = inbound.message().await {
                match msg.payload {
                    Some(worker_message::Payload::Registration(registration)) => {
                        match register(registration, &registry, &outbound).await {
                            Some(id) => worker_id = Some(id),
                            None => break,
                        }
                    }
                    Some(worker_message::Payload::Heartbeat(heartbeat)) => {
                        if !record_heartbeat(heartbeat, &registry) && worker_id.is_some() {
                            let dropped = worker_id.take().unwrap_or_default();
                            warn!(
                                "Worker {} was dropped by the server; closing its stream so it registers again",
                                dropped
                            );
                            let _ = outbound
                                .send(Err(Status::unavailable(
                                    "the server dropped this worker after a silence; register again",
                                )))
                                .await;
                            break;
                        }
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
                        let reporter = result.worker_id.clone();
                        let report =
                            WorkerReport::Result(reporter, Box::new(internal_result(result)));
                        if let Err(e) = result_tx.send(report).await {
                            error!("Failed to send result to engine: {}", e);
                        }
                    }
                    None => warn!("Received WorkerMessage with no payload"),
                }
            }

            if let Some(id) = worker_id {
                registry.unregister(&id);
                if result_tx
                    .send(WorkerReport::Lost(id.clone()))
                    .await
                    .is_err()
                {
                    engine.worker_lost(&id).await;
                }
                info!("Worker {} disconnected and unregistered", id);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// The wire form of an assignment. The input root travels internally as
/// `<hash>-<size>`; a value without a parsable size is sent with size `0`,
/// as is the action digest, whose size is not tracked.
fn convert_assignment(internal: WorkAssignment) -> ProtoWorkAssignment {
    let input_root_digest = internal.input_root_digest.map(|s| {
        let parsed = s
            .rsplit_once('-')
            .and_then(|(hash, size)| Some((hash.to_string(), size.parse::<i64>().ok()?)));
        let (hash, size_bytes) = parsed.unwrap_or((s, 0));
        Digest { hash, size_bytes }
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
        environment_variables: internal
            .environment_variables
            .into_iter()
            .map(|(name, value)| EnvironmentVariable { name, value })
            .collect(),
        output_files: internal.output_files,
        output_directories: internal.output_directories,
        working_directory: internal.working_directory.unwrap_or_default(),
        network: match internal.network {
            NetworkPolicy::WorkerDefault => NetworkAccess::NetworkWorkerDefault,
            NetworkPolicy::Off => NetworkAccess::NetworkOff,
            NetworkPolicy::On => NetworkAccess::NetworkOn,
        } as i32,
    }
}
