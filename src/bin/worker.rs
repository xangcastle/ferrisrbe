//! Resilient RBE Worker with enterprise-grade connection management
//!
//! Features:
//! - Adaptive keepalive based on network conditions
//! - Automatic environment detection (Docker Desktop, K8s, Cloud)
//! - Exponential backoff with jitter for reconnection
//! - Merkle Tree materialization for Bazel execroot
//! - Comprehensive metrics and observability

use rbe_server::cas::backends::GrpcCasBackend;
use rbe_server::cas::CasBackend;
use rbe_server::types::DigestInfo;
use rbe_server::worker::materializer::{Materializer, MaterializerConfig};
use rbe_server::worker::output_uploader::{OutputUploader, UploaderConfig};

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
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
    server_message, worker_message, ExecutionResult as ProtoExecutionResult, ServerMessage,
    WorkAssignment, WorkerCapabilities, WorkerHeartbeat, WorkerMessage, WorkerRegistration,
};

mod resilient_connection {
    pub mod adaptive_keepalive;
    pub mod connection_manager;
    pub mod connection_state;
    pub mod health_checker;
    pub mod metrics;
    pub mod reconnection;

    #[allow(unused_imports)]
    pub use adaptive_keepalive::AdaptiveKeepalive;
    #[allow(unused_imports)]
    pub use connection_manager::ConnectionStats;
    pub use connection_manager::{
        ConfigLoader, ConnectionConfig, ConnectionEvent, ConnectionManager,
    };
    pub use connection_state::ConnectionState;
    #[allow(unused_imports)]
    pub use health_checker::{HealthCheckConfig, HealthChecker};
    #[allow(unused_imports)]
    pub use metrics::ConnectionMetrics;
    pub use reconnection::{ReconnectionPolicy, ReconnectionStrategy};

    /// Default configuration - sensible defaults for production
    ///
    /// All values can be overridden via RBE_* environment variables.
    /// This follows 12-Factor App principles.
    #[allow(dead_code)]
    pub fn default_config() -> ConnectionConfig {
        ConnectionConfig {
            initial_keepalive_interval_secs: 20,
            min_keepalive_interval_secs: 10,
            max_keepalive_interval_secs: 60,
            keepalive_timeout_secs: 15,
            tcp_keepalive_secs: 30,
            connection_timeout_secs: 30,
            max_reconnect_attempts: 10,
            reconnect_base_delay_ms: 100,
            reconnect_max_delay_ms: 30000,
            reconnect_jitter_factor: 0.25,
            health_check_interval_secs: 5,
            health_check_timeout_secs: 3,
            execution_handoff_timeout_secs: 60,
            adaptive_adjustment_threshold: 3,
            enable_metrics: true,
        }
    }
}

use resilient_connection::{
    ConfigLoader, ConnectionConfig, ConnectionEvent, ConnectionManager, ConnectionState,
};
#[allow(unused_imports)]
use resilient_connection::{ReconnectionPolicy, ReconnectionStrategy};

struct ResilientWorkerConfig {
    worker_id: String,
    server_endpoint: String,
    worker_type: String,
    cas_endpoint: String,
    labels: Vec<String>,
    max_concurrent: usize,
    workdir: String,
    connection_config: ConnectionConfig,
}

impl ResilientWorkerConfig {
    fn from_env() -> Self {
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

        Self {
            worker_id,
            server_endpoint: std::env::var("SERVER_ENDPOINT")
                .unwrap_or_else(|_| "http://rbe-server:9092".to_string()),
            worker_type: std::env::var("WORKER_TYPE").unwrap_or_else(|_| "default".to_string()),
            cas_endpoint: std::env::var("CAS_ENDPOINT")
                .unwrap_or_else(|_| "http://bazel-remote:9094".to_string()),
            labels: std::env::var("WORKER_LABELS")
                .map(|s| s.split(',').map(|s| s.to_string()).collect())
                .unwrap_or_else(|_| vec!["os=linux".to_string(), "arch=amd64".to_string()]),
            max_concurrent: std::env::var("MAX_CONCURRENT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4),
            workdir: std::env::var("WORKDIR").unwrap_or_else(|_| "/workspace".to_string()),
            connection_config,
        }
    }
}

struct ResilientRbeWorker {
    config: ResilientWorkerConfig,
    connection_manager: Arc<ConnectionManager>,
    materializer: Arc<Materializer>,
    output_uploader: Arc<OutputUploader>,
    #[allow(dead_code)]
    active_executions: Vec<String>,
}

impl ResilientRbeWorker {
    async fn new(config: ResilientWorkerConfig) -> anyhow::Result<Self> {
        let connection_manager = Arc::new(ConnectionManager::new(config.connection_config.clone()));

        let cas_cache_dir = std::path::PathBuf::from(&config.workdir).join("cas-cache");
        tokio::fs::create_dir_all(&cas_cache_dir).await?;

        let cas_endpoint =
            std::env::var("CAS_ENDPOINT").unwrap_or_else(|_| "bazel-remote:9094".to_string());
        let cas_backend: Arc<dyn CasBackend> = Arc::new(GrpcCasBackend::new(&cas_endpoint).await?);

        let materializer = Arc::new(Materializer::new(
            cas_backend.clone(),
            cas_cache_dir,
            MaterializerConfig::default(),
        ));

        let output_uploader = Arc::new(OutputUploader::new(cas_backend, UploaderConfig::default()));

        Ok(Self {
            config,
            connection_manager,
            materializer,
            output_uploader,
            active_executions: Vec::new(),
        })
    }

    async fn run(&mut self) -> anyhow::Result<()> {
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

        let _event_monitor = self.start_event_monitor();

        loop {
            match self.connect_and_work().await {
                Ok(_) => {
                    info!("Work loop completed normally");
                }
                Err(e) => {
                    error!("Work loop error: {}", e);
                }
            }

            if self.connection_manager.is_max_reconnect_exceeded().await {
                error!("Max reconnection attempts exceeded, shutting down");
                return Err(anyhow::anyhow!("Max reconnection attempts exceeded"));
            }

            let delay = self.connection_manager.next_reconnect_delay().await;
            self.connection_manager.increment_reconnect_attempt().await;

            info!("Reconnecting in {:?}...", delay);
            tokio::time::sleep(delay).await;
        }
    }

    async fn connect_and_work(&mut self) -> anyhow::Result<()> {
        self.connection_manager
            .state_machine()
            .write()
            .await
            .transition_to(
                ConnectionState::Connecting,
                Some("Starting connection".to_string()),
            );

        let channel = match self.create_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                error!("Channel creation failed: {:?}", e);
                return Err(e);
            }
        };

        self.connection_manager
            .state_machine()
            .write()
            .await
            .transition_to(
                ConnectionState::Handshaking,
                Some("Connected, starting handshake".to_string()),
            );

        let mut client = WorkerServiceClient::new(channel.clone());

        let (tx, rx) = mpsc::channel(100);
        let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);

        let response = client.stream_work(outbound).await?;
        let mut inbound = response.into_inner();

        let registration = WorkerMessage {
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
        };
        tx.send(registration).await?;
        info!("Sent registration to server");

        self.connection_manager.record_connected(channel).await;

        let heartbeat_handle = self.start_adaptive_heartbeat(tx.clone());

        let metrics_handle = self.start_metrics_reporter();

        let (assign_tx, mut assign_rx) = mpsc::channel::<WorkAssignment>(10);

        loop {
            tokio::select! {
                msg = inbound.message() => {
                    match msg {
                        Ok(Some(server_msg)) => {
                            if let Err(e) = self.handle_server_message(server_msg, &assign_tx).await {
                                error!("Error handling server message: {}", e);
                            }
                        }
                        Ok(None) => {
                            info!("Server closed connection gracefully");
                            self.connection_manager
                                .record_disconnected("Server closed connection".to_string())
                                .await;
                            break;
                        }
                        Err(e) => {
                            // Log detailed error information for debugging
                            let error_msg = format!("gRPC error receiving message: {}", e);
                            error!("{}", error_msg);

                            // Check if it's the specific HTTP/2 error we're investigating
                            let e_str = e.to_string();
                            if e_str.contains("h2 protocol error") || e_str.contains("error reading a body") {
                                error!("HTTP/2 protocol error detected - this may indicate:");
                                error!("  1. Keepalive timeout mismatch between client and server");
                                error!("  2. Connection idle timeout exceeded");
                                error!("  3. Message size exceeding buffer limits");
                                error!("Current keepalive config: interval={}s, timeout={}s",
                                    self.config.connection_config.initial_keepalive_interval_secs,
                                    self.config.connection_config.keepalive_timeout_secs);
                            }

                            self.connection_manager
                                .record_disconnected(format!("Receive error: {}", e))
                                .await;
                            break;
                        }
                    }
                }

                Some(assignment) = assign_rx.recv() => {
                    let result = self.execute_assignment(assignment).await;
                    let result_msg = WorkerMessage {
                        payload: Some(worker_message::Payload::Result(result)),
                    };
                    if tx.send(result_msg).await.is_err() {
                        error!("Failed to send result to server");
                        break;
                    }
                }

                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    // Periodic stats logging
                    self.log_connection_stats().await;
                }
            }
        }

        heartbeat_handle.abort();
        metrics_handle.abort();

        Ok(())
    }

    async fn create_channel(&self) -> anyhow::Result<Channel> {
        let config = &self.config.connection_config;

        info!(
            "Creating gRPC channel to {}...",
            self.config.server_endpoint
        );

        let endpoint = tonic::transport::Endpoint::from_shared(self.config.server_endpoint.clone())
            .map_err(|e| {
                error!("Failed to parse endpoint: {}", e);
                e
            })?
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

    fn start_adaptive_heartbeat(
        &self,
        tx: mpsc::Sender<WorkerMessage>,
    ) -> tokio::task::AbortHandle {
        let manager = self.connection_manager.clone();
        let worker_id = self.config.worker_id.clone();

        tokio::spawn(async move {
            loop {
                let interval_secs = manager.current_keepalive_interval().await.as_secs();

                tokio::time::sleep(Duration::from_secs(interval_secs)).await;

                if !manager.is_operational().await {
                    continue;
                }

                let heartbeat = WorkerMessage {
                    payload: Some(worker_message::Payload::Heartbeat(WorkerHeartbeat {
                        worker_id: worker_id.clone(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        state: 0,
                        active_executions: vec![],
                    })),
                };

                if tx.send(heartbeat).await.is_err() {
                    break;
                }
            }
        })
        .abort_handle()
    }

    fn start_event_monitor(&self) -> tokio::task::AbortHandle {
        let event_rx = self.connection_manager.event_receiver();

        tokio::spawn(async move {
            let mut rx = event_rx.write().await;
            while let Some(event) = rx.recv().await {
                match event {
                    ConnectionEvent::Connected => {
                        info!("✅ Connection established successfully");
                    }
                    ConnectionEvent::Disconnected { reason } => {
                        warn!("⚠️  Connection lost: {}", reason);
                    }
                    ConnectionEvent::Reconnecting {
                        attempt,
                        max_attempts,
                        delay_ms,
                    } => {
                        info!(
                            "🔄 Reconnecting ({}/{}) after {}ms",
                            attempt, max_attempts, delay_ms
                        );
                    }
                    ConnectionEvent::Failed { reason } => {
                        error!("❌ Connection failed: {}", reason);
                    }
                    ConnectionEvent::HealthCheckSuccess { rtt_ms } => {
                        debug!("💓 Health check OK: {}ms", rtt_ms);
                    }
                    ConnectionEvent::HealthCheckFailed { reason } => {
                        warn!("💔 Health check failed: {}", reason);
                    }
                    ConnectionEvent::NetworkDegraded { message } => {
                        warn!("📉 Network degraded: {}", message);
                    }
                    ConnectionEvent::NetworkRecovered => {
                        info!("📈 Network recovered");
                    }
                    ConnectionEvent::ExecutionHandoffCompleted { count } => {
                        info!("🔄 Execution handoff completed: {} executions", count);
                    }
                }
            }
        })
        .abort_handle()
    }

    fn start_metrics_reporter(&self) -> tokio::task::AbortHandle {
        let manager = self.connection_manager.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
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

    async fn handle_server_message(
        &mut self,
        msg: ServerMessage,
        assign_tx: &mpsc::Sender<WorkAssignment>,
    ) -> anyhow::Result<()> {
        match msg.payload {
            Some(server_message::Payload::Ack(ack)) => {
                info!("Server ACK: {}", ack.message);
            }
            Some(server_message::Payload::Assignment(assignment)) => {
                info!("Received assignment: {}", assignment.execution_id);

                if let Err(e) = assign_tx.send(assignment).await {
                    warn!("Failed to queue assignment: {}", e);
                }
            }
            Some(server_message::Payload::Cancel(cancel)) => {
                warn!("Received cancel for execution: {}", cancel.execution_id);
            }
            None => {
                warn!("Received empty server message");
            }
        }
        Ok(())
    }

    async fn execute_assignment(&self, assignment: WorkAssignment) -> ProtoExecutionResult {
        let execution_id = assignment.execution_id.clone();
        let start = std::time::Instant::now();

        info!(
            "Executing assignment: {} with command: {:?}",
            execution_id, assignment.command
        );

        let workdir = std::path::PathBuf::from(&self.config.workdir);
        let exec_dir = workdir.join(format!("exec-{}", execution_id.replace('/', "_")));

        if let Err(e) = tokio::fs::create_dir_all(&exec_dir).await {
            return ProtoExecutionResult {
                execution_id,
                worker_id: self.config.worker_id.clone(),
                exit_code: -1,
                stdout: vec![],
                stderr: format!("Failed to create workdir: {}", e).into_bytes(),
                output_digests: vec![],
                output_files: vec![],
                output_directories: vec![],
                execution_duration_ms: start.elapsed().as_millis() as i64,
                completed_at_ms: chrono::Utc::now().timestamp_millis(),
            };
        }

        if let Some(input_root_digest) = assignment.input_root_digest {
            let digest_info =
                DigestInfo::new(&input_root_digest.hash, input_root_digest.size_bytes);

            info!("Materializing input root: {}", digest_info.hash_to_string());

            match self
                .materializer
                .materialize_directory_recursive(&digest_info, &exec_dir)
                .await
            {
                Ok(stats) => {
                    info!(
                        "Materialized {} files ({} cached) in {} directories",
                        stats.file_count, stats.cached_files, stats.dir_count
                    );
                }
                Err(e) => {
                    warn!("Failed to materialize input root: {}. Continuing with empty exec dir (may work for simple actions).", e);
                    if let Err(e2) = tokio::fs::create_dir_all(&exec_dir).await {
                        error!("Failed to create exec dir: {}", e2);
                        return ProtoExecutionResult {
                            execution_id,
                            worker_id: self.config.worker_id.clone(),
                            exit_code: -1,
                            stdout: vec![],
                            stderr: format!(
                                "Failed to materialize input root: {} and create exec dir: {}",
                                e, e2
                            )
                            .into_bytes(),
                            output_digests: vec![],
                            output_files: vec![],
                            output_directories: vec![],
                            execution_duration_ms: start.elapsed().as_millis() as i64,
                            completed_at_ms: chrono::Utc::now().timestamp_millis(),
                        };
                    }
                }
            }
        }

        info!(
            "Creating output directories for {} files and {} directories",
            assignment.output_files.len(),
            assignment.output_directories.len()
        );
        for output_file in &assignment.output_files {
            if let Some(parent) = Path::new(output_file).parent() {
                let output_dir = exec_dir.join(parent);
                if let Err(e) = tokio::fs::create_dir_all(&output_dir).await {
                    warn!("Failed to create output directory {:?}: {}", output_dir, e);
                }
            }
        }
        for output_dir in &assignment.output_directories {
            let full_path = exec_dir.join(output_dir);
            if let Err(e) = tokio::fs::create_dir_all(&full_path).await {
                warn!("Failed to create output directory {:?}: {}", full_path, e);
            }
        }

        let output = if assignment.command.is_empty() {
            tokio::process::Command::new("echo")
                .arg(format!("Executed: {}", execution_id))
                .current_dir(&exec_dir)
                .output()
                .await
        } else {
            let mut cmd = tokio::process::Command::new(&assignment.command[0]);
            if assignment.command.len() > 1 {
                cmd.args(&assignment.command[1..]);
            }

            for env_var in &assignment.environment_variables {
                cmd.env(&env_var.name, &env_var.value);
            }

            cmd.current_dir(&exec_dir).output().await
        };

        match output {
            Ok(output) => {
                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                info!(
                    "Command executed: exit_code={}, stdout_len={}, stderr_len={}",
                    exit_code,
                    stdout.len(),
                    stderr.len()
                );
                if !stdout.is_empty() {
                    info!("stdout: {}", stdout);
                }
                if !stderr.is_empty() {
                    info!("stderr: {}", stderr);
                }

                let (output_files, output_directories) = if exit_code == 0 {
                    let upload_result = self
                        .output_uploader
                        .upload_outputs(
                            &exec_dir,
                            &assignment.output_files,
                            &assignment.output_directories,
                        )
                        .await;

                    let files: Vec<proto::ferris::rbe::worker::OutputFile> = upload_result
                        .files
                        .into_iter()
                        .map(|f| proto::ferris::rbe::worker::OutputFile {
                            path: f.path,
                            digest: Some(proto::ferris::rbe::worker::Digest {
                                hash: f.digest.hash_to_string(),
                                size_bytes: f.digest.size,
                            }),
                            size_bytes: f.size,
                            is_executable: f.is_executable,
                        })
                        .collect();

                    let dirs: Vec<proto::ferris::rbe::worker::OutputDirectory> = upload_result
                        .directories
                        .into_iter()
                        .map(|d| proto::ferris::rbe::worker::OutputDirectory {
                            path: d.path,
                            tree_digest: Some(proto::ferris::rbe::worker::Digest {
                                hash: d.tree_digest.hash_to_string(),
                                size_bytes: d.tree_digest.size,
                            }),
                        })
                        .collect();

                    (files, dirs)
                } else {
                    (vec![], vec![])
                };

                if std::env::var("RBE_KEEP_EXECROOT").is_err() {
                    if let Err(e) = self.materializer.cleanup_execroot(&exec_dir).await {
                        warn!("Failed to cleanup execroot: {}", e);
                    }
                }

                ProtoExecutionResult {
                    execution_id,
                    worker_id: self.config.worker_id.clone(),
                    exit_code,
                    stdout: output.stdout,
                    stderr: output.stderr,
                    output_digests: vec![],
                    output_files,
                    output_directories,
                    execution_duration_ms: start.elapsed().as_millis() as i64,
                    completed_at_ms: chrono::Utc::now().timestamp_millis(),
                }
            }
            Err(e) => {
                if std::env::var("RBE_KEEP_EXECROOT").is_err() {
                    if let Err(cleanup_err) = self.materializer.cleanup_execroot(&exec_dir).await {
                        warn!("Failed to cleanup execroot: {}", cleanup_err);
                    }
                }

                ProtoExecutionResult {
                    execution_id,
                    worker_id: self.config.worker_id.clone(),
                    exit_code: -1,
                    stdout: vec![],
                    stderr: format!("Execution failed: {}", e).into_bytes(),
                    output_digests: vec![],
                    output_files: vec![],
                    output_directories: vec![],
                    execution_duration_ms: start.elapsed().as_millis() as i64,
                    completed_at_ms: chrono::Utc::now().timestamp_millis(),
                }
            }
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = ResilientWorkerConfig::from_env();
    let mut worker = ResilientRbeWorker::new(config).await?;

    worker.run().await
}
