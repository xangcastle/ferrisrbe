pub mod action_cache_service;
pub mod byte_stream;
pub mod capabilities_service;
pub mod cas_service;
pub mod execution_service;
pub mod worker_service;

use std::net::SocketAddr;
use std::time::Duration;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use crate::cache::action_cache::L1ActionCache;
#[allow(unused_imports)]
use crate::cas::backends::DiskBackend;
use crate::cas::backends::GrpcCasBackend;
use crate::cas::SharedCasBackend;
use crate::execution::engine::ExecutionEngine;
use crate::execution::results::ResultsStore;
use crate::execution::scheduler::MultiLevelScheduler;
use crate::execution::state_machine::StateMachineManager;
use crate::worker::k8s::WorkerRegistry;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct RbeServerConfig {
    pub bind_address: String,
    pub port: u16,
    pub l1_cache_capacity: usize,
    pub l1_cache_ttl_secs: u64,
    #[allow(dead_code)]
    pub max_batch_size: usize,
    #[allow(dead_code)]
    pub max_inline_size: usize,
}

impl Default for RbeServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            port: 9092,
            l1_cache_capacity: 100_000,
            l1_cache_ttl_secs: 3600,
            max_batch_size: 4 * 1024 * 1024,
            max_inline_size: 4 * 1024 * 1024,
        }
    }
}

pub struct RbeServer {
    config: RbeServerConfig,
    cas_backend: SharedCasBackend,
    l1_cache: Arc<L1ActionCache>,
    scheduler: Arc<MultiLevelScheduler>,
    state_manager: Arc<StateMachineManager>,
    worker_registry: Arc<WorkerRegistry>,
    results_store: Arc<ResultsStore>,
    execution_engine: Arc<ExecutionEngine>,
}

impl RbeServer {
    pub async fn new(config: RbeServerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let cas_endpoint =
            std::env::var("CAS_ENDPOINT").unwrap_or_else(|_| "bazel-remote:9094".to_string());

        info!("Initializing gRPC CAS backend: endpoint={}", cas_endpoint);
        let cas_backend: SharedCasBackend = Arc::new(GrpcCasBackend::new(&cas_endpoint).await?);
        info!("CAS backend initialized successfully");

        let l1_cache = Arc::new(L1ActionCache::new(
            config.l1_cache_capacity,
            Duration::from_secs(config.l1_cache_ttl_secs),
        ));

        let scheduler = Arc::new(MultiLevelScheduler::new());
        let state_manager = Arc::new(StateMachineManager::new());
        let worker_registry = Arc::new(WorkerRegistry::new());
        let results_store = Arc::new(ResultsStore::new());

        let execution_engine = Arc::new(ExecutionEngine::new(
            scheduler.clone(),
            worker_registry.clone(),
            state_manager.clone(),
            l1_cache.clone(),
            results_store.clone(),
        ));

        Ok(Self {
            config,
            cas_backend,
            l1_cache,
            scheduler,
            state_manager,
            worker_registry,
            results_store,
            execution_engine,
        })
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let addr: SocketAddr =
            format!("{}:{}", self.config.bind_address, self.config.port).parse()?;

        info!("╔══════════════════════════════════════════════════════════════╗");
        info!("║              RBE Server - Remote Build Execution              ║");
        info!("╠══════════════════════════════════════════════════════════════╣");
        info!("║ ✓ ByteStream Service (CAS upload/download)                    ║");
        info!("║ ✓ ActionCache Service (L1: DashMap 64 shards)                 ║");
        info!("║ ✓ CAS Service (FindMissing, BatchRead, BatchUpdate)           ║");
        info!("║ ✓ Execution Service (MultiLevelScheduler + StateMachine)      ║");
        info!("║ ✓ Worker Service (Bidirectional streaming)                    ║");
        info!("║ ✓ Execution Engine (Scheduler + Worker integration)           ║");
        info!("║ ✓ Capabilities Service (REAPI v2.3/v2.4)                      ║");
        info!("╚══════════════════════════════════════════════════════════════╝");
        info!("");
        info!("Starting RBE server on {}", addr);
        info!("Configuration: {:?}", self.config);

        self.execution_engine.clone().spawn();
        info!("ExecutionEngine started");

        let byte_stream_service = byte_stream::ByteStreamService::new(self.cas_backend.clone());
        let action_cache_service =
            action_cache_service::ActionCacheService::new(self.l1_cache.clone());
        let cas_service = cas_service::CasService::new(self.cas_backend.clone());
        let execution_service = execution_service::ExecutionService::new(
            self.scheduler.clone(),
            self.state_manager.clone(),
            self.l1_cache.clone(),
            self.results_store.clone(),
            self.cas_backend.clone(),
        );
        let capabilities_service = capabilities_service::CapabilitiesService::new();
        let worker_service = worker_service::WorkerServiceImpl::new(
            self.worker_registry.clone(),
            self.execution_engine.clone(),
        );

        let tcp_keepalive_secs = std::env::var("RBE_TCP_KEEPALIVE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let http2_keepalive_interval_secs = std::env::var("RBE_HTTP2_KEEPALIVE_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let http2_keepalive_timeout_secs = std::env::var("RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let request_timeout_secs = std::env::var("RBE_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600);
        let http2_adaptive_window = std::env::var("RBE_HTTP2_ADAPTIVE_WINDOW")
            .ok()
            .map(|s| s == "true" || s == "1")
            .unwrap_or(true);

        info!("HTTP/2 Configuration: tcp_keepalive={}s, http2_interval={}s, http2_timeout={}s, request_timeout={}s, adaptive_window={}",
              tcp_keepalive_secs, http2_keepalive_interval_secs, http2_keepalive_timeout_secs,
              request_timeout_secs, http2_adaptive_window);

        // Configure TLS if certificates are provided
        let tls_cert = std::env::var("RBE_TLS_CERT").ok();
        let tls_key = std::env::var("RBE_TLS_KEY").ok();
        
        // Build base server configuration
        let mut server_builder = Server::builder()
            .tcp_keepalive(Some(Duration::from_secs(tcp_keepalive_secs)))
            .tcp_nodelay(true)
            .http2_keepalive_interval(Some(Duration::from_secs(http2_keepalive_interval_secs)))
            .http2_keepalive_timeout(Some(Duration::from_secs(http2_keepalive_timeout_secs)))
            .http2_adaptive_window(Some(http2_adaptive_window))
            .timeout(Duration::from_secs(request_timeout_secs));

        // Apply TLS if certificates are configured
        if let (Some(cert_pem), Some(key_pem)) = (&tls_cert, &tls_key) {
            info!("Configuring TLS with provided certificates");
            let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
            let tls_config = ServerTlsConfig::new().identity(identity);
            server_builder = server_builder.tls_config(tls_config)
                .map_err(|e| format!("Failed to configure TLS: {}", e))?;
            info!("TLS enabled successfully");
        } else {
            info!("Running without TLS (plaintext mode)");
        }

        server_builder
            .add_service(byte_stream_service.into_service())
            .add_service(action_cache_service.into_service())
            .add_service(cas_service.into_service())
            .add_service(execution_service.into_service())
            .add_service(capabilities_service.into_service())
            .add_service(worker_service.into_service())
            .serve(addr)
            .await?;

        Ok(())
    }
}
