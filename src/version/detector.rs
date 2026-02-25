use tonic::Request;
use tracing::{debug, trace};

use super::traits::{BazelVersion, VersionDetector};

#[derive(Clone)]
pub struct GrpcMetadataDetector;

impl GrpcMetadataDetector {
    pub fn new() -> Self {
        Self
    }

    fn extract_from_user_agent(&self, user_agent: &str) -> Option<BazelVersion> {
        trace!("Analyzing User-Agent: {}", user_agent);

        let lower = user_agent.to_lowercase();

        if let Some(pos) = lower.find("bazel/") {
            let version_part = &user_agent[pos + 6..];
            let version_str = version_part.split_whitespace().next()?;

            debug!("Version extracted from User-Agent: {}", version_str);
            return BazelVersion::parse(version_str);
        }

        None
    }

    fn extract_from_bazel_header(&self, value: &str) -> Option<BazelVersion> {
        BazelVersion::parse(value.trim())
    }

    fn extract_from_binary_metadata(&self, metadata: &[u8]) -> Option<BazelVersion> {
        if let Ok(s) = std::str::from_utf8(metadata) {
            return BazelVersion::parse(s.trim());
        }
        None
    }
}

impl Default for GrpcMetadataDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl VersionDetector for GrpcMetadataDetector {
    fn detect<T>(&self, request: &Request<T>) -> Option<BazelVersion>
    where
        T: Send + Sync + 'static,
    {
        let metadata = request.metadata();

        if let Some(value) = metadata.get("x-bazel-version") {
            if let Ok(s) = value.to_str() {
                trace!("Found header x-bazel-version: {}", s);
                if let Some(version) = self.extract_from_bazel_header(s) {
                    debug!("✅ Version detected from x-bazel-version: {}", version);
                    return Some(version);
                }
            }
        }

        if let Some(value) = metadata.get("user-agent") {
            if let Ok(s) = value.to_str() {
                if let Some(version) = self.extract_from_user_agent(s) {
                    debug!("✅ Version detected from User-Agent: {}", version);
                    return Some(version);
                }
            }
        }

        if let Some(value) = metadata.get(":authority") {
            trace!("Authority: {:?}", value);
        }

        if let Some(value) = metadata.get_bin("bazel-version-bin") {
            if let Some(version) = self.extract_from_binary_metadata(value.as_ref()) {
                debug!("✅ Version detected from binary metadata: {}", version);
                return Some(version);
            }
        }

        debug!("❌ Could not detect Bazel version from metadata");
        None
    }

    fn name(&self) -> &'static str {
        "GrpcMetadataDetector"
    }
}

#[derive(Clone)]
pub struct CompositeDetector {
    grpc_detector: GrpcMetadataDetector,
}

impl CompositeDetector {
    pub fn new() -> Self {
        Self {
            grpc_detector: GrpcMetadataDetector::new(),
        }
    }
}

impl Default for CompositeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl VersionDetector for CompositeDetector {
    fn detect<T>(&self, request: &Request<T>) -> Option<BazelVersion>
    where
        T: Send + Sync + 'static,
    {
        trace!("Trying detector: {}", self.grpc_detector.name());
        if let Some(version) = self.grpc_detector.detect(request) {
            debug!(
                "Version detected by {}: {}",
                self.grpc_detector.name(),
                version
            );
            return Some(version);
        }
        None
    }

    fn name(&self) -> &'static str {
        "CompositeDetector"
    }
}

pub struct DefaultVersionDetector {
    default_version: BazelVersion,
}

impl DefaultVersionDetector {
    pub fn new(default_version: BazelVersion) -> Self {
        Self { default_version }
    }
}

impl VersionDetector for DefaultVersionDetector {
    fn detect<T>(&self, _request: &Request<T>) -> Option<BazelVersion>
    where
        T: Send + Sync + 'static,
    {
        Some(self.default_version)
    }

    fn name(&self) -> &'static str {
        "DefaultVersionDetector"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Request;

    #[test]
    fn test_bazel_version_parse() {
        assert_eq!(
            BazelVersion::parse("8.3.0"),
            Some(BazelVersion::new(8, 3, 0))
        );
        assert_eq!(
            BazelVersion::parse("7.4.1"),
            Some(BazelVersion::new(7, 4, 1))
        );
        assert_eq!(
            BazelVersion::parse("9.0.0"),
            Some(BazelVersion::new(9, 0, 0))
        );
        assert_eq!(BazelVersion::parse("8.0"), Some(BazelVersion::new(8, 0, 0)));
        assert_eq!(BazelVersion::parse("7"), Some(BazelVersion::new(7, 0, 0)));
    }

    #[test]
    fn test_version_comparison() {
        let v7 = BazelVersion::new(7, 0, 0);
        let v8 = BazelVersion::new(8, 0, 0);
        let v8_3 = BazelVersion::new(8, 3, 0);

        assert!(v7 < v8);
        assert!(v8 < v8_3);
        assert!(v7.in_range(BazelVersion::new(7, 0, 0), Some(BazelVersion::new(8, 0, 0))));
        assert!(!v8.in_range(BazelVersion::new(7, 0, 0), Some(BazelVersion::new(8, 0, 0))));
    }

    #[test]
    fn test_recommended_reapi_version() {
        assert_eq!(
            BazelVersion::new(7, 4, 0).recommended_reapi_version(),
            super::super::traits::ReapiVersion::V2_3
        );
        assert_eq!(
            BazelVersion::new(8, 0, 0).recommended_reapi_version(),
            super::super::traits::ReapiVersion::V2_4
        );
        assert_eq!(
            BazelVersion::new(9, 0, 0).recommended_reapi_version(),
            super::super::traits::ReapiVersion::V2_4
        );
    }

    #[test]
    fn test_extract_from_user_agent() {
        let detector = GrpcMetadataDetector::new();

        assert_eq!(
            detector.extract_from_user_agent("bazel/8.3.0 grpc-java/1.50.0"),
            Some(BazelVersion::new(8, 3, 0))
        );
        assert_eq!(
            detector.extract_from_user_agent("Bazel/7.4.1"),
            Some(BazelVersion::new(7, 4, 1))
        );
        assert_eq!(detector.extract_from_user_agent("grpc-go/1.50.0"), None);
    }
}
