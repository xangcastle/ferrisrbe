//! Remote execution: scheduling, per-operation state, dispatch to workers
//! and storage of results and outputs.

pub mod engine;
pub mod output_handler;
pub mod results;
pub mod scheduler;
pub mod state_machine;
