use crate::{
    error::LLMError,
    plugin::{
        adapters::HTTPFactoryAdapter,
        host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin},
        FactoryCtor, HTTPFactoryCtor, HTTPLLMProviderFactory, LLMProviderFactory,
    },
};
use async_trait::async_trait;
use libloading::Library;
use std::path::Path;
use std::sync::Arc;
use tracing::instrument;

struct NativeFactoryWrapper {
    factory_impl: Box<dyn LLMProviderFactory>,
    _library: Arc<Library>, // The underscore indicates we hold it just for its lifetime
}

// Manually implement the trait for your wrapper
impl LLMProviderFactory for NativeFactoryWrapper {
    fn name(&self) -> &str {
        self.factory_impl.name()
    }
    fn config_schema(&self) -> serde_json::Value {
        self.factory_impl.config_schema()
    }
    fn from_config(
        &self,
        cfg: &serde_json::Value,
    ) -> Result<Box<dyn crate::LLMProvider>, LLMError> {
        self.factory_impl.from_config(cfg)
    }

    fn list_models<'a>(
        &'a self,
        cfg: &serde_json::Value,
    ) -> crate::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
        self.factory_impl.list_models(cfg)
    }
}

pub struct NativeLoader;

#[async_trait]
impl PluginLoader for NativeLoader {
    fn supported_type(&self) -> PluginType {
        PluginType::Native
    }

    #[instrument(name = "native_loader.load_plugin", skip_all, fields(plugin = %plugin.file_path.display(), name = %plugin_cfg.name))]
    async fn load_plugin(
        &self,
        plugin: ProviderPlugin,
        plugin_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        log::info!(
            "Loading native plugin '{}' from {}",
            plugin_cfg.name,
            plugin.file_path.display()
        );

        let provider = self.load_library(&plugin_cfg.name, &plugin.file_path)?;
        Ok(provider)
    }
}

impl NativeLoader {
    fn load_library(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        let lib = unsafe {
            Arc::new(Library::new(path).map_err(|e| LLMError::PluginError(format!("{:#}", e)))?)
        };

        let factory: Box<dyn LLMProviderFactory> = unsafe {
            if let Ok(async_ctor) = lib.get::<FactoryCtor>(b"plugin_factory") {
                let raw = async_ctor();
                if raw.is_null() {
                    return Err(LLMError::PluginError(format!(
                        "plugin_factory returned null in {}",
                        path.display()
                    )));
                }
                Box::from_raw(raw)
            } else if let Ok(sync_ctor) = lib.get::<HTTPFactoryCtor>(b"plugin_http_factory") {
                let raw: *mut dyn HTTPLLMProviderFactory = sync_ctor();
                if raw.is_null() {
                    return Err(LLMError::PluginError(format!(
                        "plugin_http_factory returned null in {}",
                        path.display()
                    )));
                }
                let sync_fact: Box<dyn HTTPLLMProviderFactory> = Box::from_raw(raw);
                let async_fact = HTTPFactoryAdapter::new(Arc::from(sync_fact));
                Box::new(async_fact)
            } else {
                return Err(LLMError::PluginError(format!(
                    "no plugin_factory or plugin_http_factory in {}",
                    path.display()
                )));
            }
        };

        let factory_name = factory.name();
        if factory_name != name {
            log::warn!(
                "Plugin name mismatch in {}: config name is '{}', but plugin reports '{}'. Using config name.",
                path.display(),
                name,
                factory_name
            );
        }
        Ok(Arc::new(NativeFactoryWrapper {
            factory_impl: factory,
            _library: Arc::clone(&lib),
        }))
    }
}
