//! Reconnection delay policies.

use std::time::Duration;

/// Doublings after which the backoff stops growing, independent of the cap.
const MAX_BACKOFF_EXPONENT: u32 = 10;

/// `base_ms * 2^exponent`, capped at `max_ms`, then shifted by a uniformly
/// random amount within `±jitter_factor` of itself and clamped at zero.
///
/// The exponent saturates at [`MAX_BACKOFF_EXPONENT`]. Jitter draws from the
/// thread-local `fastrand` generator, which the worker seeds from its id so
/// that a fleet restarting together does not reconnect in lockstep.
pub fn exponential_backoff(
    exponent: u32,
    base_ms: u64,
    max_ms: u64,
    jitter_factor: f64,
) -> Duration {
    let capped = base_ms
        .saturating_mul(1_u64 << exponent.min(MAX_BACKOFF_EXPONENT))
        .min(max_ms);
    let jitter_range = (capped as f64 * jitter_factor.max(0.0)) as u64;
    let jitter = if jitter_range > 0 {
        fastrand::u64(0..jitter_range * 2) as i64 - jitter_range as i64
    } else {
        0
    };
    Duration::from_millis((capped as i64 + jitter).max(0) as u64)
}

/// How the delay between reconnection attempts evolves.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum ReconnectionStrategy {
    /// No delay.
    Immediate,
    /// The same delay before every attempt.
    FixedDelay { delay_ms: u64 },
    /// [`exponential_backoff`] with these parameters.
    ExponentialBackoff {
        base_delay_ms: u64,
        max_delay_ms: u64,
        jitter_factor: f64,
    },
}

/// A strategy bounded to `max_attempts` attempts.
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

    /// Consumes one attempt and returns the delay before it; `None` once all
    /// attempts are spent.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.is_exhausted() {
            return None;
        }
        self.attempt += 1;

        Some(match self.strategy {
            ReconnectionStrategy::Immediate => Duration::ZERO,
            ReconnectionStrategy::FixedDelay { delay_ms } => Duration::from_millis(delay_ms),
            ReconnectionStrategy::ExponentialBackoff {
                base_delay_ms,
                max_delay_ms,
                jitter_factor,
            } => exponential_backoff(self.attempt - 1, base_delay_ms, max_delay_ms, jitter_factor),
        })
    }

    pub fn is_exhausted(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// Restores the full attempt budget, as after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Attempts consumed so far.
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
            assert_eq!(policy.next_delay().unwrap(), Duration::ZERO);
        }
        assert!(policy.next_delay().is_none());
    }

    #[test]
    fn test_fixed_delay() {
        let mut policy =
            ReconnectionPolicy::new(ReconnectionStrategy::FixedDelay { delay_ms: 1000 }, 3);
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert_eq!(policy.next_delay().unwrap(), Duration::from_millis(1000));
        assert!(policy.next_delay().is_none());
    }

    #[test]
    fn exponential_backoff_doubles_per_attempt() {
        let mut policy = ReconnectionPolicy::new(
            ReconnectionStrategy::ExponentialBackoff {
                base_delay_ms: 100,
                max_delay_ms: 10000,
                jitter_factor: 0.0,
            },
            5,
        );
        for expected_ms in [100, 200, 400, 800, 1600] {
            assert_eq!(
                policy.next_delay().unwrap(),
                Duration::from_millis(expected_ms)
            );
        }
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
        for expected_ms in [1000, 2000, 4000, 4000, 4000] {
            assert_eq!(
                policy.next_delay().unwrap(),
                Duration::from_millis(expected_ms)
            );
        }
    }

    #[test]
    fn jittered_backoff_stays_within_the_jitter_band() {
        for _ in 0..100 {
            let delay = exponential_backoff(2, 100, 10_000, 0.25).as_millis();
            assert!(
                (300..=500).contains(&delay),
                "{delay}ms outside 400ms ± 25%"
            );
        }
    }

    #[test]
    fn test_reset() {
        let mut policy =
            ReconnectionPolicy::new(ReconnectionStrategy::FixedDelay { delay_ms: 1000 }, 3);
        policy.next_delay();
        policy.next_delay();
        assert_eq!(policy.current_attempt(), 2);

        policy.reset();
        assert_eq!(policy.current_attempt(), 0);
        assert!(!policy.is_exhausted());
    }
}
