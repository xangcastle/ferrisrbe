//! Startup gates behind `/readyz`.
//!
//! Every store that rebuilds state from the PVC registers a gate before the
//! listeners open and clears it when its warm-up finishes. The pod is ready,
//! and receives Service traffic, once no gate remains open. `/healthz` is
//! independent of the gates and reports liveness from the moment the
//! listeners bind.

use parking_lot::Mutex;
use std::collections::BTreeSet;

/// The warm-ups still in progress, shared by the stores that own them and the
/// `/readyz` handler that reports them.
#[derive(Debug, Default)]
pub struct Readiness {
    pending: Mutex<BTreeSet<&'static str>>,
}

impl Readiness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the gate `name`. Opening a gate that is already open is a no-op.
    pub fn gate(&self, name: &'static str) {
        self.pending.lock().insert(name);
    }

    /// Closes the gate `name`. Closing an unknown gate is a no-op.
    pub fn clear(&self, name: &'static str) {
        self.pending.lock().remove(name);
    }

    /// Whether every registered gate has been cleared.
    pub fn is_ready(&self) -> bool {
        self.pending.lock().is_empty()
    }

    /// Gates still open, in a stable order for the probe body and the logs.
    pub fn pending(&self) -> Vec<&'static str> {
        self.pending.lock().iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_once_every_gate_is_cleared() {
        let readiness = Readiness::new();
        assert!(readiness.is_ready());
        readiness.gate("cas");
        readiness.gate("bes");
        assert!(!readiness.is_ready());
        assert_eq!(readiness.pending(), vec!["bes", "cas"]);
        readiness.clear("cas");
        assert!(!readiness.is_ready());
        readiness.clear("bes");
        assert!(readiness.is_ready());
        readiness.clear("never-registered");
        assert!(readiness.is_ready());
    }
}
