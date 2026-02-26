//! Resilient Worker Connection Architecture
//!
//! Provides enterprise-grade connection management for RBE workers including:
//! - Configurable keepalive via environment variables
//! - Bidirectional health checking
//! - Zero-downtime reconnection with execution handoff
//! - Comprehensive metrics and observability
//!
//! Configuration is 100% environment-based following 12-Factor App principles.
//! All timeouts, intervals, and thresholds can be configured via env vars.

pub mod adaptive_keepalive;
pub mod connection_manager;
pub mod connection_state;
pub mod health_checker;
pub mod metrics;
pub mod reconnection;

pub use adaptive_keepalive::AdaptiveKeepalive;
pub use connection_manager::{ConfigLoader, ConnectionConfig, ConnectionManager, ConnectionStats, ConnectionEvent};
pub use connection_state::ConnectionState;
pub use health_checker::{HealthCheckConfig, HealthChecker};
pub use metrics::ConnectionMetrics;
pub use reconnection::{ReconnectionPolicy, ReconnectionStrategy};

/// Default configuration - production-ready sensible defaults
///
/// These values are used when environment variables are not set.
/// All values can be overridden via environment variables (see ConfigLoader).
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

/// Environment variable names for configuration
pub mod env_vars {
    /// Interval between keepalive pings in seconds (default: 20)
    pub const KEEPALIVE_INTERVAL_SECS: &str = "RBE_KEEPALIVE_INTERVAL_SECS";
    /// Minimum keepalive interval in seconds (default: 10)
    pub const MIN_KEEPALIVE_SECS: &str = "RBE_MIN_KEEPALIVE_SECS";
    /// Maximum keepalive interval in seconds (default: 60)
    pub const MAX_KEEPALIVE_SECS: &str = "RBE_MAX_KEEPALIVE_SECS";
    /// Timeout for keepalive responses in seconds (default: 15)
    pub const KEEPALIVE_TIMEOUT_SECS: &str = "RBE_KEEPALIVE_TIMEOUT_SECS";
    /// TCP connection timeout in seconds (default: 30)
    pub const CONNECTION_TIMEOUT_SECS: &str = "RBE_CONNECTION_TIMEOUT_SECS";
    /// Maximum reconnection attempts before giving up (default: 10)
    pub const MAX_RECONNECT_ATTEMPTS: &str = "RBE_MAX_RECONNECT_ATTEMPTS";
    /// Base delay for exponential backoff in milliseconds (default: 100)
    pub const RECONNECT_BASE_DELAY_MS: &str = "RBE_RECONNECT_BASE_DELAY_MS";
    /// Maximum delay for exponential backoff in milliseconds (default: 30000)
    pub const RECONNECT_MAX_DELAY_MS: &str = "RBE_RECONNECT_MAX_DELAY_MS";
    /// Jitter factor for randomizing delays, 0.0-1.0 (default: 0.25)
    pub const RECONNECT_JITTER_FACTOR: &str = "RBE_RECONNECT_JITTER_FACTOR";
    /// Health check interval in seconds (default: 5)
    pub const HEALTH_CHECK_INTERVAL_SECS: &str = "RBE_HEALTH_CHECK_INTERVAL_SECS";
    /// Health check timeout in seconds (default: 3)
    pub const HEALTH_CHECK_TIMEOUT_SECS: &str = "RBE_HEALTH_CHECK_TIMEOUT_SECS";
    /// Execution handoff timeout during reconnection in seconds (default: 60)
    pub const EXECUTION_HANDOFF_TIMEOUT_SECS: &str = "RBE_EXECUTION_HANDOFF_TIMEOUT_SECS";
    /// Threshold for adaptive keepalive adjustment (default: 3)
    pub const ADAPTIVE_ADJUSTMENT_THRESHOLD: &str = "RBE_ADAPTIVE_ADJUSTMENT_THRESHOLD";
    /// Enable metrics collection, "true" or "false" (default: true)
    pub const ENABLE_METRICS: &str = "RBE_ENABLE_METRICS";
}
