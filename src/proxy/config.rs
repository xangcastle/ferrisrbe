//! Pull-through cache proxy configuration.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variables read by [`ProxyConfig::from_env`].
pub const ENV_ENABLED: &str = "RBE_PROXY_ENABLED";
pub const ENV_DIR: &str = "RBE_PROXY_DIR";
pub const ENV_MAX_SIZE_GB: &str = "RBE_PROXY_MAX_SIZE_GB";
pub const ENV_MUTABLE_TTL_SECS: &str = "RBE_PROXY_MUTABLE_TTL_SECS";
pub const ENV_UPSTREAM_TIMEOUT_SECS: &str = "RBE_PROXY_UPSTREAM_TIMEOUT_SECS";
pub const ENV_ALLOWED_HOSTS: &str = "RBE_PROXY_ALLOWED_HOSTS";
pub const ENV_MAX_BLOB_SIZE_MB: &str = "RBE_PROXY_MAX_BLOB_SIZE_MB";

const DEFAULT_MAX_SIZE_GB: u64 = 50;
const DEFAULT_MUTABLE_TTL_SECS: u64 = 300;
const DEFAULT_UPSTREAM_TIMEOUT_SECS: u64 = 600;
const DEFAULT_MAX_BLOB_SIZE_MB: u64 = 8 * 1024;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Configuration for the pull-through cache proxy.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Whether the `/proxy/*` routes and the Remote Asset service are served.
    pub enabled: bool,
    /// Root directory for cached blobs and their sidecars.
    pub dir: PathBuf,
    /// Disk budget for cached blobs. Zero means unlimited.
    pub max_size_bytes: u64,
    /// Retention for assets classified as mutable.
    pub mutable_ttl: Duration,
    /// Deadline for a single upstream fetch.
    pub upstream_timeout: Duration,
    /// Upstream hosts the proxy is willing to contact. Empty means any public host.
    pub allowed_hosts: Vec<String>,
    /// Blobs larger than this are streamed through but never cached.
    pub max_blob_size_bytes: u64,
}

impl ProxyConfig {
    /// Load configuration from environment variables.
    ///
    /// Unset or unparsable numeric values fall back to their defaults. The
    /// proxy is enabled unless `RBE_PROXY_ENABLED` is `0`, `false` or `no`.
    pub fn from_env() -> Self {
        let enabled = std::env::var(ENV_ENABLED)
            .ok()
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let dir = std::env::var(ENV_DIR)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data/proxy"));
        let allowed_hosts = std::env::var(ENV_ALLOWED_HOSTS)
            .map(|v| parse_host_list(&v))
            .unwrap_or_default();

        Self {
            enabled,
            dir,
            max_size_bytes: env_u64(ENV_MAX_SIZE_GB, DEFAULT_MAX_SIZE_GB) * GIB,
            mutable_ttl: Duration::from_secs(env_u64(
                ENV_MUTABLE_TTL_SECS,
                DEFAULT_MUTABLE_TTL_SECS,
            )),
            upstream_timeout: Duration::from_secs(env_u64(
                ENV_UPSTREAM_TIMEOUT_SECS,
                DEFAULT_UPSTREAM_TIMEOUT_SECS,
            )),
            allowed_hosts,
            max_blob_size_bytes: env_u64(ENV_MAX_BLOB_SIZE_MB, DEFAULT_MAX_BLOB_SIZE_MB) * MIB,
        }
    }

    /// Directory holding cached blobs, sharded two levels deep.
    pub fn blobs_dir(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    /// Directory for partially written blobs.
    pub fn temp_dir(&self) -> PathBuf {
        self.dir.join(".tmp")
    }

    /// Whether the proxy may contact `host`.
    ///
    /// An empty allow-list permits any host that is not loopback, link-local or
    /// RFC1918. Without that floor the proxy is an SSRF pivot into the cluster:
    /// anything that can reach `/proxy/` could reach the Kubernetes API or the
    /// cloud metadata endpoint through it.
    pub fn is_host_allowed(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        if is_private_host(&host) {
            return false;
        }
        if self.allowed_hosts.is_empty() {
            return true;
        }
        self.allowed_hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::from("/data/proxy"),
            max_size_bytes: DEFAULT_MAX_SIZE_GB * GIB,
            mutable_ttl: Duration::from_secs(DEFAULT_MUTABLE_TTL_SECS),
            upstream_timeout: Duration::from_secs(DEFAULT_UPSTREAM_TIMEOUT_SECS),
            allowed_hosts: Vec::new(),
            max_blob_size_bytes: DEFAULT_MAX_BLOB_SIZE_MB * MIB,
        }
    }
}

/// `name` parsed as an unsigned integer, or `default` when unset or invalid.
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Comma-separated hosts, normalized to lowercase without a trailing dot.
fn parse_host_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect()
}

/// Whether `host` names the local machine or something inside the cluster.
///
/// The check applies to the literal host: local and cluster-internal DNS
/// suffixes, and private, loopback, link-local or reserved IP literals
/// (bracketed IPv6 included). A public hostname whose DNS record points at a
/// private address is not detected here; that guarantee would require
/// resolving the name and pinning the connection to the resolved address.
pub(crate) fn is_private_host(host: &str) -> bool {
    const LOCAL_SUFFIXES: &[&str] = &[".localhost", ".local", ".internal", ".cluster.local"];
    if host == "localhost" || LOCAL_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        return true;
    }

    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => is_private_ipv4(v4),
        Ok(IpAddr::V6(v6)) => is_private_ipv6(v6),
        Err(_) => false,
    }
}

/// Loopback, RFC 1918, link-local (which includes the `169.254.169.254`
/// cloud metadata endpoint), broadcast, `0.0.0.0/8` and RFC 6598
/// carrier-grade NAT (`100.64.0.0/10`).
fn is_private_ipv4(v4: Ipv4Addr) -> bool {
    let [first, second, ..] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || first == 0
        || (first == 100 && (64..128).contains(&second))
}

/// Loopback, unspecified, unique-local (`fc00::/7`), link-local (`fe80::/10`)
/// and IPv4-mapped addresses of a private IPv4 host.
fn is_private_ipv6(v6: Ipv6Addr) -> bool {
    let first_segment = v6.segments()[0];
    v6.is_loopback()
        || v6.is_unspecified()
        || (first_segment & 0xfe00) == 0xfc00
        || (first_segment & 0xffc0) == 0xfe80
        || v6
            .to_ipv4_mapped()
            .is_some_and(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
}

/// Path to a blob given its cache key, sharded `<aa>/<bb>/<rest>` to keep
/// directory fanout bounded. Mirrors the layout of the CAS `DiskBackend`.
pub fn blob_path(blobs_dir: &Path, key: &str) -> PathBuf {
    if key.len() < 4 {
        return blobs_dir.join(key);
    }
    blobs_dir.join(&key[0..2]).join(&key[2..4]).join(&key[4..])
}

/// Path to the sidecar metadata file for a blob.
pub fn meta_path(blobs_dir: &Path, key: &str) -> PathBuf {
    let blob = blob_path(blobs_dir, key);
    let mut name = blob.file_name().unwrap_or_default().to_os_string();
    name.push(".meta");
    blob.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_hosts(hosts: &[&str]) -> ProxyConfig {
        ProxyConfig {
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_allow_list_permits_public_hosts() {
        let cfg = config_with_hosts(&[]);
        assert!(cfg.is_host_allowed("github.com"));
        assert!(cfg.is_host_allowed("files.pythonhosted.org"));
    }

    #[test]
    fn private_targets_are_refused_even_with_empty_allow_list() {
        let cfg = config_with_hosts(&[]);
        for host in [
            "localhost",
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "[::1]",
            "[fd00::1]",
            "rbe-cache.ci.svc.cluster.local",
            "metadata.internal",
        ] {
            assert!(!cfg.is_host_allowed(host), "{host} should be refused");
        }
    }

    #[test]
    fn allow_list_matches_host_and_subdomains_only() {
        let cfg = config_with_hosts(&["github.com", "pythonhosted.org"]);
        assert!(cfg.is_host_allowed("github.com"));
        assert!(cfg.is_host_allowed("objects.github.com"));
        assert!(cfg.is_host_allowed("files.pythonhosted.org"));
        assert!(!cfg.is_host_allowed("evil.com"));
        assert!(
            !cfg.is_host_allowed("notgithub.com"),
            "a bare suffix match is not a subdomain"
        );
    }

    #[test]
    fn host_matching_ignores_case_and_trailing_dot() {
        let cfg = config_with_hosts(&["github.com"]);
        assert!(cfg.is_host_allowed("GitHub.com."));
    }

    #[test]
    fn blob_and_meta_paths_are_sharded_and_adjacent() {
        let dir = Path::new("/data/proxy/blobs");
        let key = "abcdef0123456789";
        assert_eq!(
            blob_path(dir, key),
            dir.join("ab").join("cd").join("ef0123456789")
        );
        assert_eq!(
            meta_path(dir, key),
            dir.join("ab").join("cd").join("ef0123456789.meta")
        );
    }
}
