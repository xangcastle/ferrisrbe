//! Lifecycle of the worker's connection to the scheduler and its
//! environment-driven configuration.

use std::env;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use super::adaptive_keepalive::{AdaptiveKeepalive, KeepaliveStats};
use super::connection_state::{ConnectionState, StateMachine};
use super::env_vars;
use super::metrics::ConnectionMetrics;
use super::reconnection::exponential_backoff;

/// Tunables of the worker connection. [`ConfigLoader::load`] fills each one
/// from its `RBE_*` variable in [`env_vars`], falling back to
/// [`ConnectionConfig::default`].
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// HTTP/2 keepalive interval at connection start, in seconds.
    pub initial_keepalive_interval_secs: u64,
    /// Shortest interval the adaptive keepalive may shrink to, in seconds.
    pub min_keepalive_interval_secs: u64,
    /// Longest interval the adaptive keepalive may grow to, in seconds.
    pub max_keepalive_interval_secs: u64,
    /// Time allowed for a keepalive response, in seconds.
    pub keepalive_timeout_secs: u64,
    /// TCP-level keepalive, independent of HTTP/2 keepalive, in seconds.
    pub tcp_keepalive_secs: u64,
    /// Time allowed to establish the TCP connection, in seconds.
    pub connection_timeout_secs: u64,
    /// Consecutive reconnection attempts before the worker gives up.
    pub max_reconnect_attempts: u32,
    /// Backoff delay of the first reconnection attempt, in milliseconds.
    pub reconnect_base_delay_ms: u64,
    /// Upper bound of the backoff delay, in milliseconds.
    pub reconnect_max_delay_ms: u64,
    /// Fraction of each backoff delay randomized in either direction, `0.0..=1.0`.
    pub reconnect_jitter_factor: f64,
    /// Interval between application-level health checks, in seconds.
    #[allow(dead_code)]
    pub health_check_interval_secs: u64,
    /// Time allowed for one health check, in seconds.
    #[allow(dead_code)]
    pub health_check_timeout_secs: u64,
    /// Time allowed to hand in-flight executions over after reconnecting, in seconds.
    #[allow(dead_code)]
    pub execution_handoff_timeout_secs: u64,
    /// Consecutive keepalive successes required before the interval grows.
    pub adaptive_adjustment_threshold: u32,
    /// Whether [`ConnectionMetrics`] records anything.
    pub enable_metrics: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            initial_keepalive_interval_secs: 20,
            min_keepalive_interval_secs: 10,
            max_keepalive_interval_secs: 60,
            keepalive_timeout_secs: 15,
            tcp_keepalive_secs: 30,
            connection_timeout_secs: 30,
            max_reconnect_attempts: 10,
            reconnect_base_delay_ms: 100,
            reconnect_max_delay_ms: 30_000,
            reconnect_jitter_factor: 0.25,
            health_check_interval_secs: 5,
            health_check_timeout_secs: 3,
            execution_handoff_timeout_secs: 60,
            adaptive_adjustment_threshold: 3,
            enable_metrics: true,
        }
    }
}

/// Connection lifecycle notifications, delivered through
/// [`ConnectionManager::event_receiver`].
#[derive(Debug)]
pub enum ConnectionEvent {
    /// The stream to the scheduler is established.
    Connected,
    /// The stream was lost and a reconnection will follow.
    Disconnected { reason: String },
    /// A reconnection attempt is scheduled after `delay_ms`.
    Reconnecting {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// The connection was lost from a state that does not reconnect.
    Failed { reason: String },
    /// A health check succeeded with the given round trip.
    #[allow(dead_code)]
    HealthCheckSuccess { rtt_ms: u64 },
    /// A health check failed.
    #[allow(dead_code)]
    HealthCheckFailed { reason: String },
    /// In-flight executions were handed over after a reconnection.
    #[allow(dead_code)]
    ExecutionHandoffCompleted { count: usize },
    /// Keepalive round trips indicate a degraded network.
    #[allow(dead_code)]
    NetworkDegraded { message: String },
    /// The network left the degraded state.
    #[allow(dead_code)]
    NetworkRecovered,
}

/// Connection state, keepalive, metrics and reconnection accounting of one
/// worker, shared between its work loop and background tasks.
///
/// Lock discipline: [`record_connected`](Self::record_connected) and
/// [`record_disconnected`](Self::record_disconnected) acquire several locks
/// in the fixed order state machine, metrics, keepalive, reconnect counter.
/// Every other method holds at most one lock at a time, so no two methods
/// can wait on each other.
pub struct ConnectionManager {
    config: ConnectionConfig,
    state_machine: Arc<RwLock<StateMachine>>,
    adaptive_keepalive: Arc<RwLock<AdaptiveKeepalive>>,
    metrics: Arc<RwLock<ConnectionMetrics>>,
    event_tx: mpsc::Sender<ConnectionEvent>,
    event_rx: Arc<RwLock<mpsc::Receiver<ConnectionEvent>>>,
    current_channel: Arc<RwLock<Option<Channel>>>,
    reconnect_attempts: Arc<RwLock<u32>>,
}

impl ConnectionManager {
    pub fn new(config: ConnectionConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);
        let adaptive_keepalive = AdaptiveKeepalive::new(
            config.initial_keepalive_interval_secs,
            config.min_keepalive_interval_secs,
            config.max_keepalive_interval_secs,
            config.adaptive_adjustment_threshold,
        );

        Self {
            state_machine: Arc::new(RwLock::new(StateMachine::new())),
            adaptive_keepalive: Arc::new(RwLock::new(adaptive_keepalive)),
            metrics: Arc::new(RwLock::new(ConnectionMetrics::new(config.enable_metrics))),
            event_tx,
            event_rx: Arc::new(RwLock::new(event_rx)),
            current_channel: Arc::new(RwLock::new(None)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            config,
        }
    }

    #[allow(dead_code)]
    pub async fn current_state(&self) -> ConnectionState {
        self.state_machine.read().await.current()
    }

    /// Whether the connection can accept work assignments.
    pub async fn is_operational(&self) -> bool {
        self.state_machine.read().await.current().is_operational()
    }

    /// The keepalive interval currently chosen by the adaptive keepalive.
    pub async fn current_keepalive_interval(&self) -> Duration {
        self.adaptive_keepalive.read().await.current_interval()
    }

    /// A point-in-time snapshot for logs.
    pub async fn stats(&self) -> ConnectionStats {
        let state = self.state_machine.read().await;
        let keepalive = self.adaptive_keepalive.read().await;
        let metrics = self.metrics.read().await;
        let attempts = *self.reconnect_attempts.read().await;

        ConnectionStats {
            state: state.current(),
            state_duration_secs: state.duration_in_current().as_secs(),
            total_transitions: state.transition_count(),
            reconnect_attempts: attempts,
            keepalive_interval_secs: keepalive.current_interval().as_secs(),
            keepalive_stats: keepalive.stats(),
            total_connections_established: metrics.total_connections_established,
            total_reconnections: metrics.total_reconnections,
            total_disconnections: metrics.total_disconnections,
            failed_health_checks: metrics.failed_health_checks,
            average_connection_duration_secs: metrics.average_connection_duration_secs(),
        }
    }

    /// The receiving end of the [`ConnectionEvent`] channel. A single
    /// consumer is expected; it holds the lock for as long as it listens.
    pub fn event_receiver(&self) -> Arc<RwLock<mpsc::Receiver<ConnectionEvent>>> {
        self.event_rx.clone()
    }

    /// Marks the connection established over `channel`: resets the
    /// reconnection counter and the keepalive, and enters `Active`.
    pub async fn record_connected(&self, channel: Channel) {
        let mut state = self.state_machine.write().await;
        let mut metrics = self.metrics.write().await;
        let mut keepalive = self.adaptive_keepalive.write().await;
        let mut reconnect = self.reconnect_attempts.write().await;

        *reconnect = 0;
        metrics.record_connection_established();
        keepalive.reset(self.config.initial_keepalive_interval_secs);
        state.transition_to(
            ConnectionState::Active,
            Some("Successfully connected to server".to_string()),
        );
        *self.current_channel.write().await = Some(channel);

        let _ = self.event_tx.send(ConnectionEvent::Connected).await;
        info!("Connection established and operational");
    }

    /// Marks the connection lost. Enters `Reconnecting` when the current state
    /// allows reconnection and `Failed` otherwise, emitting the matching event.
    pub async fn record_disconnected(&self, reason: String) {
        let mut state = self.state_machine.write().await;
        self.metrics.write().await.record_disconnection();

        if state.should_reconnect() {
            state.transition_to(
                ConnectionState::Reconnecting,
                Some(format!("Disconnected: {}", reason)),
            );
            let _ = self
                .event_tx
                .send(ConnectionEvent::Disconnected {
                    reason: reason.clone(),
                })
                .await;
            info!("Connection lost ({}), initiating reconnection...", reason);
        } else {
            state.transition_to(
                ConnectionState::Failed,
                Some(format!("Disconnected permanently: {}", reason)),
            );
            let _ = self
                .event_tx
                .send(ConnectionEvent::Failed {
                    reason: reason.clone(),
                })
                .await;
            error!("Connection lost permanently: {}", reason);
        }

        *self.current_channel.write().await = None;
    }

    /// Counts a new reconnection attempt and returns how long to wait before
    /// it: exponential backoff from `reconnect_base_delay_ms`, doubling per
    /// previous attempt up to `reconnect_max_delay_ms`, with jitter. Emits
    /// [`ConnectionEvent::Reconnecting`] carrying the same delay.
    pub async fn begin_reconnect_attempt(&self) -> Duration {
        let attempt = {
            let mut attempts = self.reconnect_attempts.write().await;
            *attempts += 1;
            *attempts
        };
        let delay = exponential_backoff(
            attempt - 1,
            self.config.reconnect_base_delay_ms,
            self.config.reconnect_max_delay_ms,
            self.config.reconnect_jitter_factor,
        );

        let _ = self
            .event_tx
            .send(ConnectionEvent::Reconnecting {
                attempt,
                max_attempts: self.config.max_reconnect_attempts,
                delay_ms: delay.as_millis() as u64,
            })
            .await;
        info!(
            "Reconnection attempt {}/{} with delay {:?}",
            attempt, self.config.max_reconnect_attempts, delay
        );
        delay
    }

    /// Whether `max_reconnect_attempts` consecutive attempts have been made.
    pub async fn is_max_reconnect_exceeded(&self) -> bool {
        *self.reconnect_attempts.read().await >= self.config.max_reconnect_attempts
    }

    /// Shared handle to the state machine, for transitions driven by the
    /// work loop.
    pub fn state_machine(&self) -> Arc<RwLock<StateMachine>> {
        self.state_machine.clone()
    }

    /// Feeds a successful health check's round trip to the adaptive keepalive.
    #[allow(dead_code)]
    pub async fn record_health_check_success(&self, rtt: Duration) {
        self.adaptive_keepalive.write().await.record_success(rtt);
        let _ = self
            .event_tx
            .send(ConnectionEvent::HealthCheckSuccess {
                rtt_ms: rtt.as_millis() as u64,
            })
            .await;
        debug!("Health check succeeded: RTT={:?}", rtt);
    }

    /// Records a failed health check. When the keepalive then reports a
    /// degraded network, an `Active` connection moves to `Degraded`.
    #[allow(dead_code)]
    pub async fn record_health_check_failure(&self, reason: String) {
        let network_degraded = {
            let mut keepalive = self.adaptive_keepalive.write().await;
            keepalive.record_failure();
            keepalive.is_network_degraded()
        };
        self.metrics.write().await.record_failed_health_check();

        let _ = self
            .event_tx
            .send(ConnectionEvent::HealthCheckFailed {
                reason: reason.clone(),
            })
            .await;
        warn!("Health check failed: {}", reason);

        if !network_degraded {
            return;
        }
        let entered_degraded = {
            let mut state = self.state_machine.write().await;
            let active = state.current() == ConnectionState::Active;
            if active {
                state.transition_to(
                    ConnectionState::Degraded,
                    Some("Network degradation detected".to_string()),
                );
            }
            active
        };
        if entered_degraded {
            let _ = self
                .event_tx
                .send(ConnectionEvent::NetworkDegraded {
                    message: "High latency or packet loss detected".to_string(),
                })
                .await;
        }
    }

    /// Reports that `count` in-flight executions were handed over after a
    /// reconnection.
    #[allow(dead_code)]
    pub async fn record_execution_handoff(&self, count: usize) {
        let _ = self
            .event_tx
            .send(ConnectionEvent::ExecutionHandoffCompleted { count })
            .await;
        if count > 0 {
            info!(
                "Successfully handed off {} executions after reconnection",
                count
            );
        }
    }
}

/// Snapshot returned by [`ConnectionManager::stats`].
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub state: ConnectionState,
    pub state_duration_secs: u64,
    pub total_transitions: u64,
    pub reconnect_attempts: u32,
    pub keepalive_interval_secs: u64,
    #[allow(dead_code)]
    pub keepalive_stats: KeepaliveStats,
    #[allow(dead_code)]
    pub total_connections_established: u64,
    pub total_reconnections: u64,
    #[allow(dead_code)]
    pub total_disconnections: u64,
    #[allow(dead_code)]
    pub failed_health_checks: u64,
    #[allow(dead_code)]
    pub average_connection_duration_secs: f64,
}

/// Builds a [`ConnectionConfig`] from the environment.
pub struct ConfigLoader;

impl ConfigLoader {
    /// Reads every variable in [`env_vars`]. An unset variable takes its
    /// default; an unparsable one takes its default and logs a warning.
    pub fn load() -> ConnectionConfig {
        info!("Loading connection configuration from environment...");
        let config = Self::load_from_env();
        info!(
            "Configuration loaded: keepalive={}s, timeout={}s, max_reconnects={}",
            config.initial_keepalive_interval_secs,
            config.connection_timeout_secs,
            config.max_reconnect_attempts
        );
        config
    }

    fn load_from_env() -> ConnectionConfig {
        let d = ConnectionConfig::default();
        ConnectionConfig {
            initial_keepalive_interval_secs: parse_env(
                env_vars::KEEPALIVE_INTERVAL_SECS,
                d.initial_keepalive_interval_secs,
            ),
            min_keepalive_interval_secs: parse_env(
                env_vars::MIN_KEEPALIVE_SECS,
                d.min_keepalive_interval_secs,
            ),
            max_keepalive_interval_secs: parse_env(
                env_vars::MAX_KEEPALIVE_SECS,
                d.max_keepalive_interval_secs,
            ),
            keepalive_timeout_secs: parse_env(
                env_vars::KEEPALIVE_TIMEOUT_SECS,
                d.keepalive_timeout_secs,
            ),
            tcp_keepalive_secs: parse_env(env_vars::TCP_KEEPALIVE_SECS, d.tcp_keepalive_secs),
            connection_timeout_secs: parse_env(
                env_vars::CONNECTION_TIMEOUT_SECS,
                d.connection_timeout_secs,
            ),
            max_reconnect_attempts: parse_env(
                env_vars::MAX_RECONNECT_ATTEMPTS,
                d.max_reconnect_attempts,
            ),
            reconnect_base_delay_ms: parse_env(
                env_vars::RECONNECT_BASE_DELAY_MS,
                d.reconnect_base_delay_ms,
            ),
            reconnect_max_delay_ms: parse_env(
                env_vars::RECONNECT_MAX_DELAY_MS,
                d.reconnect_max_delay_ms,
            ),
            reconnect_jitter_factor: parse_env(
                env_vars::RECONNECT_JITTER_FACTOR,
                d.reconnect_jitter_factor,
            ),
            health_check_interval_secs: parse_env(
                env_vars::HEALTH_CHECK_INTERVAL_SECS,
                d.health_check_interval_secs,
            ),
            health_check_timeout_secs: parse_env(
                env_vars::HEALTH_CHECK_TIMEOUT_SECS,
                d.health_check_timeout_secs,
            ),
            execution_handoff_timeout_secs: parse_env(
                env_vars::EXECUTION_HANDOFF_TIMEOUT_SECS,
                d.execution_handoff_timeout_secs,
            ),
            adaptive_adjustment_threshold: parse_env(
                env_vars::ADAPTIVE_ADJUSTMENT_THRESHOLD,
                d.adaptive_adjustment_threshold,
            ),
            enable_metrics: parse_env_bool(env_vars::ENABLE_METRICS, d.enable_metrics),
        }
    }

    /// Logs every `RBE_*` variable with its default.
    pub fn print_available_options() {
        let d = ConnectionConfig::default();
        let options: [(&str, &str, String); 15] = [
            (
                env_vars::KEEPALIVE_INTERVAL_SECS,
                "Keepalive interval in seconds",
                d.initial_keepalive_interval_secs.to_string(),
            ),
            (
                env_vars::MIN_KEEPALIVE_SECS,
                "Minimum keepalive interval",
                d.min_keepalive_interval_secs.to_string(),
            ),
            (
                env_vars::MAX_KEEPALIVE_SECS,
                "Maximum keepalive interval",
                d.max_keepalive_interval_secs.to_string(),
            ),
            (
                env_vars::KEEPALIVE_TIMEOUT_SECS,
                "Keepalive response timeout",
                d.keepalive_timeout_secs.to_string(),
            ),
            (
                env_vars::TCP_KEEPALIVE_SECS,
                "TCP keepalive interval",
                d.tcp_keepalive_secs.to_string(),
            ),
            (
                env_vars::CONNECTION_TIMEOUT_SECS,
                "TCP connection timeout",
                d.connection_timeout_secs.to_string(),
            ),
            (
                env_vars::MAX_RECONNECT_ATTEMPTS,
                "Max reconnection attempts",
                d.max_reconnect_attempts.to_string(),
            ),
            (
                env_vars::RECONNECT_BASE_DELAY_MS,
                "Base reconnect delay ms",
                d.reconnect_base_delay_ms.to_string(),
            ),
            (
                env_vars::RECONNECT_MAX_DELAY_MS,
                "Max reconnect delay ms",
                d.reconnect_max_delay_ms.to_string(),
            ),
            (
                env_vars::RECONNECT_JITTER_FACTOR,
                "Jitter factor 0.0-1.0",
                d.reconnect_jitter_factor.to_string(),
            ),
            (
                env_vars::HEALTH_CHECK_INTERVAL_SECS,
                "Health check interval",
                d.health_check_interval_secs.to_string(),
            ),
            (
                env_vars::HEALTH_CHECK_TIMEOUT_SECS,
                "Health check timeout",
                d.health_check_timeout_secs.to_string(),
            ),
            (
                env_vars::EXECUTION_HANDOFF_TIMEOUT_SECS,
                "Handoff timeout",
                d.execution_handoff_timeout_secs.to_string(),
            ),
            (
                env_vars::ADAPTIVE_ADJUSTMENT_THRESHOLD,
                "Adaptive threshold",
                d.adaptive_adjustment_threshold.to_string(),
            ),
            (
                env_vars::ENABLE_METRICS,
                "Enable metrics true/false",
                d.enable_metrics.to_string(),
            ),
        ];
        info!("Available RBE_* environment variables:");
        for (name, description, default) in options {
            info!("  {:<34} - {} (default: {})", name, description, default);
        }
    }
}

fn parse_env<T: FromStr + Display>(var: &str, default: T) -> T {
    match env::var(var) {
        Ok(val) => val.parse::<T>().unwrap_or_else(|_| {
            warn!(
                "Invalid value for {}: '{}', using default {}",
                var, val, default
            );
            default
        }),
        Err(_) => default,
    }
}

/// Accepts `true`/`1`/`yes` and `false`/`0`/`no`, case-insensitively.
fn parse_env_bool(var: &str, default: bool) -> bool {
    match env::var(var) {
        Ok(val) => match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => {
                warn!(
                    "Invalid value for {}: '{}', using default {}",
                    var, val, default
                );
                default
            }
        },
        Err(_) => default,
    }
}
