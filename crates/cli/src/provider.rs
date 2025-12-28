use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::native::NativeLoader, host::PluginRegistry,
};
use std::path::PathBuf;

use crate::cli_args::CliArgs;
use crate::secret_store::SecretStore;
use crate::utils::find_config_in_home;
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
    if let Some(ref s) = args.backend {
        return Some(split_provider(s));
    }
    if let Ok(store) = SecretStore::new() {
        if let Some(default) = store.get_default_provider() {
            return Some(split_provider(default));
        }
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
    let cfg_file = if let Some(cfg) = &args.provider_config {
        PathBuf::from(cfg)
    } else {
        find_config_in_home(&["providers.json", "providers.toml", "providers.yaml"]).map_err(
            |_| {
                LLMError::InvalidRequest(
                    "Config file for providers is missing. Please provide one!".to_string(),
                )
            },
        )?
    };

    let mut registry = PluginRegistry::from_path(cfg_file)?;

    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));
    registry.load_all_plugins().await;

    Ok(registry)
}
