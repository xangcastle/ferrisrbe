use std::collections::VecDeque;
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tracing::{debug, info, warn};

/// Adaptive keepalive that adjusts interval based on network conditions
/// 
/// Strategy:
/// - Start with conservative interval (e.g., 15s)
/// - If all keepalives succeed for N consecutive attempts, increase interval (up to max)
/// - If any keepalive fails, immediately decrease interval (down to min)
/// - Track latency to detect network degradation early
#[allow(dead_code)]
pub struct AdaptiveKeepalive {
    /// Current keepalive interval
    current_interval_secs: u64,
    /// Minimum allowed interval (aggressive keepalive)
    #[allow(dead_code)]
    min_interval_secs: u64,
    /// Maximum allowed interval (conservative)
    #[allow(dead_code)]
    max_interval_secs: u64,
    /// How many consecutive successes before increasing interval
    #[allow(dead_code)]
    adjustment_threshold: u32,
    /// Consecutive successful keepalives
    #[allow(dead_code)]
    consecutive_successes: u32,
    /// Consecutive failed keepalives
    #[allow(dead_code)]
    consecutive_failures: u32,
    /// History of round-trip times for latency tracking
    rtt_history: VecDeque<Duration>,
    /// Maximum RTT history size
    #[allow(dead_code)]
    max_rtt_history: usize,
    /// Last adjustment time
    last_adjustment: Instant,
    /// Cooldown between adjustments
    #[allow(dead_code)]
    adjustment_cooldown: Duration,
}

#[derive(Debug, Clone)]
pub struct KeepaliveStats {
    #[allow(dead_code)]
    pub current_interval_secs: u64,
    #[allow(dead_code)]
    pub consecutive_successes: u32,
    #[allow(dead_code)]
    pub consecutive_failures: u32,
    #[allow(dead_code)]
    pub average_rtt_ms: u64,
    #[allow(dead_code)]
    pub max_rtt_ms: u64,
    #[allow(dead_code)]
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
            rtt_history: VecDeque::with_capacity(100),
            max_rtt_history: 100,
            last_adjustment: Instant::now(),
            adjustment_cooldown: Duration::from_secs(30),
        }
    }

    /// Record a successful keepalive with measured RTT
    #[allow(dead_code)]
    pub fn record_success(&mut self, rtt: Duration) {
        self.consecutive_successes += 1;
        self.consecutive_failures = 0;

        self.rtt_history.push_back(rtt);
        if self.rtt_history.len() > self.max_rtt_history {
            self.rtt_history.pop_front();
        }

        debug!(
            "Keepalive success: RTT={:?}, consecutive_successes={}",
            rtt, self.consecutive_successes
        );

        self.maybe_increase_interval();
    }

    /// Record a failed keepalive
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

    /// Get current recommended interval
    pub fn current_interval(&self) -> Duration {
        Duration::from_secs(self.current_interval_secs)
    }

    /// Get statistics for observability
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
        if Instant::now().duration_since(self.last_adjustment) < self.adjustment_cooldown {
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
        if Instant::now().duration_since(self.last_adjustment) < Duration::from_secs(5) {
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

    /// Reset to initial state (e.g., after reconnection)
    pub fn reset(&mut self, initial_interval_secs: u64) {
        info!("Resetting adaptive keepalive to {}s", initial_interval_secs);
        self.current_interval_secs = initial_interval_secs;
        self.consecutive_successes = 0;
        self.consecutive_failures = 0;
        self.rtt_history.clear();
        self.last_adjustment = Instant::now();
    }

    /// Check if network appears degraded (high RTT variance or recent failures)
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
        
        // Should decrease by 50%
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
    fn test_stats() {
        let mut ak = AdaptiveKeepalive::new(15, 5, 60, 3);
        
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
