//! Serves the BES UI bundle embedded in the binary.
//!
//! Content-hashed files under `assets/` are cached forever; everything else,
//! including the `index.html` fallback for client-side routes, is revalidated
//! on every load. Paths under `/api/` never fall back to the SPA.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use include_dir::Dir;
use once_cell::sync::Lazy;

const API_PREFIX: &str = "/api/";
const HASHED_ASSETS_PREFIX: &str = "assets/";
const INDEX_HTML: &str = "index.html";
const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const REVALIDATE_CACHE_CONTROL: &str = "no-cache";

/// Placeholder in `ui/index.html` that the server replaces with the runtime
/// configuration the SPA reads at startup (the Keycloak login settings).
const ENV_CONFIG_MARKER: &str = r#"<script id="env-config" type="application/json">{}</script>"#;

/// The `env-config` script tag, rendered once from the environment.
static ENV_CONFIG_SCRIPT: Lazy<String> = Lazy::new(|| {
    let payload = env_config(|name| std::env::var(name).ok());
    format!(
        r#"<script id="env-config" type="application/json">{}</script>"#,
        payload
    )
});

/// The login settings of the SPA from `RBE_KEYCLOAK_URL`,
/// `RBE_KEYCLOAK_REALM` and `RBE_KEYCLOAK_UI_CLIENT`, as `var` reads them.
/// Without a Keycloak URL and realm they are empty, and the SPA runs without
/// login.
fn env_config(var: impl Fn(&str) -> Option<String>) -> serde_json::Value {
    let var = |name: &str| var(name).filter(|value| !value.is_empty());
    match (var("RBE_KEYCLOAK_URL"), var("RBE_KEYCLOAK_REALM")) {
        (Some(url), Some(realm)) => serde_json::json!({
            "KEYCLOAK_URL": url,
            "KEYCLOAK_REALM": realm,
            "KEYCLOAK_CLIENT_ID": var("RBE_KEYCLOAK_UI_CLIENT")
                .unwrap_or_else(|| "ferris-rbe-ui".to_string()),
        }),
        _ => serde_json::json!({}),
    }
}

/// `index.html` with the runtime configuration injected. Content that is not
/// valid UTF-8 is served unchanged.
fn render_index(contents: &[u8]) -> Vec<u8> {
    std::str::from_utf8(contents)
        .map(|html| {
            html.replace(ENV_CONFIG_MARKER, &ENV_CONFIG_SCRIPT)
                .into_bytes()
        })
        .unwrap_or_else(|_| contents.to_vec())
}

/// Serves `assets` for every path `router` does not handle, falling back to
/// `index.html` so client-side routes resolve.
pub fn with_embedded_ui(router: Router, assets: &'static Dir<'static>) -> Router {
    router.fallback(move |uri: Uri| async move { serve(assets, uri.path()) })
}

/// The embedded file for `path`, or `index.html` when no file matches;
/// `404` for API paths and when the bundle has no `index.html`.
fn serve(assets: &'static Dir<'static>, path: &str) -> Response {
    if path.starts_with(API_PREFIX) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let relative = path.trim_start_matches('/');
    let exact = if relative.is_empty() {
        None
    } else {
        assets.get_file(relative)
    };
    let (file, immutable) = match exact {
        Some(file) => (file, relative.starts_with(HASHED_ASSETS_PREFIX)),
        None => match assets.get_file(INDEX_HTML) {
            Some(index) => (index, false),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mime = mime_guess::from_path(file.path()).first_or_octet_stream();
    let content_type = HeaderValue::from_str(mime.as_ref())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let cache_control = if immutable {
        IMMUTABLE_CACHE_CONTROL
    } else {
        REVALIDATE_CACHE_CONTROL
    };
    let body = if file.path() == std::path::Path::new(INDEX_HTML) {
        render_index(file.contents())
    } else {
        file.contents().to_vec()
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        )
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use axum::routing::get;
    use include_dir::{DirEntry, File};
    use tower::ServiceExt;

    static ASSETS: Dir<'static> = Dir::new(
        "dist",
        &[
            DirEntry::File(File::new("index.html", b"<html>spa</html>")),
            DirEntry::File(File::new("assets/app-abc123.js", b"console.log(1)")),
        ],
    );

    static INDEX_WITH_MARKER: Dir<'static> = Dir::new(
        "dist",
        &[DirEntry::File(File::new(
            "index.html",
            b"<html><head><script id=\"env-config\" type=\"application/json\">{}</script></head><body></body></html>",
        ))],
    );

    fn app() -> Router {
        with_embedded_ui(
            Router::new().route("/api/health", get(|| async { "ok" })),
            &ASSETS,
        )
    }

    async fn get_path(path: &str) -> Response {
        app()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn hashed_assets_are_served_immutable_with_their_mime() {
        let response = get_path("/assets/app-abc123.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("javascript"));
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));
    }

    #[tokio::test]
    async fn client_side_routes_fall_back_to_index() {
        for path in ["/", "/builds/abc", "/targets"] {
            let response = get_path(path).await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        }
    }

    #[tokio::test]
    async fn unknown_api_paths_are_not_swallowed_by_the_spa() {
        assert_eq!(get_path("/api/health").await.status(), StatusCode::OK);
        assert_eq!(get_path("/api/nope").await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn index_html_carries_the_runtime_config() {
        let app = with_embedded_ui(Router::new(), &INDEX_WITH_MARKER);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(&*ENV_CONFIG_SCRIPT));
    }

    #[test]
    fn login_settings_need_both_the_keycloak_url_and_realm() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(env_config(env(&[])), serde_json::json!({}));
        assert_eq!(
            env_config(env(&[("RBE_KEYCLOAK_URL", "https://kc.example.com")])),
            serde_json::json!({})
        );
        assert_eq!(
            env_config(env(&[
                ("RBE_KEYCLOAK_URL", "https://kc.example.com"),
                ("RBE_KEYCLOAK_REALM", "ferris"),
            ])),
            serde_json::json!({
                "KEYCLOAK_URL": "https://kc.example.com",
                "KEYCLOAK_REALM": "ferris",
                "KEYCLOAK_CLIENT_ID": "ferris-rbe-ui",
            })
        );
    }
}
