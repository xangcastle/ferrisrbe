//! `server`: the FerrisRBE process, serving every REAPI and BES endpoint
//! plus the embedded web UI built into `RBE_UI_DIST`.
//!
//! Configuration comes from `RBE_*` environment variables (see
//! [`RbeServerConfig::from_env`]); log filtering follows `RUST_LOG`,
//! defaulting to `info`.

use include_dir::{include_dir, Dir};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use rbe_server::server::{RbeServer, RbeServerConfig};

static UI_ASSETS: Dir<'static> = include_dir!("$RBE_UI_DIST");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rbe_server::types::init_global_base_instant();

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = RbeServerConfig {
        ui_assets: Some(&UI_ASSETS),
        ..RbeServerConfig::from_env()
    };

    let server = RbeServer::new(config).await?;
    server.run().await?;

    Ok(())
}
