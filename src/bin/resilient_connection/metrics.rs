//! Counters of the worker connection's history.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Connection durations kept for [`ConnectionMetrics::average_connection_duration_secs`].
const DURATION_HISTORY_LEN: usize = 100;

/// Connection counters. When disabled every `record_*` call is a no-op.
pub struct ConnectionMetrics {
    pub enabled: bool,
    pub total_connections_established: u64,
    pub total_reconnections: u64,
    pub total_disconnections: u64,
    pub failed_health_checks: u64,
    pub connection_durations: VecDeque<Duration>,
    last_connection_time: Option<Instant>,
}

impl ConnectionMetrics {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            total_connections_established: 0,
            total_reconnections: 0,
            total_disconnections: 0,
            failed_health_checks: 0,
            connection_durations: VecDeque::with_capacity(DURATION_HISTORY_LEN),
            last_connection_time: None,
        }
    }

    /// Counts a connection; every one after the first also counts as a
    /// reconnection.
    pub fn record_connection_established(&mut self) {
        if !self.enabled {
            return;
        }

        self.total_connections_established += 1;

        if self.last_connection_time.is_some() {
            self.total_reconnections += 1;
        }

        self.last_connection_time = Some(Instant::now());
    }

    /// Counts a disconnection and records how long the connection lasted.
    pub fn record_disconnection(&mut self) {
        if !self.enabled {
            return;
        }

        self.total_disconnections += 1;

        if let Some(start) = self.last_connection_time.take() {
            let duration = Instant::now().duration_since(start);
            self.connection_durations.push_back(duration);

            if self.connection_durations.len() > DURATION_HISTORY_LEN {
                self.connection_durations.pop_front();
            }
        }
    }

    #[allow(dead_code)]
    pub fn record_failed_health_check(&mut self) {
        if !self.enabled {
            return;
        }
        self.failed_health_checks += 1;
    }

    /// Mean duration of the recent connections, in seconds; zero when none
    /// has ended.
    pub fn average_connection_duration_secs(&self) -> f64 {
        if self.connection_durations.is_empty() {
            return 0.0;
        }

        let total: Duration = self.connection_durations.iter().sum();
        total.as_secs_f64() / self.connection_durations.len() as f64
    }
}
