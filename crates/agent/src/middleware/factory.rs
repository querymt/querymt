//! Middleware factory registry for config-based middleware creation
//!
//! This module provides a factory pattern for creating middleware from configuration files.
//! Each middleware type can register a factory that knows how to create instances from
//! a raw JSON/TOML config value.
//!
//! # Example
//!
//! ```toml
//! [[middleware]]
//! type = "dedup_check"
//! threshold = 0.8
//! min_lines = 5
//! ```
//!
//! The registry will look up the "dedup_check" factory and pass the config to it.

use crate::agent::core::QueryMTAgent;
use crate::middleware::MiddlewareDriver;
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Factory trait for creating middleware from config
///
/// Implement this trait for each middleware type that should be configurable
/// via TOML/JSON config files.
pub trait MiddlewareFactory: Send + Sync {
    /// Type name used in config files (e.g., "dedup_check")
    fn type_name(&self) -> &'static str;

    /// Create middleware instance from raw JSON config
    ///
    /// The config value contains all fields from the TOML section except "type".
    /// Returns an error if the middleware is disabled or config is invalid.
    fn create(&self, config: &Value, agent: &QueryMTAgent) -> Result<Arc<dyn MiddlewareDriver>>;
}

/// Global middleware registry (lazy singleton)
///
/// Access this to create middleware from config entries:
/// ```ignore
/// use crate::middleware::MIDDLEWARE_REGISTRY;
///
/// let middleware = MIDDLEWARE_REGISTRY.create("dedup_check", &config, &agent)?;
/// ```
pub static MIDDLEWARE_REGISTRY: Lazy<MiddlewareRegistry> = Lazy::new(MiddlewareRegistry::new);

/// Registry of available middleware factories
pub struct MiddlewareRegistry {
    factories: HashMap<&'static str, Arc<dyn MiddlewareFactory>>,
}

impl MiddlewareRegistry {
    fn new() -> Self {
        let mut registry = Self {
            factories: HashMap::new(),
        };
        // Register built-in factories
        registry.register(Arc::new(super::dedup_check::DedupCheckFactory));
        registry
    }

    fn register(&mut self, factory: Arc<dyn MiddlewareFactory>) {
        self.factories.insert(factory.type_name(), factory);
    }

    /// Get a factory by type name
    pub fn get(&self, type_name: &str) -> Option<&Arc<dyn MiddlewareFactory>> {
        self.factories.get(type_name)
    }

    /// Create middleware from type name and config
    ///
    /// Returns an error if:
    /// - The middleware type is unknown
    /// - The middleware is disabled (config has `enabled = false`)
    /// - The config is invalid for this middleware type
    pub fn create(
        &self,
        type_name: &str,
        config: &Value,
        agent: &QueryMTAgent,
    ) -> Result<Arc<dyn MiddlewareDriver>> {
        let factory = self
            .factories
            .get(type_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown middleware type: {}", type_name))?;
        factory.create(config, agent)
    }

    /// List all registered middleware type names
    pub fn type_names(&self) -> Vec<&'static str> {
        self.factories.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_has_dedup_check() {
        let types = MIDDLEWARE_REGISTRY.type_names();
        assert!(types.contains(&"dedup_check"));
    }

    #[test]
    fn test_unknown_middleware_type() {
        // Create a minimal agent for testing - this will fail but we're testing the error path
        let result = MIDDLEWARE_REGISTRY.get("unknown_type");
        assert!(result.is_none());
    }
}
