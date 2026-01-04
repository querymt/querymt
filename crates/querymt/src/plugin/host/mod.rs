use crate::{error::LLMError, plugin::LLMProviderFactory};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;
use std::{collections::HashMap, path::PathBuf};
use tracing::instrument;

pub mod config;
pub use config::{PluginConfig, ProviderConfig};

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum PluginType {
    Wasm,
    Native,
}

#[derive(Debug, Clone)]
pub struct ProviderPlugin {
    pub plugin_type: PluginType,
    pub file_path: PathBuf,
}

#[async_trait]
pub trait PluginLoader: Send + Sync {
    fn supported_type(&self) -> PluginType;
    async fn load_plugin(
        &self,
        plugin: ProviderPlugin,
        plugin_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError>;
}

mod oci;

#[cfg(feature = "native")]
pub mod native;

pub struct PluginRegistry {
    loaders: HashMap<PluginType, Box<dyn PluginLoader>>,
    factories: RwLock<HashMap<String, Arc<dyn LLMProviderFactory>>>,
    pub oci_downloader: Arc<oci::OciDownloader>,
    pub config: config::PluginConfig,
    pub cache_path: PathBuf,
}

impl PluginRegistry {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, LLMError> {
        let config =
            PluginConfig::from_path(path).map_err(|e| LLMError::PluginError(e.to_string()))?;

        let cache_dir = dirs::cache_dir()
            .map(|mut path| {
                path.push("querymt");
                path
            })
            .unwrap();
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| LLMError::InvalidRequest(format!("{:#}", e)))?;

        Ok(PluginRegistry {
            loaders: HashMap::new(),
            factories: RwLock::new(HashMap::new()),
            oci_downloader: Arc::new(oci::OciDownloader::new(config.oci.clone())),
            config,
            cache_path: cache_dir,
        })
    }

    pub fn from_default_path() -> Result<Self, LLMError> {
        Self::from_path(crate::plugin::default_providers_path())
    }

    pub fn register_loader(&mut self, loader: Box<dyn PluginLoader>) {
        self.loaders.insert(loader.supported_type(), loader);
    }

    #[instrument(name = "plugin_registry.load_all_plugins", skip_all)]
    pub async fn load_all_plugins(&self) {
        log::debug!("Loading all configured plugins...");
        for provider_cfg in &self.config.providers {
            match self.load_and_process_plugin(provider_cfg).await {
                Ok(provider) => {
                    log::info!("Adding '{}' provider to registry", provider_cfg.name);
                    self.factories
                        .write()
                        .unwrap()
                        .insert(provider_cfg.name.clone(), provider);
                }
                Err(e) => log::error!("Failed to process provider '{}': {}", provider_cfg.name, e),
            }
        }
    }

    #[instrument(name = "plugin_registry.load_and_process_plugin", skip_all, fields(provider = %provider_cfg.name))]
    pub async fn load_and_process_plugin(
        &self,
        provider_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        log::debug!("Processing plugin: {:?}", provider_cfg);

        let provider_plugin;
        if provider_cfg.path.starts_with("oci") {
            let image_reference = provider_cfg.path.strip_prefix("oci://").unwrap();
            provider_plugin = self
                .oci_downloader
                .pull_and_extract(image_reference, None, &self.cache_path, false)
                .await
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
            log::debug!(
                "Discovered type '{:?}' via OCI annotation.",
                provider_plugin.plugin_type
            );
        } else {
            let file_path = Path::new(&provider_cfg.path);
            if !file_path.exists() {
                return Err(LLMError::PluginError(format!(
                    "Local file not found at path: {}",
                    provider_cfg.path
                )));
            }

            let plugin_type = if provider_cfg.path.ends_with("wasm") {
                PluginType::Wasm
            } else if provider_cfg.path.ends_with(std::env::consts::DLL_EXTENSION) {
                PluginType::Native
            } else {
                return Err(LLMError::PluginError(format!(
                    "Unable to load local provider plugin: {}",
                    provider_cfg.path
                )));
            };

            provider_plugin = ProviderPlugin {
                plugin_type,
                file_path: file_path.to_path_buf(),
            }
        }

        if let Some(loader) = self.loaders.get(&provider_plugin.plugin_type) {
            loader.load_plugin(provider_plugin, provider_cfg).await
        } else {
            Err(LLMError::PluginError(format!(
                "No registered loader for plugin type '{:?}'",
                provider_plugin.plugin_type
            )))
        }
    }

    pub fn get(&self, provider: &str) -> Option<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().get(provider).cloned()
    }

    pub fn list(&self) -> Vec<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().values().cloned().collect()
    }
}
