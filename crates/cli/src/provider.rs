use querymt::dynamic::PluginRegistryDynamicExt;
use querymt::plugin::host::PluginRegistry;
use querymt::provider_config::provider_static_config_json;

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
    if let Some(ref s) = args.backend {
        return Some(split_provider(s));
    }
    if let Ok(store) = SecretStore::new()
        && let Some(default) = store.get_default_provider()
    {
        return Some(split_provider(&default));
    }
    None
}

fn provider_has_explicit_api_key(registry: &PluginRegistry, provider: &str) -> bool {
    provider_static_config_json(registry, provider)
        .ok()
        .and_then(|cfg| {
            cfg.get("api_key")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .map(str::to_string)
        })
        .is_some_and(|s| !s.is_empty())
}

/// Try to resolve an API key from CLI args, OAuth tokens, secret store, or environment
///
/// Priority order:
/// 1. CLI args (--api-key)
/// 2. providers.toml [providers.config].api_key (if explicitly set)
/// 3. OAuth tokens (if provider supports OAuth and tokens are valid)
/// 4. Secret store (API key)
/// 5. Environment variable
pub async fn get_api_key(
    provider: &str,
    args: &CliArgs,
    registry: &PluginRegistry,
) -> Option<String> {
    // 1. Check CLI args first (highest priority)
    if let Some(key) = &args.api_key {
        return Some(key.clone());
    }

    // 2. Respect provider-specific static configuration from providers.toml.
    // If an API key is explicitly set there, do not override it from OAuth/secret/env.
    if provider_has_explicit_api_key(registry, provider) {
        log::debug!(
            "Provider '{}' has explicit api_key in providers config; skipping external key fallback",
            provider
        );
        return None;
    }

    // 3. Check for OAuth tokens (prefer OAuth over API keys when no explicit config key exists)
    if let Ok(oauth_provider) = querymt_utils::oauth::get_oauth_provider(provider, None)
        && let Ok(mut store) = SecretStore::new()
    {
        // Try to get a valid OAuth token (will refresh if needed)
        if let Ok(token) =
            querymt_utils::oauth::get_valid_token(oauth_provider.as_ref(), &mut store).await
        {
            log::debug!("Using OAuth token for {}", provider);
            return Some(token);
        }
    }

    // 4. Fall back to API key from secret store or environment
    if let Some(factory) = registry.get(provider).await
        && let Some(http_factory) = factory.as_http()
        && let Some(name) = http_factory.api_key_name()
    {
        return SecretStore::new()
            .ok()
            .and_then(|store| store.get(&name))
            .or_else(|| std::env::var(name).ok());
    }
    None
}

/// Initializes provider registry WITHOUT loading plugins (lazy loading)
pub async fn get_provider_registry(args: &CliArgs) -> Result<PluginRegistry, LLMError> {
    let cfg_path = querymt_utils::providers::get_providers_config(args.provider_config.clone())
        .await
        .map_err(|e| LLMError::GenericError(format!("{:?}", e)))?;

    Ok(PluginRegistry::from_path(cfg_path)?.with_dynamic_loaders())
}
