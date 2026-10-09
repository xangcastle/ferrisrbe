//! Authentication of the HTTP and gRPC surfaces.
//!
//! Both accept the same credentials: a configured static token, or an RS256
//! access token issued by the Keycloak realm in `RBE_KEYCLOAK_URL` and
//! `RBE_KEYCLOAK_REALM`. Without that realm only static tokens are accepted.
//!
//! - HTTP (`RBE_AUTH_ENABLED`): `/api/*` and `/mcp` are protected. Probes
//!   (`/healthz`, `/readyz`, `/metrics`), the pull-through proxy and the SPA
//!   stay open: Bazel's downloader cannot do OIDC, and the SPA is useless
//!   without an authorized API.
//! - gRPC (`RBE_GRPC_AUTH_ENABLED`): every call to the REAPI, BES, Remote
//!   Asset and worker services is protected, except load-balancer health
//!   checks. Bazel presents the credential through `--credential_helper`, so
//!   the token never appears on a command line recorded by the BES.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::RwLock;
use tonic::body::BoxBody;
use tracing::debug;

/// Minimum spacing between JWKS fetches. A token signed with an unknown key
/// triggers at most one refetch per interval, so a flood of forged `kid`s
/// cannot turn into a flood of requests to Keycloak.
const JWKS_MIN_RETRY: Duration = Duration::from_secs(2);
const JWKS_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// gRPC paths reachable without credentials: load-balancer health checks,
/// which cannot present a token.
const GRPC_OPEN_PATH_PREFIXES: &[&str] = &["/grpc.health.v1.Health/", "/AWS.ALB/"];

/// How long a Keycloak token stays trusted after its signature was verified,
/// never beyond its own `exp`. Every gRPC call carries the token, and an RS256
/// verification per CAS read would dominate the latency of small blobs.
const VERIFIED_TOKEN_TTL: Duration = Duration::from_secs(300);
const VERIFIED_TOKEN_CAPACITY: u64 = 10_000;

/// Who may call the protected HTTP surfaces, from the `RBE_AUTH_*` and
/// `RBE_KEYCLOAK_*` environment.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// When `false`, every HTTP request passes through unauthenticated.
    pub enabled: bool,
    /// When `false`, every gRPC call passes through unauthenticated.
    pub grpc_enabled: bool,
    /// Empty when no realm is configured: then no Keycloak token validates.
    pub keycloak_url: String,
    pub realm: String,
    /// Empty list accepts any `azp`; otherwise the token client must match.
    pub allowed_clients: Vec<String>,
    /// Long-lived bearer tokens for headless clients that cannot run the
    /// Keycloak flow (e.g. the MCP client in an editor). Compared by SHA-256.
    pub static_tokens: Vec<String>,
}

/// The non-empty, trimmed entries of a comma-separated list.
fn comma_separated(list: &str) -> Vec<String> {
    list.split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

impl AuthConfig {
    pub fn from_env() -> Self {
        let flag = |name: &str| {
            std::env::var(name)
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false)
        };
        let enabled = flag("RBE_AUTH_ENABLED");
        let grpc_enabled = flag("RBE_GRPC_AUTH_ENABLED");
        let keycloak_url = std::env::var("RBE_KEYCLOAK_URL").unwrap_or_default();
        let realm = std::env::var("RBE_KEYCLOAK_REALM").unwrap_or_default();
        let allowed_clients =
            comma_separated(&std::env::var("RBE_AUTH_ALLOWED_CLIENTS").unwrap_or_default());
        let static_tokens =
            comma_separated(&std::env::var("RBE_AUTH_STATIC_TOKENS").unwrap_or_default());
        Self {
            enabled,
            grpc_enabled,
            keycloak_url,
            realm,
            allowed_clients,
            static_tokens,
        }
    }

    /// The realm's issuer URL, which every accepted token's `iss` must equal.
    pub fn issuer(&self) -> String {
        format!(
            "{}/realms/{}",
            self.keycloak_url.trim_end_matches('/'),
            self.realm
        )
    }

    /// Where the realm publishes its signing keys.
    pub fn jwks_url(&self) -> String {
        format!("{}/protocol/openid-connect/certs", self.issuer())
    }
}

/// Why a request was refused; rendered as the body of the 401 response.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("missing bearer token")]
    MissingToken,

    #[error("invalid token: {0}")]
    InvalidToken(#[from] jsonwebtoken::errors::Error),

    #[error("token client not allowed")]
    ClientNotAllowed,

    #[error("unknown signing key")]
    UnknownKey,

    #[error("failed to fetch JWKS: {0}")]
    JwksFetch(#[from] reqwest::Error),

    #[error("invalid JWKS: {0}")]
    JwksFormat(String),

    #[error("no Keycloak realm configured (RBE_KEYCLOAK_URL, RBE_KEYCLOAK_REALM)")]
    NoRealm,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// The claims of a validated Keycloak access token that the server reads.
#[derive(Debug, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub exp: u64,
    pub azp: Option<String>,
    pub preferred_username: Option<String>,
}

#[derive(Default)]
struct JwksCache {
    by_kid: HashMap<String, Arc<DecodingKey>>,
    last_fetch: Option<Instant>,
}

impl JwksCache {
    /// The cached key for `kid`; `Err(UnknownKey)` when it is absent and the
    /// keys were fetched within [`JWKS_MIN_RETRY`]; `Ok(None)` when absent and
    /// a refetch is due.
    fn lookup(&self, kid: &str) -> Result<Option<Arc<DecodingKey>>, AuthError> {
        if let Some(key) = self.by_kid.get(kid) {
            return Ok(Some(key.clone()));
        }
        match self.last_fetch {
            Some(fetched) if fetched.elapsed() < JWKS_MIN_RETRY => Err(AuthError::UnknownKey),
            _ => Ok(None),
        }
    }
}

/// Whether a token whose authorized party is `azp` may call the server. An
/// empty allowlist admits every client, including tokens without `azp`.
fn client_allowed(allowed_clients: &[String], azp: Option<&str>) -> bool {
    allowed_clients.is_empty()
        || azp.is_some_and(|azp| allowed_clients.iter().any(|client| client == azp))
}

/// Configuration plus the realm's signing keys, fetched lazily and cached by
/// key id.
pub struct AuthState {
    config: AuthConfig,
    http: reqwest::Client,
    keys: RwLock<JwksCache>,
    /// SHA-256 of Keycloak tokens whose signature was already verified,
    /// mapped to their `exp`; see [`VERIFIED_TOKEN_TTL`].
    verified: moka::future::Cache<[u8; 32], u64>,
}

impl AuthState {
    pub fn new(config: AuthConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(JWKS_HTTP_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            config,
            http,
            keys: RwLock::new(JwksCache::default()),
            verified: moka::future::Cache::builder()
                .max_capacity(VERIFIED_TOKEN_CAPACITY)
                .time_to_live(VERIFIED_TOKEN_TTL)
                .build(),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

    pub fn from_env() -> Self {
        Self::new(AuthConfig::from_env())
    }

    /// The RS256 RSA keys of `jwks` by key id; keys without an id, of another
    /// type or algorithm, or with malformed components are skipped.
    fn parse_jwks(jwks: &Jwks) -> HashMap<String, Arc<DecodingKey>> {
        let mut by_kid = HashMap::new();
        for key in &jwks.keys {
            let (Some(kid), Some(n), Some(e)) = (&key.kid, &key.n, &key.e) else {
                continue;
            };
            if key.kty != "RSA" {
                continue;
            }
            if let Some(alg) = &key.alg {
                if alg != "RS256" {
                    continue;
                }
            }
            if let Ok(decoding_key) = DecodingKey::from_rsa_components(n, e) {
                by_kid.insert(kid.clone(), Arc::new(decoding_key));
            }
        }
        by_kid
    }

    /// The signing key for `kid`, refetching the JWKS when the key is not
    /// cached. The lookup is repeated under the write lock, so concurrent
    /// requests for the same new key trigger a single fetch.
    async fn decoding_key(&self, kid: &str) -> Result<Arc<DecodingKey>, AuthError> {
        if let Some(key) = self.keys.read().await.lookup(kid)? {
            return Ok(key);
        }

        let mut cache = self.keys.write().await;
        if let Some(key) = cache.lookup(kid)? {
            return Ok(key);
        }

        let body = self
            .http
            .get(self.config.jwks_url())
            .send()
            .await?
            .text()
            .await?;
        let jwks: Jwks =
            serde_json::from_str(&body).map_err(|e| AuthError::JwksFormat(e.to_string()))?;
        cache.by_kid = Self::parse_jwks(&jwks);
        cache.last_fetch = Some(Instant::now());
        cache.by_kid.get(kid).cloned().ok_or(AuthError::UnknownKey)
    }

    /// Validates an RS256 access token: signature against the realm's keys,
    /// expiry, issuer, and the `azp` claim against the client allowlist.
    ///
    /// Audience is not checked: Keycloak access tokens carry `aud="account"`,
    /// and the calling client is identified by `azp` instead.
    pub async fn validate(&self, token: &str) -> Result<Claims, AuthError> {
        if self.config.keycloak_url.is_empty() || self.config.realm.is_empty() {
            return Err(AuthError::NoRealm);
        }
        let header = decode_header(token)?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::InvalidToken(
                jsonwebtoken::errors::ErrorKind::InvalidAlgorithm.into(),
            ));
        }
        let kid = header.kid.ok_or(AuthError::UnknownKey)?;
        let key = self.decoding_key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.config.issuer()]);
        validation.validate_aud = false;
        let data = decode::<Claims>(token, &key, &validation)?;

        if !client_allowed(&self.config.allowed_clients, data.claims.azp.as_deref()) {
            return Err(AuthError::ClientNotAllowed);
        }
        Ok(data.claims)
    }

    /// Whether `token` grants access: a configured static token, or a
    /// Keycloak access token that [`validate`] accepts. A Keycloak token is
    /// verified once and then trusted until the earlier of its `exp` and
    /// [`VERIFIED_TOKEN_TTL`].
    ///
    /// [`validate`]: AuthState::validate
    pub async fn authorize(&self, token: &str) -> Result<(), AuthError> {
        if token_is_static(&self.config.static_tokens, token) {
            return Ok(());
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if let Some(exp) = self.verified.get(&digest).await {
            if exp > unix_now() {
                return Ok(());
            }
            self.verified.invalidate(&digest).await;
        }
        let claims = self.validate(token).await?;
        self.verified.insert(digest, claims.exp).await;
        Ok(())
    }

    /// Enforces [`AuthConfig::grpc_enabled`] on one gRPC call to `path`:
    /// open paths and a disabled configuration pass, anything else needs an
    /// `authorization: Bearer` credential that [`authorize`] accepts.
    ///
    /// [`authorize`]: AuthState::authorize
    async fn authorize_grpc(&self, path: &str, headers: &HeaderMap) -> Result<(), AuthError> {
        if !self.config.grpc_enabled
            || GRPC_OPEN_PATH_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix))
        {
            return Ok(());
        }
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(AuthError::MissingToken)?;
        self.authorize(token).await
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Tower layer applying [`GrpcAuthService`] to every service of the gRPC
/// server.
#[derive(Clone)]
pub struct GrpcAuthLayer {
    state: Arc<AuthState>,
}

impl GrpcAuthLayer {
    pub fn new(state: Arc<AuthState>) -> Self {
        Self { state }
    }
}

impl<S> tower::Layer<S> for GrpcAuthLayer {
    type Service = GrpcAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GrpcAuthService {
            inner,
            state: self.state.clone(),
        }
    }
}

/// Answers a gRPC call that lacks an accepted credential with
/// `UNAUTHENTICATED`, as a trailers-only response, before it reaches a
/// service; every other call is forwarded untouched.
#[derive(Clone)]
pub struct GrpcAuthService<S> {
    inner: S,
    state: Arc<AuthState>,
}

impl<S> tower::Service<axum::http::Request<BoxBody>> for GrpcAuthService<S>
where
    S: tower::Service<axum::http::Request<BoxBody>, Response = axum::http::Response<BoxBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::http::Response<BoxBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: axum::http::Request<BoxBody>) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let state = self.state.clone();
        Box::pin(async move {
            if let Err(e) = state
                .authorize_grpc(request.uri().path(), request.headers())
                .await
            {
                debug!("gRPC call to {} rejected: {}", request.uri().path(), e);
                return Ok(tonic::Status::unauthenticated(e.to_string()).into_http());
            }
            inner.call(request).await
        })
    }
}

/// Whether `path` belongs to a protected surface: the REST API and MCP.
fn needs_auth(path: &str) -> bool {
    path.starts_with("/api/") || path == "/api" || path.starts_with("/mcp")
}

/// Whether `token` is one of the configured static tokens.
///
/// Tokens are compared by SHA-256 digest: every comparison runs over the
/// same fixed length regardless of how much of the plaintext matches.
fn token_is_static(static_tokens: &[String], token: &str) -> bool {
    if static_tokens.is_empty() {
        return false;
    }
    let presented = Sha256::digest(token.as_bytes());
    static_tokens
        .iter()
        .any(|configured| Sha256::digest(configured.as_bytes())[..] == presented[..])
}

/// The bearer token of a request: the `Authorization: Bearer` header, or the
/// `token` query parameter, which browser WebSocket handshakes use because
/// they cannot set headers.
fn presented_token(req: &Request) -> Result<&str, AuthError> {
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if let Some(token) = from_header {
        return Ok(token);
    }
    let query = req.uri().query().ok_or(AuthError::MissingToken)?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .ok_or(AuthError::MissingToken)
}

/// Axum middleware enforcing [`AuthConfig`] on the protected paths: a static
/// token or a valid Keycloak access token is required, anything else gets a
/// `401` with a `WWW-Authenticate: Bearer` challenge.
pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    req: Request,
    next: Next,
) -> Response {
    if !state.config.enabled || !needs_auth(req.uri().path()) {
        return next.run(req).await;
    }

    let token = match presented_token(&req) {
        Ok(token) => token,
        Err(e) => return unauthorized(e),
    };

    if token_is_static(&state.config.static_tokens, token) {
        debug!("authorized static token for {}", req.uri().path());
        return next.run(req).await;
    }

    match state.validate(token).await {
        Ok(claims) => {
            debug!(
                "authorized {} for {}",
                claims
                    .preferred_username
                    .as_deref()
                    .unwrap_or("<no username>"),
                req.uri().path()
            );
            next.run(req).await
        }
        Err(e) => unauthorized(e),
    }
}

fn unauthorized(error: AuthError) -> Response {
    debug!("request rejected: {}", error);
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"ferris-rbe\"")],
        error.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use tower::ServiceExt;

    const SAMPLE_JWKS: &str = r#"{
        "keys": [
            {
                "kid": "rsa-key-1",
                "kty": "RSA",
                "alg": "RS256",
                "use": "sig",
                "n": "oahUIoWw0K0usKNu_O6JuOW77y8fV3d6Va2XYf8WlCUQ",
                "e": "AQAB"
            },
            {
                "kid": "ec-key",
                "kty": "EC",
                "alg": "ES256",
                "crv": "P-256",
                "x": "MKBCTNIcKUSDii11ySs3526iDZ8AiTo7Tu6KPAqv7D4",
                "y": "4Etl6SRW2YiLUrN5vfvVHuhp7x8PxltmWWlbbM4IFyM"
            },
            {
                "kty": "RSA",
                "n": "no-kid-key",
                "e": "AQAB"
            }
        ]
    }"#;

    #[test]
    fn jwks_parsing_keeps_only_rs256_keys_with_kid() {
        let jwks: Jwks = serde_json::from_str(SAMPLE_JWKS).unwrap();
        let by_kid = AuthState::parse_jwks(&jwks);
        assert_eq!(by_kid.len(), 1);
        assert!(by_kid.contains_key("rsa-key-1"));
    }

    #[test]
    fn only_api_and_mcp_paths_require_authentication() {
        for path in ["/api/stats", "/api/builds/abc", "/api", "/mcp", "/mcp/x"] {
            assert!(needs_auth(path), "{path}");
        }
        for path in [
            "/healthz",
            "/readyz",
            "/metrics",
            "/",
            "/builds",
            "/proxy/github.com/foo",
            "/apihealth",
        ] {
            assert!(!needs_auth(path), "{path}");
        }
    }

    #[test]
    fn azp_must_be_in_the_allowlist_unless_it_is_empty() {
        let allowed = vec!["rbe-cli".to_string()];
        assert!(client_allowed(&allowed, Some("rbe-cli")));
        assert!(!client_allowed(&allowed, Some("other-client")));
        assert!(!client_allowed(&allowed, None));

        assert!(client_allowed(&[], Some("anything")));
        assert!(client_allowed(&[], None));
    }

    #[test]
    fn comma_separated_lists_drop_blank_entries() {
        assert_eq!(
            comma_separated(" rbe-cli, ,ferris-rbe-ui ,"),
            vec!["rbe-cli".to_string(), "ferris-rbe-ui".to_string()]
        );
        assert!(comma_separated("").is_empty());
    }

    #[tokio::test]
    async fn without_a_realm_only_static_tokens_pass() {
        let state = AuthState::new(AuthConfig {
            enabled: true,
            grpc_enabled: true,
            keycloak_url: String::new(),
            realm: String::new(),
            allowed_clients: vec![],
            static_tokens: vec!["s3cret".to_string()],
        });
        assert!(state.authorize("s3cret").await.is_ok());
        assert!(matches!(
            state.authorize("header.payload.signature").await,
            Err(AuthError::NoRealm)
        ));
    }

    #[tokio::test]
    async fn disabled_auth_passes_everything_through() {
        let state = Arc::new(AuthState::new(AuthConfig {
            enabled: false,
            grpc_enabled: false,
            keycloak_url: "https://keycloak.example.com".to_string(),
            realm: "ferris".to_string(),
            allowed_clients: vec!["rbe-cli".to_string()],
            static_tokens: vec![],
        }));
        let app = Router::new()
            .route("/api/stats", get(|| async { "stats" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn enabled_auth_rejects_api_but_not_probes() {
        let state = Arc::new(AuthState::new(AuthConfig {
            enabled: true,
            grpc_enabled: false,
            keycloak_url: "https://keycloak.example.com".to_string(),
            realm: "ferris".to_string(),
            allowed_clients: vec!["rbe-cli".to_string()],
            static_tokens: vec![],
        }));
        let app = Router::new()
            .route("/api/stats", get(|| async { "stats" }))
            .route("/healthz", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn static_token_authorizes_headless_clients() {
        let state = Arc::new(AuthState::new(AuthConfig {
            enabled: true,
            grpc_enabled: false,
            keycloak_url: "https://keycloak.example.com".to_string(),
            realm: "ferris".to_string(),
            allowed_clients: vec!["rbe-cli".to_string()],
            static_tokens: vec!["opaque-machine-token".to_string()],
        }));
        let app = Router::new()
            .route("/api/stats", get(|| async { "stats" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware));

        let authorized = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, "Bearer opaque-machine-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);

        let rejected = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/stats")
                    .header(header::AUTHORIZATION, "Bearer opaque-machine-tokfn")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

        let via_query = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/stats?token=opaque-machine-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_query.status(), StatusCode::OK);
    }

    fn grpc_state(grpc_enabled: bool) -> Arc<AuthState> {
        Arc::new(AuthState::new(AuthConfig {
            enabled: false,
            grpc_enabled,
            keycloak_url: "http://127.0.0.1:9".to_string(),
            realm: "ferris".to_string(),
            allowed_clients: vec!["rbe-cli".to_string()],
            static_tokens: vec!["ci-machine-token".to_string()],
        }))
    }

    /// A gRPC client of `state` in front of a service that always answers
    /// `200` with an empty body; returns the `grpc-status` of the reply, or
    /// `None` when the call reached the service.
    async fn grpc_status_of(
        state: Arc<AuthState>,
        path: &str,
        authorization: Option<&str>,
    ) -> Option<String> {
        use tower::{Layer as _, Service as _};
        let backend = tower::service_fn(|_: axum::http::Request<BoxBody>| async {
            Ok::<_, std::convert::Infallible>(axum::http::Response::new(tonic::body::empty_body()))
        });
        let mut service = GrpcAuthLayer::new(state).layer(backend);
        let mut request = axum::http::Request::builder().uri(path);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        let response = service
            .call(request.body(tonic::body::empty_body()).unwrap())
            .await
            .unwrap();
        response
            .headers()
            .get("grpc-status")
            .map(|v| v.to_str().unwrap().to_string())
    }

    const CAS_PATH: &str =
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs";

    #[tokio::test]
    async fn grpc_calls_pass_while_grpc_auth_is_disabled() {
        assert_eq!(
            grpc_status_of(grpc_state(false), CAS_PATH, None).await,
            None
        );
    }

    #[tokio::test]
    async fn grpc_calls_without_a_credential_are_unauthenticated() {
        let unauthenticated = Some((tonic::Code::Unauthenticated as i32).to_string());
        assert_eq!(
            grpc_status_of(grpc_state(true), CAS_PATH, None).await,
            unauthenticated
        );
        assert_eq!(
            grpc_status_of(grpc_state(true), CAS_PATH, Some("Basic abc")).await,
            unauthenticated,
            "only bearer credentials are accepted"
        );
        assert_eq!(
            grpc_status_of(grpc_state(true), CAS_PATH, Some("Bearer not-a-jwt")).await,
            unauthenticated
        );
    }

    #[tokio::test]
    async fn grpc_static_token_reaches_the_service() {
        assert_eq!(
            grpc_status_of(grpc_state(true), CAS_PATH, Some("Bearer ci-machine-token")).await,
            None
        );
    }

    #[tokio::test]
    async fn grpc_health_checks_need_no_credential() {
        for path in ["/grpc.health.v1.Health/Check", "/AWS.ALB/healthcheck"] {
            assert_eq!(
                grpc_status_of(grpc_state(true), path, None).await,
                None,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn verified_keycloak_tokens_are_trusted_until_their_expiry() {
        let state = grpc_state(true);
        let live = "live-token";
        let lapsed = "lapsed-token";
        let digest = |t: &str| -> [u8; 32] { Sha256::digest(t.as_bytes()).into() };
        state.verified.insert(digest(live), unix_now() + 60).await;
        state.verified.insert(digest(lapsed), unix_now() - 1).await;

        assert!(state.authorize(live).await.is_ok());
        assert!(
            state.authorize(lapsed).await.is_err(),
            "an expired cached token must be re-verified, and this one cannot be"
        );
        assert!(state.verified.get(&digest(lapsed)).await.is_none());
    }

    #[test]
    fn token_is_static_matches_by_digest() {
        let tokens = vec!["alpha".to_string(), "beta".to_string()];
        assert!(token_is_static(&tokens, "alpha"));
        assert!(token_is_static(&tokens, "beta"));
        assert!(!token_is_static(&tokens, "alphb"));
        assert!(!token_is_static(&tokens, ""));
        assert!(!token_is_static(&[], "alpha"));
    }
}
