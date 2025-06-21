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

pub struct NativeLoader;

#[async_trait]
impl PluginLoader for NativeLoader {
    fn supported_type(&self) -> PluginType {
        PluginType::Native
    }

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
        let lib =
            unsafe { Library::new(path).map_err(|e| LLMError::PluginError(format!("{:#}", e)))? };

        let factory: Arc<dyn LLMProviderFactory> = unsafe {
            if let Ok(async_ctor) = lib.get::<FactoryCtor>(b"plugin_factory") {
                let raw = async_ctor();
                if raw.is_null() {
                    return Err(LLMError::PluginError(format!(
                        "plugin_factory returned null in {}",
                        path.display()
                    )));
                }
                Arc::from_raw(raw)
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
                Arc::new(async_fact)
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

        // The library must remain loaded for the duration of the program.
        std::mem::forget(lib);
        Ok(factory)
    }
}
