use querymt::builder::LLMBuilder;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};

use crate::cli_args::CliArgs;
use crate::secret_store::SecretStore;
use querymt::error::LLMError;
use serde_json::Value;

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

    // 3. Fall back to API key from secret store or environment
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

    let mut registry = PluginRegistry::from_path(cfg_path)?;
    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));

    Ok(registry)
}

/// Applies provider-specific config from providers.toml to the builder.
pub fn apply_provider_config(
    mut builder: LLMBuilder,
    registry: &PluginRegistry,
    provider: &str,
) -> Result<LLMBuilder, LLMError> {
    let provider_cfg = registry
        .config
        .providers
        .iter()
        .find(|cfg| cfg.name == provider);
    let Some(provider_cfg) = provider_cfg else {
        return Ok(builder);
    };
    let Some(config) = &provider_cfg.config else {
        return Ok(builder);
    };

    for (key, value) in config {
        let json: Value = serde_json::to_value(value)?;
        builder = builder.parameter(key.clone(), json);
    }

    Ok(builder)
}
