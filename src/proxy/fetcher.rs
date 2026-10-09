//! Upstream HTTP client for the pull-through cache.
//!
//! Bodies are never buffered: the response is handed back as a chunk stream so
//! the caller can tee it to disk and to the waiting client at the same time.
//! Memory use per fetch is bounded by the chunk size, not by the artifact,
//! which keeps concurrent multi-hundred-megabyte toolchain downloads within
//! the container's memory limit.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::redirect;

use crate::proxy::config::ProxyConfig;
use crate::proxy::error::{ProxyError, ProxyResult};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("FerrisRBE-proxy/", env!("CARGO_PKG_VERSION"));
const MAX_REDIRECTS: usize = 10;

/// A successful, streaming response from an origin server.
pub struct UpstreamResponse {
    pub content_type: Option<String>,
    pub etag: Option<String>,
    /// `Content-Length`, when the origin sent one.
    pub content_length: Option<u64>,
    /// Body chunks as they arrive; a transport failure mid-body is yielded as
    /// an error item.
    pub body: BoxStream<'static, ProxyResult<Bytes>>,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("content_type", &self.content_type)
            .field("etag", &self.etag)
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

/// HTTP client pinned to the proxy's egress policy: the configured upstream
/// timeout, a fixed connect timeout, and a redirect policy that holds every
/// hop to the same allow-list as the original request.
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    pub fn new(config: &ProxyConfig) -> ProxyResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(config.upstream_timeout)
            .redirect(redirect_policy(config.clone()))
            .build()
            .map_err(|e| ProxyError::Storage(format!("building upstream client: {e}")))?;

        Ok(Self { client })
    }

    /// Fetch `url` with `headers`, returning the body as a stream.
    ///
    /// Any non-success status is an [`ProxyError::UpstreamStatus`]; nothing
    /// but a 2xx response is ever handed to the caller.
    pub async fn get(&self, url: &str, headers: &HeaderMap) -> ProxyResult<UpstreamResponse> {
        let response = self
            .client
            .get(url)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| transport_error(url, e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(ProxyError::UpstreamStatus {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }

        let content_type = header_string(&response, reqwest::header::CONTENT_TYPE);
        let etag = header_string(&response, reqwest::header::ETAG);
        let content_length = response.content_length();

        let url_owned = url.to_string();
        let body = response
            .bytes_stream()
            .map(move |chunk| chunk.map_err(|e| transport_error(&url_owned, e)))
            .boxed();

        Ok(UpstreamResponse {
            content_type,
            etag,
            content_length,
            body,
        })
    }
}

/// Redirects are followed up to [`MAX_REDIRECTS`] hops, and only onto hosts
/// that [`ProxyConfig::is_host_allowed`] accepts. Checking only the original
/// URL would let an origin redirect the proxy onto the cloud metadata
/// endpoint or the in-cluster API server.
fn redirect_policy(config: ProxyConfig) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let host = attempt.url().host_str().map(str::to_owned);
        match host {
            Some(host) if config.is_host_allowed(&host) => attempt.follow(),
            Some(host) => attempt.error(format!("redirect to disallowed host: {host}")),
            None => attempt.error("redirect target has no host"),
        }
    })
}

/// A timeout becomes [`ProxyError::UpstreamTimeout`]; any other transport
/// failure keeps its cause chain in [`ProxyError::Upstream`].
fn transport_error(url: &str, error: reqwest::Error) -> ProxyError {
    if error.is_timeout() {
        ProxyError::UpstreamTimeout(url.to_string())
    } else {
        ProxyError::Upstream {
            url: url.to_string(),
            source: Box::new(error),
        }
    }
}

fn header_string(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Rebuild an absolute upstream URL from a `/proxy/` path.
///
/// `github.com/org/repo/archive.tar.gz?ref=main` becomes
/// `https://github.com/org/repo/archive.tar.gz?ref=main`; an explicit
/// `http://` or `https://` prefix is honoured. Returns `(host, url)`, with
/// the host lowercased and stripped of its port so the caller can check it
/// against the allow-list before any socket is opened.
///
/// Rejected as [`ProxyError::BadRequest`]: an empty path, a missing host,
/// `.` or `..` segments (which would escape the approved host prefix), and
/// credentials in the authority (which would be forwarded to the origin).
pub fn parse_proxy_path(path: &str, query: Option<&str>) -> ProxyResult<(String, String)> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(ProxyError::BadRequest("empty proxy path".into()));
    }
    if trimmed.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(ProxyError::BadRequest(
            "path traversal in proxy path".into(),
        ));
    }

    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else {
        ("https", trimmed)
    };

    let mut parts = rest.splitn(2, '/');
    let host = parts.next().unwrap_or_default();
    let remainder = parts.next().unwrap_or("");

    if host.is_empty() {
        return Err(ProxyError::BadRequest("missing upstream host".into()));
    }
    if host.contains('@') {
        return Err(ProxyError::BadRequest(
            "credentials are not accepted in the upstream authority".into(),
        ));
    }

    let host_only = host.rsplit_once(':').map_or(host, |(h, _)| h);

    let mut url = format!("{scheme}://{host}/{remainder}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }

    Ok((host_only.to_ascii_lowercase(), url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_an_https_url_from_the_proxy_path() {
        let (host, url) = parse_proxy_path("github.com/org/repo/v1.tar.gz", None).unwrap();
        assert_eq!(host, "github.com");
        assert_eq!(url, "https://github.com/org/repo/v1.tar.gz");
    }

    #[test]
    fn preserves_the_query_string() {
        let (_, url) = parse_proxy_path("example.com/file.whl", Some("token=abc&x=1")).unwrap();
        assert_eq!(url, "https://example.com/file.whl?token=abc&x=1");
    }

    #[test]
    fn honours_an_explicit_scheme_prefix() {
        let (host, url) = parse_proxy_path("http://example.com/a", None).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(url, "http://example.com/a");
    }

    #[test]
    fn strips_the_port_when_reporting_the_host() {
        let (host, url) = parse_proxy_path("example.com:8443/a", None).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(url, "https://example.com:8443/a");
    }

    #[test]
    fn host_only_paths_still_produce_a_root_url() {
        let (host, url) = parse_proxy_path("example.com", None).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(url, "https://example.com/");
    }

    #[test]
    fn rejects_traversal_and_empty_paths() {
        assert!(parse_proxy_path("", None).is_err());
        assert!(parse_proxy_path("/", None).is_err());
        assert!(parse_proxy_path("example.com/../../etc/passwd", None).is_err());
        assert!(parse_proxy_path("example.com/a/./b", None).is_err());
    }

    #[test]
    fn rejects_credentials_in_the_authority() {
        assert!(parse_proxy_path("user:pass@example.com/a", None).is_err());
    }

    #[test]
    fn host_is_lowercased_for_allow_list_comparison() {
        let (host, _) = parse_proxy_path("GitHub.COM/org/repo", None).unwrap();
        assert_eq!(host, "github.com");
    }

    #[test]
    fn client_builds_with_the_configured_policy() {
        let config = ProxyConfig::default();
        assert!(Fetcher::new(&config).is_ok());
    }
}
