use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use super::adaptive_keepalive::AdaptiveKeepalive;
use super::connection_state::{ConnectionState, StateMachine};
#[allow(unused_imports)]
use super::health_checker::{HealthCheckConfig, HealthChecker};
use super::metrics::ConnectionMetrics;
#[allow(unused_imports)]
use super::reconnection::{ReconnectionPolicy, ReconnectionStrategy};
use std::env;

/// Configuration for resilient connection management
#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    /// Initial keepalive interval in seconds
    pub initial_keepalive_interval_secs: u64,
    /// Minimum keepalive interval (most aggressive)
    pub min_keepalive_interval_secs: u64,
    /// Maximum keepalive interval (most conservative)
    pub max_keepalive_interval_secs: u64,
    /// Timeout for keepalive responses
    pub keepalive_timeout_secs: u64,
    /// TCP keepalive interval (separate from HTTP/2 keepalive)
    pub tcp_keepalive_secs: u64,
    /// TCP connection timeout
    pub connection_timeout_secs: u64,
    /// Maximum reconnection attempts before giving up
    pub max_reconnect_attempts: u32,
    /// Base delay for exponential backoff (ms)
    pub reconnect_base_delay_ms: u64,
    /// Maximum delay for exponential backoff (ms)
    pub reconnect_max_delay_ms: u64,
    /// Jitter factor (0.0-1.0) for randomizing delays
    pub reconnect_jitter_factor: f64,
    /// Health check interval in seconds
    #[allow(dead_code)]
    pub health_check_interval_secs: u64,
    /// Health check timeout in seconds
    #[allow(dead_code)]
    pub health_check_timeout_secs: u64,
    /// Timeout for execution handoff during reconnection
    #[allow(dead_code)]
    pub execution_handoff_timeout_secs: u64,
    /// Threshold of consecutive successes before increasing keepalive interval
    pub adaptive_adjustment_threshold: u32,
    /// Enable metrics collection
    pub enable_metrics: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            initial_keepalive_interval_secs: 15,
            min_keepalive_interval_secs: 5,
            max_keepalive_interval_secs: 60,
            keepalive_timeout_secs: 10,
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

/// Events that can occur during connection lifecycle
#[derive(Debug)]
pub enum ConnectionEvent {
    /// Successfully connected to server
    Connected,
    /// Connection lost, will attempt reconnection
    Disconnected { reason: String },
    /// Reconnection attempt started
    Reconnecting {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// Maximum reconnection attempts exceeded
    Failed { reason: String },
    /// Health check succeeded
    #[allow(dead_code)]
    HealthCheckSuccess { rtt_ms: u64 },
    /// Health check failed
    #[allow(dead_code)]
    HealthCheckFailed { reason: String },
    /// Execution handoff completed (after reconnection)
    #[allow(dead_code)]
    ExecutionHandoffCompleted { count: usize },
    /// Network degradation detected
    #[allow(dead_code)]
    NetworkDegraded { message: String },
    /// Network recovered
    #[allow(dead_code)]
    NetworkRecovered,
}

/// Manages resilient connection lifecycle for a worker
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
            config: config.clone(),
            state_machine: Arc::new(RwLock::new(StateMachine::new())),
            adaptive_keepalive: Arc::new(RwLock::new(adaptive_keepalive)),
            metrics: Arc::new(RwLock::new(ConnectionMetrics::new(config.enable_metrics))),
            event_tx,
            event_rx: Arc::new(RwLock::new(event_rx)),
            current_channel: Arc::new(RwLock::new(None)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
        }
    }

    /// Get current connection state
    #[allow(dead_code)]
    pub async fn current_state(&self) -> ConnectionState {
        self.state_machine.read().await.current()
    }

    /// Check if connection is operational
    pub async fn is_operational(&self) -> bool {
        self.state_machine.read().await.current().is_operational()
    }

    /// Get current keepalive interval
    pub async fn current_keepalive_interval(&self) -> Duration {
        self.adaptive_keepalive.read().await.current_interval()
    }

    /// Get connection statistics
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

    /// Event channel for monitoring connection lifecycle
    pub fn event_receiver(&self) -> Arc<RwLock<mpsc::Receiver<ConnectionEvent>>> {
        self.event_rx.clone()
    }

    /// Record successful connection establishment
    pub async fn record_connected(&self, channel: Channel) {
        let mut state = self.state_machine.write().await;
        let mut metrics = self.metrics.write().await;
        let mut keepalive = self.adaptive_keepalive.write().await;
        let mut reconnect = self.reconnect_attempts.write().await;

        *reconnect = 0;

        // Record connection in metrics
        metrics.record_connection_established();

        keepalive.reset(self.config.initial_keepalive_interval_secs);

        // Update state
        state.transition_to(
            ConnectionState::Active,
            Some("Successfully connected to server".to_string()),
        );

        *self.current_channel.write().await = Some(channel);

        // Emit event
        let _ = self.event_tx.send(ConnectionEvent::Connected).await;

        info!("Connection established and operational");
    }

    /// Record disconnection and initiate reconnection if appropriate
    pub async fn record_disconnected(&self, reason: String) {
        let mut state = self.state_machine.write().await;
        let mut metrics = self.metrics.write().await;

        metrics.record_disconnection();

        // Check if we should attempt reconnection
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

    /// Calculate next reconnection delay with exponential backoff and jitter
    pub async fn next_reconnect_delay(&self) -> Duration {
        let attempt = *self.reconnect_attempts.read().await;

        let base_delay = self.config.reconnect_base_delay_ms;
        let max_delay = self.config.reconnect_max_delay_ms;

        let exponential_delay = base_delay * (1_u64 << attempt.min(10));
        let capped_delay = exponential_delay.min(max_delay);

        let jitter_range = (capped_delay as f64 * self.config.reconnect_jitter_factor) as u64;
        let jitter = if jitter_range > 0 {
            let r = fastrand::u64(0..(jitter_range * 2));
            r as i64 - jitter_range as i64
        } else {
            0
        };

        let final_delay = (capped_delay as i64 + jitter).max(0) as u64;

        Duration::from_millis(final_delay)
    }

    /// Increment reconnection attempt counter
    pub async fn increment_reconnect_attempt(&self) {
        let mut attempts = self.reconnect_attempts.write().await;
        *attempts += 1;

        let delay = self.next_reconnect_delay().await;

        let _ = self
            .event_tx
            .send(ConnectionEvent::Reconnecting {
                attempt: *attempts,
                max_attempts: self.config.max_reconnect_attempts,
                delay_ms: delay.as_millis() as u64,
            })
            .await;

        info!(
            "Reconnection attempt {}/{} with delay {:?}",
            *attempts, self.config.max_reconnect_attempts, delay
        );
    }

    /// Check if max reconnection attempts exceeded
    pub async fn is_max_reconnect_exceeded(&self) -> bool {
        *self.reconnect_attempts.read().await >= self.config.max_reconnect_attempts
    }

    /// Get direct access to state machine for transitions
    pub fn state_machine(&self) -> Arc<RwLock<StateMachine>> {
        self.state_machine.clone()
    }

    /// Record successful health check
    #[allow(dead_code)]
    pub async fn record_health_check_success(&self, rtt: Duration) {
        let mut keepalive = self.adaptive_keepalive.write().await;
        keepalive.record_success(rtt);

        let _ = self
            .event_tx
            .send(ConnectionEvent::HealthCheckSuccess {
                rtt_ms: rtt.as_millis() as u64,
            })
            .await;

        debug!("Health check succeeded: RTT={:?}", rtt);
    }

    /// Record failed health check
    #[allow(dead_code)]
    pub async fn record_health_check_failure(&self, reason: String) {
        let mut keepalive = self.adaptive_keepalive.write().await;
        let mut metrics = self.metrics.write().await;

        keepalive.record_failure();
        metrics.record_failed_health_check();

        let _ = self
            .event_tx
            .send(ConnectionEvent::HealthCheckFailed {
                reason: reason.clone(),
            })
            .await;

        warn!("Health check failed: {}", reason);

        if keepalive.is_network_degraded() {
            let mut state = self.state_machine.write().await;
            if state.current() == ConnectionState::Active {
                state.transition_to(
                    ConnectionState::Degraded,
                    Some("Network degradation detected".to_string()),
                );

                let _ = self
                    .event_tx
                    .send(ConnectionEvent::NetworkDegraded {
                        message: "High latency or packet loss detected".to_string(),
                    })
                    .await;
            }
        }
    }

    /// Record execution handoff completion
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

/// Statistics snapshot for observability
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub state: ConnectionState,
    pub state_duration_secs: u64,
    pub total_transitions: u64,
    pub reconnect_attempts: u32,
    pub keepalive_interval_secs: u64,
    #[allow(dead_code)]
    pub keepalive_stats: super::adaptive_keepalive::KeepaliveStats,
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

/// Configuration loader with environment variable support
///
/// All connection parameters can be configured via environment variables.
/// This follows the pattern from buildfarm's PR #2494.
pub struct ConfigLoader;

impl ConfigLoader {
    /// Load configuration from environment variables with sensible defaults
    ///
    /// All RBE_* environment variables are read and applied over the defaults.
    /// This follows 12-Factor App principles for configuration.
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
        ConnectionConfig {
            initial_keepalive_interval_secs: Self::parse_env_u64("RBE_KEEPALIVE_INTERVAL_SECS", 20),
            min_keepalive_interval_secs: Self::parse_env_u64("RBE_MIN_KEEPALIVE_SECS", 10),
            max_keepalive_interval_secs: Self::parse_env_u64("RBE_MAX_KEEPALIVE_SECS", 60),
            keepalive_timeout_secs: Self::parse_env_u64("RBE_KEEPALIVE_TIMEOUT_SECS", 15),
            tcp_keepalive_secs: Self::parse_env_u64("RBE_TCP_KEEPALIVE_SECS", 30),
            connection_timeout_secs: Self::parse_env_u64("RBE_CONNECTION_TIMEOUT_SECS", 30),
            max_reconnect_attempts: Self::parse_env_u32("RBE_MAX_RECONNECT_ATTEMPTS", 10),
            reconnect_base_delay_ms: Self::parse_env_u64("RBE_RECONNECT_BASE_DELAY_MS", 100),
            reconnect_max_delay_ms: Self::parse_env_u64("RBE_RECONNECT_MAX_DELAY_MS", 30000),
            reconnect_jitter_factor: Self::parse_env_f64("RBE_RECONNECT_JITTER_FACTOR", 0.25),
            health_check_interval_secs: Self::parse_env_u64("RBE_HEALTH_CHECK_INTERVAL_SECS", 5),
            health_check_timeout_secs: Self::parse_env_u64("RBE_HEALTH_CHECK_TIMEOUT_SECS", 3),
            execution_handoff_timeout_secs: Self::parse_env_u64(
                "RBE_EXECUTION_HANDOFF_TIMEOUT_SECS",
                60,
            ),
            adaptive_adjustment_threshold: Self::parse_env_u32(
                "RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD",
                3,
            ),
            enable_metrics: Self::parse_env_bool("RBE_ENABLE_METRICS", true),
        }
    }

    fn parse_env_u64(var: &str, default: u64) -> u64 {
        match env::var(var) {
            Ok(val) => val.parse::<u64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid value for {}: '{}', using default {}",
                    var, val, default
                );
                default
            }),
            Err(_) => default,
        }
    }

    fn parse_env_u32(var: &str, default: u32) -> u32 {
        match env::var(var) {
            Ok(val) => val.parse::<u32>().unwrap_or_else(|_| {
                warn!(
                    "Invalid value for {}: '{}', using default {}",
                    var, val, default
                );
                default
            }),
            Err(_) => default,
        }
    }

    fn parse_env_f64(var: &str, default: f64) -> f64 {
        match env::var(var) {
            Ok(val) => val.parse::<f64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid value for {}: '{}', using default {}",
                    var, val, default
                );
                default
            }),
            Err(_) => default,
        }
    }

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

    /// Print available configuration options
    pub fn print_available_options() {
        info!("Available RBE_* environment variables:");
        info!("  RBE_KEEPALIVE_INTERVAL_SECS      - Keepalive interval in seconds (default: 20)");
        info!("  RBE_MIN_KEEPALIVE_SECS           - Minimum keepalive interval (default: 10)");
        info!("  RBE_MAX_KEEPALIVE_SECS           - Maximum keepalive interval (default: 60)");
        info!("  RBE_KEEPALIVE_TIMEOUT_SECS       - Keepalive response timeout (default: 15)");
        info!("  RBE_CONNECTION_TIMEOUT_SECS      - TCP connection timeout (default: 30)");
        info!("  RBE_MAX_RECONNECT_ATTEMPTS       - Max reconnection attempts (default: 10)");
        info!("  RBE_RECONNECT_BASE_DELAY_MS      - Base reconnect delay ms (default: 100)");
        info!("  RBE_RECONNECT_MAX_DELAY_MS       - Max reconnect delay ms (default: 30000)");
        info!("  RBE_RECONNECT_JITTER_FACTOR      - Jitter factor 0.0-1.0 (default: 0.25)");
        info!("  RBE_HEALTH_CHECK_INTERVAL_SECS   - Health check interval (default: 5)");
        info!("  RBE_HEALTH_CHECK_TIMEOUT_SECS    - Health check timeout (default: 3)");
        info!("  RBE_EXECUTION_HANDOFF_TIMEOUT_SECS - Handoff timeout (default: 60)");
        info!("  RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD - Adaptive threshold (default: 3)");
        info!("  RBE_ENABLE_METRICS               - Enable metrics true/false (default: true)");
    }
}
