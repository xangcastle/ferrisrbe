//! FerrisRBE Remote Cache Server
//!
//! Standalone cache-only server that replaces `buchgr/bazel-remote-cache`.
//! Implements REAPI v2.4:
//!   - ContentAddressableStorage (FindMissing, BatchRead, BatchUpdate)
//!   - ByteStream (streaming upload/download)
//!   - ActionCache (Get/Update ActionResult)
//!   - Capabilities
//!
//! Also exposes a lightweight HTTP `/status` endpoint for health checks.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::Server;
use tracing::info;
use tracing_subscriber::EnvFilter;

use rbe_server::cache::disk_action_cache::DiskActionCache;
use rbe_server::cas::backends::DiskBackend;
use rbe_server::server::{action_cache_service, byte_stream, capabilities_service, cas_service};

#[derive(Debug, Clone)]
struct CacheConfig {
    bind_address: String,
    grpc_port: u16,
    http_port: u16,
    cache_dir: String,
    max_size_gb: u64,
    ac_max_size_gb: u64,
    ac_ttl_secs: u64,
    l1_capacity: usize,
    l1_ttl_secs: u64,
}

impl CacheConfig {
    fn from_env() -> Self {
        let bind_address =
            std::env::var("RBE_CACHE_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0".to_string());
        let grpc_port = std::env::var("RBE_CACHE_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9094);
        let http_port = std::env::var("RBE_CACHE_HTTP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);
        let cache_dir = std::env::var("RBE_CACHE_DIR").unwrap_or_else(|_| "/data".to_string());
        let max_size_gb = std::env::var("RBE_CACHE_MAX_SIZE_GB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        let ac_max_size_gb = std::env::var("RBE_CACHE_AC_MAX_SIZE_GB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        let ac_ttl_secs = std::env::var("RBE_CACHE_AC_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        let l1_capacity = std::env::var("RBE_L1_CACHE_CAPACITY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        let l1_ttl_secs = std::env::var("RBE_L1_CACHE_TTL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);

        Self {
            bind_address,
            grpc_port,
            http_port,
            cache_dir,
            max_size_gb,
            ac_max_size_gb,
            ac_ttl_secs,
            l1_capacity,
            l1_ttl_secs,
        }
    }
}

/// Lightweight HTTP server that only serves `/status`.
async fn run_http_status_server(
    bind_address: String,
    port: u16,
    cas_backend: Arc<DiskBackend>,
    ac_backend: Arc<DiskActionCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = format!("{}:{}", bind_address, port).parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("HTTP /status server listening on {}", addr);

    loop {
        let (mut stream, peer) = listener.accept().await?;
        let cas = cas_backend.clone();
        let ac = ac_backend.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_http_connection(&mut stream, cas, ac).await {
                tracing::debug!("HTTP connection from {} error: {}", peer, e);
            }
        });
    }
}

async fn handle_http_connection(
    stream: &mut TcpStream,
    _cas_backend: Arc<DiskBackend>,
    _ac_backend: Arc<DiskActionCache>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = [0u8; 1024];
    let n = stream.peek(&mut buf).await.unwrap_or(0);
    if n == 0 {
        return Ok(());
    }
    // Consume the request so the connection can close cleanly.
    let _ = stream.read(&mut buf).await?;

    let body = r#"{"ok": true}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rbe_server::types::init_global_base_instant();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let config = CacheConfig::from_env();

    info!("╔══════════════════════════════════════════════════════════════╗");
    info!("║              FerrisRBE Remote Cache Server                    ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║ ✓ ByteStream Service (CAS upload/download)                    ║");
    info!("║ ✓ ActionCache Service (L1 + disk-backed L2)                   ║");
    info!("║ ✓ CAS Service (FindMissing, BatchRead, BatchUpdate)           ║");
    info!("║ ✓ Capabilities Service (REAPI v2.4 cache-only)                ║");
    info!("║ ✓ HTTP /status endpoint                                       ║");
    info!("╚══════════════════════════════════════════════════════════════╝");
    info!("Configuration: {:?}", config);

    let cas_dir = format!("{}/cas", config.cache_dir);
    let cas_backend: Arc<DiskBackend> = Arc::new(
        DiskBackend::with_max_size(&cas_dir, config.max_size_gb)
            .await
            .map_err(|e| format!("Failed to initialize CAS backend: {}", e))?,
    );

    let ac_dir = format!("{}/ac", config.cache_dir);
    let ac_backend: Arc<DiskActionCache> = Arc::new(
        DiskActionCache::new(
            &ac_dir,
            config.ac_max_size_gb,
            Duration::from_secs(config.ac_ttl_secs),
            config.l1_capacity,
            Duration::from_secs(config.l1_ttl_secs),
        )
        .await
        .map_err(|e| format!("Failed to initialize ActionCache backend: {}", e))?,
    );

    let grpc_addr: SocketAddr = format!("{}:{}", config.bind_address, config.grpc_port).parse()?;
    info!("Starting gRPC cache server on {}", grpc_addr);

    let byte_stream_service = byte_stream::ByteStreamService::new(cas_backend.clone());
    let action_cache_service = action_cache_service::ActionCacheService::new(
        ac_backend.clone() as Arc<dyn rbe_server::cache::action_cache::ActionCacheStore>
    );
    let cas_service = cas_service::CasService::new(cas_backend.clone());
    let capabilities_service = capabilities_service::CapabilitiesService::new(false);

    let max_msg_size = std::env::var("RBE_MAX_GRPC_MSG_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100 * 1024 * 1024);

    let grpc_server = Server::builder()
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .tcp_nodelay(true)
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(15)))
        .http2_adaptive_window(Some(true))
        .timeout(Duration::from_secs(600))
        .add_service(
            byte_stream_service
                .into_service()
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            action_cache_service
                .into_service()
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            cas_service
                .into_service()
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .add_service(
            capabilities_service
                .into_service()
                .max_decoding_message_size(max_msg_size)
                .max_encoding_message_size(max_msg_size),
        )
        .serve(grpc_addr);

    let http_server = run_http_status_server(
        config.bind_address,
        config.http_port,
        cas_backend.clone(),
        ac_backend.clone(),
    );

    tokio::select! {
        result = grpc_server => {
            result?;
        }
        result = http_server => {
            result?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    const CONFIG_KEYS: &[&str] = &[
        "RBE_CACHE_BIND_ADDRESS",
        "RBE_CACHE_PORT",
        "RBE_CACHE_HTTP_PORT",
        "RBE_CACHE_DIR",
        "RBE_CACHE_MAX_SIZE_GB",
        "RBE_CACHE_AC_MAX_SIZE_GB",
        "RBE_CACHE_AC_TTL_SECS",
        "RBE_L1_CACHE_CAPACITY",
        "RBE_L1_CACHE_TTL_SECS",
    ];

    fn clear_config_env() {
        for key in CONFIG_KEYS {
            env::remove_var(key);
        }
    }

    #[test]
    fn test_config_parsing() {
        // Defaults
        clear_config_env();
        let config = CacheConfig::from_env();
        assert_eq!(config.bind_address, "0.0.0.0");
        assert_eq!(config.grpc_port, 9094);
        assert_eq!(config.http_port, 8080);
        assert_eq!(config.cache_dir, "/data");
        assert_eq!(config.max_size_gb, 100);
        assert_eq!(config.ac_max_size_gb, 10);
        assert_eq!(config.ac_ttl_secs, 3600);
        assert_eq!(config.l1_capacity, 100_000);
        assert_eq!(config.l1_ttl_secs, 3600);

        // Overrides from environment
        env::set_var("RBE_CACHE_BIND_ADDRESS", "127.0.0.1");
        env::set_var("RBE_CACHE_PORT", "19094");
        env::set_var("RBE_CACHE_HTTP_PORT", "18080");
        env::set_var("RBE_CACHE_DIR", "/tmp/cache");
        env::set_var("RBE_CACHE_MAX_SIZE_GB", "50");
        env::set_var("RBE_CACHE_AC_MAX_SIZE_GB", "5");
        env::set_var("RBE_CACHE_AC_TTL_SECS", "7200");
        env::set_var("RBE_L1_CACHE_CAPACITY", "1000");
        env::set_var("RBE_L1_CACHE_TTL_SECS", "600");

        let config = CacheConfig::from_env();
        assert_eq!(config.bind_address, "127.0.0.1");
        assert_eq!(config.grpc_port, 19094);
        assert_eq!(config.http_port, 18080);
        assert_eq!(config.cache_dir, "/tmp/cache");
        assert_eq!(config.max_size_gb, 50);
        assert_eq!(config.ac_max_size_gb, 5);
        assert_eq!(config.ac_ttl_secs, 7200);
        assert_eq!(config.l1_capacity, 1000);
        assert_eq!(config.l1_ttl_secs, 600);

        // Invalid values fall back to defaults
        env::set_var("RBE_CACHE_PORT", "not-a-number");
        let config = CacheConfig::from_env();
        assert_eq!(config.grpc_port, 9094);
    }
}
