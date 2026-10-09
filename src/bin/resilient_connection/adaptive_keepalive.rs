//! Keepalive interval that adapts to observed network conditions.

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Round-trip samples kept for latency statistics.
const RTT_HISTORY_LEN: usize = 100;
/// Minimum time between two interval increases.
const INCREASE_COOLDOWN: Duration = Duration::from_secs(30);
/// Minimum time between two interval decreases.
const DECREASE_COOLDOWN: Duration = Duration::from_secs(5);

/// Keepalive interval that trades probe traffic for failure detection speed.
///
/// After `adjustment_threshold` consecutive successes the interval grows by
/// 25%, at most once per [`INCREASE_COOLDOWN`] and never above the maximum.
/// Every failure halves it, at most once per [`DECREASE_COOLDOWN`] and never
/// below the minimum. Round trips feed [`is_network_degraded`](Self::is_network_degraded).
#[allow(dead_code)]
pub struct AdaptiveKeepalive {
    current_interval_secs: u64,
    min_interval_secs: u64,
    max_interval_secs: u64,
    adjustment_threshold: u32,
    consecutive_successes: u32,
    consecutive_failures: u32,
    rtt_history: VecDeque<Duration>,
    last_adjustment: Instant,
}

/// Snapshot of an [`AdaptiveKeepalive`]; round trips in milliseconds, zero
/// when none has been recorded.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct KeepaliveStats {
    pub current_interval_secs: u64,
    pub consecutive_successes: u32,
    pub consecutive_failures: u32,
    pub average_rtt_ms: u64,
    pub max_rtt_ms: u64,
    pub min_rtt_ms: u64,
}

impl AdaptiveKeepalive {
    pub fn new(
        initial_interval_secs: u64,
        min_interval_secs: u64,
        max_interval_secs: u64,
        adjustment_threshold: u32,
    ) -> Self {
        Self {
            current_interval_secs: initial_interval_secs,
            min_interval_secs,
            max_interval_secs,
            adjustment_threshold,
            consecutive_successes: 0,
            consecutive_failures: 0,
            rtt_history: VecDeque::with_capacity(RTT_HISTORY_LEN),
            last_adjustment: initial_last_adjustment(),
        }
    }

    /// Records a successful keepalive and its round trip.
    #[allow(dead_code)]
    pub fn record_success(&mut self, rtt: Duration) {
        self.consecutive_successes += 1;
        self.consecutive_failures = 0;

        self.rtt_history.push_back(rtt);
        if self.rtt_history.len() > RTT_HISTORY_LEN {
            self.rtt_history.pop_front();
        }

        debug!(
            "Keepalive success: RTT={:?}, consecutive_successes={}",
            rtt, self.consecutive_successes
        );

        self.maybe_increase_interval();
    }

    /// Records a failed keepalive.
    #[allow(dead_code)]
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        self.consecutive_successes = 0;

        warn!(
            "Keepalive failure #{}: current_interval={}s",
            self.consecutive_failures, self.current_interval_secs
        );

        self.decrease_interval();
    }

    /// The interval to wait before the next keepalive.
    pub fn current_interval(&self) -> Duration {
        Duration::from_secs(self.current_interval_secs)
    }

    /// Current [`KeepaliveStats`].
    pub fn stats(&self) -> KeepaliveStats {
        let (avg, max, min) = if self.rtt_history.is_empty() {
            (0, 0, 0)
        } else {
            let sum: Duration = self.rtt_history.iter().sum();
            let avg = sum / self.rtt_history.len() as u32;
            let max = *self.rtt_history.iter().max().unwrap_or(&Duration::ZERO);
            let min = *self.rtt_history.iter().min().unwrap_or(&Duration::ZERO);
            (
                avg.as_millis() as u64,
                max.as_millis() as u64,
                min.as_millis() as u64,
            )
        };

        KeepaliveStats {
            current_interval_secs: self.current_interval_secs,
            consecutive_successes: self.consecutive_successes,
            consecutive_failures: self.consecutive_failures,
            average_rtt_ms: avg,
            max_rtt_ms: max,
            min_rtt_ms: min,
        }
    }

    #[allow(dead_code)]
    fn maybe_increase_interval(&mut self) {
        if self.last_adjustment.elapsed() < INCREASE_COOLDOWN {
            return;
        }

        if self.consecutive_successes < self.adjustment_threshold {
            return;
        }

        if self.current_interval_secs >= self.max_interval_secs {
            return;
        }

        let new_interval = (self.current_interval_secs * 5 / 4).min(self.max_interval_secs);

        if new_interval != self.current_interval_secs {
            info!(
                "Increasing keepalive interval: {}s -> {}s (after {} consecutive successes)",
                self.current_interval_secs, new_interval, self.consecutive_successes
            );
            self.current_interval_secs = new_interval;
            self.last_adjustment = Instant::now();
            self.consecutive_successes = 0;
        }
    }

    #[allow(dead_code)]
    fn decrease_interval(&mut self) {
        if self.last_adjustment.elapsed() < DECREASE_COOLDOWN {
            return;
        }

        let new_interval = (self.current_interval_secs / 2).max(self.min_interval_secs);

        if new_interval != self.current_interval_secs {
            info!(
                "Decreasing keepalive interval: {}s -> {}s (after {} consecutive failures)",
                self.current_interval_secs, new_interval, self.consecutive_failures
            );
            self.current_interval_secs = new_interval;
            self.last_adjustment = Instant::now();
        }
    }

    /// Returns to `initial_interval_secs` with no history, as on a new connection.
    pub fn reset(&mut self, initial_interval_secs: u64) {
        info!("Resetting adaptive keepalive to {}s", initial_interval_secs);
        self.current_interval_secs = initial_interval_secs;
        self.consecutive_successes = 0;
        self.consecutive_failures = 0;
        self.rtt_history.clear();
        self.last_adjustment = Instant::now();
    }

    /// Whether the last keepalive failed, or the mean of the last 10 round
    /// trips exceeds twice the mean of the 10 before them.
    #[allow(dead_code)]
    pub fn is_network_degraded(&self) -> bool {
        if self.consecutive_failures > 0 {
            return true;
        }

        if self.rtt_history.len() < 10 {
            return false;
        }

        let recent: Vec<_> = self.rtt_history.iter().rev().take(10).collect();
        let older: Vec<_> = self.rtt_history.iter().rev().skip(10).take(10).collect();

        if older.is_empty() {
            return false;
        }

        let recent_avg: Duration = recent.iter().copied().sum::<Duration>() / recent.len() as u32;
        let older_avg: Duration = older.iter().copied().sum::<Duration>() / older.len() as u32;

        recent_avg > older_avg * 2
    }
}

/// The adjustment timestamp a new keepalive starts from. Tests start with
/// both cooldowns already elapsed so that the first adjustment applies at
/// once; production starts the cooldown at construction.
fn initial_last_adjustment() -> Instant {
    let now = Instant::now();
    if cfg!(test) {
        now.checked_sub(Duration::from_secs(60)).unwrap_or(now)
    } else {
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_interval() {
        let ak = AdaptiveKeepalive::new(15, 5, 60, 3);
        assert_eq!(ak.current_interval().as_secs(), 15);
    }

    #[test]
    fn test_failure_decreases_interval() {
        let mut ak = AdaptiveKeepalive::new(20, 5, 60, 3);

        ak.record_failure();
        assert_eq!(ak.current_interval().as_secs(), 10);
    }

    #[test]
    fn test_failure_respects_min() {
        let mut ak = AdaptiveKeepalive::new(10, 5, 60, 3);

        ak.record_failure();
        assert_eq!(ak.current_interval().as_secs(), 5);

        ak.record_failure();
        assert_eq!(ak.current_interval().as_secs(), 5);
    }

    #[test]
    fn test_success_increases_interval() {
        let mut ak = AdaptiveKeepalive::new(16, 5, 60, 3);

        ak.record_success(Duration::from_millis(10));
        ak.record_success(Duration::from_millis(10));
        ak.record_success(Duration::from_millis(10));

        assert_eq!(ak.current_interval().as_secs(), 20);
    }

    #[test]
    fn stats_report_rtt_and_success_streak() {
        let threshold_above_sample_count = 100;
        let mut ak = AdaptiveKeepalive::new(15, 5, 60, threshold_above_sample_count);

        ak.record_success(Duration::from_millis(10));
        ak.record_success(Duration::from_millis(20));
        ak.record_success(Duration::from_millis(30));

        let stats = ak.stats();
        assert_eq!(stats.consecutive_successes, 3);
        assert_eq!(stats.average_rtt_ms, 20);
        assert_eq!(stats.max_rtt_ms, 30);
        assert_eq!(stats.min_rtt_ms, 10);
    }
}
