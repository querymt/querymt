use crate::model_heuristics::ModelDefaults;
use crate::session::error::SessionError;
use querymt::LLMParams;
use querymt::error::LLMError;
use querymt::plugin::LLMProviderFactory;
use querymt::plugin::host::PluginRegistry;
use querymt::provider_config::{
    ConfigOverrideMode, ProviderConfigBuilder,
    ResolvedProviderConfig as SharedResolvedProviderConfig,
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) enum ProviderConfigMode<'a> {
    Runtime {
        model: &'a str,
        params: Option<&'a Value>,
        api_key_override: Option<&'a str>,
        session_id: &'a str,
    },
    CatalogListing,
}

pub(crate) struct ResolvedProviderConfig {
    #[cfg(feature = "oauth")]
    pub builder_config: Value,
    pub pruned_config_str: String,
    pub pruned_keys: Vec<String>,
    pub use_oauth_resolver: bool,
}

fn active_provider_matches(initial_config: &LLMParams, provider_name: &str) -> bool {
    initial_config.provider.as_deref() == Some(provider_name)
}

fn apply_initial_overrides(
    builder: &mut ProviderConfigBuilder,
    initial_config: &LLMParams,
    provider_name: &str,
) {
    if !active_provider_matches(initial_config, provider_name) {
        return;
    }

    if let Some(base_url) = &initial_config.base_url {
        builder.set("base_url", base_url.clone().into());
    } else if let Some(base_url) = initial_config
        .custom
        .as_ref()
        .and_then(|m| m.get("base_url"))
        .and_then(Value::as_str)
    {
        builder.set("base_url", base_url.to_string().into());
    }

    if let Some(model) = &initial_config.model {
        builder.set("model", model.clone().into());
    } else if let Some(model) = initial_config
        .custom
        .as_ref()
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        builder.set("model", model.to_string().into());
    }
}

fn api_key_from_config(cfg: &Value) -> Option<String> {
    cfg.get("api_key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn missing_credentials_error(provider_name: &str, env_var_name: Option<&str>) -> SessionError {
    let msg = if let Some(env_name) = env_var_name {
        format!(
            "No API key found for provider '{}'. Set {} or run 'qmt auth login {}'",
            provider_name, env_name, provider_name
        )
    } else {
        format!(
            "No credentials found for provider '{}'. Run 'qmt auth login {}'",
            provider_name, provider_name
        )
    };
    SessionError::ProviderError(LLMError::AuthError(msg))
}

pub(crate) async fn resolve_provider_config(
    registry: &PluginRegistry,
    initial_config: &LLMParams,
    factory: &Arc<dyn LLMProviderFactory>,
    provider_name: &str,
    mode: ProviderConfigMode<'_>,
) -> Result<ResolvedProviderConfig, SessionError> {
    let mut builder = ProviderConfigBuilder::from_registry_provider(registry, provider_name)?;
    let mut use_oauth_resolver = false;

    match mode {
        ProviderConfigMode::Runtime {
            model,
            params,
            api_key_override,
            session_id,
        } => {
            builder.set("model", model.into());

            if let Some(params_value) = params {
                builder.merge_value(params_value, ConfigOverrideMode::Overwrite);
            }

            let defaults = ModelDefaults::for_model(provider_name, model);
            defaults.apply_to(builder.value_mut(), session_id);

            if let Some(http_factory) = factory.as_http() {
                let env_var_name = http_factory.api_key_name();
                let api_key = if let Some(key) = api_key_override {
                    Some(key.to_string())
                } else if let Some(key) = api_key_from_config(builder.value()) {
                    log::debug!(
                        "Using inline API key from config for provider '{}'",
                        provider_name
                    );
                    Some(key)
                } else {
                    let preferred_method =
                        crate::session::provider::preferred_auth_method(provider_name);
                    log::debug!(
                        "Resolving API key for provider '{}' (preferred: {:?}, env_var: {:?})",
                        provider_name,
                        preferred_method,
                        env_var_name
                    );
                    crate::session::provider::resolve_api_key_with_preference(
                        provider_name,
                        env_var_name.as_deref(),
                        preferred_method.as_ref(),
                        &mut use_oauth_resolver,
                    )
                    .await
                };

                if let Some(key) = api_key {
                    builder.set("api_key", key.into());
                } else {
                    return Err(missing_credentials_error(
                        provider_name,
                        env_var_name.as_deref(),
                    ));
                }
            }
        }
        ProviderConfigMode::CatalogListing => {
            apply_initial_overrides(&mut builder, initial_config, provider_name);

            if let Some(http_factory) = factory.as_http()
                && api_key_from_config(builder.value()).is_none()
            {
                let preferred_method =
                    crate::session::provider::preferred_auth_method(provider_name);
                let env_var_name = http_factory.api_key_name();
                let api_key = crate::session::provider::resolve_api_key_with_preference(
                    provider_name,
                    env_var_name.as_deref(),
                    preferred_method.as_ref(),
                    &mut use_oauth_resolver,
                )
                .await;

                if let Some(key) = api_key {
                    builder.set("api_key", key.into());
                } else {
                    return Err(missing_credentials_error(
                        provider_name,
                        env_var_name.as_deref(),
                    ));
                }
            }

            if factory.as_http().is_none()
                && builder
                    .value()
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                return Err(SessionError::InvalidOperation(format!(
                    "No configured model was found for provider '{}'",
                    provider_name
                )));
            }
        }
    }

    let SharedResolvedProviderConfig {
        #[cfg(feature = "oauth")]
        full_config,
        pruned_config_str,
        pruned_keys,
        ..
    } = builder.prune_for_factory(factory.as_ref())?;

    Ok(ResolvedProviderConfig {
        #[cfg(feature = "oauth")]
        builder_config: full_config,
        pruned_config_str,
        pruned_keys,
        use_oauth_resolver,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::HTTPLLMProvider;
    use querymt::chat::{ChatMessage, ChatResponse, Tool};
    use querymt::completion::{CompletionRequest, CompletionResponse};
    use querymt::error::LLMError;
    use querymt::plugin::host::PluginRegistry;
    use querymt::plugin::{HTTPLLMProviderFactory, LLMProviderFactory};
    use std::sync::Arc;

    struct DummyHttpProvider;

    impl querymt::chat::http::HTTPChatProvider for DummyHttpProvider {
        fn chat_request(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<http::Request<Vec<u8>>, LLMError> {
            unimplemented!()
        }

        fn parse_chat(
            &self,
            _resp: http::Response<Vec<u8>>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            unimplemented!()
        }
    }

    impl querymt::completion::http::HTTPCompletionProvider for DummyHttpProvider {
        fn complete_request(
            &self,
            _req: &CompletionRequest,
        ) -> Result<http::Request<Vec<u8>>, LLMError> {
            unimplemented!()
        }

        fn parse_complete(
            &self,
            _resp: http::Response<Vec<u8>>,
        ) -> Result<CompletionResponse, LLMError> {
            unimplemented!()
        }
    }

    impl querymt::embedding::http::HTTPEmbeddingProvider for DummyHttpProvider {
        fn embed_request(&self, _inputs: &[String]) -> Result<http::Request<Vec<u8>>, LLMError> {
            unimplemented!()
        }

        fn parse_embed(&self, _resp: http::Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
            unimplemented!()
        }
    }

    impl HTTPLLMProvider for DummyHttpProvider {
        fn tools(&self) -> Option<&[Tool]> {
            None
        }
    }

    struct DummyFactory {
        api_key_name: Option<String>,
        schema: &'static str,
    }

    impl HTTPLLMProviderFactory for DummyFactory {
        fn name(&self) -> &str {
            "openai"
        }

        fn api_key_name(&self) -> Option<String> {
            self.api_key_name.clone()
        }

        fn config_schema(&self) -> String {
            self.schema.to_string()
        }

        fn list_models_request(&self, _cfg: &str) -> Result<http::Request<Vec<u8>>, LLMError> {
            unimplemented!()
        }

        fn parse_list_models(
            &self,
            _resp: http::Response<Vec<u8>>,
        ) -> Result<Vec<String>, LLMError> {
            unimplemented!()
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
            Ok(Box::new(DummyHttpProvider))
        }
    }

    fn registry_with_provider(config_body: &str) -> PluginRegistry {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("providers.toml");
        std::fs::write(&config_path, config_body).expect("write config");
        let registry = PluginRegistry::from_path(&config_path).expect("registry");
        std::mem::forget(temp);
        registry
    }

    fn adapted_factory(
        api_key_name: Option<&str>,
        schema: &'static str,
    ) -> Arc<dyn LLMProviderFactory> {
        Arc::new(querymt::plugin::adapters::HTTPFactoryAdapter::new(
            Arc::new(DummyFactory {
                api_key_name: api_key_name.map(str::to_string),
                schema,
            }),
        ))
    }

    #[tokio::test]
    async fn catalog_listing_merges_static_config_and_preserves_inline_api_key() {
        let registry = registry_with_provider(
            "[[providers]]\nname = \"xiaomi\"\npath = \"dummy\"\n\n[providers.config]\nbase_url = \"https://example.invalid/v1\"\napi_key = \"bogus\"\n",
        );
        let initial = LLMParams::default();
        let factory = adapted_factory(
            Some("OPENAI_API_KEY"),
            r#"{"properties":{"base_url":{},"api_key":{}}}"#,
        );

        let resolved = resolve_provider_config(
            &registry,
            &initial,
            &factory,
            "xiaomi",
            ProviderConfigMode::CatalogListing,
        )
        .await
        .expect("resolved config");

        #[cfg(feature = "oauth")]
        {
            assert_eq!(
                resolved.builder_config["base_url"],
                "https://example.invalid/v1"
            );
            assert_eq!(resolved.builder_config["api_key"], "bogus");
        }
        assert_eq!(resolved.pruned_keys, Vec::<String>::new());
    }

    #[tokio::test]
    async fn catalog_listing_applies_active_base_url_override() {
        let registry = registry_with_provider(
            "[[providers]]\nname = \"xiaomi\"\npath = \"dummy\"\n\n[providers.config]\nbase_url = \"https://example.invalid/v1\"\napi_key = \"bogus\"\n",
        );
        let initial = LLMParams::default()
            .provider("xiaomi")
            .base_url("https://override.invalid/v1");
        let factory = adapted_factory(
            Some("OPENAI_API_KEY"),
            r#"{"properties":{"base_url":{},"api_key":{}}}"#,
        );

        #[cfg(feature = "oauth")]
        {
            let resolved = resolve_provider_config(
                &registry,
                &initial,
                &factory,
                "xiaomi",
                ProviderConfigMode::CatalogListing,
            )
            .await
            .expect("resolved config");

            assert_eq!(
                resolved.builder_config["base_url"],
                "https://override.invalid/v1"
            );
        }

        #[cfg(not(feature = "oauth"))]
        {
            let _ = resolve_provider_config(
                &registry,
                &initial,
                &factory,
                "xiaomi",
                ProviderConfigMode::CatalogListing,
            )
            .await
            .expect("resolved config");
        }
    }

    #[tokio::test]
    async fn runtime_mode_uses_api_key_override_before_static_key() {
        let registry = registry_with_provider(
            "[[providers]]\nname = \"xiaomi\"\npath = \"dummy\"\n\n[providers.config]\napi_key = \"bogus\"\n",
        );
        let initial = LLMParams::default();
        let factory = adapted_factory(
            Some("OPENAI_API_KEY"),
            r#"{"properties":{"api_key":{},"model":{}}}"#,
        );

        let resolved = resolve_provider_config(
            &registry,
            &initial,
            &factory,
            "xiaomi",
            ProviderConfigMode::Runtime {
                model: "gpt-4o-mini",
                params: None,
                api_key_override: Some("override-key"),
                session_id: "session-1",
            },
        )
        .await
        .expect("resolved config");

        #[cfg(feature = "oauth")]
        {
            assert_eq!(resolved.builder_config["api_key"], "override-key");
            assert_eq!(resolved.builder_config["model"], "gpt-4o-mini");
        }
        assert!(!resolved.use_oauth_resolver);
    }
}
