//! Execution workers: registration and assignment, input materialization,
//! output upload, persistent multiplex workers and local worker pools.

pub mod k8s;
pub mod materializer;
pub mod multiplex;
pub mod output_uploader;
pub mod pool;
pub mod process;

pub use k8s::WorkerState;

use std::time::Duration;

/// Outcome of running an action on a pooled worker.
#[derive(Debug, Clone)]
pub struct ActionResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub execution_duration: Duration,
}

/// Process-unique identifier of a pooled worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u64);

impl WorkerId {
    /// A fresh id, unique within the process.
    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Failures of worker pool operations.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("Worker unavailable: {0}")]
    Unavailable(String),

    #[error("Execution timeout")]
    Timeout,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
