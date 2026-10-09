//! FerrisRBE execution worker.
//!
//! Holds a bidirectional gRPC stream to the scheduler, registers its
//! capabilities, and executes up to `MAX_CONCURRENT` [`WorkAssignment`]s at
//! once, each in its own directory
//! under `WORKDIR`: the input root is materialized from the CAS, the command
//! runs in the action's working directory with exactly the action's
//! environment, bounded by its timeout and cancellable by the server, and its
//! outputs are uploaded back to the CAS whatever the exit code. The worker's
//! container is the isolation boundary: any program, including system tools
//! such as `/bin/bash`, may run. A lost stream is re-established with jittered
//! exponential backoff (see [`resilient_connection`]).

use rbe_server::cas::backends::{DiskBackend, GrpcCasBackend};
use rbe_server::cas::CasBackend;
use rbe_server::execution::output_handler::{inline_output_threshold, max_capture_size};
use rbe_server::grpc_credentials::BearerAuth;
use rbe_server::types::DigestInfo;
use rbe_server::worker::materializer::{MaterializedRoot, Materializer, MaterializerConfig};
use rbe_server::worker::output_uploader::{OutputUploader, UploaderConfig};
use rbe_server::worker::process::{self, Captured, ProcessSpec, Termination, DEFAULT_PATH};

use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

pub mod proto {
    pub mod ferris {
        pub mod rbe {
            pub mod worker {
                tonic::include_proto!("ferris.rbe.worker");
            }
        }
    }
}

use proto::ferris::rbe::worker::worker_service_client::WorkerServiceClient;
use proto::ferris::rbe::worker::{
    server_message, worker_heartbeat, worker_message, Digest,
    ExecutionResult as ProtoExecutionResult, InputGroup as ProtoInputGroup,
    InputStats as ProtoInputStats, NetworkAccess, OutputDirectory, OutputFile, OutputSymlink,
    PhaseTimestamps, ServerMessage, WorkAssignment, WorkerCapabilities, WorkerHeartbeat,
    WorkerMessage, WorkerRegistration,
};

mod resilient_connection;

use resilient_connection::{
    ConfigLoader, ConnectionConfig, ConnectionEvent, ConnectionManager, ConnectionState,
};

/// The input counts of a materialized root as the server receives them.
fn proto_input_stats(root: &MaterializedRoot) -> ProtoInputStats {
    ProtoInputStats {
        files: root.file_count as u64,
        bytes: root.input_bytes,
        groups: root
            .input_groups
            .iter()
            .map(|g| ProtoInputGroup {
                name: g.name.clone(),
                files: g.files,
                bytes: g.bytes,
            })
            .collect(),
    }
}

/// Timeout of an assignment that carries none.
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Interval of the connection statistics log line.
const STATS_INTERVAL: Duration = Duration::from_secs(60);

/// Default of `DRAIN_TIMEOUT_SECS`: under the two-minute EC2 spot
/// interruption notice, with room for the pod's grace period.
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(90);

/// Default of `RBE_WORKER_CACHE_MAX_GB`, the size budget of the local blob
/// cache in GiB.
const DEFAULT_CACHE_MAX_GB: u64 = 100;

/// Directory of the local blob cache under `WORKDIR`.
const BLOB_CACHE_DIR: &str = "blob-cache";

/// The unbounded cache of earlier workers, in another layout; removed at
/// startup so it never counts against the volume.
const LEGACY_CACHE_DIR: &str = "cas-cache";

/// Worker settings, read from the environment once at startup.
struct ResilientWorkerConfig {
    /// `WORKER_ID`, else `HOSTNAME`, else a random `worker-<uuid>`.
    worker_id: String,
    /// `SERVER_ENDPOINT`: the scheduler's gRPC address.
    server_endpoint: String,
    /// `WORKER_TYPE`, reported at registration.
    worker_type: String,
    /// `CAS_ENDPOINT`: the CAS inputs are read from and outputs written to.
    cas_endpoint: String,
    /// `WORKER_LABELS`, comma-separated `key=value` pairs; by default the
    /// [`host_labels`] of the machine the worker runs on.
    labels: Vec<String>,
    /// `MAX_CONCURRENT`: executions the worker accepts at once.
    max_concurrent: usize,
    /// `WORKDIR`: root of the CAS cache and of every execution directory.
    workdir: String,
    /// `RBE_WORKER_TOKEN`: bearer token presented to the server and the CAS,
    /// which a server with gRPC authentication requires; none when unset.
    auth: BearerAuth,
    connection_config: ConnectionConfig,
    /// `DRAIN_TIMEOUT_SECS`: how long a SIGTERM waits for the assignments
    /// the worker holds before it disconnects and the server requeues them.
    drain_timeout: Duration,
    /// `RBE_WORKER_CACHE_MAX_GB`: size budget of the local blob cache, in
    /// GiB; least recently used blobs are evicted past it. `0` is unbounded.
    cache_max_gb: u64,
    /// `RBE_WORKER_NETWORK`: `on` (the default) or `off`, the network access
    /// of an action whose platform sets no `Network` property.
    network_by_default: bool,
}

impl ResilientWorkerConfig {
    fn from_env() -> anyhow::Result<Self> {
        let worker_id = std::env::var("WORKER_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("worker-{}", uuid::Uuid::new_v4()));

        let connection_config = ConfigLoader::load();
        if std::env::var("RBE_PRINT_CONFIG_OPTIONS").is_ok() {
            ConfigLoader::print_available_options();
        }

        let auth = BearerAuth::from_env("RBE_WORKER_TOKEN")
            .map_err(|e| anyhow::anyhow!("RBE_WORKER_TOKEN: {e}"))?;
        let network_by_default = match std::env::var("RBE_WORKER_NETWORK").as_deref() {
            Err(_) | Ok("on") => true,
            Ok("off") => false,
            Ok(other) => anyhow::bail!("RBE_WORKER_NETWORK must be on or off, not {other:?}"),
        };

        Ok(Self {
            worker_id,
            server_endpoint: std::env::var("SERVER_ENDPOINT")
                .unwrap_or_else(|_| "http://rbe-server:9092".to_string()),
            worker_type: std::env::var("WORKER_TYPE").unwrap_or_else(|_| "default".to_string()),
            cas_endpoint: std::env::var("CAS_ENDPOINT")
                .unwrap_or_else(|_| "http://rbe-cache:9094".to_string()),
            labels: std::env::var("WORKER_LABELS")
                .map(|s| s.split(',').map(|s| s.to_string()).collect())
                .unwrap_or_else(|_| host_labels(std::env::consts::OS, std::env::consts::ARCH)),
            max_concurrent: std::env::var("MAX_CONCURRENT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4),
            workdir: std::env::var("WORKDIR").unwrap_or_else(|_| "/workspace".to_string()),
            auth,
            connection_config,
            drain_timeout: std::env::var("DRAIN_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(Duration::from_secs)
                .unwrap_or(DEFAULT_DRAIN_TIMEOUT),
            cache_max_gb: std::env::var("RBE_WORKER_CACHE_MAX_GB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_CACHE_MAX_GB),
            network_by_default,
        })
    }
}

struct ResilientRbeWorker {
    config: ResilientWorkerConfig,
    connection_manager: Arc<ConnectionManager>,
    materializer: Arc<Materializer>,
    output_uploader: Arc<OutputUploader>,
    /// Set on SIGTERM; heartbeats then report `DRAINING`.
    draining: Arc<AtomicBool>,
}

impl ResilientRbeWorker {
    async fn new(config: ResilientWorkerConfig) -> anyhow::Result<Self> {
        let connection_manager = Arc::new(ConnectionManager::new(config.connection_config.clone()));

        let workdir = PathBuf::from(&config.workdir);
        remove_legacy_cache(workdir.join(LEGACY_CACHE_DIR));
        let blob_cache = Arc::new(
            DiskBackend::with_max_size(workdir.join(BLOB_CACHE_DIR), config.cache_max_gb)
                .await?
                .without_fsync(),
        );
        warm_up_blob_cache(blob_cache.clone());

        let cas_backend: Arc<dyn CasBackend> =
            Arc::new(GrpcCasBackend::new(&config.cas_endpoint, config.auth.clone()).await?);
        let materializer = Arc::new(Materializer::new(
            cas_backend.clone(),
            blob_cache,
            MaterializerConfig::default(),
        ));
        let output_uploader = Arc::new(OutputUploader::new(cas_backend, UploaderConfig::default()));

        Ok(Self {
            config,
            connection_manager,
            materializer,
            output_uploader,
            draining: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Serves the scheduler until `max_reconnect_attempts` consecutive
    /// connections fail, or until a SIGTERM drains it.
    async fn run(&mut self) -> anyhow::Result<()> {
        let mut sigterm = signal(SignalKind::terminate())?;
        info!("╔═══════════════════════════════════════════════════════════╗");
        info!("║     Resilient RBE Worker - Enterprise Connection Mgmt     ║");
        info!("╠═══════════════════════════════════════════════════════════╣");
        info!("║  Features:                                                ║");
        info!("║    ✓ Adaptive keepalive (network-aware)                   ║");
        info!("║    ✓ Auto environment detection                           ║");
        info!("║    ✓ Exponential backoff with jitter                      ║");
        info!("║    ✓ Zero-downtime reconnection                           ║");
        info!("╚═══════════════════════════════════════════════════════════╝");
        info!("");
        info!("Worker ID: {}", self.config.worker_id);
        info!("Server: {}", self.config.server_endpoint);
        info!("CAS: {}", self.config.cas_endpoint);
        info!(
            "Network by default: {}",
            if self.config.network_by_default {
                "on"
            } else {
                "off"
            }
        );

        let _event_monitor = self.start_event_monitor();

        loop {
            match self.connect_and_work(&mut sigterm).await {
                Ok(_) => info!("Work loop completed normally"),
                Err(e) => error!("Work loop error: {}", e),
            }

            if self.draining.load(Ordering::Acquire) {
                info!("Drained, shutting down");
                return Ok(());
            }

            if self.connection_manager.is_max_reconnect_exceeded().await {
                error!("Max reconnection attempts exceeded, shutting down");
                return Err(anyhow::anyhow!("Max reconnection attempts exceeded"));
            }

            let delay = self.connection_manager.begin_reconnect_attempt().await;
            info!("Reconnecting in {:?}...", delay);
            tokio::time::sleep(delay).await;
        }
    }

    /// One connection: opens the stream, registers, then serves assignments
    /// until the stream ends or fails.
    ///
    /// On SIGTERM the worker reports `DRAINING`, so the server stops
    /// assigning to it, and keeps running what it holds. It disconnects once
    /// that is done or after `drain_timeout`; the server requeues whatever is
    /// left when the stream drops.
    async fn connect_and_work(&mut self, sigterm: &mut Signal) -> anyhow::Result<()> {
        self.transition(ConnectionState::Connecting, "Starting connection")
            .await;

        let channel = self.create_channel().await.inspect_err(|e| {
            error!("Channel creation failed: {:?}", e);
        })?;

        self.transition(
            ConnectionState::Handshaking,
            "Connected, starting handshake",
        )
        .await;

        let mut client =
            WorkerServiceClient::with_interceptor(channel.clone(), self.config.auth.clone());
        let (tx, rx) = mpsc::channel(100);
        let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
        let mut inbound = client.stream_work(outbound).await?.into_inner();

        tx.send(self.registration()).await?;
        info!("Sent registration to server");

        self.connection_manager.record_connected(channel).await;

        let heartbeat_handle = self.start_adaptive_heartbeat(tx.clone());
        let metrics_handle = self.start_metrics_reporter();
        let this = &*self;
        let capacity = this.config.max_concurrent.max(1);
        let mut running: FuturesUnordered<Execution<'_>> = FuturesUnordered::new();
        let mut cancels: HashMap<String, oneshot::Sender<()>> = HashMap::new();
        let mut pending: VecDeque<WorkAssignment> = VecDeque::new();
        let mut drain_deadline: Option<tokio::time::Instant> = None;

        loop {
            while running.len() < capacity {
                let Some(assignment) = pending.pop_front() else {
                    break;
                };
                let (cancel_tx, cancel_rx) = oneshot::channel();
                cancels.insert(assignment.execution_id.clone(), cancel_tx);
                running.push(Box::pin(this.execute_assignment(assignment, cancel_rx)));
            }

            tokio::select! {
                msg = inbound.message() => {
                    match msg {
                        Ok(Some(server_msg)) => match this.handle_server_message(server_msg) {
                            Some(ServerEvent::Assignment(assignment)) => pending.push_back(assignment),
                            Some(ServerEvent::Cancel(cancelled)) => match cancels.remove(&cancelled) {
                                Some(cancel) => {
                                    let _ = cancel.send(());
                                }
                                None => debug!("Cancel for {} matches no running assignment", cancelled),
                            },
                            None => {}
                        },
                        Ok(None) => {
                            info!("Server closed connection gracefully");
                            this.connection_manager
                                .record_disconnected("Server closed connection".to_string())
                                .await;
                            break;
                        }
                        Err(e) => {
                            this.log_stream_error(&e);
                            this.connection_manager
                                .record_disconnected(format!("Receive error: {}", e))
                                .await;
                            break;
                        }
                    }
                }

                Some(result) = running.next(), if !running.is_empty() => {
                    cancels.remove(&result.execution_id);
                    let result_msg = WorkerMessage {
                        payload: Some(worker_message::Payload::Result(result)),
                    };
                    if tx.send(result_msg).await.is_err() {
                        error!("Failed to send result to server");
                        break;
                    }
                }

                _ = sigterm.recv(), if drain_deadline.is_none() => {
                    info!(
                        "SIGTERM: draining {} running and {} queued assignments for up to {:?}",
                        running.len(),
                        pending.len(),
                        this.config.drain_timeout
                    );
                    this.draining.store(true, Ordering::Release);
                    drain_deadline = Some(tokio::time::Instant::now() + this.config.drain_timeout);
                    if tx.send(this.heartbeat()).await.is_err() {
                        break;
                    }
                }

                _ = sleep_until_deadline(drain_deadline), if drain_deadline.is_some() => {
                    warn!(
                        "Drain timed out with {} running and {} queued assignments; the server requeues them",
                        running.len(),
                        pending.len()
                    );
                    break;
                }

                _ = tokio::time::sleep(STATS_INTERVAL) => {
                    this.log_connection_stats().await;
                }
            }

            if drain_deadline.is_some() && running.is_empty() && pending.is_empty() {
                break;
            }
        }

        heartbeat_handle.abort();
        metrics_handle.abort();
        if drain_deadline.is_some() {
            drop(tx);
            wait_for_server_close(&mut inbound).await;
        }
        Ok(())
    }

    async fn transition(&self, state: ConnectionState, reason: &str) {
        self.connection_manager
            .state_machine()
            .write()
            .await
            .transition_to(state, Some(reason.to_string()));
    }

    fn registration(&self) -> WorkerMessage {
        WorkerMessage {
            payload: Some(worker_message::Payload::Registration(WorkerRegistration {
                worker_id: self.config.worker_id.clone(),
                worker_type: self.config.worker_type.clone(),
                labels: self.config.labels.clone(),
                capabilities: Some(WorkerCapabilities {
                    memory_mb: 4096,
                    cpu_millicores: 2000,
                    max_concurrent: self.config.max_concurrent as i32,
                }),
            })),
        }
    }

    /// Logs a receive error. HTTP/2 protocol errors also log the keepalive
    /// settings, the most common cause of an intermediary resetting an idle
    /// stream.
    fn log_stream_error(&self, e: &tonic::Status) {
        error!("gRPC error receiving message: {}", e);

        let message = e.to_string();
        if message.contains("h2 protocol error") || message.contains("error reading a body") {
            error!("HTTP/2 protocol error detected - this may indicate:");
            error!("  1. Keepalive timeout mismatch between client and server");
            error!("  2. Connection idle timeout exceeded");
            error!("  3. Message size exceeding buffer limits");
            error!(
                "Current keepalive config: interval={}s, timeout={}s",
                self.config
                    .connection_config
                    .initial_keepalive_interval_secs,
                self.config.connection_config.keepalive_timeout_secs
            );
        }
    }

    async fn create_channel(&self) -> anyhow::Result<Channel> {
        let config = &self.config.connection_config;
        info!(
            "Creating gRPC channel to {}...",
            self.config.server_endpoint
        );

        let endpoint = tonic::transport::Endpoint::from_shared(self.config.server_endpoint.clone())
            .inspect_err(|e| error!("Failed to parse endpoint: {}", e))?
            .connect_timeout(Duration::from_secs(config.connection_timeout_secs))
            .tcp_keepalive(Some(Duration::from_secs(config.tcp_keepalive_secs)))
            .tcp_nodelay(true)
            .keep_alive_while_idle(true)
            .http2_adaptive_window(true);

        info!(
            "Channel configuration: keepalive={}s, timeout={}s, connect_timeout={}s",
            config.initial_keepalive_interval_secs,
            config.keepalive_timeout_secs,
            config.connection_timeout_secs
        );

        match endpoint.connect().await {
            Ok(channel) => {
                info!("Successfully connected to gRPC server");
                Ok(channel)
            }
            Err(e) => {
                error!("Failed to connect to gRPC server: {:?}", e);
                Err(e.into())
            }
        }
    }

    /// Sends a heartbeat every adaptive keepalive interval while the
    /// connection is operational, until the stream's sender closes.
    /// A heartbeat carrying `DRAINING` once a SIGTERM arrived, `IDLE`
    /// otherwise (the server keeps its own slot accounting).
    fn heartbeat(&self) -> WorkerMessage {
        heartbeat_message(
            &self.config.worker_id,
            self.draining.load(Ordering::Acquire),
        )
    }

    fn start_adaptive_heartbeat(
        &self,
        tx: mpsc::Sender<WorkerMessage>,
    ) -> tokio::task::AbortHandle {
        let manager = self.connection_manager.clone();
        let worker_id = self.config.worker_id.clone();
        let draining = self.draining.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(manager.current_keepalive_interval().await).await;

                if !manager.is_operational().await {
                    continue;
                }

                let heartbeat = heartbeat_message(&worker_id, draining.load(Ordering::Acquire));
                if tx.send(heartbeat).await.is_err() {
                    break;
                }
            }
        })
        .abort_handle()
    }

    /// Logs every [`ConnectionEvent`] for the lifetime of the worker.
    fn start_event_monitor(&self) -> tokio::task::AbortHandle {
        let event_rx = self.connection_manager.event_receiver();

        tokio::spawn(async move {
            let mut rx = event_rx.write().await;
            while let Some(event) = rx.recv().await {
                match event {
                    ConnectionEvent::Connected => info!("Connection established successfully"),
                    ConnectionEvent::Disconnected { reason } => {
                        warn!("Connection lost: {}", reason)
                    }
                    ConnectionEvent::Reconnecting {
                        attempt,
                        max_attempts,
                        delay_ms,
                    } => info!(
                        "Reconnecting ({}/{}) after {}ms",
                        attempt, max_attempts, delay_ms
                    ),
                    ConnectionEvent::Failed { reason } => error!("Connection failed: {}", reason),
                    ConnectionEvent::HealthCheckSuccess { rtt_ms } => {
                        debug!("Health check OK: {}ms", rtt_ms)
                    }
                    ConnectionEvent::HealthCheckFailed { reason } => {
                        warn!("Health check failed: {}", reason)
                    }
                    ConnectionEvent::NetworkDegraded { message } => {
                        warn!("Network degraded: {}", message)
                    }
                    ConnectionEvent::NetworkRecovered => info!("Network recovered"),
                    ConnectionEvent::ExecutionHandoffCompleted { count } => {
                        info!("Execution handoff completed: {} executions", count)
                    }
                }
            }
        })
        .abort_handle()
    }

    fn start_metrics_reporter(&self) -> tokio::task::AbortHandle {
        let manager = self.connection_manager.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(STATS_INTERVAL);
            loop {
                interval.tick().await;
                let stats = manager.stats().await;
                info!(
                    "Connection stats: state={} (for {}s), keepalive={}s, reconnects={}",
                    stats.state,
                    stats.state_duration_secs,
                    stats.keepalive_interval_secs,
                    stats.total_reconnections
                );
            }
        })
        .abort_handle()
    }

    /// What one server message asks the run loop to do, if anything. It never
    /// waits: the loop that drains assignments is the one handling the message,
    /// so a bounded hand-off here would deadlock on a burst larger than it.
    fn handle_server_message(&self, msg: ServerMessage) -> Option<ServerEvent> {
        match msg.payload {
            Some(server_message::Payload::Ack(ack)) => info!("Server ACK: {}", ack.message),
            Some(server_message::Payload::Assignment(assignment)) => {
                info!("Received assignment: {}", assignment.execution_id);
                return Some(ServerEvent::Assignment(assignment));
            }
            Some(server_message::Payload::Cancel(cancel)) => {
                info!("Received cancel for execution: {}", cancel.execution_id);
                return Some(ServerEvent::Cancel(cancel.execution_id));
            }
            None => warn!("Received empty server message"),
        }
        None
    }

    /// A result for an action that could not run: `error` carries `stderr`,
    /// with exit code `-1` and no outputs.
    fn failed_result(
        &self,
        execution_id: String,
        started: Instant,
        mut timestamps: PhaseTimestamps,
        stderr: String,
    ) -> ProtoExecutionResult {
        timestamps.worker_completed_ms = now_ms();
        ProtoExecutionResult {
            execution_id,
            worker_id: self.config.worker_id.clone(),
            exit_code: -1,
            stdout: vec![],
            error: stderr.clone(),
            timed_out: false,
            stderr: stderr.into_bytes(),
            output_digests: vec![],
            output_files: vec![],
            output_directories: vec![],
            output_symlinks: vec![],
            execution_duration_ms: started.elapsed().as_millis() as i64,
            completed_at_ms: chrono::Utc::now().timestamp_millis(),
            timestamps: Some(timestamps),
            input_stats: None,
            stdout_digest: None,
            stderr_digest: None,
        }
    }

    /// Runs one assignment in `WORKDIR/exec-<execution_id>` and reports its
    /// result. Infrastructure failures (execution directory, input root,
    /// spawn) surface as a failed result, never as an error that would drop
    /// the stream. Outputs are uploaded whatever the exit code, including
    /// after a timeout, but not after a cancellation.
    async fn execute_assignment(
        &self,
        assignment: WorkAssignment,
        cancel: oneshot::Receiver<()>,
    ) -> ProtoExecutionResult {
        let execution_id = assignment.execution_id.clone();
        let started = Instant::now();
        let mut timestamps = PhaseTimestamps {
            worker_start_ms: now_ms(),
            ..Default::default()
        };
        info!("Executing assignment {}", execution_id);

        let exec_dir = PathBuf::from(&self.config.workdir)
            .join(format!("exec-{}", execution_id.replace('/', "_")));
        if let Err(e) = tokio::fs::create_dir_all(&exec_dir).await {
            return self.failed_result(
                execution_id,
                started,
                timestamps,
                format!("Failed to create workdir: {}", e),
            );
        }

        let mut input_stats = None;
        if let Some(input_root_digest) = &assignment.input_root_digest {
            timestamps.input_fetch_start_ms = now_ms();
            let materialized = self
                .materialize_input_root(input_root_digest, &exec_dir)
                .await;
            timestamps.input_fetch_completed_ms = now_ms();
            match materialized {
                Ok(root) => input_stats = Some(proto_input_stats(&root)),
                Err(stderr) => {
                    self.cleanup_execroot(&exec_dir).await;
                    return self.failed_result(execution_id, started, timestamps, stderr);
                }
            }
        }

        let Some(program) = assignment.command.first().cloned() else {
            self.cleanup_execroot(&exec_dir).await;
            return self.failed_result(
                execution_id,
                started,
                timestamps,
                "Empty command".to_string(),
            );
        };
        let working_dir = working_dir(&exec_dir, &assignment.working_directory);
        create_output_parents(&working_dir, &assignment).await;
        let (home_dir, tmp_dir) = create_sandbox_dirs(&exec_dir).await;

        let isolate_network = !network_allowed(&assignment, self.config.network_by_default);
        let spec = ProcessSpec {
            program: program.clone(),
            args: assignment.command[1..].to_vec(),
            env: action_environment(&assignment, &home_dir, &tmp_dir),
            working_dir: working_dir.clone(),
            timeout: timeout_of(&assignment),
            isolate_network,
            stdout_path: exec_dir.with_extension("stdout"),
            stderr_path: exec_dir.with_extension("stderr"),
            capture_limit: max_capture_size() as u64,
        };
        timestamps.execution_start_ms = now_ms();
        let run = process::run(spec, cancelled(cancel)).await;
        timestamps.execution_completed_ms = now_ms();
        let outcome = match run {
            Ok(outcome) => outcome,
            Err(e) => {
                self.cleanup_execroot(&exec_dir).await;
                return self.failed_result(
                    execution_id,
                    started,
                    timestamps,
                    if isolate_network {
                        format!("Failed to start {} without network: {}", program, e)
                    } else {
                        format!("Failed to start {}: {}", program, e)
                    },
                );
            }
        };
        info!(
            "Assignment {} ended: {:?} after {:?} (stdout {} of {} bytes, stderr {} of {} bytes)",
            execution_id,
            outcome.termination,
            outcome.duration,
            outcome.stdout.size,
            outcome.stdout.written,
            outcome.stderr.size,
            outcome.stderr.written
        );
        let (stdout, stdout_digest) = self.stdio_result(&outcome.stdout).await;
        let (stderr, stderr_digest) = self.stdio_result(&outcome.stderr).await;

        let (output_files, output_directories, output_symlinks) = match outcome.termination {
            Termination::Cancelled => (vec![], vec![], vec![]),
            _ => {
                timestamps.output_upload_start_ms = now_ms();
                let uploaded = self.upload_outputs(&working_dir, &assignment).await;
                timestamps.output_upload_completed_ms = now_ms();
                uploaded
            }
        };
        self.cleanup_execroot(&exec_dir).await;
        timestamps.worker_completed_ms = now_ms();

        ProtoExecutionResult {
            execution_id,
            worker_id: self.config.worker_id.clone(),
            exit_code: outcome.exit_code(),
            timed_out: outcome.termination == Termination::TimedOut,
            error: if outcome.termination == Termination::Cancelled {
                "Cancelled by the server".to_string()
            } else {
                String::new()
            },
            stdout,
            stderr,
            stdout_digest,
            stderr_digest,
            output_digests: vec![],
            output_files,
            output_directories,
            output_symlinks,
            execution_duration_ms: started.elapsed().as_millis() as i64,
            completed_at_ms: chrono::Utc::now().timestamp_millis(),
            timestamps: Some(timestamps),
            input_stats,
        }
    }

    /// Materializes the input root into `exec_dir`. The error is the stderr
    /// of the failed result: an action never runs without all its inputs.
    async fn materialize_input_root(
        &self,
        input_root_digest: &Digest,
        exec_dir: &Path,
    ) -> Result<MaterializedRoot, String> {
        let digest_info = DigestInfo::new(&input_root_digest.hash, input_root_digest.size_bytes)
            .map_err(|e| format!("Invalid input_root_digest: {}", e))?;

        let stats = self
            .materializer
            .materialize_directory_recursive(&digest_info, exec_dir)
            .await
            .map_err(|e| {
                let message = format!("Failed to materialize input root {}: {}", digest_info, e);
                warn!("{} in {}", message, exec_dir.display());
                message
            })?;
        info!(
            "Materialized {} files ({} cached) and {} symlinks in {} directories",
            stats.file_count, stats.cached_files, stats.symlink_count, stats.dir_count
        );
        Ok(stats)
    }

    async fn upload_outputs(
        &self,
        exec_dir: &Path,
        assignment: &WorkAssignment,
    ) -> (Vec<OutputFile>, Vec<OutputDirectory>, Vec<OutputSymlink>) {
        let uploaded = self
            .output_uploader
            .upload_outputs(
                exec_dir,
                &assignment.output_files,
                &assignment.output_directories,
            )
            .await;

        let files = uploaded
            .files
            .into_iter()
            .map(|f| OutputFile {
                path: f.path,
                digest: Some(Digest {
                    hash: f.digest.hash_to_string(),
                    size_bytes: f.digest.size,
                }),
                size_bytes: f.size,
                is_executable: f.is_executable,
            })
            .collect();
        let directories = uploaded
            .directories
            .into_iter()
            .map(|d| OutputDirectory {
                path: d.path,
                tree_digest: Some(Digest {
                    hash: d.tree_digest.hash_to_string(),
                    size_bytes: d.tree_digest.size,
                }),
            })
            .collect();
        let symlinks = uploaded
            .symlinks
            .into_iter()
            .map(|s| OutputSymlink {
                path: s.path,
                target: s.target,
            })
            .collect();
        (files, directories, symlinks)
    }

    /// Removes `exec_dir` unless `RBE_KEEP_EXECROOT` is set.
    /// The bytes to send inline for one captured stream, or the digest it was
    /// stored in the CAS under when it exceeds the inline threshold, so
    /// neither the result message nor the server holds a large output. A
    /// failed upload sends the threshold's worth inline instead. The file is
    /// removed either way.
    async fn stdio_result(&self, captured: &Captured) -> (Vec<u8>, Option<Digest>) {
        let threshold = inline_output_threshold();
        let result = if captured.size <= threshold as u64 {
            (captured.read().await.unwrap_or_default(), None)
        } else {
            match self.output_uploader.upload_file(&captured.path).await {
                Ok(digest) => (
                    Vec::new(),
                    Some(Digest {
                        hash: digest.hash_to_string(),
                        size_bytes: digest.size,
                    }),
                ),
                Err(e) => {
                    warn!(
                        "Could not store {} in the CAS, sending its first {} bytes inline: {}",
                        captured.path.display(),
                        threshold,
                        e
                    );
                    let mut bytes = captured.read().await.unwrap_or_default();
                    bytes.truncate(threshold);
                    (bytes, None)
                }
            }
        };
        if std::env::var("RBE_KEEP_EXECROOT").is_err() {
            let _ = tokio::fs::remove_file(&captured.path).await;
        }
        result
    }

    async fn cleanup_execroot(&self, exec_dir: &Path) {
        if std::env::var("RBE_KEEP_EXECROOT").is_ok() {
            return;
        }
        if let Err(e) = self.materializer.cleanup_execroot(exec_dir).await {
            warn!("Failed to cleanup execroot: {}", e);
        }
    }

    async fn log_connection_stats(&self) {
        let stats = self.connection_manager.stats().await;
        debug!(
            "Connection state: {} ({}s), transitions: {}, reconnects: {}",
            stats.state,
            stats.state_duration_secs,
            stats.total_transitions,
            stats.reconnect_attempts
        );
    }
}

/// Creates the parent directory of every declared output file and every
/// declared output directory, relative to the working directory, as REAPI
/// requires before the command runs.
async fn create_output_parents(working_dir: &Path, assignment: &WorkAssignment) {
    let file_parents = assignment
        .output_files
        .iter()
        .filter_map(|file| Path::new(file).parent());
    let directories = assignment.output_directories.iter().map(Path::new);
    for relative in file_parents.chain(directories) {
        let dir = working_dir.join(relative);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            warn!("Failed to create output directory {:?}: {}", dir, e);
        }
    }
}

/// The `HOME` and `TMPDIR` of the command, private to its execution
/// directory so that actions never share or leak state through them.
async fn create_sandbox_dirs(exec_dir: &Path) -> (PathBuf, PathBuf) {
    let home_dir = exec_dir.join(".home");
    let tmp_dir = exec_dir.join(".tmp");
    for dir in [&home_dir, &tmp_dir] {
        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            warn!("Failed to create sandbox dir {:?}: {}", dir, e);
        }
    }
    (home_dir, tmp_dir)
}

/// The `os` and `arch` labels of a host, in the vocabulary the server
/// derives from REAPI platforms (`aarch64` is `arm64`, `x86_64` is `amd64`).
fn host_labels(os: &str, arch: &str) -> Vec<String> {
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    vec![format!("os={os}"), format!("arch={arch}")]
}

/// A server message the run loop acts on.
enum ServerEvent {
    /// Queued until a slot is free.
    Assignment(WorkAssignment),
    /// Cancels the running assignment with this execution id.
    Cancel(String),
}

/// One assignment running on this worker; up to `MAX_CONCURRENT` run at once.
type Execution<'a> = Pin<Box<dyn Future<Output = ProtoExecutionResult> + Send + 'a>>;

/// Resolves when the server cancels the assignment. Dropping the sender
/// (the assignment finished) is not a cancellation.
async fn cancelled(cancel: oneshot::Receiver<()>) {
    if cancel.await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Accounts the blobs a previous container left in the cache, in the
/// background: until it finishes they are linkable but not yet counted
/// against the budget, so eviction only starts late, never wrongly.
fn warm_up_blob_cache(blob_cache: Arc<DiskBackend>) {
    tokio::spawn(async move {
        if let Err(e) = blob_cache.warm_up().await {
            warn!("Blob cache warm-up failed: {}", e);
        }
    });
}

/// Deletes `dir` in the background; it may hold hundreds of thousands of
/// files.
fn remove_legacy_cache(dir: PathBuf) {
    tokio::spawn(async move {
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => info!("Removed the legacy blob cache {}", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "Could not remove the legacy blob cache {}: {}",
                dir.display(),
                e
            ),
        }
    });
}

/// A heartbeat for `worker_id`, `DRAINING` when `draining`.
fn heartbeat_message(worker_id: &str, draining: bool) -> WorkerMessage {
    let state = if draining {
        worker_heartbeat::State::Draining
    } else {
        worker_heartbeat::State::Idle
    };
    WorkerMessage {
        payload: Some(worker_message::Payload::Heartbeat(WorkerHeartbeat {
            worker_id: worker_id.to_string(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
            state: state as i32,
            active_executions: vec![],
        })),
    }
}

/// How long a drained worker waits for the server to close the stream after
/// it closed its own half.
const STREAM_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits until the server ends `inbound`, which it does once it has read
/// everything the worker sent before closing its half: dropping the stream
/// right after the last `send` can lose the last results still buffered.
async fn wait_for_server_close(inbound: &mut tonic::Streaming<ServerMessage>) {
    let closed = async { while let Ok(Some(_)) = inbound.message().await {} };
    if tokio::time::timeout(STREAM_CLOSE_TIMEOUT, closed)
        .await
        .is_err()
    {
        warn!(
            "The server did not close the stream within {:?}",
            STREAM_CLOSE_TIMEOUT
        );
    }
}

/// Resolves at `deadline`; never when there is none.
async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The working directory of the command: `working_directory` under the
/// execution root, or the root itself when empty.
/// The wall clock in Unix milliseconds, the unit of `PhaseTimestamps`.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn working_dir(exec_dir: &Path, working_directory: &str) -> PathBuf {
    if working_directory.is_empty() {
        exec_dir.to_path_buf()
    } else {
        exec_dir.join(working_directory)
    }
}

/// Whether the action may use the network: its `Network` platform property,
/// else the worker's default.
fn network_allowed(assignment: &WorkAssignment, by_default: bool) -> bool {
    match NetworkAccess::try_from(assignment.network) {
        Ok(NetworkAccess::NetworkOff) => false,
        Ok(NetworkAccess::NetworkOn) => true,
        Ok(NetworkAccess::NetworkWorkerDefault) | Err(_) => by_default,
    }
}

/// The assignment's timeout, or [`DEFAULT_ACTION_TIMEOUT`] when it has none.
fn timeout_of(assignment: &WorkAssignment) -> Duration {
    u64::try_from(assignment.timeout_sec)
        .ok()
        .filter(|&secs| secs > 0)
        .map_or(DEFAULT_ACTION_TIMEOUT, Duration::from_secs)
}

/// The command's environment: `PATH`, `HOME` and `TMPDIR` defaults, each
/// replaced by the action's own value when it sets one, then every other
/// variable of the action, in its order.
fn action_environment(
    assignment: &WorkAssignment,
    home_dir: &Path,
    tmp_dir: &Path,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".to_string(), DEFAULT_PATH.to_string()),
        ("HOME".to_string(), home_dir.display().to_string()),
        ("TMPDIR".to_string(), tmp_dir.display().to_string()),
    ];
    for var in &assignment.environment_variables {
        env.retain(|(name, _)| name != &var.name);
        env.push((var.name.clone(), var.value.clone()));
    }
    env
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = ResilientWorkerConfig::from_env()?;
    seed_fastrand_from_worker_id(&config.worker_id);
    info!(
        "Worker {} for {} (credentials: {})",
        config.worker_id,
        config.server_endpoint,
        if config.auth.is_authenticated() {
            "bearer token"
        } else {
            "none"
        }
    );

    ResilientRbeWorker::new(config).await?.run().await
}

/// Seeds the thread-local `fastrand` generator from a hash of `worker_id`.
///
/// Reconnection jitter then differs between workers yet is reproducible for
/// each one, so a fleet restarting at the same moment spreads its
/// reconnections instead of hitting the scheduler in lockstep.
fn seed_fastrand_from_worker_id(worker_id: &str) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    worker_id.hash(&mut hasher);
    let seed = hasher.finish();

    fastrand::seed(seed);
    info!("Seeded fastrand with worker_id={} seed={}", worker_id, seed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::ferris::rbe::worker::EnvironmentVariable;

    fn assignment(env: &[(&str, &str)], timeout_sec: i32) -> WorkAssignment {
        WorkAssignment {
            environment_variables: env
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            timeout_sec,
            ..Default::default()
        }
    }

    #[test]
    fn the_network_property_overrides_the_worker_default() {
        let with = |network: NetworkAccess| WorkAssignment {
            network: network as i32,
            ..Default::default()
        };
        for by_default in [true, false] {
            assert!(!network_allowed(
                &with(NetworkAccess::NetworkOff),
                by_default
            ));
            assert!(network_allowed(&with(NetworkAccess::NetworkOn), by_default));
            assert_eq!(
                network_allowed(&with(NetworkAccess::NetworkWorkerDefault), by_default),
                by_default
            );
        }
    }

    #[test]
    fn the_action_environment_overrides_the_defaults_and_keeps_its_order() {
        let env = action_environment(
            &assignment(
                &[("LANG", "C.UTF-8"), ("PATH", "/custom/bin"), ("A", "1")],
                0,
            ),
            Path::new("/x/.home"),
            Path::new("/x/.tmp"),
        );
        assert_eq!(
            env,
            vec![
                ("HOME".to_string(), "/x/.home".to_string()),
                ("TMPDIR".to_string(), "/x/.tmp".to_string()),
                ("LANG".to_string(), "C.UTF-8".to_string()),
                ("PATH".to_string(), "/custom/bin".to_string()),
                ("A".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn without_action_variables_the_defaults_apply() {
        let env = action_environment(&assignment(&[], 0), Path::new("/h"), Path::new("/t"));
        assert_eq!(env[0], ("PATH".to_string(), DEFAULT_PATH.to_string()));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn default_labels_describe_the_host_in_the_server_vocabulary() {
        assert_eq!(
            host_labels("linux", "aarch64"),
            vec!["os=linux", "arch=arm64"]
        );
        assert_eq!(
            host_labels("linux", "x86_64"),
            vec!["os=linux", "arch=amd64"]
        );
    }

    #[test]
    fn the_working_directory_is_relative_to_the_execution_root() {
        let root = Path::new("/work/exec-1");
        assert_eq!(working_dir(root, ""), root);
        assert_eq!(working_dir(root, "pkg/sub"), root.join("pkg/sub"));
    }

    #[test]
    fn a_missing_or_nonpositive_timeout_uses_the_default() {
        assert_eq!(timeout_of(&assignment(&[], 0)), DEFAULT_ACTION_TIMEOUT);
        assert_eq!(timeout_of(&assignment(&[], -5)), DEFAULT_ACTION_TIMEOUT);
        assert_eq!(timeout_of(&assignment(&[], 42)), Duration::from_secs(42));
    }

    #[tokio::test]
    async fn dropping_the_cancel_sender_is_not_a_cancellation() {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        drop(cancel_tx);
        let fired = tokio::time::timeout(Duration::from_millis(100), cancelled(cancel_rx)).await;
        assert!(fired.is_err(), "a dropped sender must not cancel");

        let (cancel_tx, cancel_rx) = oneshot::channel();
        cancel_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), cancelled(cancel_rx))
            .await
            .expect("an explicit cancel resolves");
    }
}
