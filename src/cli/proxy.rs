//! Local reverse proxy that carries a kubectl port-forward to FerrisRBE.
//!
//! `connect` spawns `kubectl port-forward` against the in-cluster rbe-cache,
//! with HTTP on an internal port and gRPC on `localhost:<grpc port>`, then
//! serves `http://localhost:<port>` that injects the Keycloak Bearer token
//! (refreshing on expiry or 401) before forwarding to the pod. Response bodies
//! stream untouched, so MCP streamable-HTTP/SSE works through it. gRPC is not
//! proxied: Bazel attaches the token itself through `--credential_helper`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use tokio::sync::RwLock;

use crate::oauth::{self, OAuthConfig};
use crate::store::{self, StoredTokens};

/// Headers scoped to a single connection, never forwarded in either
/// direction. `host` is included so the upstream sees its own authority.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// Shared state of the local proxy: the upstream port-forward and the
/// current tokens, which every request may refresh.
struct ProxyState {
    client: reqwest::Client,
    config: OAuthConfig,
    token_endpoint: String,
    /// Base URL of the port-forward, `http://127.0.0.1:<internal port>`.
    target: String,
    tokens: RwLock<StoredTokens>,
}

impl ProxyState {
    /// The current access token, refreshed first if it is about to expire.
    async fn valid_token(&self) -> anyhow::Result<String> {
        let expired = { self.tokens.read().await.expired() };
        if !expired {
            return Ok(self.tokens.read().await.access_token.clone());
        }
        self.refresh().await?;
        Ok(self.tokens.read().await.access_token.clone())
    }

    /// Renew the tokens and persist them. A failure to persist is ignored:
    /// the renewed tokens stay valid for this process either way.
    async fn refresh(&self) -> anyhow::Result<()> {
        let current = self.tokens.read().await.clone();
        let renewed = oauth::refresh_with_endpoint(&self.config, &self.token_endpoint, &current)
            .await
            .context("refrescando el token")?;
        *self.tokens.write().await = renewed;
        store::save(&self.tokens.read().await.clone()).ok();
        Ok(())
    }
}

/// Options of `rbe-cli connect`.
#[derive(Debug)]
struct ConnectArgs {
    port: u16,
    grpc_port: u16,
    namespace: String,
    service: String,
    remote_port: u16,
    remote_grpc_port: u16,
}

/// Parse `connect` options over their defaults, of which the namespace and
/// service come from `RBE_CONNECT_NAMESPACE` and `RBE_CONNECT_SERVICE` when
/// set; an unknown flag, a flag without a value or a port that does not
/// parse is an error.
fn parse_args(args: &[String]) -> anyhow::Result<ConnectArgs> {
    let env_or = |name: &str, default: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let mut parsed = ConnectArgs {
        port: 18080,
        grpc_port: 19094,
        namespace: env_or("RBE_CONNECT_NAMESPACE", "default"),
        service: env_or("RBE_CONNECT_SERVICE", "rbe-cache"),
        remote_port: 8080,
        remote_grpc_port: 9094,
    };
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .with_context(|| format!("{flag} requiere un valor"))
        };
        let port = |value: String| {
            value
                .parse::<u16>()
                .with_context(|| format!("{flag}: puerto invalido: {value}"))
        };
        match flag.as_str() {
            "--port" => parsed.port = port(value()?)?,
            "--grpc-port" => parsed.grpc_port = port(value()?)?,
            "--namespace" => parsed.namespace = value()?,
            "--service" => parsed.service = value()?,
            "--remote-port" => parsed.remote_port = port(value()?)?,
            "--remote-grpc-port" => parsed.remote_grpc_port = port(value()?)?,
            other => bail!("flag desconocida: {other}"),
        }
    }
    Ok(parsed)
}

/// An unused local port, as chosen by the OS for an ephemeral bind.
fn free_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Poll until something accepts TCP connections on `127.0.0.1:port`.
async fn wait_for_port(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("el port-forward no abrio el puerto {port} en {timeout:?}")
}

/// `rbe-cli connect`: start `kubectl port-forward` to the rbe-cache service
/// and serve an authenticating reverse proxy on `localhost:<port>` until
/// interrupted. The port-forward is killed when the proxy stops; if it does
/// not open both ports within 30 seconds, kubectl's stderr is reported.
pub async fn connect(args: &[String]) -> anyhow::Result<()> {
    let args = parse_args(args)?;
    let mut tokens = store::load()?
        .ok_or_else(|| anyhow::anyhow!("no hay tokens; corre primero: rbe-cli login"))?;
    let config = OAuthConfig::of(&tokens);
    if tokens.expired() {
        tokens = oauth::refresh(&config, &tokens).await?;
        store::save(&tokens)?;
    }

    let internal_port = free_port()?;
    let mut port_forward = tokio::process::Command::new("kubectl")
        .args([
            "-n",
            &args.namespace,
            "port-forward",
            &format!("svc/{}", args.service),
            "--address",
            "127.0.0.1",
            &format!("{internal_port}:{}", args.remote_port),
            &format!("{}:{}", args.grpc_port, args.remote_grpc_port),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("lanzando kubectl port-forward (kubectl debe estar en PATH)")?;

    let forwarded = async {
        wait_for_port(internal_port, Duration::from_secs(30)).await?;
        wait_for_port(args.grpc_port, Duration::from_secs(30)).await
    };
    if let Err(e) = forwarded.await {
        if let Some(stderr) = port_forward.stderr.take() {
            let mut message = String::new();
            use tokio::io::AsyncReadExt as _;
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut message).await;
            bail!("{e}\nkubectl dijo: {message}");
        }
        bail!("{e}");
    }
    println!(
        "port-forward activo: svc/{} ({}) -> 127.0.0.1:{}",
        args.service, args.namespace, internal_port
    );

    let discovered = oauth::discover(&config).await?;
    let state = Arc::new(ProxyState {
        client: reqwest::Client::builder().build()?,
        token_endpoint: discovered.token_endpoint,
        target: format!("http://127.0.0.1:{internal_port}"),
        tokens: RwLock::new(tokens),
        config,
    });

    let app = Router::new()
        .route("/", any(forward))
        .fallback(forward)
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port)).await?;
    println!("FerrisRBE (API + /mcp) en http://localhost:{}", args.port);
    println!(
        "FerrisRBE gRPC en grpc://localhost:{} (bazel build --config=remote-cache)",
        args.grpc_port
    );

    let result = axum::serve(listener, app).await;
    port_forward.kill().await.ok();
    result.context("proxy local")?;
    Ok(())
}

/// Forward one request to the port-forward with a Bearer token.
///
/// A `401` from upstream triggers one token refresh and one retry; a second
/// `401` is returned to the client as is.
async fn forward(State(state): State<Arc<ProxyState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => return bad_gateway(format!("leyendo el cuerpo: {e}")),
    };
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let url = format!("{}{}", state.target, path);
    let Ok(method) = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) else {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    };

    for attempt in 0..2 {
        let token = match state.valid_token().await {
            Ok(token) => token,
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    format!("token no renovable: {e}; corre: rbe-cli login"),
                )
                    .into_response()
            }
        };

        let Some(headers) = upstream_headers(&parts.headers, &token) else {
            return bad_gateway("token con caracteres invalidos".to_string());
        };

        let response = state
            .client
            .request(method.clone(), &url)
            .headers(headers)
            .body(bytes.to_vec())
            .send()
            .await;

        match response {
            Ok(response) => {
                if response.status() == StatusCode::UNAUTHORIZED
                    && attempt == 0
                    && state.refresh().await.is_ok()
                {
                    continue;
                }
                return map_response(response).await;
            }
            Err(e) => return bad_gateway(format!("reenviando a {}: {e}", state.target)),
        }
    }
    bad_gateway("agotados los reintentos".to_string())
}

/// The client's headers minus hop-by-hop ones, its own `Authorization` and
/// `Content-Length`, plus `Authorization: Bearer <token>`. `None` when the
/// token is not a valid header value.
fn upstream_headers(client_headers: &HeaderMap, token: &str) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in client_headers {
        let as_str = name.as_str();
        if HOP_BY_HOP.contains(&as_str)
            || as_str == header::AUTHORIZATION
            || as_str == header::CONTENT_LENGTH
        {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }
    let bearer = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    headers.insert(header::AUTHORIZATION, bearer);
    Some(headers)
}

/// Relay an upstream response, streaming its body untouched.
async fn map_response(response: reqwest::Response) -> Response {
    let mut builder = Response::builder().status(response.status().as_u16());
    for (name, value) in response.headers() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder
        .body(Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|e| bad_gateway(format!("construyendo la respuesta: {e}")))
}

fn bad_gateway(message: String) -> Response {
    (StatusCode::BAD_GATEWAY, message).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_defaults() {
        let args = parse_args(&[]).unwrap();
        assert_eq!(args.port, 18080);
        assert_eq!(args.grpc_port, 19094);
        assert_eq!(args.namespace, "default");
        assert_eq!(args.service, "rbe-cache");
        assert_eq!(args.remote_port, 8080);
        assert_eq!(args.remote_grpc_port, 9094);
    }

    #[test]
    fn connect_overrides() {
        let args = ["--grpc-port", "29094", "--remote-grpc-port", "9095"].map(String::from);
        let args = parse_args(&args).unwrap();
        assert_eq!(args.grpc_port, 29094);
        assert_eq!(args.remote_grpc_port, 9095);
    }

    #[test]
    fn bad_flags_are_errors() {
        let error = |args: &[&str]| {
            parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(error(&["--nope"]), "flag desconocida: --nope");
        assert_eq!(error(&["--port"]), "--port requiere un valor");
        assert_eq!(error(&["--port", "ouch"]), "--port: puerto invalido: ouch");
    }

    #[test]
    fn hop_by_hop_list_covers_the_basics() {
        assert!(HOP_BY_HOP.contains(&"connection"));
        assert!(HOP_BY_HOP.contains(&"upgrade"));
        assert!(!HOP_BY_HOP.contains(&"accept"));
    }
}
