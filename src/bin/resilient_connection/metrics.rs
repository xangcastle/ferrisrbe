use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Connection metrics for observability and debugging
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
            connection_durations: VecDeque::with_capacity(100),
            last_connection_time: None,
        }
    }

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

    pub fn record_disconnection(&mut self) {
        if !self.enabled {
            return;
        }
        
        self.total_disconnections += 1;
        
        if let Some(start) = self.last_connection_time.take() {
            let duration = Instant::now().duration_since(start);
            self.connection_durations.push_back(duration);
            
            if self.connection_durations.len() > 100 {
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

    pub fn average_connection_duration_secs(&self) -> f64 {
        if self.connection_durations.is_empty() {
            return 0.0;
        }
        
        let total: Duration = self.connection_durations.iter().sum();
        total.as_secs_f64() / self.connection_durations.len() as f64
    }
}
