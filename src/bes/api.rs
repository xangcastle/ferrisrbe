//! BES REST API and static UI server.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info};

use crate::bes::config::BesConfig;
use crate::bes::models::HealthResponse;
use crate::bes::storage::BesStorage;

/// Shared application state for the BES API.
#[derive(Debug, Clone)]
pub struct BesApiState {
    pub storage: BesStorage,
    pub config: BesConfig,
}

/// BES REST API server.
#[derive(Debug, Clone)]
pub struct BesApi {
    state: Arc<BesApiState>,
}

impl BesApi {
    /// Create a new API server backed by the given storage and configuration.
    pub fn new(storage: BesStorage, config: BesConfig) -> Self {
        Self {
            state: Arc::new(BesApiState { storage, config }),
        }
    }

    /// Build the Axum router.
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        Router::new()
            .route("/api/health", get(health_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/builds", get(list_builds_handler))
            .route("/api/builds/{id}", get(get_build_handler))
            .route("/api/builds/{id}/events", get(get_events_handler))
            .route("/api/builds/{id}/misses", get(get_misses_handler))
            .route("/api/builds/{id}/targets", get(get_build_targets_handler))
            .route("/api/targets", get(list_targets_handler))
            .route("/api/targets/{label}", get(get_target_history_handler))
            .route("/api/tests", get(list_tests_handler))
            .route("/api/tests/{label}", get(get_test_history_handler))
            .fallback_service(ServeDir::new(&self.state.config.static_dir).fallback(
                ServeFile::new(self.state.config.static_dir.join("index.html")),
            ))
            .with_state(state)
    }

    /// Run the API server on the configured UI port.
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let addr = format!(
            "{}:{}",
            self.state.config.bind_address, self.state.config.ui_port
        );
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!("BES UI/API server listening on http://{}", addr);
        axum::serve(listener, self.router()).await?;
        Ok(())
    }
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse::default())
}

async fn stats_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    match state.storage.stats().await {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => {
            error!("Failed to compute stats: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn list_builds_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    match state.storage.list_builds().await {
        Ok(builds) => Json(builds).into_response(),
        Err(e) => {
            error!("Failed to list builds: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_build_handler(
    State(state): State<Arc<BesApiState>>,
    Path(id): Path<String>,
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

async fn get_events_handler(
    State(state): State<Arc<BesApiState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.storage.get_events(&id).await {
        Ok(events) => Json(events).into_response(),
        Err(e) => {
            error!("Failed to get events for {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_misses_handler(
    State(state): State<Arc<BesApiState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.storage.get_misses(&id).await {
        Ok(misses) => Json(misses).into_response(),
        Err(e) => {
            error!("Failed to get misses for {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_build_targets_handler(
    State(state): State<Arc<BesApiState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.storage.get_build_targets(&id).await {
        Ok(targets) => Json(targets).into_response(),
        Err(e) => {
            error!("Failed to get targets for build {}: {}", id, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn list_targets_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    match state.storage.list_targets().await {
        Ok(targets) => Json(targets).into_response(),
        Err(e) => {
            error!("Failed to list targets: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_target_history_handler(
    State(state): State<Arc<BesApiState>>,
    Path(label): Path<String>,
) -> impl IntoResponse {
    match state.storage.get_target_history(&label).await {
        Ok(history) => Json(history).into_response(),
        Err(e) => {
            error!("Failed to get target history for {}: {}", label, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn list_tests_handler(State(state): State<Arc<BesApiState>>) -> impl IntoResponse {
    match state.storage.list_tests().await {
        Ok(tests) => Json(tests).into_response(),
        Err(e) => {
            error!("Failed to list tests: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_test_history_handler(
    State(state): State<Arc<BesApiState>>,
    Path(label): Path<String>,
) -> impl IntoResponse {
    match state.storage.get_test_history(&label).await {
        Ok(history) => Json(history).into_response(),
        Err(e) => {
            error!("Failed to get test history for {}: {}", label, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
