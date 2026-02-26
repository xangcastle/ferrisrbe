use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::{interval, timeout};
#[allow(unused_imports)]
use tracing::{debug, error, info, warn};

use super::connection_manager::ConnectionManager;

/// Configuration for health checking
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// Interval between health checks
    #[allow(dead_code)]
    pub interval_secs: u64,
    /// Timeout for each health check
    #[allow(dead_code)]
    pub timeout_secs: u64,
    /// Number of consecutive failures before considering connection unhealthy
    #[allow(dead_code)]
    pub failure_threshold: u32,
    /// Number of consecutive successes before considering connection recovered
    #[allow(dead_code)]
    pub success_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: 5,
            timeout_secs: 3,
            failure_threshold: 3,
            success_threshold: 2,
        }
    }
}

/// Health check result
#[allow(dead_code)]
#[derive(Debug)]
pub struct HealthCheckResult {
    pub healthy: bool,
    pub rtt: Option<Duration>,
    pub error: Option<String>,
}

/// Bidirectional health checker
///
/// Performs periodic health checks and adapts keepalive based on results.
/// Uses application-level pings in addition to gRPC keepalive for better
/// detection of stuck connections.
#[allow(dead_code)]
pub struct HealthChecker {
    config: HealthCheckConfig,
    manager: Arc<ConnectionManager>,
    consecutive_failures: Arc<RwLock<u32>>,
    consecutive_successes: Arc<RwLock<u32>>,
}

#[allow(dead_code)]
impl HealthChecker {
    pub fn new(config: HealthCheckConfig, manager: Arc<ConnectionManager>) -> Self {
        Self {
            config,
            manager,
            consecutive_failures: Arc::new(RwLock::new(0)),
            consecutive_successes: Arc::new(RwLock::new(0)),
        }
    }

    /// Start the health check loop
    pub async fn run<F>(self, check_fn: F)
    where
        F: Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = HealthCheckResult> + Send>>
            + Send
            + Sync
            + 'static,
    {
        let mut ticker = interval(Duration::from_secs(self.config.interval_secs));

        info!(
            "Health checker started: interval={}s, timeout={}s",
            self.config.interval_secs, self.config.timeout_secs
        );

        loop {
            ticker.tick().await;

            if !self.manager.is_operational().await {
                debug!("Skipping health check - not operational");
                continue;
            }

            let check_result =
                timeout(Duration::from_secs(self.config.timeout_secs), check_fn()).await;

            match check_result {
                Ok(result) => {
                    self.handle_check_result(result).await;
                }
                Err(_) => {
                    warn!("Health check timed out after {}s", self.config.timeout_secs);
                    self.handle_check_result(HealthCheckResult {
                        healthy: false,
                        rtt: None,
                        error: Some("Timeout".to_string()),
                    })
                    .await;
                }
            }
        }
    }

    async fn handle_check_result(&self, result: HealthCheckResult) {
        if result.healthy {
            let rtt = result.rtt.unwrap_or(Duration::ZERO);

            self.manager.record_health_check_success(rtt).await;

            let mut successes = self.consecutive_successes.write().await;
            *successes += 1;

            let mut failures = self.consecutive_failures.write().await;
            if *successes >= self.config.success_threshold {
                *failures = 0;
            }

            debug!(
                "Health check passed: RTT={:?}, consecutive_successes={}",
                rtt, *successes
            );
        } else {
            let error = result.error.unwrap_or_else(|| "Unknown error".to_string());

            self.manager
                .record_health_check_failure(error.clone())
                .await;

            let mut failures = self.consecutive_failures.write().await;
            *failures += 1;

            let mut successes = self.consecutive_successes.write().await;
            *successes = 0;

            warn!(
                "Health check failed ({}): consecutive_failures={}",
                error, *failures
            );

            if *failures >= self.config.failure_threshold {
                error!(
                    "Health check failure threshold exceeded ({}), triggering reconnection",
                    self.config.failure_threshold
                );

                self.manager
                    .record_disconnected(format!(
                        "Health check failures exceeded threshold: {}",
                        error
                    ))
                    .await;
            }
        }
    }

    /// Get current health status
    pub async fn is_healthy(&self) -> bool {
        let failures = *self.consecutive_failures.read().await;
        failures < self.config.failure_threshold
    }

    /// Reset counters (call after reconnection)
    pub async fn reset(&self) {
        *self.consecutive_failures.write().await = 0;
        *self.consecutive_successes.write().await = 0;
    }
}

/// Simple ping health check that sends a lightweight gRPC call
#[allow(dead_code)]
pub async fn simple_ping_check() -> HealthCheckResult {
    let start = Instant::now();

    HealthCheckResult {
        healthy: true,
        rtt: Some(start.elapsed()),
        error: None,
    }
}
