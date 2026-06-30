//! BES (Build Event Service) backend module.
//!
//! Provides a gRPC `PublishBuildEvent` service, persistent JSONL event storage,
//! derived per-invocation summaries, and a REST API/UI server.

pub mod api;
pub mod config;
pub mod models;
pub mod service;
pub mod storage;

pub use api::BesApi;
pub use config::BesConfig;
pub use service::BesService;
pub use storage::BesStorage;
