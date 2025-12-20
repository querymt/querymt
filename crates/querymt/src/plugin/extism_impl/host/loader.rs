use super::ExtismFactory;
use crate::{
    error::LLMError,
    plugin::{
        host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin},
        LLMProviderFactory,
    },
};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::instrument;

pub struct ExtismLoader;

#[async_trait]
impl PluginLoader for ExtismLoader {
    fn supported_type(&self) -> PluginType {
        PluginType::Wasm
    }

    #[instrument(name = "extism_loader.load_plugin", skip_all, fields(plugin = %plugin.file_path.display()))]
    async fn load_plugin(
        &self,
        plugin: ProviderPlugin,
        plugin_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        let bytes = std::fs::read(&plugin.file_path)?;
        let provider = ExtismFactory::load(bytes, &plugin_cfg.config)?;
        Ok(Arc::new(provider))
    }
}
