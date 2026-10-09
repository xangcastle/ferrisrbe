//! BES REST API under `/api/`.
//!
//! Serves builds, their events, console, targets and cache misses, the
//! cross-build target and test histories, and snapshots of the worker
//! registry and the pull-through cache. Listings are paginated server-side.
//! Storage failures are logged and answered with `500`; endpoints whose
//! backing component is not configured answer `501`.

use std::fmt::Display;
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::bes::config::{is_invocation_id, BesConfig};
use crate::bes::models::{BuildListItem, HealthResponse, TargetExecutionStatus};
use crate::bes::profile::ProfileLookup;
use crate::bes::storage::{BesStorage, DEFAULT_PAGE_SIZE};
use crate::cas::SharedCasBackend;
use crate::proxy::PullThroughCache;
use crate::types::DigestInfo;
use crate::worker::k8s::WorkerRegistry;

/// Shared state of the BES API handlers.
#[derive(Clone)]
pub struct BesApiState {
    pub storage: BesStorage,
    pub config: BesConfig,
    /// Serves the stdout/stderr blobs Bazel uploaded for failed actions.
    pub cas: Option<SharedCasBackend>,
    /// Backs GET /api/workers with a REST snapshot of the registry.
    pub workers: Option<Arc<WorkerRegistry>>,
    /// Backs GET /api/proxy/externals with the pull-through cache listing.
    pub proxy: Option<Arc<PullThroughCache>>,
}

/// BES REST API server.
#[derive(Clone)]
pub struct BesApi {
    state: Arc<BesApiState>,
}

impl BesApi {
    /// An API backed by `storage`, with no CAS, worker registry or proxy.
    pub fn new(storage: BesStorage, config: BesConfig) -> Self {
        Self {
            state: Arc::new(BesApiState {
                storage,
                config,
                cas: None,
                workers: None,
                proxy: None,
            }),
        }
    }

    /// Enables `GET /api/blobs/{hash}/{size}`.
    pub fn with_cas(mut self, cas: SharedCasBackend) -> Self {
        let state = Arc::make_mut(&mut self.state);
        state.cas = Some(cas);
        self
    }

    /// Enables `GET /api/workers`.
    pub fn with_workers(mut self, workers: Arc<WorkerRegistry>) -> Self {
        let state = Arc::make_mut(&mut self.state);
        state.workers = Some(workers);
        self
    }

    /// Enables `GET /api/proxy/externals`.
    pub fn with_proxy(mut self, proxy: Arc<PullThroughCache>) -> Self {
        let state = Arc::make_mut(&mut self.state);
        state.proxy = Some(proxy);
        self
    }

    /// The router serving every `/api/` endpoint.
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        Router::new()
            .route("/api/health", get(health_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/workers", get(list_workers_handler))
            .route("/api/builds", get(list_builds_handler))
            .route("/api/builds/{id}", get(get_build_handler))
            .route("/api/builds/{id}/events", get(get_events_handler))
            .route(
                "/api/builds/{id}/timeline",
                get(get_action_timeline_handler),
            )
            .route("/api/builds/{id}/log", get(get_log_handler))
            .route("/api/builds/{id}/inputs", get(get_build_inputs_handler))
            .route("/api/builds/{id}/profile", get(get_profile_handler))
            .route("/api/blobs/{hash}/{size}", get(get_blob_handler))
            .route("/api/builds/{id}/misses", get(get_misses_handler))
            .route("/api/builds/{id}/targets", get(get_build_targets_handler))
            .route("/api/targets", get(list_targets_handler))
            .route("/api/targets/{label}", get(get_target_history_handler))
            .route("/api/tests", get(list_tests_handler))
            .route("/api/tests/{label}", get(get_test_history_handler))
            .route("/api/proxy/externals", get(list_proxy_externals_handler))
            .with_state(state)
    }
}

/// `result` as a JSON `200`, or a logged `500` whose log line reads
/// `"{failure}: {error}"`.
fn json_or_500<T: Serialize, E: Display>(
    result: Result<T, E>,
    failure: impl FnOnce() -> String,
) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => {
            error!("{}: {}", failure(), e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse::default())
}

async fn stats_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    json_or_500(state.storage.stats().await, || {
        "Failed to compute stats".to_string()
    })
}

async fn list_workers_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    match &state.workers {
        Some(registry) => Json(registry.snapshot()).into_response(),
        None => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

/// Query of `GET /api/proxy/externals`.
#[derive(Debug, Default, Deserialize)]
struct ExternalsQuery {
    /// Page size; clamped to the server maximum.
    limit: Option<usize>,
    offset: Option<usize>,
    /// Case-insensitive substring of the artifact's URL or package name.
    q: Option<String>,
    /// Only artifacts of this ecosystem (`python`, `npm`, `go`, ...).
    ecosystem: Option<String>,
    /// `downloads` (default), `size`, `newest` or `name`.
    sort: Option<String>,
}

async fn list_proxy_externals_handler(
    State(state): State<Arc<BesApiState>>,
    Query(query): Query<ExternalsQuery>,
) -> impl IntoResponse {
    match &state.proxy {
        Some(cache) => {
            let ecosystem = match query.ecosystem.as_deref().filter(|e| !e.is_empty()) {
                None => None,
                Some(value) => match crate::proxy::artifact::Ecosystem::parse(value) {
                    Some(ecosystem) => Some(ecosystem),
                    None => return StatusCode::BAD_REQUEST.into_response(),
                },
            };
            let page = cache
                .list_externals_page(
                    query.limit.unwrap_or(DEFAULT_PAGE_SIZE),
                    query.offset.unwrap_or(0),
                    query.q.as_deref().unwrap_or(""),
                    ecosystem,
                    crate::proxy::ExternalsSort::parse(query.sort.as_deref().unwrap_or("")),
                )
                .await;
            Json(page).into_response()
        }
        None => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

/// Query of `GET /api/builds`.
#[derive(Debug, Default, Deserialize)]
struct BuildsQuery {
    /// Exact match against the BRANCH_NAME build metadata.
    branch: Option<String>,
    /// Exact match against the PR_NUMBER build metadata.
    pr: Option<String>,
    /// Page size; clamped to the server maximum.
    limit: Option<usize>,
    /// `next_cursor` of the previous page.
    cursor: Option<String>,
}

/// Whether `build` carries the requested branch and pull request in its
/// build metadata. An absent filter matches every build.
fn matches_build_filters(build: &BuildListItem, branch: Option<&str>, pr: Option<&str>) -> bool {
    let metadata_matches = |key: &str, want: Option<&str>| match want {
        None => true,
        Some(want) => build.metadata.get(key).map(|v| v == want).unwrap_or(false),
    };
    metadata_matches("BRANCH_NAME", branch) && metadata_matches("PR_NUMBER", pr)
}

async fn list_builds_handler(
    State(state): State<Arc<BesApiState>>,
    Query(query): Query<BuildsQuery>,
) -> impl IntoResponse {
    let page = state
        .storage
        .list_builds_page(
            query.limit.unwrap_or(DEFAULT_PAGE_SIZE),
            query.cursor.as_deref(),
            |b| matches_build_filters(b, query.branch.as_deref(), query.pr.as_deref()),
        )
        .await;
    json_or_500(page, || "Failed to list builds".to_string())
}

/// The `{id}` of a `/api/builds/{id}` route. An id that is not an
/// invocation id ([`is_invocation_id`]) is `404`: no build can have it, and
/// it never reaches a path under the BES data directory.
struct InvocationPath(String);

impl<S: Send + Sync> FromRequestParts<S> for InvocationPath {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        if is_invocation_id(&id) {
            Ok(Self(id))
        } else {
            Err(StatusCode::NOT_FOUND)
        }
    }
}

async fn get_build_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    match state.storage.get_build(&id).await {
        Ok(Some(summary)) => Json(summary).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to get build {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_build_inputs_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    match state.storage.get_build_inputs(&id).await {
        Ok(Some(inputs)) => Json(inputs).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to read the inputs of build {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The build's Bazel profile summarized for a Gantt chart: 404 when the
/// build announced no profile, 410 when the CAS no longer holds it.
async fn get_profile_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    let Some(cas) = &state.cas else {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    };
    match crate::bes::profile::load(&state.storage, cas, &id).await {
        Ok(ProfileLookup::Found(profile)) => Json(profile).into_response(),
        Ok(ProfileLookup::NotAnnounced) => StatusCode::NOT_FOUND.into_response(),
        Ok(ProfileLookup::Evicted) => StatusCode::GONE.into_response(),
        Ok(ProfileLookup::Unreadable(e)) => {
            error!("Failed to parse the profile of {}: {}", id, e);
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
        Err(e) => {
            error!("Failed to load the profile of {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Query of `GET /api/builds/{id}/events`.
#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    /// Only the events worth reading: command, metadata, failures, finish.
    #[serde(default)]
    important: bool,
}

async fn get_events_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
    Query(query): Query<EventsQuery>,
) -> impl IntoResponse {
    json_or_500(state.storage.get_events(&id, query.important).await, || {
        format!("Failed to get events for {}", id)
    })
}

async fn get_action_timeline_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    json_or_500(state.storage.get_action_timeline(&id).await, || {
        format!("Failed to build action timeline for {}", id)
    })
}

async fn get_log_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    match state.storage.get_console(&id).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(e) => {
            error!("Failed to get log for {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A CAS blob as text; used for the stdout and stderr of failed actions.
async fn get_blob_handler(
    State(state): State<Arc<BesApiState>>,
    Path((hash, size)): Path<(String, i64)>,
) -> impl IntoResponse {
    let Some(cas) = &state.cas else {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    };
    let Ok(digest) = DigestInfo::new(&hash, size) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match cas.read(&digest).await {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            bytes.to_vec(),
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to read blob {}/{}: {}", hash, size, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_misses_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    json_or_500(state.storage.get_misses(&id).await, || {
        format!("Failed to get misses for {}", id)
    })
}

async fn get_build_targets_handler(
    State(state): State<Arc<BesApiState>>,
    InvocationPath(id): InvocationPath,
) -> impl IntoResponse {
    json_or_500(state.storage.get_build_targets(&id).await, || {
        format!("Failed to get targets for build {}", id)
    })
}

/// Query of `GET /api/targets` and `GET /api/tests`; `status` applies to
/// targets only.
#[derive(Debug, Deserialize)]
struct LabelsQuery {
    /// Page size; clamped to the server maximum.
    limit: Option<usize>,
    offset: Option<usize>,
    /// Case-insensitive substring of the label (or target kind).
    q: Option<String>,
    /// Latest status to keep: success, failure, cached or dependency.
    status: Option<TargetExecutionStatus>,
}

async fn list_targets_handler(
    State(state): State<Arc<BesApiState>>,
    Query(query): Query<LabelsQuery>,
) -> impl IntoResponse {
    let page = state
        .storage
        .list_targets_page(
            query.limit.unwrap_or(DEFAULT_PAGE_SIZE),
            query.offset.unwrap_or(0),
            query.q.as_deref().unwrap_or(""),
            query.status,
        )
        .await;
    json_or_500(page, || "Failed to list targets".to_string())
}

async fn get_target_history_handler(
    State(state): State<Arc<BesApiState>>,
    Path(label): Path<String>,
) -> impl IntoResponse {
    json_or_500(state.storage.get_target_history(&label).await, || {
        format!("Failed to get target history for {}", label)
    })
}

async fn list_tests_handler(
    State(state): State<Arc<BesApiState>>,
    Query(query): Query<LabelsQuery>,
) -> impl IntoResponse {
    let page = state
        .storage
        .list_tests_page(
            query.limit.unwrap_or(DEFAULT_PAGE_SIZE),
            query.offset.unwrap_or(0),
            query.q.as_deref().unwrap_or(""),
        )
        .await;
    json_or_500(page, || "Failed to list tests".to_string())
}

async fn get_test_history_handler(
    State(state): State<Arc<BesApiState>>,
    Path(label): Path<String>,
) -> impl IntoResponse {
    json_or_500(state.storage.get_test_history(&label).await, || {
        format!("Failed to get test history for {}", label)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::k8s::WorkerState;
    use axum::body::Body;
    use std::time::Instant;
    use tower::ServiceExt;

    async fn api_with_workers(workers: Option<Arc<WorkerRegistry>>) -> Router {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = BesStorage::new(BesConfig {
            data_dir: temp.path().to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        let api = BesApi::new(storage, BesConfig::default());
        match workers {
            Some(workers) => api.with_workers(workers),
            None => api,
        }
        .router()
    }

    fn registry_with_one_worker() -> Arc<WorkerRegistry> {
        let registry = Arc::new(WorkerRegistry::new());
        let (assignment_tx, _assignment_rx) = tokio::sync::mpsc::channel(1);
        registry.register(crate::worker::k8s::WorkerInfo {
            worker_id: "worker-1".to_string(),
            worker_type: "default".to_string(),
            labels: vec!["os=linux".to_string()],
            state: WorkerState::Idle,
            capacity: 1,
            running: vec![],
            last_heartbeat: Instant::now(),
            assignment_tx,
        });
        registry
    }

    async fn get(router: Router, path: &str) -> axum::response::Response {
        router
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn workers_endpoint_returns_the_registry_snapshot() {
        let router = api_with_workers(Some(registry_with_one_worker())).await;
        let response = get(router, "/api/workers").await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let workers: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0]["worker_id"], "worker-1");
        assert_eq!(workers[0]["state"], "idle");
    }

    #[tokio::test]
    async fn a_build_id_outside_the_data_directory_is_not_found() {
        for path in [
            "/api/builds/..%2F..%2Fetc/events",
            "/api/builds/%2Fetc%2Fpasswd/log",
            "/api/builds/by-label/targets",
        ] {
            let response = get(api_with_workers(None).await, path).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn workers_endpoint_without_a_registry_is_not_implemented() {
        let router = api_with_workers(None).await;
        let response = get(router, "/api/workers").await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    async fn api_with_proxy(cache: Option<Arc<PullThroughCache>>) -> (Router, tempfile::TempDir) {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = BesStorage::new(BesConfig {
            data_dir: temp.path().to_path_buf(),
            max_retention_days: None,
        })
        .await
        .unwrap();
        let api = BesApi::new(storage, BesConfig::default());
        let router = match cache {
            Some(cache) => api.with_proxy(cache),
            None => api,
        }
        .router();
        (router, temp)
    }

    #[tokio::test]
    async fn externals_endpoint_lists_artifacts_with_download_counts() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = crate::proxy::ProxyConfig {
            dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let cache = PullThroughCache::new(config, Arc::new(crate::metrics::Metrics::new()))
            .await
            .unwrap();

        let url = "https://github.com/example/tool/releases/download/v1/tool.tar.gz";
        let key = crate::proxy::store::ProxyStore::cache_key(url);
        cache
            .index()
            .insert(
                key.clone(),
                Arc::new(crate::proxy::BlobMeta {
                    url: url.to_string(),
                    size: 1024,
                    sha256_hex: "ab".repeat(32),
                    content_type: None,
                    etag: None,
                    fetched_at_unix: 1_700_000_000,
                    immutable: true,
                    downloads: 0,
                }),
            )
            .await;
        cache.index().record_download(&key);
        cache.index().record_download(&key);
        cache.index().record_download(&key);

        let (router, _temp) = api_with_proxy(Some(cache)).await;
        let response = get(router, "/api/proxy/externals").await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["total"], 1);
        let externals = page["items"].as_array().unwrap();
        assert_eq!(externals.len(), 1);
        assert_eq!(externals[0]["url"], url);
        assert_eq!(externals[0]["downloads"], 3);
        assert_eq!(externals[0]["immutable"], true);
        assert_eq!(externals[0]["size"], 1024);
    }

    #[tokio::test]
    async fn externals_endpoint_without_a_proxy_is_not_implemented() {
        let (router, _temp) = api_with_proxy(None).await;
        let response = get(router, "/api/proxy/externals").await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn externals_endpoint_paginates_and_filters_by_url() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = crate::proxy::ProxyConfig {
            dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let cache = PullThroughCache::new(config, Arc::new(crate::metrics::Metrics::new()))
            .await
            .unwrap();

        for (i, url) in [
            "https://github.com/example/one/release.tar.gz",
            "https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz",
            "https://crates.io/api/v1/crates/serde/1.0.0/download",
        ]
        .into_iter()
        .enumerate()
        {
            let key = crate::proxy::store::ProxyStore::cache_key(url);
            cache
                .index()
                .insert(
                    key,
                    Arc::new(crate::proxy::BlobMeta {
                        url: url.to_string(),
                        size: 100 + i as u64,
                        sha256_hex: format!("{:064x}", i),
                        content_type: None,
                        etag: None,
                        fetched_at_unix: 1_700_000_000,
                        immutable: true,
                        downloads: 0,
                    }),
                )
                .await;
        }

        let (router, _temp) = api_with_proxy(Some(cache)).await;

        let response = get(router.clone(), "/api/proxy/externals?limit=2&offset=0").await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["total"], 3);
        assert_eq!(page["items"].as_array().unwrap().len(), 2);

        let response = get(router.clone(), "/api/proxy/externals?q=npmjs").await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(
            page["items"][0]["url"],
            "https://registry.npmjs.org/left-pad/-/left-pad-1.0.0.tgz"
        );

        let response = get(router, "/api/proxy/externals?q=nothing-matches").await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["total"], 0);
        assert!(page["items"].as_array().unwrap().is_empty());
    }

    fn build_with_metadata(entries: &[(&str, &str)]) -> BuildListItem {
        BuildListItem {
            metadata: entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn build_filters_match_exact_metadata() {
        let build = build_with_metadata(&[
            ("BRANCH_NAME", "xangcastle/ci-webpack-strategy"),
            ("PR_NUMBER", "1705"),
        ]);

        assert!(matches_build_filters(
            &build,
            Some("xangcastle/ci-webpack-strategy"),
            Some("1705")
        ));
        assert!(!matches_build_filters(&build, Some("main"), None));
        assert!(!matches_build_filters(
            &build,
            Some("xangcastle/ci-webpack-strategy"),
            Some("1")
        ));
        assert!(matches_build_filters(&build, None, None));
    }

    #[test]
    fn build_filters_reject_summaries_without_metadata() {
        let build = build_with_metadata(&[]);
        assert!(!matches_build_filters(&build, Some("main"), None));
        assert!(!matches_build_filters(&build, None, Some("1705")));
        assert!(matches_build_filters(&build, None, None));
    }
}
