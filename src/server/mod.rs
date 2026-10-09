//! The FerrisRBE server process: every gRPC service (REAPI CAS, Action
//! Cache, ByteStream, Execution, Capabilities, Remote Asset, worker control
//! and BES) on one port, and the HTTP surface (BES API, embedded UI,
//! pull-through proxy, MCP, metrics and probes) on another.
//!
//! Both listeners open before any store has loaded its state from disk.
//! Each store rebuilds its in-memory index in the background behind a
//! readiness gate, so `/healthz` answers from the first second while
//! `/readyz` holds traffic until every index reflects the PVC. `SIGTERM`
//! drains both listeners and flushes pending counters before exit.

pub mod action_cache_service;
pub mod auth;
pub mod byte_stream;
pub mod capabilities_service;
pub mod cas_service;
pub mod critical_actions;
pub mod execution_service;
pub mod remote_asset_service;
pub mod request_metadata;
pub mod worker_service;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use include_dir::Dir;
use std::future::IntoFuture;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::{error, info, warn};

use crate::bes::{with_embedded_ui, BesApi, BesConfig, BesService, BesStorage, LiveBus};

use crate::cache::action_cache::ActionCacheStore;
use crate::cache::action_cache_log::ActionCacheLog;
use crate::cache::disk_action_cache::DiskActionCache;
use crate::cache::tiered_action_cache::TieredActionCache;
use crate::cas::backends::{DiskBackend, ObjectStoreBackend, TieredBackend};
use crate::cas::SharedCasBackend;
use crate::execution::engine::ExecutionEngine;
use crate::execution::results::ResultsStore;
use crate::execution::scheduler::MultiLevelScheduler;
use crate::execution::state_machine::StateMachineManager;
use crate::metrics::Metrics;
use crate::proxy::{ProxyConfig, ProxyState, PullThroughCache};
use crate::readiness::Readiness;
use crate::worker::k8s::WorkerRegistry;

/// A UI or API request that runs longer than this is answered 408; the
/// proxy streams large artifacts but every read is bounded well below it.
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 300;

/// After the shutdown signal, in-flight requests get this long to finish
/// before the process exits on its own; the kubelet's grace period is the
/// hard limit.
const SHUTDOWN_DRAIN_SECS: u64 = 30;

/// How often in-memory proxy download counters are persisted to their
/// sidecars. A crash loses at most one interval of counts.
const DOWNLOAD_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// How often CAS size and entry count are published as metrics.
const CAS_STATS_INTERVAL: Duration = Duration::from_secs(30);
/// How often the execution queue and fleet gauges are sampled.
const EXECUTION_LOAD_INTERVAL: Duration = Duration::from_secs(10);

/// Listener addresses, storage layout and cache budgets of the server.
#[derive(Debug, Clone)]
pub struct RbeServerConfig {
    /// Interface both listeners bind to.
    pub bind_address: String,
    /// gRPC: CAS, ActionCache, ByteStream, Remote Asset, Execution, Worker, BES.
    pub port: u16,
    /// HTTP: BES API, embedded UI, pull-through proxy, metrics and probes.
    pub http_port: u16,
    /// PVC root; the CAS, ActionCache, proxy and BES live under it.
    pub cache_dir: String,
    /// Disk budget of the CAS, in GiB; least recently used blobs are evicted past it.
    pub cas_max_size_gb: u64,
    /// Disk budget of the Action Cache, in GiB.
    pub ac_max_size_gb: u64,
    /// Age after which an Action Cache entry is treated as a miss.
    pub ac_ttl_secs: u64,
    /// Entries held by the in-memory Action Cache in front of the disk.
    pub l1_cache_capacity: usize,
    pub l1_cache_ttl_secs: u64,
    /// S3 bucket behind the disk CAS and Action Cache ([`TieredBackend`],
    /// [`TieredActionCache`]); `None` keeps both on disk only.
    pub cas_s3_bucket: Option<String>,
    /// Key prefix of the CAS and Action Cache objects in [`Self::cas_s3_bucket`].
    pub cas_s3_prefix: String,
    /// UI bundle served at `/`; `None` serves the API only.
    pub ui_assets: Option<&'static Dir<'static>>,
}

/// The value of environment variable `key`, or `default` when it is unset
/// or does not parse.
fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl RbeServerConfig {
    /// Configuration from the `RBE_*` environment, with the [`Default`]
    /// values for anything unset.
    pub fn from_env() -> Self {
        Self {
            bind_address: std::env::var("RBE_BIND_ADDRESS")
                .unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env_or("RBE_PORT", 9094),
            http_port: env_or("RBE_HTTP_PORT", 8080),
            cache_dir: std::env::var("RBE_CACHE_DIR").unwrap_or_else(|_| "/data".to_string()),
            cas_max_size_gb: env_or("RBE_CACHE_MAX_SIZE_GB", 100),
            ac_max_size_gb: env_or("RBE_CACHE_AC_MAX_SIZE_GB", 10),
            ac_ttl_secs: env_or("RBE_CACHE_AC_TTL_SECS", 3600),
            l1_cache_capacity: env_or("RBE_L1_CACHE_CAPACITY", 100_000),
            l1_cache_ttl_secs: env_or("RBE_L1_CACHE_TTL_SECS", 3600),
            cas_s3_bucket: std::env::var("RBE_CAS_S3_BUCKET")
                .ok()
                .filter(|b| !b.is_empty()),
            cas_s3_prefix: std::env::var("RBE_CAS_S3_PREFIX").unwrap_or_default(),
            ui_assets: None,
        }
    }
}

impl Default for RbeServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            port: 9094,
            http_port: 8080,
            cache_dir: "/data".to_string(),
            cas_max_size_gb: 100,
            ac_max_size_gb: 10,
            ac_ttl_secs: 3600,
            l1_cache_capacity: 100_000,
            l1_cache_ttl_secs: 3600,
            cas_s3_bucket: None,
            cas_s3_prefix: String::new(),
            ui_assets: None,
        }
    }
}

/// A configured server whose stores are open but not yet loaded; [`run`]
/// starts the listeners and the background warm-ups.
///
/// [`run`]: RbeServer::run
pub struct RbeServer {
    config: RbeServerConfig,
    cas_backend: Arc<DiskBackend>,
    /// What the services read and write: [`Self::cas_backend`], alone or in
    /// front of S3.
    cas: SharedCasBackend,
    action_cache: Arc<DiskActionCache>,
    /// What the services read and write: [`Self::action_cache`], alone or in
    /// front of S3.
    action_cache_store: Arc<dyn ActionCacheStore>,
    action_cache_log: Arc<ActionCacheLog>,
    proxy_cache: Arc<PullThroughCache>,
    metrics: Arc<Metrics>,
    scheduler: Arc<MultiLevelScheduler>,
    state_manager: Arc<StateMachineManager>,
    worker_registry: Arc<WorkerRegistry>,
    results_store: Arc<ResultsStore>,
    execution_engine: Arc<ExecutionEngine>,
    bes_config: BesConfig,
    bes_storage: BesStorage,
    live_bus: LiveBus,
}

/// Samples the scheduler queue and the worker fleet into the `rbe_execution_*`
/// and `rbe_worker*` gauges, which drive the worker autoscaler.
fn spawn_execution_load_collector(
    scheduler: Arc<MultiLevelScheduler>,
    registry: Arc<WorkerRegistry>,
    metrics: Arc<Metrics>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(EXECUTION_LOAD_INTERVAL);
        loop {
            interval.tick().await;
            metrics.record_execution_load(scheduler.queued_len(), registry.load());
        }
    });
}

fn spawn_cas_stats_collector(backend: Arc<DiskBackend>, metrics: Arc<Metrics>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CAS_STATS_INTERVAL);
        loop {
            interval.tick().await;
            let size = backend.size_bytes();
            let entries = backend.count_entries().await;
            metrics.record_cas_stats(size, entries);
        }
    });
}

/// Persists the proxy's in-memory download counters every
/// [`DOWNLOAD_FLUSH_INTERVAL`]; serving never pays a disk write for them.
fn spawn_download_flusher(proxy: Arc<PullThroughCache>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DOWNLOAD_FLUSH_INTERVAL);
        loop {
            interval.tick().await;
            proxy.flush_downloads().await;
        }
    });
}

/// Keepalive, timeout and flow-control settings of the gRPC listener, from
/// the `RBE_TCP_KEEPALIVE_SECS`, `RBE_HTTP2_*` and `RBE_REQUEST_TIMEOUT_SECS`
/// environment.
struct GrpcTransport {
    tcp_keepalive_secs: u64,
    http2_keepalive_interval_secs: u64,
    http2_keepalive_timeout_secs: u64,
    request_timeout_secs: u64,
    http2_adaptive_window: bool,
}

impl GrpcTransport {
    fn from_env() -> Self {
        Self {
            tcp_keepalive_secs: env_or("RBE_TCP_KEEPALIVE_SECS", 60),
            http2_keepalive_interval_secs: env_or("RBE_HTTP2_KEEPALIVE_INTERVAL_SECS", 60),
            http2_keepalive_timeout_secs: env_or("RBE_HTTP2_KEEPALIVE_TIMEOUT_SECS", 30),
            request_timeout_secs: env_or("RBE_REQUEST_TIMEOUT_SECS", 600),
            http2_adaptive_window: std::env::var("RBE_HTTP2_ADAPTIVE_WINDOW")
                .map(|s| s == "true" || s == "1")
                .unwrap_or(true),
        }
    }

    /// A tonic server builder with these settings, serving TLS when both
    /// `RBE_TLS_CERT` and `RBE_TLS_KEY` hold PEM data and plaintext otherwise.
    fn server_builder(&self) -> Result<Server, Box<dyn std::error::Error>> {
        info!("HTTP/2 Configuration: tcp_keepalive={}s, http2_interval={}s, http2_timeout={}s, request_timeout={}s, adaptive_window={}",
              self.tcp_keepalive_secs, self.http2_keepalive_interval_secs, self.http2_keepalive_timeout_secs,
              self.request_timeout_secs, self.http2_adaptive_window);

        let builder = Server::builder()
            .tcp_keepalive(Some(Duration::from_secs(self.tcp_keepalive_secs)))
            .tcp_nodelay(true)
            .http2_keepalive_interval(Some(Duration::from_secs(
                self.http2_keepalive_interval_secs,
            )))
            .http2_keepalive_timeout(Some(Duration::from_secs(self.http2_keepalive_timeout_secs)))
            .http2_adaptive_window(Some(self.http2_adaptive_window))
            .timeout(Duration::from_secs(self.request_timeout_secs));

        let (Ok(cert_pem), Ok(key_pem)) =
            (std::env::var("RBE_TLS_CERT"), std::env::var("RBE_TLS_KEY"))
        else {
            info!("Running without TLS (plaintext mode)");
            return Ok(builder);
        };
        info!("Configuring TLS with provided certificates");
        let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
        let builder = builder
            .tls_config(ServerTlsConfig::new().identity(identity))
            .map_err(|e| format!("Failed to configure TLS: {}", e))?;
        info!("TLS enabled successfully");
        Ok(builder)
    }
}

impl RbeServer {
    /// Opens every store under [`RbeServerConfig::cache_dir`] without
    /// scanning it; loading their state from disk is deferred to [`run`].
    ///
    /// [`run`]: RbeServer::run
    pub async fn new(config: RbeServerConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let cas_backend = Arc::new(
            DiskBackend::with_max_size(format!("{}/cas", config.cache_dir), config.cas_max_size_gb)
                .await
                .map_err(|e| format!("Failed to initialize CAS backend: {}", e))?,
        );
        let action_cache = Arc::new(
            DiskActionCache::new(
                format!("{}/ac", config.cache_dir),
                config.ac_max_size_gb,
                Duration::from_secs(config.ac_ttl_secs),
                config.l1_cache_capacity,
                Duration::from_secs(config.l1_cache_ttl_secs),
            )
            .await
            .map_err(|e| format!("Failed to initialize ActionCache backend: {}", e))?,
        );
        let mut cas: SharedCasBackend = cas_backend.clone();
        let mut action_cache_store: Arc<dyn ActionCacheStore> = action_cache.clone();
        if let Some(bucket) = &config.cas_s3_bucket {
            let remote = ObjectStoreBackend::s3_from_env(bucket, config.cas_s3_prefix.clone())
                .map_err(|e| format!("Failed to initialize the S3 CAS tier: {}", e))?;
            remote
                .check_access()
                .await
                .map_err(|e| format!("S3 CAS tier s3://{bucket} is not usable: {}", e))?;
            info!(
                "CAS and ActionCache tiers: s3://{}/{}{{cas,ac}}/",
                bucket, config.cas_s3_prefix
            );
            action_cache_store = Arc::new(TieredActionCache::new(
                action_cache.clone(),
                Arc::new(remote.action_cache()),
            ));
            cas = Arc::new(TieredBackend::new(cas_backend.clone(), Arc::new(remote)));
        }
        info!(
            "Cache opened at {} (cas_max_gb={} ac_max_gb={} ac_ttl_secs={})",
            config.cache_dir, config.cas_max_size_gb, config.ac_max_size_gb, config.ac_ttl_secs
        );

        let metrics = Arc::new(Metrics::new());

        let mut proxy_config = ProxyConfig::from_env();
        if std::env::var(crate::proxy::config::ENV_DIR).is_err() {
            proxy_config.dir = PathBuf::from(&config.cache_dir).join("proxy");
        }
        let proxy_cache = PullThroughCache::new(proxy_config, metrics.clone())
            .await
            .map_err(|e| format!("Failed to initialize pull-through cache: {}", e))?;

        let scheduler = Arc::new(MultiLevelScheduler::new());
        let state_manager = Arc::new(StateMachineManager::new());
        let worker_registry = Arc::new(WorkerRegistry::new());
        let results_store = Arc::new(ResultsStore::new());
        let action_cache_log = Arc::new(ActionCacheLog::new());

        let recorder =
            execution_service::ActionRecorder::new(action_cache_store.clone(), cas.clone())
                .with_metrics(metrics.clone())
                .with_action_cache_log(action_cache_log.clone());
        let execution_engine = Arc::new(
            ExecutionEngine::new(
                scheduler.clone(),
                worker_registry.clone(),
                state_manager.clone(),
                results_store.clone(),
                Arc::new(recorder),
            )
            .with_metrics(metrics.clone()),
        );

        let mut bes_config = BesConfig::from_env();
        if std::env::var("RBE_BES_DATA_DIR").is_err() {
            bes_config.data_dir = PathBuf::from(&config.cache_dir).join("bes");
        }
        let live_bus = LiveBus::new();
        let bes_storage = BesStorage::new(bes_config.clone())
            .await?
            .with_action_cache_log(action_cache_log.clone())
            .with_live_bus(live_bus.clone())
            .with_metrics(metrics.clone());
        info!("BES storage opened at {:?}", bes_config.data_dir);

        Ok(Self {
            config,
            cas_backend,
            cas,
            action_cache,
            action_cache_store,
            action_cache_log,
            proxy_cache,
            metrics,
            scheduler,
            state_manager,
            worker_registry,
            results_store,
            execution_engine,
            bes_config,
            bes_storage,
            live_bus,
        })
    }

    /// Serves until `SIGTERM` or Ctrl-C.
    ///
    /// On the signal both listeners stop accepting and in-flight requests get
    /// up to [`SHUTDOWN_DRAIN_SECS`] to finish; pending proxy download counts
    /// are then flushed. A listener that stops on its own is an error and
    /// ends the process rather than leaving the other port serving alone.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        let addr: SocketAddr =
            format!("{}:{}", self.config.bind_address, self.config.port).parse()?;

        info!(
            "FerrisRBE listening on {} (CAS, ActionCache, Remote Asset, Execution, WorkerService, BES); embedded_ui={}",
            addr,
            self.config.ui_assets.is_some()
        );

        self.execution_engine.clone().spawn();
        info!("ExecutionEngine started");

        let metrics = self.metrics.clone();
        metrics.clone().spawn_process_collector();
        spawn_cas_stats_collector(self.cas_backend.clone(), metrics.clone());
        spawn_execution_load_collector(
            self.scheduler.clone(),
            self.worker_registry.clone(),
            metrics.clone(),
        );

        let readiness = Arc::new(Readiness::new());
        self.spawn_warm_ups(readiness.clone());

        if self.proxy_cache.config().enabled {
            spawn_download_flusher(self.proxy_cache.clone());
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            shutdown_signal().await;
            info!("Shutdown signal received; draining connections");
            let _ = shutdown_tx.send(true);
        });

        let critical_actions = critical_actions::CriticalActions::from_env()?;
        if !critical_actions.is_empty() {
            info!("Critical actions: {:?}", critical_actions);
        }
        let cas = self.cas.clone();
        let action_cache_store = self.action_cache_store.clone();
        let byte_stream_service = byte_stream::ByteStreamService::new(cas.clone(), metrics.clone());
        let result_verifier = Arc::new(action_cache_service::ResultVerifier::new(cas.clone()));
        let action_cache_service = action_cache_service::ActionCacheService::with_verifier(
            action_cache_store.clone(),
            result_verifier.clone(),
            metrics.clone(),
        )
        .with_log(self.action_cache_log.clone());
        let cas_service = cas_service::CasService::new(cas.clone(), metrics.clone());
        let execution_service = execution_service::ExecutionService::new(
            self.scheduler.clone(),
            self.state_manager.clone(),
            action_cache_store,
            self.results_store.clone(),
            cas.clone(),
        )
        .with_critical_actions(critical_actions)
        .with_verifier(result_verifier)
        .with_metrics(metrics.clone())
        .with_action_cache_log(self.action_cache_log.clone());
        let capabilities_service =
            capabilities_service::CapabilitiesService::new(true, metrics.clone());
        let worker_service = worker_service::WorkerServiceImpl::new(
            self.worker_registry.clone(),
            self.execution_engine.clone(),
        );
        let remote_asset_service = remote_asset_service::RemoteAssetService::new(
            self.proxy_cache.clone(),
            self.cas_backend.clone(),
            metrics.clone(),
        );

        let auth_state = Arc::new(auth::AuthState::from_env());
        let auth_config = auth_state.config();
        info!(
            "Auth: http={} grpc={} issuer={} allowed_clients={:?}",
            if auth_config.enabled {
                "enforced"
            } else {
                "open"
            },
            if auth_config.grpc_enabled {
                "enforced"
            } else {
                "open"
            },
            auth_config.issuer(),
            auth_config.allowed_clients
        );
        let mut server_builder = GrpcTransport::from_env()
            .server_builder()?
            .layer(auth::GrpcAuthLayer::new(auth_state.clone()));

        let bes_service = BesService::new(self.bes_storage.clone());
        let grpc = server_builder
            .add_service(byte_stream_service.into_service())
            .add_service(action_cache_service.into_service())
            .add_service(cas_service.into_service())
            .add_service(execution_service.into_service())
            .add_service(capabilities_service.into_service())
            .add_service(worker_service.into_service())
            .add_service(remote_asset_service.into_service())
            .add_service(bes_service.into_service())
            .serve_with_shutdown(addr, stop_when(shutdown_rx.clone()));

        let http_addr: SocketAddr =
            format!("{}:{}", self.config.bind_address, self.config.http_port).parse()?;
        let proxy_state = Arc::new(ProxyState {
            cache: self.proxy_cache.clone(),
            metrics: metrics.clone(),
            readiness,
        });
        self.live_bus.watch_workers(self.worker_registry.clone());
        let mut http = crate::proxy::router(proxy_state)
            .merge(
                BesApi::new(self.bes_storage.clone(), self.bes_config.clone())
                    .with_cas(cas.clone())
                    .with_workers(self.worker_registry.clone())
                    .with_proxy(self.proxy_cache.clone())
                    .router(),
            )
            .merge(crate::bes::live::router(
                self.live_bus.clone(),
                self.worker_registry.clone(),
            ))
            .merge(crate::mcp::router(
                self.bes_storage.clone(),
                Some(cas.clone()),
                Some(self.worker_registry.clone()),
                Some(self.proxy_cache.clone()),
                metrics.clone(),
            ));
        if let Some(assets) = self.config.ui_assets {
            http = with_embedded_ui(http, assets);
        }
        http = http
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                auth::auth_middleware,
            ))
            .layer(TimeoutLayer::with_status_code(
                axum::http::StatusCode::REQUEST_TIMEOUT,
                Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS),
            ))
            .layer(CatchPanicLayer::new());
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        info!(
            "HTTP server listening on http://{} (BES API, UI, proxy, metrics, probes)",
            http_addr
        );
        let http_server =
            axum::serve(listener, http).with_graceful_shutdown(stop_when(shutdown_rx.clone()));

        let mut grpc_task = tokio::spawn(grpc);
        let mut http_task = tokio::spawn(http_server.into_future());
        tokio::select! {
            result = &mut grpc_task => {
                exited("gRPC", join_outcome(result), &shutdown_rx)?;
                drain("HTTP", http_task).await;
            }
            result = &mut http_task => {
                exited("HTTP", join_outcome(result), &shutdown_rx)?;
                drain("gRPC", grpc_task).await;
            }
        }
        self.proxy_cache.flush_downloads().await;
        info!("FerrisRBE stopped");
        Ok(())
    }

    /// Loads the in-memory state of every store from the PVC while the
    /// listeners already serve.
    ///
    /// Each store holds a readiness gate until its load completes; BES
    /// maintenance (sweeps and retention) starts only once BES has loaded. A
    /// load that fails is [`fatal`]: the pod restarts instead of serving a
    /// partial cache.
    fn spawn_warm_ups(&self, readiness: Arc<Readiness>) {
        readiness.gate("cas");
        readiness.gate("action-cache");
        readiness.gate("bes");
        if self.proxy_cache.config().enabled {
            readiness.gate("proxy");
        }

        let cas = self.cas_backend.clone();
        let gates = readiness.clone();
        tokio::spawn(async move {
            match cas.warm_up().await {
                Ok(()) => gates.clear("cas"),
                Err(e) => fatal("CAS warm-up failed", &e),
            }
        });

        let action_cache = self.action_cache.clone();
        let gates = readiness.clone();
        tokio::spawn(async move {
            match action_cache.warm_up().await {
                Ok(()) => gates.clear("action-cache"),
                Err(e) => fatal("ActionCache warm-up failed", &e),
            }
        });

        let bes = self.bes_storage.clone();
        let gates = readiness.clone();
        tokio::spawn(async move {
            match bes.warm_up().await {
                Ok(()) => {
                    gates.clear("bes");
                    bes.spawn_maintenance();
                    bes.spawn_live_counters();
                }
                Err(e) => fatal("BES warm-up failed", &e),
            }
        });

        if self.proxy_cache.config().enabled {
            let proxy = self.proxy_cache.clone();
            let gates = readiness.clone();
            tokio::spawn(async move {
                match proxy.warm_up().await {
                    Ok(()) => gates.clear("proxy"),
                    Err(e) => fatal("Pull-through cache warm-up failed", &e),
                }
            });
        }
    }
}

/// Logs `error` and exits with status 1.
fn fatal(what: &str, error: &dyn std::fmt::Display) -> ! {
    error!("{}: {}", what, error);
    std::process::exit(1)
}

/// Resolves once the shutdown flag is raised.
async fn stop_when(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// Resolves on `SIGTERM` from the kubelet or Ctrl-C at a terminal.
///
/// The binary runs as PID 1 in its container, and the kernel applies no
/// default action to signals sent to PID 1: without this handler `SIGTERM`
/// is ignored until the grace period expires and the pod is killed.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(e) => {
                warn!("SIGTERM handler not installed: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Flattens a listener task's join result into a single error message.
fn join_outcome<E: std::fmt::Display>(
    result: Result<Result<(), E>, tokio::task::JoinError>,
) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// A listener that stops without a shutdown signal (bind failure, panic)
/// takes the process down rather than leaving the other port serving alone.
fn exited(
    name: &str,
    outcome: Result<(), String>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    outcome.map_err(|e| format!("{name} server failed: {e}"))?;
    if !*shutdown.borrow() {
        return Err(format!("{name} server exited without a shutdown signal").into());
    }
    Ok(())
}

/// Waits up to [`SHUTDOWN_DRAIN_SECS`] for a listener to finish its
/// in-flight requests; past that the process exits without it.
async fn drain<E: std::fmt::Display>(name: &str, task: JoinHandle<Result<(), E>>) {
    match tokio::time::timeout(Duration::from_secs(SHUTDOWN_DRAIN_SECS), task).await {
        Ok(result) => {
            if let Err(e) = join_outcome(result) {
                warn!("{} server ended with an error during drain: {}", name, e);
            }
        }
        Err(_) => warn!(
            "{} server still draining after {}s; exiting anyway",
            name, SHUTDOWN_DRAIN_SECS
        ),
    }
}
