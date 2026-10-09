//! States of the worker connection and a recorder of their transitions.

use std::fmt;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Transitions kept by [`StateMachine::history`].
const HISTORY_LEN: usize = 100;

/// State of the worker's stream to the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection, and none in progress.
    Disconnected,
    /// Establishing the gRPC channel.
    Connecting,
    /// Channel open, registration not yet acknowledged.
    Handshaking,
    /// Registered and accepting work.
    Active,
    /// Accepting work while health checks report a degraded network.
    #[allow(dead_code)]
    Degraded,
    /// Waiting to reconnect after a loss.
    Reconnecting,
    /// Lost from a state that does not reconnect.
    Failed,
}

impl ConnectionState {
    /// Whether work assignments can be accepted: `Active` or `Degraded`.
    pub fn is_operational(&self) -> bool {
        matches!(self, ConnectionState::Active | ConnectionState::Degraded)
    }

    /// Whether the state is `Failed`.
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(self, ConnectionState::Failed)
    }

    /// Whether a loss in this state leads to `Reconnecting` rather than `Failed`.
    pub fn can_reconnect(&self) -> bool {
        matches!(
            self,
            ConnectionState::Disconnected | ConnectionState::Failed | ConnectionState::Reconnecting
        )
    }

    /// One-line human-readable description.
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

/// The current [`ConnectionState`], when it was entered, and the last
/// [`HISTORY_LEN`] transitions with their reasons.
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

    /// Moves to `new_state`, recording `reason`. A transition to the current
    /// state is a no-op.
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

        if self.state_history.len() > HISTORY_LEN {
            self.state_history.remove(0);
        }
    }

    pub fn current(&self) -> ConnectionState {
        self.current_state
    }

    /// Time since the current state was entered.
    pub fn duration_in_current(&self) -> Duration {
        Instant::now().duration_since(self.state_entry_time)
    }

    /// Transitions since construction.
    pub fn transition_count(&self) -> u64 {
        self.transition_count
    }

    /// Recorded transitions, oldest first, as `(state, entered_at, reason)`.
    #[allow(dead_code)]
    pub fn history(&self) -> &[(ConnectionState, Instant, Option<String>)] {
        &self.state_history
    }

    /// [`ConnectionState::can_reconnect`] of the current state.
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

        sm.transition_to(
            ConnectionState::Connecting,
            Some("Starting connection".to_string()),
        );
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
