//! BES (Build Event Service) backend module.
//!
//! Provides a gRPC `PublishBuildEvent` service, persistent JSONL event storage,
//! derived per-invocation summaries, a REST API and the embedded UI.

pub mod api;
pub mod config;
pub mod console;
pub mod live;
pub mod models;
pub mod profile;
pub mod redact;
pub mod service;
pub mod storage;
pub mod ui;

pub use api::BesApi;
pub use config::BesConfig;
pub use live::{LiveBus, LiveEvent};
pub use service::BesService;
pub use storage::BesStorage;
pub use ui::with_embedded_ui;
