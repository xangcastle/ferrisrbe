//! Remote Asset API (`build.bazel.remote.asset.v1.Fetch`), the gRPC
//! counterpart of the HTTP `/proxy/` routes, used by Bazel's
//! `--experimental_remote_downloader`.
//!
//! The server fetches the URI itself, places the artifact in the CAS and
//! answers with its digest only. When the blob is already cached, Bazel
//! transfers nothing; unlike the HTTP path, the bytes never travel to the
//! runner unless an action needs them.
//!
//! Both paths share one [`PullThroughCache`]: a fetch through either warms the
//! other, and concurrent requests for the same URL are coalesced across them.

use std::sync::Arc;
use std::time::Instant;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tonic::{Code, Request, Response, Status};
use tracing::{info, warn};

use crate::cas::backends::DiskBackend;
use crate::metrics::Metrics;
use crate::proto::build::bazel::remote::asset::v1::fetch_server::{Fetch, FetchServer};
use crate::proto::build::bazel::remote::asset::v1::{
    FetchBlobRequest, FetchBlobResponse, FetchDirectoryRequest, FetchDirectoryResponse, Qualifier,
};
use crate::proto::build::bazel::remote::execution::v2::Digest;
use crate::proxy::config::is_private_host;
use crate::proxy::error::ProxyError;
use crate::proxy::fetcher::parse_proxy_path;
use crate::proxy::PullThroughCache;
use crate::types::DigestInfo;

const SERVICE_NAME: &str = "RemoteAsset";

/// Qualifier Bazel uses to pin the expected content hash, as an SRI string
/// (`sha256-<base64>`). Treated as advisory input to verify against, not as a
/// requirement — Bazel re-checks the hash client-side regardless.
const QUALIFIER_CHECKSUM_SRI: &str = "checksum.sri";

/// `http_header:<name>`, applies to every URI in the request.
const QUALIFIER_HTTP_HEADER_PREFIX: &str = "http_header:";

/// `http_header_url:<index>:<name>`, bound to one URI; wins over the generic form.
const QUALIFIER_HTTP_HEADER_URL_PREFIX: &str = "http_header_url:";

/// `Fetch` over the pull-through cache, promoting fetched artifacts into the CAS.
pub struct RemoteAssetService {
    cache: Arc<PullThroughCache>,
    cas: Arc<DiskBackend>,
    metrics: Arc<Metrics>,
}

impl RemoteAssetService {
    pub fn new(cache: Arc<PullThroughCache>, cas: Arc<DiskBackend>, metrics: Arc<Metrics>) -> Self {
        Self {
            cache,
            cas,
            metrics,
        }
    }

    pub fn into_service(self) -> FetchServer<Self> {
        FetchServer::new(self)
    }

    /// Fetches one URI through the pull-through cache, verifies it against
    /// `expected` when given, and hardlinks the cached file into the CAS.
    ///
    /// A URI that points back at this server's own `/proxy/` route is first
    /// unwrapped to the upstream URL it proxies, so the artifact is cached
    /// under one key regardless of which path requested it.
    async fn fetch_one(
        &self,
        uri: &str,
        expected: Option<&ExpectedChecksum>,
        headers: &HeaderMap,
    ) -> Result<Digest, ProxyError> {
        if !self.cache.config().enabled {
            return Err(ProxyError::Storage("pull-through proxy is disabled".into()));
        }

        let uri = unwrap_loopback_uri(uri);
        let host =
            uri_host(&uri).ok_or_else(|| ProxyError::BadRequest(format!("no host in {uri}")))?;
        if !self.cache.config().is_host_allowed(&host) {
            return Err(ProxyError::HostNotAllowed(host));
        }

        let meta = self
            .cache
            .resolve(&uri, expected.is_some(), headers)
            .await?;

        if let Some(expected) = expected {
            if !expected.matches(&meta.sha256_hex) {
                return Err(ProxyError::ChecksumMismatch {
                    url: uri.to_string(),
                    expected: expected.hex.clone(),
                    actual: meta.sha256_hex.clone(),
                });
            }
        }

        let digest = DigestInfo::new(&meta.sha256_hex, meta.size as i64)
            .map_err(|e| ProxyError::Storage(format!("invalid digest for {uri}: {e}")))?;

        let key = crate::proxy::store::ProxyStore::cache_key(&uri);
        self.cas
            .adopt_link(&digest, &self.cache.blob_path(&key))
            .await
            .map_err(|e| ProxyError::Storage(format!("promoting {uri} into CAS: {e}")))?;

        Ok(Digest {
            hash: meta.sha256_hex.clone(),
            size_bytes: meta.size as i64,
        })
    }
}

#[tonic::async_trait]
impl Fetch for RemoteAssetService {
    /// Fetches the asset from the first of `uris` that succeeds; the URIs are
    /// mirrors of one asset and are tried in order.
    ///
    /// Failure of every mirror is reported in the response `status`, not as
    /// a gRPC error: an unsatisfiable fetch is a valid answer, and Bazel
    /// falls back to downloading directly when it receives one.
    async fn fetch_blob(
        &self,
        request: Request<FetchBlobRequest>,
    ) -> Result<Response<FetchBlobResponse>, Status> {
        let started = Instant::now();
        let request = request.into_inner();

        if !request.qualifiers.is_empty() {
            info!(
                qualifiers = ?request.qualifiers.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(),
                "FetchBlob qualifiers"
            );
        }

        let expected = parse_checksum_sri(&request.qualifiers);
        if request.uris.is_empty() {
            return Err(Status::invalid_argument("FetchBlobRequest.uris is empty"));
        }

        let mut last_error = None;
        for (index, uri) in request.uris.iter().enumerate() {
            let headers = headers_for_uri(&request.qualifiers, index);
            match self.fetch_one(uri, expected.as_ref(), &headers).await {
                Ok(digest) => {
                    self.metrics.record_grpc_request(
                        SERVICE_NAME,
                        "FetchBlob",
                        started.elapsed(),
                        "ok",
                    );
                    return Ok(Response::new(FetchBlobResponse {
                        status: Some(ok_status()),
                        uri: uri.clone(),
                        qualifiers: request.qualifiers.clone(),
                        expires_at: None,
                        blob_digest: Some(digest),
                        digest_function: request.digest_function,
                    }));
                }
                Err(e) => {
                    warn!(uri, error = %e, "FetchBlob mirror failed");
                    last_error = Some(e);
                }
            }
        }

        let error = last_error.expect("uris was checked to be non-empty");
        self.metrics
            .record_grpc_request(SERVICE_NAME, "FetchBlob", started.elapsed(), "error");

        Ok(Response::new(FetchBlobResponse {
            status: Some(error_status(&error)),
            uri: request.uris.first().cloned().unwrap_or_default(),
            qualifiers: request.qualifiers,
            expires_at: None,
            blob_digest: None,
            digest_function: request.digest_function,
        }))
    }

    /// `UNIMPLEMENTED`. Bazel's remote downloader only calls `FetchBlob`;
    /// directory assets would require unpacking archives server-side.
    async fn fetch_directory(
        &self,
        _request: Request<FetchDirectoryRequest>,
    ) -> Result<Response<FetchDirectoryResponse>, Status> {
        Err(Status::unimplemented(
            "FetchDirectory is not supported; use FetchBlob",
        ))
    }
}

/// An expected content hash extracted from the request qualifiers.
#[derive(Debug, Clone)]
pub struct ExpectedChecksum {
    /// Lowercase hex SHA-256.
    pub hex: String,
}

impl ExpectedChecksum {
    fn matches(&self, actual_hex: &str) -> bool {
        self.hex.eq_ignore_ascii_case(actual_hex)
    }
}

/// Parse a `checksum.sri` qualifier into a hex SHA-256.
///
/// Subresource Integrity encodes the digest as `<alg>-<base64>`, and may list
/// several space-separated candidates. Only SHA-256 is understood; anything
/// else yields `None` so the fetch proceeds unverified rather than failing on a
/// hash this server cannot check.
pub fn parse_checksum_sri(qualifiers: &[Qualifier]) -> Option<ExpectedChecksum> {
    let raw = qualifiers
        .iter()
        .find(|q| q.name == QUALIFIER_CHECKSUM_SRI)?
        .value
        .as_str();

    raw.split_whitespace()
        .filter_map(|candidate| candidate.strip_prefix("sha256-"))
        .find_map(decode_base64)
        .map(|bytes| ExpectedChecksum {
            hex: hex::encode(bytes),
        })
}

/// The HTTP headers Bazel attached for the URI at `index`: every
/// `http_header:<name>` qualifier, overridden by `http_header_url:<index>:<name>`
/// qualifiers for that URI. Malformed qualifiers are logged and dropped.
pub fn headers_for_uri(qualifiers: &[Qualifier], index: usize) -> HeaderMap {
    let mut headers = HeaderMap::new();

    for qualifier in qualifiers {
        if let Some(name) = qualifier.name.strip_prefix(QUALIFIER_HTTP_HEADER_PREFIX) {
            insert_header(&mut headers, name, &qualifier.value);
        }
    }

    for qualifier in qualifiers {
        let Some(rest) = qualifier
            .name
            .strip_prefix(QUALIFIER_HTTP_HEADER_URL_PREFIX)
        else {
            continue;
        };
        let Some((position, name)) = rest.split_once(':') else {
            warn!(qualifier = %qualifier.name, "malformed http_header_url qualifier");
            continue;
        };
        let Ok(position) = position.parse::<usize>() else {
            warn!(qualifier = %qualifier.name, "malformed http_header_url qualifier");
            continue;
        };
        if position == index {
            insert_header(&mut headers, name, &qualifier.value);
        }
    }

    headers
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    match (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        (Ok(name), Ok(value)) => {
            headers.insert(name, value);
        }
        _ => warn!(name, "dropping qualifier that is not a valid HTTP header"),
    }
}

/// Standard base64 (RFC 4648 alphabet, padding optional); `None` on any
/// character outside the alphabet.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let bytes = input.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;

    for &b in bytes {
        let value = TABLE.iter().position(|&c| c == b)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// The lowercase host of a URI, without userinfo or port; `None` when the
/// authority is empty.
pub fn uri_host(uri: &str) -> Option<String> {
    let rest = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_and_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host_and_port
        .rsplit_once(':')
        .map_or(host_and_port, |(h, _)| h);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// The upstream URL behind a private-host `/proxy/<host>/<path>` URI, or
/// `uri` unchanged when it is public or not a proxy route.
fn unwrap_loopback_uri(uri: &str) -> String {
    let Some(host) = uri_host(uri) else {
        return uri.to_string();
    };
    if !is_private_host(&host) {
        return uri.to_string();
    }
    let Some((_, after_scheme)) = uri.split_once("://") else {
        return uri.to_string();
    };
    let Some((_, path_query)) = after_scheme.split_once('/') else {
        return uri.to_string();
    };
    let Some(inner) = path_query.strip_prefix("proxy/") else {
        return uri.to_string();
    };
    let (path, query) = match inner.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (inner, None),
    };
    parse_proxy_path(path, query)
        .map(|(_, url)| url)
        .unwrap_or_else(|_| uri.to_string())
}

fn ok_status() -> crate::proto::google::rpc::Status {
    crate::proto::google::rpc::Status {
        code: Code::Ok as i32,
        message: String::new(),
        details: Vec::new(),
    }
}

/// The `google.rpc.Status` reported for a failed fetch.
fn error_status(error: &ProxyError) -> crate::proto::google::rpc::Status {
    let code = match error {
        ProxyError::UpstreamStatus { status: 404, .. } => Code::NotFound,
        ProxyError::HostNotAllowed(_) => Code::PermissionDenied,
        ProxyError::BadRequest(_) | ProxyError::ChecksumMismatch { .. } => Code::InvalidArgument,
        ProxyError::UpstreamTimeout(_) => Code::DeadlineExceeded,
        ProxyError::UpstreamStatus { .. } | ProxyError::Upstream { .. } => Code::Unavailable,
        _ => Code::Internal,
    };
    crate::proto::google::rpc::Status {
        code: code as i32,
        message: error.to_string(),
        details: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qualifier(name: &str, value: &str) -> Qualifier {
        Qualifier {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn decodes_a_sha256_sri_qualifier() {
        let hello_world_sha256 = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let hex = hello_world_sha256;
        let raw = hex::decode(hex).unwrap();
        let b64 = encode_base64(&raw);

        let parsed =
            parse_checksum_sri(&[qualifier(QUALIFIER_CHECKSUM_SRI, &format!("sha256-{b64}"))])
                .expect("sri should parse");
        assert_eq!(parsed.hex, hex);
        assert!(parsed.matches(hex));
        assert!(parsed.matches(&hex.to_uppercase()));
        assert!(!parsed.matches(&"0".repeat(64)));
    }

    #[test]
    fn picks_the_sha256_candidate_from_a_multi_algorithm_sri() {
        let hex = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        let b64 = encode_base64(&hex::decode(hex).unwrap());
        let value = format!("sha512-abcd sha256-{b64}");
        assert_eq!(
            parse_checksum_sri(&[qualifier(QUALIFIER_CHECKSUM_SRI, &value)])
                .unwrap()
                .hex,
            hex
        );
    }

    #[test]
    fn unknown_or_absent_qualifiers_skip_verification() {
        assert!(parse_checksum_sri(&[]).is_none());
        assert!(parse_checksum_sri(&[qualifier("bazel.canonical_id", "x")]).is_none());
        assert!(
            parse_checksum_sri(&[qualifier(QUALIFIER_CHECKSUM_SRI, "sha512-abcd")]).is_none(),
            "an algorithm this server cannot compute must not fail the fetch"
        );
    }

    #[test]
    fn generic_http_headers_apply_to_every_uri() {
        let qualifiers = [qualifier(
            "http_header:Accept",
            "application/vnd.oci.image.manifest.v1+json",
        )];
        for index in 0..3 {
            let headers = headers_for_uri(&qualifiers, index);
            assert_eq!(
                headers.get("accept").unwrap(),
                "application/vnd.oci.image.manifest.v1+json"
            );
        }
    }

    #[test]
    fn url_specific_headers_bind_to_their_index_and_win_over_generic() {
        let qualifiers = [
            qualifier("http_header:Authorization", "Bearer generic"),
            qualifier("http_header_url:1:Authorization", "Bearer mirror-one"),
        ];
        assert_eq!(
            headers_for_uri(&qualifiers, 0)
                .get("authorization")
                .unwrap(),
            "Bearer generic"
        );
        assert_eq!(
            headers_for_uri(&qualifiers, 1)
                .get("authorization")
                .unwrap(),
            "Bearer mirror-one"
        );
        assert_eq!(
            headers_for_uri(&qualifiers, 2)
                .get("authorization")
                .unwrap(),
            "Bearer generic"
        );
    }

    #[test]
    fn non_header_and_malformed_qualifiers_are_ignored() {
        let qualifiers = [
            qualifier(QUALIFIER_CHECKSUM_SRI, "sha256-abcd"),
            qualifier("bazel.canonical_id", "x"),
            qualifier("http_header_url:notanumber:Authorization", "Bearer x"),
            qualifier("http_header_url:0", "no-name"),
            qualifier("http_header:Bad Header", "x"),
            qualifier("http_header:X-Ok", "fine"),
        ];
        let headers = headers_for_uri(&qualifiers, 0);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("x-ok").unwrap(), "fine");
    }

    #[test]
    fn extracts_hosts_from_absolute_uris() {
        assert_eq!(
            uri_host("https://github.com/org/repo"),
            Some("github.com".into())
        );
        assert_eq!(
            uri_host("http://example.com:8080/a"),
            Some("example.com".into())
        );
        assert_eq!(uri_host("https://GitHub.com/a"), Some("github.com".into()));
        assert_eq!(
            uri_host("https://user@example.com/a"),
            Some("example.com".into())
        );
        assert_eq!(uri_host("https://example.com"), Some("example.com".into()));
        assert_eq!(uri_host("https://"), None);
    }

    #[test]
    fn unwraps_loopback_proxy_uris_to_the_upstream() {
        let unwrapped = unwrap_loopback_uri(
            "http://rbe-cache.ci.svc.cluster.local:8080/proxy/bcr.bazel.build/modules/rules_proto/7.0.2/MODULE.bazel",
        );
        assert_eq!(
            unwrapped,
            "https://bcr.bazel.build/modules/rules_proto/7.0.2/MODULE.bazel"
        );
    }

    #[test]
    fn loopback_unwrap_preserves_the_query_string() {
        let unwrapped = unwrap_loopback_uri("http://10.100.0.1:8080/proxy/example.com/a?x=1");
        assert_eq!(unwrapped, "https://example.com/a?x=1");
    }

    #[test]
    fn public_uris_are_not_unwrapped() {
        let uri = "https://bcr.bazel.build/modules/rules_proto/7.0.2/MODULE.bazel";
        assert_eq!(unwrap_loopback_uri(uri), uri);
        let proxied_public = "https://example.com/proxy/evil.com/x";
        assert_eq!(unwrap_loopback_uri(proxied_public), proxied_public);
    }

    #[test]
    fn private_uris_without_a_proxy_path_are_not_unwrapped() {
        let uri = "http://rbe-cache.ci.svc.cluster.local:8080/metrics";
        assert_eq!(unwrap_loopback_uri(uri), uri);
    }

    #[test]
    fn maps_proxy_errors_onto_grpc_status_codes() {
        assert_eq!(
            error_status(&ProxyError::UpstreamStatus {
                status: 404,
                url: "u".into()
            })
            .code,
            5
        );
        assert_eq!(
            error_status(&ProxyError::HostNotAllowed("h".into())).code,
            7
        );
        assert_eq!(
            error_status(&ProxyError::UpstreamTimeout("u".into())).code,
            4
        );
        assert_eq!(
            error_status(&ProxyError::ChecksumMismatch {
                url: "u".into(),
                expected: "a".into(),
                actual: "b".into()
            })
            .code,
            3
        );
    }

    /// Test-only base64 encoder, mirroring [`decode_base64`].
    fn encode_base64(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(TABLE[(n >> 6) as usize & 63] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(TABLE[n as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn base64_round_trips() {
        for input in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world"] {
            assert_eq!(decode_base64(&encode_base64(input)).unwrap(), input);
        }
    }
}
