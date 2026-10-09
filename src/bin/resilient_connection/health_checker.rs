//! Application-level health checks of the worker connection.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::{interval, timeout};
use tracing::{debug, error, info, warn};

use super::connection_manager::ConnectionManager;

/// Health check cadence and the streaks that change connection health.
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// Interval between checks, in seconds.
    #[allow(dead_code)]
    pub interval_secs: u64,
    /// Time allowed for one check, in seconds; exceeding it counts as a failure.
    #[allow(dead_code)]
    pub timeout_secs: u64,
    /// Consecutive failures that mark the connection lost.
    #[allow(dead_code)]
    pub failure_threshold: u32,
    /// Consecutive successes that clear the failure streak.
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

/// Outcome of one health check.
#[allow(dead_code)]
#[derive(Debug)]
pub struct HealthCheckResult {
    pub healthy: bool,
    pub rtt: Option<Duration>,
    pub error: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
struct Streak {
    failures: u32,
    successes: u32,
}

/// Periodic application-level ping of the connection, complementing gRPC
/// keepalive: it detects a stream that is alive at the transport level but
/// no longer served. Results feed the adaptive keepalive; a failure streak
/// of `failure_threshold` reports the connection lost to the
/// [`ConnectionManager`].
#[allow(dead_code)]
pub struct HealthChecker {
    config: HealthCheckConfig,
    manager: Arc<ConnectionManager>,
    streak: RwLock<Streak>,
}

#[allow(dead_code)]
impl HealthChecker {
    pub fn new(config: HealthCheckConfig, manager: Arc<ConnectionManager>) -> Self {
        Self {
            config,
            manager,
            streak: RwLock::new(Streak::default()),
        }
    }

    /// Runs `check_fn` every `interval_secs` while the connection is
    /// operational. Never returns.
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

            let result =
                match timeout(Duration::from_secs(self.config.timeout_secs), check_fn()).await {
                    Ok(result) => result,
                    Err(_) => {
                        warn!("Health check timed out after {}s", self.config.timeout_secs);
                        HealthCheckResult {
                            healthy: false,
                            rtt: None,
                            error: Some("Timeout".to_string()),
                        }
                    }
                };
            self.handle_check_result(result).await;
        }
    }

    async fn handle_check_result(&self, result: HealthCheckResult) {
        if result.healthy {
            let rtt = result.rtt.unwrap_or(Duration::ZERO);
            self.manager.record_health_check_success(rtt).await;

            let successes = {
                let mut streak = self.streak.write().await;
                streak.successes += 1;
                if streak.successes >= self.config.success_threshold {
                    streak.failures = 0;
                }
                streak.successes
            };
            debug!(
                "Health check passed: RTT={:?}, consecutive_successes={}",
                rtt, successes
            );
            return;
        }

        let error = result.error.unwrap_or_else(|| "Unknown error".to_string());
        self.manager
            .record_health_check_failure(error.clone())
            .await;

        let failures = {
            let mut streak = self.streak.write().await;
            streak.failures += 1;
            streak.successes = 0;
            streak.failures
        };
        warn!(
            "Health check failed ({}): consecutive_failures={}",
            error, failures
        );

        if failures >= self.config.failure_threshold {
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

    /// Whether the failure streak is below `failure_threshold`.
    pub async fn is_healthy(&self) -> bool {
        self.streak.read().await.failures < self.config.failure_threshold
    }

    /// Clears both streaks, as after a reconnection.
    pub async fn reset(&self) {
        *self.streak.write().await = Streak::default();
    }
}
