

use tracing::debug;

use crate::proto::build::bazel::remote::execution::v2::ServerCapabilities;
use crate::version::handlers::{reapi_v2_0_semver, reapi_v2_4_semver};
use crate::version::traits::{BazelVersion, BazelVersionHandler, ReapiField};

pub struct Bazel8Handler;

impl Bazel8Handler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Bazel8Handler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BazelVersionHandler for Bazel8Handler {
    fn version_range(&self) -> (BazelVersion, Option<BazelVersion>) {

        (BazelVersion::new(8, 0, 0), Some(BazelVersion::new(9, 0, 0)))
    }

    fn adapt_capabilities(&self, caps: &mut ServerCapabilities, version: BazelVersion) {
        debug!(
            "Adapting capabilities for Bazel 8.x (version {})",
            version
        );

        caps.deprecated_api_version = Some(reapi_v2_0_semver());
        caps.low_api_version = Some(reapi_v2_0_semver());
        caps.high_api_version = Some(reapi_v2_4_semver());

        if let Some(ref mut exec_caps) = caps.execution_capabilities {
            exec_caps.exec_enabled = true;
        }

        if let Some(ref mut cache_caps) = caps.cache_capabilities {

            debug!("Configuring CacheCapabilities for Bazel 8.x");

            cache_caps.max_batch_total_size_bytes = 4 * 1024 * 1024;
        }

        debug!(
            "Capabilities configured: deprecated={:?}, low={:?}, high={:?}",
            caps.deprecated_api_version, caps.low_api_version, caps.high_api_version
        );
    }

    fn requires_field(&self, field: ReapiField) -> bool {
        match field {

            ReapiField::DeprecatedApiVersion => true,

            ReapiField::LowApiVersion | ReapiField::HighApiVersion => true,

            ReapiField::CacheCapabilitiesExtended => true,
            ReapiField::ZstdCompression => true,
            ReapiField::SymlinkStrategy => false,
        }
    }

    fn name(&self) -> &'static str {
        "Bazel8Handler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_range() {
        let handler = Bazel8Handler::new();
        let (min, max) = handler.version_range();

        assert_eq!(min, BazelVersion::new(8, 0, 0));
        assert_eq!(max, Some(BazelVersion::new(9, 0, 0)));

        assert!(BazelVersion::new(8, 3, 0).in_range(min, max));

        assert!(!BazelVersion::new(7, 4, 0).in_range(min, max));

        assert!(!BazelVersion::new(9, 0, 0).in_range(min, max));
    }

    #[test]
    fn test_requires_deprecated_field() {
        let handler = Bazel8Handler::new();
        assert!(handler.requires_field(ReapiField::DeprecatedApiVersion));
        assert!(handler.requires_field(ReapiField::LowApiVersion));
        assert!(handler.requires_field(ReapiField::HighApiVersion));
    }
}
