//! Lifecycle of a worker's gRPC stream to the scheduler: a connection state
//! machine, adaptive keepalive, exponential reconnection backoff, health
//! checks and connection metrics.
//!
//! Every tunable comes from an `RBE_*` environment variable listed in
//! [`env_vars`]; [`ConfigLoader`] reads them over
//! [`ConnectionConfig::default`].

pub mod adaptive_keepalive;
pub mod connection_manager;
pub mod connection_state;
pub mod health_checker;
pub mod metrics;
pub mod reconnection;

pub use connection_manager::{ConfigLoader, ConnectionConfig, ConnectionEvent, ConnectionManager};
pub use connection_state::ConnectionState;

/// Environment variables read by [`ConfigLoader`], one per
/// [`ConnectionConfig`] field. Defaults are those of
/// [`ConnectionConfig::default`].
pub mod env_vars {
    /// Keepalive interval at connection start, in seconds.
    pub const KEEPALIVE_INTERVAL_SECS: &str = "RBE_KEEPALIVE_INTERVAL_SECS";
    /// Shortest adaptive keepalive interval, in seconds.
    pub const MIN_KEEPALIVE_SECS: &str = "RBE_MIN_KEEPALIVE_SECS";
    /// Longest adaptive keepalive interval, in seconds.
    pub const MAX_KEEPALIVE_SECS: &str = "RBE_MAX_KEEPALIVE_SECS";
    /// Time allowed for a keepalive response, in seconds.
    pub const KEEPALIVE_TIMEOUT_SECS: &str = "RBE_KEEPALIVE_TIMEOUT_SECS";
    /// TCP-level keepalive, in seconds.
    pub const TCP_KEEPALIVE_SECS: &str = "RBE_TCP_KEEPALIVE_SECS";
    /// Time allowed to establish the TCP connection, in seconds.
    pub const CONNECTION_TIMEOUT_SECS: &str = "RBE_CONNECTION_TIMEOUT_SECS";
    /// Consecutive reconnection attempts before the worker exits.
    pub const MAX_RECONNECT_ATTEMPTS: &str = "RBE_MAX_RECONNECT_ATTEMPTS";
    /// First backoff delay, in milliseconds.
    pub const RECONNECT_BASE_DELAY_MS: &str = "RBE_RECONNECT_BASE_DELAY_MS";
    /// Backoff delay cap, in milliseconds.
    pub const RECONNECT_MAX_DELAY_MS: &str = "RBE_RECONNECT_MAX_DELAY_MS";
    /// Backoff jitter as a fraction of the delay, `0.0..=1.0`.
    pub const RECONNECT_JITTER_FACTOR: &str = "RBE_RECONNECT_JITTER_FACTOR";
    /// Interval between health checks, in seconds.
    pub const HEALTH_CHECK_INTERVAL_SECS: &str = "RBE_HEALTH_CHECK_INTERVAL_SECS";
    /// Time allowed for one health check, in seconds.
    pub const HEALTH_CHECK_TIMEOUT_SECS: &str = "RBE_HEALTH_CHECK_TIMEOUT_SECS";
    /// Time allowed to hand executions over after reconnecting, in seconds.
    pub const EXECUTION_HANDOFF_TIMEOUT_SECS: &str = "RBE_EXECUTION_HANDOFF_TIMEOUT_SECS";
    /// Consecutive keepalive successes before the interval grows.
    pub const ADAPTIVE_ADJUSTMENT_THRESHOLD: &str = "RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD";
    /// `true`/`1`/`yes` or `false`/`0`/`no`: whether metrics are recorded.
    pub const ENABLE_METRICS: &str = "RBE_ENABLE_METRICS";
}
