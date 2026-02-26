use tracing::debug;

use crate::proto::build::bazel::remote::execution::v2::ServerCapabilities;
use crate::version::handlers::{reapi_v2_0_semver, reapi_v2_3_semver, reapi_v2_4_semver};
use crate::version::traits::{BazelVersion, BazelVersionHandler, ReapiField};

pub struct Bazel7Handler;

impl Bazel7Handler {
    pub fn new() -> Self {
        Self
    }

    fn prefers_v2_4(&self, version: BazelVersion) -> bool {
        version.minor >= 4
    }
}

impl Default for Bazel7Handler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BazelVersionHandler for Bazel7Handler {
    fn version_range(&self) -> (BazelVersion, Option<BazelVersion>) {
        (BazelVersion::new(7, 0, 0), Some(BazelVersion::new(8, 0, 0)))
    }

    fn adapt_capabilities(&self, caps: &mut ServerCapabilities, version: BazelVersion) {
        debug!("Adapting capabilities for Bazel 7.x (version {})", version);

        if self.prefers_v2_4(version) {
            caps.deprecated_api_version = Some(reapi_v2_0_semver());
            caps.low_api_version = Some(reapi_v2_0_semver());
            caps.high_api_version = Some(reapi_v2_4_semver());
        } else {
            caps.deprecated_api_version = None;
            caps.low_api_version = Some(reapi_v2_0_semver());
            caps.high_api_version = Some(reapi_v2_3_semver());
        }

        if let Some(ref mut exec_caps) = caps.execution_capabilities {
            exec_caps.exec_enabled = true;
        }
    }

    fn requires_field(&self, field: ReapiField) -> bool {
        match field {
            ReapiField::DeprecatedApiVersion => false,

            ReapiField::LowApiVersion | ReapiField::HighApiVersion => true,

            ReapiField::CacheCapabilitiesExtended => false,
            ReapiField::ZstdCompression => false,
            ReapiField::SymlinkStrategy => false,
        }
    }

    fn name(&self) -> &'static str {
        "Bazel7Handler"
    }
}
