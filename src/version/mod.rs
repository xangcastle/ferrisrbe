

pub mod detector;
pub mod handlers;
pub mod registry;
pub mod traits;

pub use detector::CompositeDetector;
pub use registry::VersionRegistry;
pub use traits::{
    BazelVersion, BazelVersionHandler, DetectionSource, VersionContext, VersionDetector,
};

use std::sync::Arc;
use tonic::Request;
use tracing::{debug, info};

#[derive(Clone)]
pub struct VersionManager {
    detector: CompositeDetector,
    registry: Arc<VersionRegistry>,
}

impl VersionManager {

    pub fn new() -> Self {
        Self {
            detector: CompositeDetector::new(),
            registry: Arc::new(VersionRegistry::with_defaults()),
        }
    }

    pub fn detect<T>(&self, request: &Request<T>) -> VersionContext
    where
        T: Send + Sync + 'static,
    {
        match self.detector.detect(request) {
            Some(version) => {
                info!("🔍 Detected Bazel version: {}", version);
                VersionContext::with_version(version, DetectionSource::GrpcHeader)
            }
            None => {
                debug!("Could not detect Bazel version, using defaults");
                VersionContext::unknown()
            }
        }
    }

    pub fn get_handler(&self, context: &VersionContext) -> Arc<dyn BazelVersionHandler> {
        self.registry.get_handler_optional(context.bazel_version)
    }

    pub fn detect_and_get_handler<T>(
        &self,
        request: &Request<T>,
    ) -> (VersionContext, Arc<dyn BazelVersionHandler>)
    where
        T: Send + Sync + 'static,
    {
        let context = self.detect(request);
        let handler = self.get_handler(&context);
        (context, handler)
    }

    pub fn handler_count(&self) -> usize {
        self.registry.handler_count()
    }
}

impl Default for VersionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_manager_new() {
        let manager = VersionManager::new();
        assert_eq!(manager.handler_count(), 3);
    }

    #[test]
    fn test_version_manager_default() {
        let manager: VersionManager = Default::default();
        assert_eq!(manager.handler_count(), 3);
    }
}
