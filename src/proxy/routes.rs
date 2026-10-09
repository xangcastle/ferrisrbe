//! HTTP surface of the cache server: Prometheus metrics, the Kubernetes
//! probes and, when enabled, the `/proxy/<host>/<path>` pull-through routes.
//!
//! `/healthz` (liveness) and `/readyz` (readiness) are distinct: the process
//! is live as soon as it serves HTTP, and ready only once every store has
//! rebuilt its in-memory state from the PVC.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use crate::metrics::Metrics;
use crate::proxy::error::ProxyError;
use crate::proxy::fetcher::parse_proxy_path;
use crate::proxy::pull::{PullThroughCache, Served};
use crate::readiness::Readiness;

/// Chunk size for reading cached blobs off the PVC.
const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Shared state for the HTTP handlers.
#[derive(Debug)]
pub struct ProxyState {
    pub cache: Arc<PullThroughCache>,
    pub metrics: Arc<Metrics>,
    /// Startup gates of the whole server, the proxy's own sweep included.
    pub readiness: Arc<Readiness>,
}

/// Build the router for the cache server's HTTP port.
pub fn router(state: Arc<ProxyState>) -> Router {
    let mut router = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz", get(health_handler))
        .route("/readyz", get(ready_handler));

    if state.cache.config().enabled {
        router = router
            .route("/proxy/{*path}", get(proxy_handler))
            .route("/proxy/", get(empty_proxy_handler))
            .route("/proxy", get(empty_proxy_handler));
    }

    router.with_state(state)
}

/// Serve the HTTP port until the process is torn down.
pub async fn serve(
    state: Arc<ProxyState>,
    bind_address: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{bind_address}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    if state.cache.config().enabled {
        info!("HTTP server listening on http://{addr} (metrics + pull-through proxy)");
    } else {
        info!("HTTP server listening on http://{addr} (metrics only; proxy disabled)");
    }
    axum::serve(listener, router(state)).await?;
    Ok(())
}

async fn metrics_handler(State(state): State<Arc<ProxyState>>) -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        state.metrics.gather(),
    )
        .into_response()
}

/// Liveness: the process is running and the router is wired.
async fn health_handler() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Readiness: `200` once every startup gate in `state.readiness` has
/// cleared, `503` naming the pending gates until then.
///
/// The proxy's sweep is one of those gates, registered and cleared by the
/// server like the CAS, action cache and BES warm-ups; `state.readiness` is
/// the single source of truth. Serving the proxy before its sweep would
/// report misses for artifacts already on disk and re-fetch every one.
async fn ready_handler(State(state): State<Arc<ProxyState>>) -> Response {
    let pending = state.readiness.pending();
    if pending.is_empty() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("warming up: {}", pending.join(", ")),
        )
            .into_response()
    }
}

async fn empty_proxy_handler() -> Response {
    error_response(&ProxyError::BadRequest(
        "expected /proxy/<host>/<path>".into(),
    ))
}

/// `GET /proxy/<host>/<path>`: validate the host against the egress policy,
/// then serve from cache or fetch through. Every outcome is recorded in the
/// request metrics by host and status label.
async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let started = Instant::now();

    let (host, url) = match parse_proxy_path(&path, query.as_deref()) {
        Ok(parsed) => parsed,
        Err(e) => {
            state
                .metrics
                .record_proxy_request("invalid", e.metric_label(), started.elapsed());
            return error_response(&e);
        }
    };

    if !state.cache.config().is_host_allowed(&host) {
        let e = ProxyError::HostNotAllowed(host.clone());
        state
            .metrics
            .record_proxy_request(&host, e.metric_label(), started.elapsed());
        return error_response(&e);
    }

    match state.cache.get(&url).await {
        Ok(served) => {
            let (status, response) = build_response(served);
            state
                .metrics
                .record_proxy_request(&host, status, started.elapsed());
            response
        }
        Err(e) => {
            error!(url, error = %e, "proxy request failed");
            state
                .metrics
                .record_proxy_request(&host, e.metric_label(), started.elapsed());
            error_response(&e)
        }
    }
}

/// Turn a resolved artifact into an HTTP response and its metrics label
/// (`hit`, `coalesced` or `miss`).
///
/// Only what the origin said about the body is forwarded: content type,
/// length and ETag, plus `x-ferris-cache: HIT|MISS`. Anything more risks
/// breaking client-side checksum or content-negotiation logic that build
/// tools rely on.
fn build_response(served: Served) -> (&'static str, Response) {
    match served {
        Served::FromDisk {
            meta,
            file,
            coalesced,
        } => {
            let mut headers = HeaderMap::new();
            insert_content_type(&mut headers, meta.content_type.as_deref());
            if let Ok(value) = HeaderValue::from_str(&meta.size.to_string()) {
                headers.insert(header::CONTENT_LENGTH, value);
            }
            if let Some(etag) = meta
                .etag
                .as_deref()
                .and_then(|e| HeaderValue::from_str(e).ok())
            {
                headers.insert(header::ETAG, etag);
            }
            let label = if coalesced { "coalesced" } else { "hit" };
            headers.insert("x-ferris-cache", HeaderValue::from_static("HIT"));

            let body = Body::from_stream(ReaderStream::with_capacity(file, READ_CHUNK_SIZE));
            (label, (StatusCode::OK, headers, body).into_response())
        }
        Served::Live {
            content_type,
            content_length,
            chunks,
        } => {
            let mut headers = HeaderMap::new();
            insert_content_type(&mut headers, content_type.as_deref());
            if let Some(value) =
                content_length.and_then(|l| HeaderValue::from_str(&l.to_string()).ok())
            {
                headers.insert(header::CONTENT_LENGTH, value);
            }
            headers.insert("x-ferris-cache", HeaderValue::from_static("MISS"));

            let body = Body::from_stream(ReceiverStream::new(chunks));
            ("miss", (StatusCode::OK, headers, body).into_response())
        }
    }
}

/// The origin's content type, or `application/octet-stream` when it sent
/// none or an invalid one.
fn insert_content_type(headers: &mut HeaderMap, content_type: Option<&str>) {
    let value = content_type
        .and_then(|ct| HeaderValue::from_str(ct).ok())
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_TYPE, value);
}

fn error_response(error: &ProxyError) -> Response {
    let status =
        StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, format!("{error}\n")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::ProxyConfig;
    use tempfile::TempDir;

    async fn state_with(config: ProxyConfig) -> (TempDir, Arc<ProxyState>) {
        let dir = TempDir::new().unwrap();
        let config = ProxyConfig {
            dir: dir.path().to_path_buf(),
            ..config
        };
        let metrics = Arc::new(Metrics::new());
        let cache = PullThroughCache::new(config, metrics.clone())
            .await
            .unwrap();
        (
            dir,
            Arc::new(ProxyState {
                cache,
                metrics,
                readiness: Arc::new(Readiness::new()),
            }),
        )
    }

    #[tokio::test]
    async fn router_builds_with_the_proxy_enabled() {
        let (_d, state) = state_with(ProxyConfig::default()).await;
        let _ = router(state);
    }

    #[tokio::test]
    async fn router_builds_with_the_proxy_disabled() {
        let (_d, state) = state_with(ProxyConfig {
            enabled: false,
            ..Default::default()
        })
        .await;
        let _ = router(state);
    }

    #[tokio::test]
    async fn readiness_is_false_until_the_index_is_warm() {
        let (_d, state) = state_with(ProxyConfig::default()).await;
        assert!(!state.cache.index().is_ready());
        state.cache.warm_up().await.unwrap();
        assert!(state.cache.index().is_ready());
    }

    #[tokio::test]
    async fn a_disabled_proxy_reports_ready_immediately() {
        let (_d, state) = state_with(ProxyConfig {
            enabled: false,
            ..Default::default()
        })
        .await;
        assert!(!state.cache.index().is_ready());
        let response = ready_handler(State(state)).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "no sweep ever runs for a disabled proxy, so readiness must not wait on one"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_prometheus_text() {
        let (_d, state) = state_with(ProxyConfig::default()).await;
        let response = metrics_handler(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; version=0.0.4"
        );
    }

    #[test]
    fn error_responses_carry_the_mapped_status() {
        assert_eq!(
            error_response(&ProxyError::HostNotAllowed("evil.com".into())).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            error_response(&ProxyError::BadRequest("nope".into())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            error_response(&ProxyError::UpstreamTimeout("https://x/".into())).status(),
            StatusCode::GATEWAY_TIMEOUT
        );
    }
}
