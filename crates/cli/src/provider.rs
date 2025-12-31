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
            return Some(split_provider(&default));
        }
    }
    None
}

/// Try to resolve an API key from CLI args, OAuth tokens, secret store, or environment
///
/// Priority order:
/// 1. CLI args (--api-key)
/// 2. OAuth tokens (if provider supports OAuth and tokens are valid)
/// 3. Secret store (API key)
/// 4. Environment variable
pub async fn get_api_key(
    provider: &str,
    args: &CliArgs,
    registry: &PluginRegistry,
) -> Option<String> {
    // 1. Check CLI args first (highest priority)
    if let Some(key) = &args.api_key {
        return Some(key.clone());
    }

    // 2. Check for OAuth tokens (prefer OAuth over API keys)
    if let Ok(oauth_provider) = crate::auth::get_oauth_provider(provider, None) {
        if let Ok(mut store) = SecretStore::new() {
            // Try to get a valid OAuth token (will refresh if needed)
            if let Ok(token) =
                crate::auth::get_valid_token(oauth_provider.as_ref(), &mut store).await
            {
                log::debug!("Using OAuth token for {}", provider);
                return Some(token);
            }
        }
    }

    // 3. Fall back to API key from secret store or environment
    registry.get(provider).and_then(|factory| {
        factory.as_http()?.api_key_name().and_then(|name| {
            SecretStore::new()
                .ok()
                .and_then(|store| store.get(&name))
                .or_else(|| std::env::var(name).ok())
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
