//! Error type for the pull-through cache proxy.

use thiserror::Error;

/// Result of a proxy operation.
pub type ProxyResult<T> = std::result::Result<T, ProxyError>;

/// Failure of a proxied fetch, classified by who is at fault: the request,
/// the upstream origin, or the proxy's own storage.
#[derive(Error, Debug)]
pub enum ProxyError {
    /// The request path is not `/proxy/<host>/<path>` or is otherwise unusable.
    #[error("Malformed proxy path: {0}")]
    BadRequest(String),

    /// The upstream host is private or outside the allow-list.
    #[error("Upstream host not allowed: {0}")]
    HostNotAllowed(String),

    /// The origin answered with a non-success status.
    #[error("Upstream returned {status} for {url}")]
    UpstreamStatus { status: u16, url: String },

    /// The request to the origin failed before a status was received. The
    /// message includes the full cause chain.
    #[error("Upstream request failed for {url}: {}", error_chain(.source.as_ref()))]
    Upstream {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The origin did not complete within the upstream timeout.
    #[error("Upstream fetch timed out for {0}")]
    UpstreamTimeout(String),

    /// The artifact is larger than the cacheable size limit.
    #[error("Blob exceeds the cacheable size limit: {size} bytes")]
    BlobTooLarge { size: u64 },

    /// The downloaded bytes do not hash to the digest the caller required.
    #[error("Checksum mismatch for {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    /// The on-disk cache could not be read or written.
    #[error("Cache storage error: {0}")]
    Storage(String),

    /// I/O failure on the proxy's side.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ProxyError {
    /// HTTP status this error maps to.
    ///
    /// A 4xx from the origin is passed through unchanged, so a missing
    /// artifact is a 404 to the client as well. Every other upstream failure
    /// surfaces as 502 or 504 rather than 500, which keeps a broken origin
    /// distinguishable from a broken proxy.
    pub fn status_code(&self) -> u16 {
        match self {
            ProxyError::BadRequest(_) => 400,
            ProxyError::HostNotAllowed(_) => 403,
            ProxyError::BlobTooLarge { .. } => 413,
            ProxyError::UpstreamStatus { status, .. } if (400..500).contains(status) => *status,
            ProxyError::UpstreamStatus { .. } => 502,
            ProxyError::Upstream { .. } | ProxyError::ChecksumMismatch { .. } => 502,
            ProxyError::UpstreamTimeout(_) => 504,
            ProxyError::Storage(_) | ProxyError::Io(_) => 500,
        }
    }

    /// Stable label for the `status` dimension of the request counter.
    pub fn metric_label(&self) -> &'static str {
        match self {
            ProxyError::BadRequest(_) => "bad_request",
            ProxyError::HostNotAllowed(_) => "host_denied",
            ProxyError::UpstreamStatus { .. } => "upstream_status",
            ProxyError::Upstream { .. } => "upstream_error",
            ProxyError::UpstreamTimeout(_) => "upstream_timeout",
            ProxyError::BlobTooLarge { .. } => "too_large",
            ProxyError::ChecksumMismatch { .. } => "checksum_mismatch",
            ProxyError::Storage(_) | ProxyError::Io(_) => "storage_error",
        }
    }
}

/// `error` followed by every cause in its `source()` chain, joined by `": "`.
///
/// reqwest reports some decisive details, such as the host of a refused
/// redirect, only through `source()`, never through `Display`.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        current = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_client_errors_are_passed_through() {
        let err = ProxyError::UpstreamStatus {
            status: 404,
            url: "https://example.com/x".into(),
        };
        assert_eq!(err.status_code(), 404);
    }

    #[test]
    fn upstream_server_errors_become_bad_gateway() {
        let err = ProxyError::UpstreamStatus {
            status: 503,
            url: "https://example.com/x".into(),
        };
        assert_eq!(err.status_code(), 502);
    }

    #[test]
    fn timeouts_map_to_gateway_timeout() {
        assert_eq!(
            ProxyError::UpstreamTimeout("https://example.com/x".into()).status_code(),
            504
        );
    }

    #[derive(Debug)]
    struct Wrapped {
        inner: std::io::Error,
    }

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error following redirect for url (https://x/)")
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.inner)
        }
    }

    #[test]
    fn upstream_errors_include_their_cause_chain() {
        let inner = std::io::Error::other("redirect to disallowed host: s3.amazonaws.com");
        let err = ProxyError::Upstream {
            url: "https://index.docker.io/v2/x".into(),
            source: Box::new(Wrapped { inner }),
        };
        let text = err.to_string();
        assert!(text.contains("error following redirect"), "{text}");
        assert!(
            text.contains("redirect to disallowed host: s3.amazonaws.com"),
            "{text}"
        );
    }
}
