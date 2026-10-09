//! Pull-through cache proxy for CI dependency downloads.
//!
//! Sits in front of the origins that build tools fetch from — GitHub releases,
//! Maven Central, PyPI, npm — so a fleet of ephemeral CI runners pays the
//! egress cost for an artifact once instead of once per runner.
//!
//! Two front doors share one cache:
//!
//! * `/proxy/<host>/<path>` over HTTP, driven by Bazel's
//!   `--experimental_downloader_config` and by `HTTP_PROXY` for pip, npm and apt.
//! * The Remote Asset gRPC service, driven by Bazel's
//!   `--experimental_remote_downloader`, which hands back a CAS digest so the
//!   bytes never travel to the runner at all.

pub mod artifact;
pub mod classifier;
pub mod config;
pub mod error;
pub mod fetcher;
pub mod index;
pub mod pull;
pub mod routes;
pub mod singleflight;
pub mod store;

pub use config::ProxyConfig;
pub use error::{ProxyError, ProxyResult};
pub use index::ProxyEntrySnapshot;
pub use pull::{EcosystemTotal, ExternalsPage, ExternalsSort};
pub use pull::{PullThroughCache, Served};
pub use routes::{router, serve, ProxyState};
pub use store::BlobMeta;
