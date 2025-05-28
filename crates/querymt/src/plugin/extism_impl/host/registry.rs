use crate::{
    error::LLMError,
    plugin::{
        extism_impl::host::{ExtismConfig, ExtismFactory, OciDownloader},
        LLMProviderFactory, ProviderRegistry,
    },
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
};
use tracing::instrument;

pub struct ExtismProviderRegistry {
    config: ExtismConfig,
    factories: RwLock<HashMap<String, Arc<dyn LLMProviderFactory>>>,
    oci_downloader: Arc<OciDownloader>,
}

impl ExtismProviderRegistry {
    pub async fn new<P: AsRef<Path>>(plugin_cfg: P) -> Result<Self> {
        let cfg = ExtismConfig::from_path(plugin_cfg).expect("ExtismPlugin config path");
        let oci_config = cfg.oci.clone();
        let initial_map: HashMap<String, Arc<dyn LLMProviderFactory>> = HashMap::new();
        let registry = ExtismProviderRegistry {
            config: cfg,
            factories: RwLock::new(initial_map),
            oci_downloader: Arc::new(OciDownloader::new(oci_config)),
        };
        registry.load_plugins().await?;
        Ok(registry)
    }

    #[instrument(skip(self))]
    async fn load_plugins(&self) -> Result<()> {
        for plugin_cfg in &self.config.providers {
            let wasm_content = if plugin_cfg.path.starts_with("http") {
                reqwest::get(&plugin_cfg.path)
                    .await?
                    .bytes()
                    .await?
                    .to_vec()
            } else if plugin_cfg.path.starts_with("oci") {
                let image_reference = plugin_cfg.path.strip_prefix("oci://").unwrap();
                let target_file_path = "/plugin.wasm";
                let mut hasher = Sha256::new();
                hasher.update(image_reference);
                let hash = hasher.finalize();
                let short_hash = &hex::encode(hash)[..7];
                let cache_dir = dirs::cache_dir()
                    .map(|mut path| {
                        path.push("querymt");
                        path
                    })
                    .unwrap();
                std::fs::create_dir_all(&cache_dir)?;

                let local_output_path =
                    cache_dir.join(format!("{}-{}.wasm", plugin_cfg.name, short_hash));
                let local_output_path = local_output_path.to_str().unwrap();

                if let Err(e) = self
                    .oci_downloader
                    .pull_and_extract(image_reference, target_file_path, local_output_path)
                    .await
                {
                    log::error!("Error pulling oci plugin: {}", e);
                    return Err(anyhow::anyhow!("Failed to pull OCI plugin: {}", e));
                }
                log::info!(
                    "cache plugin `{}` to : {}",
                    plugin_cfg.name,
                    local_output_path
                );
                tokio::fs::read(local_output_path).await?
            } else {
                tokio::fs::read(&plugin_cfg.path)
                    .await
                    .expect(&format!("Error loading: {}", plugin_cfg.path))
            };

            match ExtismFactory::load(wasm_content, &plugin_cfg.config) {
                Ok(provider) => {
                    self.factories
                        .write()
                        .map_err(|e| LLMError::PluginError(e.to_string()))?
                        .insert(plugin_cfg.name.clone(), Arc::new(provider));
                    log::info!("Loaded provider {}", plugin_cfg.name);
                }
                Err(e) => {
                    log::error!(
                        "Error while loading '{:?}' plugin: {:?}",
                        plugin_cfg.name,
                        e
                    );
                }
            }
        }
        Ok(())
    }
}

impl ProviderRegistry for ExtismProviderRegistry {
    fn get(&self, provider: &str) -> Option<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().get(provider).cloned()
    }

    fn list(&self) -> Vec<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().values().cloned().collect()
    }
}
