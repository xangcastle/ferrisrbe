

use std::sync::Arc;
use tracing::{debug, info, warn};

use super::handlers::{Bazel7Handler, Bazel8Handler, Bazel9Handler};
use super::traits::{BazelVersion, BazelVersionHandler};

pub struct VersionRegistry {
    handlers: Vec<Arc<dyn BazelVersionHandler>>,
    default_handler: Arc<dyn BazelVersionHandler>,
}

impl VersionRegistry {

    pub fn new() -> Self {

        let default_handler: Arc<dyn BazelVersionHandler> = Arc::new(Bazel9Handler::new());

        Self {
            handlers: Vec::new(),
            default_handler,
        }
    }

    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register_default_handlers();
        registry
    }

    pub fn register_default_handlers(&mut self) {
        info!("🏗️  Registering default version handlers");

        self.register(Arc::new(Bazel7Handler::new()));
        self.register(Arc::new(Bazel8Handler::new()));
        self.register(Arc::new(Bazel9Handler::new()));

        info!("✅ {} handlers registered", self.handlers.len());
    }

    pub fn register(&mut self, handler: Arc<dyn BazelVersionHandler>) {
        let (min, max) = handler.version_range();
        info!(
            "📋 Registering handler '{}' for range [{}, {})",
            handler.name(),
            min,
            max.map(|v| v.to_string())
                .unwrap_or_else(|| "∞".to_string())
        );
        self.handlers.push(handler);
    }

    pub fn get_handler(&self, version: BazelVersion) -> Arc<dyn BazelVersionHandler> {
        debug!("Finding handler for Bazel {}", version);

        for handler in &self.handlers {
            let (min, max) = handler.version_range();
            if version.in_range(min, max) {
                debug!("✅ Handler found: '{}'", handler.name());
                return handler.clone();
            }
        }

        warn!(
            "⚠️  No specific handler found for Bazel {}, using default '{}'*",
            version,
            self.default_handler.name()
        );
        self.default_handler.clone()
    }

    pub fn get_handler_optional(
        &self,
        version: Option<BazelVersion>,
    ) -> Arc<dyn BazelVersionHandler> {
        match version {
            Some(v) => self.get_handler(v),
            None => {
                debug!(
                    "Using default handler: '{}'",
                    self.default_handler.name()
                );
                self.default_handler.clone()
            }
        }
    }

    pub fn list_handlers(&self) -> Vec<(String, String, Option<String>)> {
        self.handlers
            .iter()
            .map(|h| {
                let (min, max) = h.version_range();
                (
                    h.name().to_string(),
                    min.to_string(),
                    max.map(|v| v.to_string()),
                )
            })
            .collect()
    }

    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    pub fn set_default_handler(&mut self, handler: Arc<dyn BazelVersionHandler>) {
        info!("🔄 Default handler changed to: '{}'", handler.name());
        self.default_handler = handler;
    }
}

impl Default for VersionRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

unsafe impl Send for VersionRegistry {}
unsafe impl Sync for VersionRegistry {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::traits::ReapiField;

    #[test]
    fn test_registry_default_handlers() {
        let registry = VersionRegistry::with_defaults();

        assert_eq!(registry.handler_count(), 3);

        let handlers = registry.list_handlers();
        assert_eq!(handlers.len(), 3);

        let names: Vec<String> = handlers.iter().map(|(n, _, _)| n.clone()).collect();
        assert!(names.contains(&"Bazel7Handler".to_string()));
        assert!(names.contains(&"Bazel8Handler".to_string()));
        assert!(names.contains(&"Bazel9Handler".to_string()));
    }

    #[test]
    fn test_get_handler_v7() {
        let registry = VersionRegistry::with_defaults();

        let handler = registry.get_handler(BazelVersion::new(7, 0, 0));
        assert_eq!(handler.name(), "Bazel7Handler");

        let handler = registry.get_handler(BazelVersion::new(7, 4, 1));
        assert_eq!(handler.name(), "Bazel7Handler");

        let handler = registry.get_handler(BazelVersion::new(7, 99, 99));
        assert_eq!(handler.name(), "Bazel7Handler");
    }

    #[test]
    fn test_get_handler_v8() {
        let registry = VersionRegistry::with_defaults();

        let handler = registry.get_handler(BazelVersion::new(8, 0, 0));
        assert_eq!(handler.name(), "Bazel8Handler");

        let handler = registry.get_handler(BazelVersion::new(8, 3, 0));
        assert_eq!(handler.name(), "Bazel8Handler");

        let handler = registry.get_handler(BazelVersion::new(8, 99, 99));
        assert_eq!(handler.name(), "Bazel8Handler");
    }

    #[test]
    fn test_get_handler_v9() {
        let registry = VersionRegistry::with_defaults();

        let handler = registry.get_handler(BazelVersion::new(9, 0, 0));
        assert_eq!(handler.name(), "Bazel9Handler");

        let handler = registry.get_handler(BazelVersion::new(9, 1, 0));
        assert_eq!(handler.name(), "Bazel9Handler");

        let handler = registry.get_handler(BazelVersion::new(10, 0, 0));
        assert_eq!(handler.name(), "Bazel9Handler");
    }

    #[test]
    fn test_get_handler_optional() {
        let registry = VersionRegistry::with_defaults();

        let handler = registry.get_handler_optional(Some(BazelVersion::new(8, 0, 0)));
        assert_eq!(handler.name(), "Bazel8Handler");

        let handler = registry.get_handler_optional(None);
        assert_eq!(handler.name(), "Bazel9Handler");
    }

    #[test]
    fn test_version_range_boundaries() {
        let registry = VersionRegistry::with_defaults();

        let handler = registry.get_handler(BazelVersion::new(7, 99, 99));
        assert_eq!(handler.name(), "Bazel7Handler");

        let handler = registry.get_handler(BazelVersion::new(8, 0, 0));
        assert_eq!(handler.name(), "Bazel8Handler");

        let handler = registry.get_handler(BazelVersion::new(8, 99, 99));
        assert_eq!(handler.name(), "Bazel8Handler");

        let handler = registry.get_handler(BazelVersion::new(9, 0, 0));
        assert_eq!(handler.name(), "Bazel9Handler");
    }
}
