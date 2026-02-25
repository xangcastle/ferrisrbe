use std::fmt;
use std::time::{Duration, Instant};
#[allow(unused_imports)]
use tracing::{debug, info, warn};

/// Connection state machine for resilient worker connections
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Handshaking,
    Active,
    #[allow(dead_code)]
    Degraded,
    Reconnecting,
    Failed,
}

impl ConnectionState {
    /// Check if the connection is operational (can receive assignments)
    pub fn is_operational(&self) -> bool {
        matches!(self, ConnectionState::Active | ConnectionState::Degraded)
    }

    /// Check if the connection is in a terminal state
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(self, ConnectionState::Failed)
    }

    /// Check if reconnection is allowed from this state
    pub fn can_reconnect(&self) -> bool {
        matches!(
            self,
            ConnectionState::Disconnected
                | ConnectionState::Failed
                | ConnectionState::Reconnecting
        )
    }

    /// Get human-readable description
    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            ConnectionState::Disconnected => "Not connected to server",
            ConnectionState::Connecting => "Establishing TCP connection",
            ConnectionState::Handshaking => "Performing protocol handshake",
            ConnectionState::Active => "Fully operational",
            ConnectionState::Degraded => "Operational with issues",
            ConnectionState::Reconnecting => "Attempting to reconnect",
            ConnectionState::Failed => "Connection failed permanently",
        }
    }
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Tracks state transitions and timing for observability
pub struct StateMachine {
    current_state: ConnectionState,
    state_history: Vec<(ConnectionState, Instant, Option<String>)>,
    state_entry_time: Instant,
    transition_count: u64,
}

impl StateMachine {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            current_state: ConnectionState::Disconnected,
            state_history: vec![(ConnectionState::Disconnected, now, None)],
            state_entry_time: now,
            transition_count: 0,
        }
    }

    /// Transition to a new state with optional reason
    pub fn transition_to(&mut self, new_state: ConnectionState, reason: Option<String>) {
        if self.current_state == new_state {
            return;
        }

        let now = Instant::now();
        let duration_in_previous = now.duration_since(self.state_entry_time);

        info!(
            "Connection state transition: {} -> {} (spent {:?} in previous state)",
            self.current_state, new_state, duration_in_previous
        );

        if let Some(ref r) = reason {
            debug!("Transition reason: {}", r);
        }

        self.state_history.push((new_state, now, reason));
        self.current_state = new_state;
        self.state_entry_time = now;
        self.transition_count += 1;

        if self.state_history.len() > 100 {
            self.state_history.remove(0);
        }
    }

    /// Get current state
    pub fn current(&self) -> ConnectionState {
        self.current_state
    }

    /// Get duration in current state
    pub fn duration_in_current(&self) -> Duration {
        Instant::now().duration_since(self.state_entry_time)
    }

    /// Get total number of state transitions
    pub fn transition_count(&self) -> u64 {
        self.transition_count
    }

    /// Get state history
    #[allow(dead_code)]
    pub fn history(&self) -> &[(ConnectionState, Instant, Option<String>)] {
        &self.state_history
    }

    /// Check if we should attempt reconnection
    pub fn should_reconnect(&self) -> bool {
        self.current_state.can_reconnect()
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_transitions() {
        let mut sm = StateMachine::new();
        assert_eq!(sm.current(), ConnectionState::Disconnected);

        sm.transition_to(ConnectionState::Connecting, Some("Starting connection".to_string()));
        assert_eq!(sm.current(), ConnectionState::Connecting);
        assert_eq!(sm.transition_count(), 1);

        sm.transition_to(ConnectionState::Active, None);
        assert_eq!(sm.current(), ConnectionState::Active);
        assert!(sm.current().is_operational());
    }

    #[test]
    fn test_operational_states() {
        assert!(!ConnectionState::Disconnected.is_operational());
        assert!(!ConnectionState::Connecting.is_operational());
        assert!(!ConnectionState::Handshaking.is_operational());
        assert!(ConnectionState::Active.is_operational());
        assert!(ConnectionState::Degraded.is_operational());
        assert!(!ConnectionState::Reconnecting.is_operational());
        assert!(!ConnectionState::Failed.is_operational());
    }
}
