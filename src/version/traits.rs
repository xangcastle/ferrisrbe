use crate::proto::build::bazel::remote::execution::v2::ServerCapabilities;
use tonic::Request;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BazelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl BazelVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub const MIN_SUPPORTED: Self = Self::new(7, 0, 0);

    pub fn parse(version_str: &str) -> Option<Self> {
        let parts: Vec<&str> = version_str.split('.').collect();
        if parts.is_empty() {
            return None;
        }

        let major = parts.first()?.parse().ok()?;
        let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);

        Some(Self {
            major,
            minor,
            patch,
        })
    }

    pub fn in_range(&self, min: BazelVersion, max: Option<BazelVersion>) -> bool {
        self >= &min && max.as_ref().map(|m| self < m).unwrap_or(true)
    }

    pub fn recommended_reapi_version(&self) -> ReapiVersion {
        match self.major {
            7 => ReapiVersion::V2_3,
            8 => ReapiVersion::V2_4,
            9 => ReapiVersion::V2_4,
            _ => ReapiVersion::V2_4,
        }
    }
}

impl std::fmt::Display for BazelVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapiVersion {
    V2_3,
    V2_4,
}

impl ReapiVersion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::V2_3 => "2.3",
            Self::V2_4 => "2.4",
        }
    }
}

pub trait VersionDetector: Send + Sync {
    fn detect<T>(&self, request: &Request<T>) -> Option<BazelVersion>
    where
        T: Send + Sync + 'static;

    fn name(&self) -> &'static str;
}

#[async_trait::async_trait]
pub trait BazelVersionHandler: Send + Sync {
    fn version_range(&self) -> (BazelVersion, Option<BazelVersion>);

    fn adapt_capabilities(&self, caps: &mut ServerCapabilities, version: BazelVersion);

    fn requires_field(&self, field: ReapiField) -> bool;

    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapiField {
    DeprecatedApiVersion,

    LowApiVersion,

    HighApiVersion,

    CacheCapabilitiesExtended,

    ZstdCompression,

    SymlinkStrategy,
}

#[derive(Debug, Clone)]
pub struct VersionContext {
    pub bazel_version: Option<BazelVersion>,
    pub reapi_version: ReapiVersion,
    pub detected_from: DetectionSource,
}

impl VersionContext {
    pub fn unknown() -> Self {
        Self {
            bazel_version: None,
            reapi_version: ReapiVersion::V2_4,
            detected_from: DetectionSource::Default,
        }
    }

    pub fn with_version(version: BazelVersion, source: DetectionSource) -> Self {
        Self {
            reapi_version: version.recommended_reapi_version(),
            bazel_version: Some(version),
            detected_from: source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionSource {
    GrpcHeader,

    UserAgent,

    BazelMetadata,

    Default,
}

impl std::fmt::Display for DetectionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GrpcHeader => write!(f, "gRPC header"),
            Self::UserAgent => write!(f, "User-Agent"),
            Self::BazelMetadata => write!(f, "Bazel metadata"),
            Self::Default => write!(f, "default"),
        }
    }
}
