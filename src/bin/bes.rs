//! FerrisRBE Build Event Service (BES) + UI server.
//!
//! Receives Bazel Build Event Protocol (BEP) events via gRPC and serves a
//! React UI to inspect builds, actions, and cache misses.

use std::net::SocketAddr;
use std::time::Duration;

use tonic::transport::Server;
use tracing::info;
use tracing_subscriber::EnvFilter;

use rbe_server::bes::{BesApi, BesConfig, BesService, BesStorage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rbe_server::types::init_global_base_instant();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = BesConfig::from_env();

    info!("╔══════════════════════════════════════════════════════════════╗");
    info!("║         FerrisRBE Build Event Service (BES + UI)              ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║ ✓ PublishBuildEvent gRPC service                              ║");
    info!("║ ✓ Persistent JSONL event storage                              ║");
    info!("║ ✓ REST API for builds/events/misses                           ║");
    info!("║ ✓ Static UI serving                                           ║");
    info!("╚══════════════════════════════════════════════════════════════╝");
    info!("Configuration: {:?}", config);

    let storage = BesStorage::new(config.clone()).await?;
    info!("BES storage initialized at {:?}", config.data_dir);

    let bes_service = BesService::new(storage.clone());
    let bes_api = BesApi::new(storage, config.clone());

    let grpc_addr: SocketAddr = format!("{}:{}", config.bind_address, config.grpc_port).parse()?;
    let grpc_server = Server::builder()
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true)
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(15)))
        .http2_adaptive_window(Some(true))
        .timeout(Duration::from_secs(600))
        .add_service(bes_service.into_service())
        .serve(grpc_addr);

    info!("Starting BES gRPC server on {}", grpc_addr);

    tokio::select! {
        result = grpc_server => {
            result?;
        }
        result = bes_api.run() => {
            result?;
        }
    }

    Ok(())
}
