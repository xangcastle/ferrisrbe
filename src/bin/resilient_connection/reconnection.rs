use std::time::Duration;
#[allow(unused_imports)]
use tracing::{debug, info, warn};

/// Generate random number using fastrand (already in deps)
#[allow(dead_code)]
fn random_u64(max: u64) -> u64 {
    fastrand::u64(0..max)
}

/// Generate random float between 0 and 1
#[allow(dead_code)]
fn random_f64() -> f64 {
    fastrand::f64()
}

/// Reconnection strategies
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ReconnectionStrategy {
    /// Immediate reconnection (no delay)
    Immediate,
    /// Fixed delay between attempts
    FixedDelay { delay_ms: u64 },
    /// Exponential backoff with optional jitter
    ExponentialBackoff {
        base_delay_ms: u64,
        max_delay_ms: u64,
        jitter_factor: f64,
    },
}

/// Policy for reconnection attempts
#[allow(dead_code)]
pub struct ReconnectionPolicy {
    pub strategy: ReconnectionStrategy,
    pub max_attempts: u32,
    pub attempt: u32,
}

#[allow(dead_code)]
impl ReconnectionPolicy {
    pub fn new(strategy: ReconnectionStrategy, max_attempts: u32) -> Self {
        Self {
            strategy,
            max_attempts,
            attempt: 0,
        }
    }

    /// Get next delay and increment attempt counter
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempt >= self.max_attempts {
            return None;
        }

        self.attempt += 1;

        let delay = match self.strategy {
            ReconnectionStrategy::Immediate => Duration::ZERO,
            ReconnectionStrategy::FixedDelay { delay_ms } => Duration::from_millis(delay_ms),
            ReconnectionStrategy::ExponentialBackoff {
                base_delay_ms,
                max_delay_ms,
                jitter_factor,
            } => {
                let exponent = (self.attempt - 1).min(10) as u64;
                let exponential = base_delay_ms * (1_u64 << exponent);
                let capped = exponential.min(max_delay_ms);

                if jitter_factor > 0.0 {
                    let jitter_range = (capped as f64 * jitter_factor) as u64;
                    let jitter = if jitter_range > 0 {
                        let r = random_u64(jitter_range * 2);
                        r as i64 - jitter_range as i64
                    } else {
                        0
                    };
                    let final_delay = (capped as i64 + jitter).max(0) as u64;
                    Duration::from_millis(final_delay)
                } else {
                    Duration::from_millis(capped)
                }
            }
        };

        Some(delay)
    }

    /// Check if we've exhausted all attempts
    pub fn is_exhausted(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// Reset attempt counter (call after successful connection)
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Get current attempt number
    pub fn current_attempt(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_immediate_reconnection() {
        let mut policy = ReconnectionPolicy::new(ReconnectionStrategy::Immediate, 5);
        
        for _ in 0..5 {
            let delay = policy.next_delay().unwrap();
            assert_eq!(delay, Duration::ZERO);
        }
        
        assert!(policy.next_delay().is_none());
    }

    #[test]
    fn test_fixed_delay() {
        let mut policy = ReconnectionPolicy::new(
            ReconnectionStrategy::FixedDelay { delay_ms: 1000 },
            3,
        );
        
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert!(policy.next_delay().is_none());
    }

    #[test]
    fn test_exponential_backoff() {
        let mut policy = ReconnectionPolicy::new(
            ReconnectionStrategy::ExponentialBackoff {
                base_delay_ms: 100,
                max_delay_ms: 10000,
                jitter_factor: 0.0,
            },
            5,
        );
        
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(100));
        // 100 * 2^1 = 200
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(200));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(400));
        // 100 * 2^3 = 800
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(800));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1600));
    }

    #[test]
    fn test_exponential_backoff_with_cap() {
        let mut policy = ReconnectionPolicy::new(
            ReconnectionStrategy::ExponentialBackoff {
                base_delay_ms: 1000,
                max_delay_ms: 4000,
                jitter_factor: 0.0,
            },
            5,
        );
        
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(2000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(4000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(4000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(4000));
    }

    #[test]
    fn test_reset() {
        let mut policy = ReconnectionPolicy::new(
            ReconnectionStrategy::FixedDelay { delay_ms: 1000 },
            3,
        );
        
        policy.next_delay();
        policy.next_delay();
        assert_eq!(policy.current_attempt(), 2);
        
        policy.reset();
        assert_eq!(policy.current_attempt(), 0);
        assert!(!policy.is_exhausted());
    }
}
