use crate::{error::LLMError, plugin::LLMProviderFactory};
use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};
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
pub use oci::{OciDownloadPhase, OciDownloadProgress, OciProgressCallback};

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
        Self::from_config(config)
    }

    /// Creates a registry from an already-parsed config.
    ///
    /// This uses the default cache directory (and creates it if missing).
    pub fn from_config(config: PluginConfig) -> Result<Self, LLMError> {
        let cache_dir = dirs::cache_dir()
            .map(|mut path| {
                path.push("querymt");
                path
            })
            .unwrap();
        Self::from_config_with_cache_path(config, cache_dir)
    }

    /// Creates a registry from an already-parsed config using a caller-supplied cache path.
    ///
    /// The cache directory is created if it does not exist.
    pub fn from_config_with_cache_path(
        config: PluginConfig,
        cache_path: PathBuf,
    ) -> Result<Self, LLMError> {
        std::fs::create_dir_all(&cache_path)
            .map_err(|e| LLMError::InvalidRequest(format!("{:#}", e)))?;

        Ok(PluginRegistry {
            loaders: HashMap::new(),
            factories: RwLock::new(HashMap::new()),
            oci_downloader: Arc::new(oci::OciDownloader::new(config.oci.clone())),
            config,
            cache_path,
        })
    }

    pub fn register_loader(&mut self, loader: Box<dyn PluginLoader>) {
        self.loaders.insert(loader.supported_type(), loader);
    }

    #[instrument(name = "plugin_registry.load_all_plugins", skip_all)]
    pub async fn load_all_plugins(&self) {
        // Skip providers that are already loaded (idempotency)
        // We need to collect loaded provider names to avoid holding the lock
        let loaded_names: std::collections::HashSet<String> = {
            let already_loaded = self.factories.read().unwrap();
            already_loaded.keys().cloned().collect()
        };

        let to_load: Vec<_> = self
            .config
            .providers
            .iter()
            .filter(|cfg| !loaded_names.contains(&cfg.name))
            .collect();

        if to_load.is_empty() {
            log::debug!(
                "All {} configured plugins already loaded, skipping",
                self.config.providers.len()
            );
            return;
        }

        log::debug!(
            "Loading {} of {} configured plugins in parallel...",
            to_load.len(),
            self.config.providers.len()
        );

        let mut futures: FuturesUnordered<_> = to_load
            .into_iter()
            .map(|cfg| async move {
                let result = self.load_and_process_plugin(cfg).await;
                (cfg, result)
            })
            .collect();

        while let Some((provider_cfg, result)) = futures.next().await {
            match result {
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
                .pull_and_extract(image_reference, None, &self.cache_path, false, None)
                .await
                .map_err(|e| {
                    LLMError::PluginError(format!(
                        "Failed to fetch OCI plugin for provider '{}' from '{}': {}",
                        provider_cfg.name, provider_cfg.path, e
                    ))
                })?;
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

    /// Get a provider factory, loading it lazily if not already loaded.
    ///
    /// This method will first check if the provider is already loaded in the registry.
    /// If not, it will attempt to load it from the configuration and cache it.
    pub async fn get(&self, provider: &str) -> Option<Arc<dyn LLMProviderFactory>> {
        // First check if already loaded
        if let Some(factory) = self.factories.read().unwrap().get(provider).cloned() {
            return Some(factory);
        }

        // Not loaded yet, find the provider config
        let provider_cfg = self.config.providers.iter().find(|p| p.name == provider)?;

        // Try to load it
        match self.load_and_process_plugin(provider_cfg).await {
            Ok(factory) => {
                log::info!("Lazy loaded provider '{}'", provider);
                self.factories
                    .write()
                    .unwrap()
                    .insert(provider.to_string(), factory.clone());
                Some(factory)
            }
            Err(e) => {
                log::error!("Failed to lazy load provider '{}': {}", provider, e);
                None
            }
        }
    }

    pub fn list(&self) -> Vec<Arc<dyn LLMProviderFactory>> {
        self.factories.read().unwrap().values().cloned().collect()
    }

    /// Force-update all OCI-based provider plugins, reporting progress via callback.
    ///
    /// Returns a vec of `(provider_name, result)` pairs, one per OCI provider found
    /// in the registry configuration.
    pub async fn update_oci_plugins(
        &self,
        progress: Option<OciProgressCallback>,
    ) -> Vec<(String, Result<ProviderPlugin, Box<dyn std::error::Error + Send + Sync>>)> {
        let mut results = Vec::new();
        for provider_cfg in &self.config.providers {
            if let Some(image_ref) = provider_cfg.path.strip_prefix("oci://") {
                let result = self
                    .oci_downloader
                    .pull_and_extract(image_ref, None, &self.cache_path, true, progress.clone())
                    .await
                    .map_err(|e| {
                        // Convert the untyped error into a Send + Sync error
                        let msg = e.to_string();
                        let boxed: Box<dyn std::error::Error + Send + Sync> =
                            msg.into();
                        boxed
                    });
                results.push((provider_cfg.name.clone(), result));
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_path(suffix: &str) -> PathBuf {
        let pid = std::process::id();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!("querymt-test-{pid}-{now_ms}-{suffix}"))
    }

    #[test]
    fn from_config_with_cache_path_creates_dir() {
        let cache_path = unique_tmp_path("cache-dir").join("nested");
        if cache_path.exists() {
            fs::remove_dir_all(&cache_path).unwrap();
        }

        let cfg = PluginConfig {
            providers: Vec::new(),
            oci: None,
        };

        let registry = PluginRegistry::from_config_with_cache_path(cfg, cache_path.clone())
            .expect("from_config_with_cache_path should succeed");

        assert!(cache_path.is_dir(), "cache dir should be created");
        assert_eq!(registry.cache_path, cache_path);
        assert!(registry.config.providers.is_empty());
    }

    #[test]
    fn from_path_parses_config_and_builds_registry() {
        let cfg_path = unique_tmp_path("providers").with_extension("toml");
        fs::write(&cfg_path, "providers = []\n").unwrap();

        let registry = PluginRegistry::from_path(&cfg_path).expect("from_path should succeed");

        assert!(registry.cache_path.ends_with("querymt"));
        assert!(
            registry.cache_path.is_dir(),
            "default cache dir should exist"
        );
        assert!(registry.config.providers.is_empty());
    }

    // ── Progress type re-export tests ─────────────────────────────────────────

    #[test]
    fn oci_progress_types_are_reexported() {
        // Compile-time check: these types must be accessible from the host module.
        let _phase: OciDownloadPhase = OciDownloadPhase::Downloading;
        let _progress = OciDownloadProgress {
            phase: OciDownloadPhase::Resolving,
            bytes_downloaded: 0,
            bytes_total: None,
            percent: None,
        };
        let _cb: OciProgressCallback = std::sync::Arc::new(|_| {});
    }

    // ── update_oci_plugins tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn update_oci_plugins_returns_empty_for_no_oci_providers() {
        let cache_path = unique_tmp_path("update-oci-empty");
        let cfg = config::PluginConfig {
            providers: Vec::new(),
            oci: None,
        };
        let registry =
            PluginRegistry::from_config_with_cache_path(cfg, cache_path).expect("registry");
        let results = registry.update_oci_plugins(None).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn update_oci_plugins_skips_non_oci_providers() {
        let cache_path = unique_tmp_path("update-oci-skip");
        let cfg = config::PluginConfig {
            providers: vec![config::ProviderConfig {
                name: "local-plugin".to_string(),
                path: "/some/local/plugin.wasm".to_string(),
                config: None,
            }],
            oci: None,
        };
        let registry =
            PluginRegistry::from_config_with_cache_path(cfg, cache_path).expect("registry");
        let results = registry.update_oci_plugins(None).await;
        // Local providers are skipped; only oci:// ones are processed.
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn update_oci_plugins_invokes_progress_callback_for_oci_providers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let cache_path = unique_tmp_path("update-oci-cb");
        // Use a non-resolvable fake OCI reference so pull fails quickly.
        let cfg = config::PluginConfig {
            providers: vec![config::ProviderConfig {
                name: "fake-plugin".to_string(),
                path: "oci://localhost:9999/fake/image:latest".to_string(),
                config: None,
            }],
            oci: None,
        };
        let registry =
            PluginRegistry::from_config_with_cache_path(cfg, cache_path).expect("registry");

        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        let cb: OciProgressCallback = Arc::new(move |_p| {
            cc.fetch_add(1, Ordering::SeqCst);
        });

        let results = registry.update_oci_plugins(Some(cb)).await;
        // Should have exactly one result (the fake provider).
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "fake-plugin");
        // The call must have failed (no real registry), but that's expected.
        assert!(results[0].1.is_err());
        // At minimum the Resolving callback was invoked.
        assert!(call_count.load(Ordering::SeqCst) >= 1);
    }
}
