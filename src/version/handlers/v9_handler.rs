use tracing::debug;

use crate::proto::build::bazel::remote::execution::v2::ServerCapabilities;
use crate::version::handlers::{reapi_v2_0_semver, reapi_v2_4_semver};
use crate::version::traits::{BazelVersion, BazelVersionHandler, ReapiField};

pub struct Bazel9Handler;

impl Bazel9Handler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Bazel9Handler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BazelVersionHandler for Bazel9Handler {
    fn version_range(&self) -> (BazelVersion, Option<BazelVersion>) {
        (BazelVersion::new(9, 0, 0), None)
    }

    fn adapt_capabilities(&self, caps: &mut ServerCapabilities, version: BazelVersion) {
        debug!("Adapting capabilities for Bazel 9.x+ (version {})", version);

        caps.deprecated_api_version = Some(reapi_v2_0_semver());
        caps.low_api_version = Some(reapi_v2_0_semver());
        caps.high_api_version = Some(reapi_v2_4_semver());

        if let Some(ref mut exec_caps) = caps.execution_capabilities {
            exec_caps.exec_enabled = true;
        }

        if let Some(ref mut cache_caps) = caps.cache_capabilities {
            debug!("Configuring extended CacheCapabilities for Bazel 9.x");

            cache_caps.max_batch_total_size_bytes = 4 * 1024 * 1024;
        }

        debug!(
            "Capabilities configured for Bazel {}: deprecated={:?}, low={:?}, high={:?}",
            version, caps.deprecated_api_version, caps.low_api_version, caps.high_api_version
        );
    }

    fn requires_field(&self, field: ReapiField) -> bool {
        match field {
            ReapiField::DeprecatedApiVersion => true,
            ReapiField::LowApiVersion => true,
            ReapiField::HighApiVersion => true,

            ReapiField::CacheCapabilitiesExtended => true,
            ReapiField::ZstdCompression => true,
            ReapiField::SymlinkStrategy => true,
        }
    }

    fn name(&self) -> &'static str {
        "Bazel9Handler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_range() {
        let handler = Bazel9Handler::new();
        let (min, max) = handler.version_range();

        assert_eq!(min, BazelVersion::new(9, 0, 0));
        assert_eq!(max, None);

        assert!(BazelVersion::new(9, 0, 0).in_range(min, max));
        assert!(BazelVersion::new(9, 1, 0).in_range(min, max));
        assert!(BazelVersion::new(10, 0, 0).in_range(min, max));

        assert!(!BazelVersion::new(8, 3, 0).in_range(min, max));
    }

    #[test]
    fn test_all_fields_required() {
        let handler = Bazel9Handler::new();

        assert!(handler.requires_field(ReapiField::DeprecatedApiVersion));
        assert!(handler.requires_field(ReapiField::LowApiVersion));
        assert!(handler.requires_field(ReapiField::HighApiVersion));
        assert!(handler.requires_field(ReapiField::CacheCapabilitiesExtended));
        assert!(handler.requires_field(ReapiField::ZstdCompression));
        assert!(handler.requires_field(ReapiField::SymlinkStrategy));
    }
}
