use dirs;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::native::NativeLoader, host::PluginRegistry,
};
use std::path::PathBuf;

use crate::cli_args::CliArgs;
use crate::secret_store::SecretStore;
use querymt::error::LLMError;

/// Splits "provider:model" or just "provider" into (provider, Option<model>)
pub fn split_provider(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((p, m)) => (p.to_string(), Some(m.to_string())),
        None => (s.to_string(), None),
    }
}

/// Retrieves provider and model information from CLI args or default store
pub fn get_provider_info(args: &CliArgs) -> Option<(String, Option<String>)> {
    if let Ok(store) = SecretStore::new() {
        if let Some(default) = store.get_default_provider() {
            return Some(split_provider(default));
        }
    }
    if let Some(ref s) = args.backend {
        return Some(split_provider(s));
    }
    None
}

/// Try to resolve an API key from CLI args, secret store, or environment
pub fn get_api_key(provider: &str, args: &CliArgs, registry: &PluginRegistry) -> Option<String> {
    args.api_key.clone().or_else(|| {
        registry.get(provider).and_then(|factory| {
            factory.as_http()?.api_key_name().and_then(|name| {
                SecretStore::new()
                    .ok()
                    .and_then(|store| store.get(&name).cloned())
                    .or_else(|| std::env::var(name).ok())
            })
        })
    })
}

/// Initializes provider registry (from config path or default ~/.qmt)
pub async fn get_provider_registry(args: &CliArgs) -> Result<PluginRegistry, LLMError> {
    // Determine config file path
    let mut registry = if let Some(cfg) = &args.provider_config {
        PluginRegistry::from_path(cfg)?
    } else {
        let mut config_file: Option<PathBuf> = None;
        if let Some(home) = dirs::home_dir() {
            let config_dir = home.join(".qmt");
            if config_dir.exists() {
                for name in &["providers.json", "providers.toml", "providers.yaml"] {
                    let candidate = config_dir.join(name);
                    if candidate.is_file() {
                        config_file = Some(candidate);
                        break;
                    }
                }
            }
        }
        let cfg_file = config_file.ok_or_else(|| {
            LLMError::InvalidRequest(
                "Config file for providers is missing. Please provide one!".to_string(),
            )
        })?;
        PluginRegistry::from_path(cfg_file)?
    };

    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));
    registry.load_all_plugins().await;

    Ok(registry)
}
