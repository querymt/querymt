use crate::model_heuristics::{ModelDefaults, OutputCaps};
use crate::session::error::SessionError;
use querymt::LLMParams;
use querymt::error::LLMError;
use querymt::plugin::host::ProviderBinding;
use querymt::provider_config::{
    ConfigOverrideMode, ProviderConfigBuilder,
    ResolvedProviderConfig as SharedResolvedProviderConfig,
};
use serde_json::Value;

pub(crate) enum ProviderConfigMode<'a> {
    Runtime {
        model: &'a str,
        params: Option<&'a Value>,
        api_key_override: Option<&'a str>,
        session_id: &'a str,
    },
    /// Non-chat calls share credentials but must not inherit chat model defaults.
    Speech {
        model: &'a str,
        params: Option<&'a Value>,
        api_key_override: Option<&'a str>,
    },
    CatalogListing,
}

pub(crate) struct ResolvedProviderConfig {
    #[cfg(feature = "oauth")]
    pub builder_config: Value,
    pub pruned_config_str: String,
    pub generation_settings: crate::agent::utils::genai::GenerationSettings,
    /// Initial output cap and escalation ceiling; `None` when no cap is known.
    pub output_caps: Option<OutputCaps>,
    /// Reasoning effort left in the final (pruned) config, when the provider takes one.
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    pub pruned_keys: Vec<String>,
    pub use_oauth_resolver: bool,
}

impl ResolvedProviderConfig {
    pub(crate) fn build_provider(
        &self,
        binding: &ProviderBinding,
    ) -> Result<std::sync::Arc<dyn querymt::LLMProvider>, LLMError> {
        let factory = &binding.factory;
        if !self.pruned_keys.is_empty() {
            const MAX_KEYS_TO_LOG: usize = 50;
            let shown = self.pruned_keys.len().min(MAX_KEYS_TO_LOG);
            let suffix = if self.pruned_keys.len() > shown {
                format!(" (+{} more)", self.pruned_keys.len() - shown)
            } else {
                String::new()
            };

            log::warn!(
                "Pruned unsupported config keys for provider '{}': {}{}",
                binding.logical_name,
                self.pruned_keys[..shown].join(", "),
                suffix
            );
        }
        let _use_oauth_resolver = self.use_oauth_resolver;

        // If OAuth was used, attach a resolver so expired tokens are refreshed
        // transparently on each request.
        //
        // We try the generic LLMProviderFactory path first: Extism providers
        // handle HTTP internally and accept set_key_resolver directly on the
        // LLMProvider. For native HTTP providers the generic path wraps the
        // inner HTTP provider in Arc before we can set the resolver, so we
        // fall back to the HTTPLLMProviderFactory path to set it on the inner
        // provider before wrapping.
        #[cfg(feature = "oauth")]
        if _use_oauth_resolver {
            let initial_key = self
                .builder_config
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("");
            let resolver = std::sync::Arc::new(crate::auth::OAuthKeyResolver::new(
                &binding.logical_name,
                initial_key,
            ));
            // Generic path — works for Extism providers that implement set_key_resolver.
            let mut provider = factory.from_config(&self.pruned_config_str)?;
            provider.set_key_resolver(resolver.clone());
            if provider.key_resolver().is_some() {
                return Ok(std::sync::Arc::from(provider));
            }
            // Fallback for native HTTP providers: set the resolver on the inner
            // HTTPLLMProvider before it gets wrapped in Arc by the adapter.
            if let Some(http_factory) = factory.as_http() {
                let mut inner = http_factory.from_config(&self.pruned_config_str)?;
                inner.set_key_resolver(resolver);
                return Ok(std::sync::Arc::new(
                    querymt::adapters::LLMProviderFromHTTP::new(inner),
                ));
            }
            // Neither path attached a resolver — return the provider as-is.
            log::warn!(
                "OAuthKeyResolver could not be attached for provider '{}'",
                binding.logical_name
            );
            return Ok(std::sync::Arc::from(provider));
        }
        Ok(std::sync::Arc::from(
            factory.from_config(&self.pruned_config_str)?,
        ))
    }
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
    binding: &ProviderBinding,
    initial_config: &LLMParams,
    mode: ProviderConfigMode<'_>,
) -> Result<ResolvedProviderConfig, SessionError> {
    let mut builder = ProviderConfigBuilder::from_binding(binding);
    let factory = &binding.factory;
    let provider_name = binding.logical_name.as_str();
    let mut use_oauth_resolver = false;
    let mut output_caps = None;

    match mode {
        ProviderConfigMode::Runtime {
            model,
            params,
            api_key_override,
            ..
        }
        | ProviderConfigMode::Speech {
            model,
            params,
            api_key_override,
        } => {
            builder.set("model", model.into());

            if let Some(params_value) = params {
                builder.merge_value(params_value, ConfigOverrideMode::Overwrite);
            }

            if let ProviderConfigMode::Runtime { session_id, .. } = mode {
                // Only parse the models.dev cache when it can change the caps.
                let model_info = ModelDefaults::wants_model_info(provider_name, builder.value())
                    .then(|| crate::model_info::get_model_info(provider_name, model))
                    .flatten();
                let defaults = ModelDefaults::for_model(provider_name, model, model_info.as_ref());
                // Resolve caps before applying defaults so an explicit `max_tokens`
                // is recognised as its own ceiling.
                output_caps = defaults.output_caps(builder.value());
                defaults.apply_to(builder.value_mut(), session_id);
            }

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
                        crate::session::provider::preferred_auth_method(provider_name).await;
                    log::debug!(
                        "Resolving API key for provider '{}' (preferred: {:?}, env_var: {:?})",
                        provider_name,
                        preferred_method.as_ref().ok().and_then(Option::as_ref),
                        env_var_name
                    );
                    crate::session::provider::resolve_api_key_with_preference(
                        provider_name,
                        env_var_name.as_deref(),
                        preferred_method,
                        &mut use_oauth_resolver,
                    )
                    .await?
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
                    crate::session::provider::preferred_auth_method(provider_name).await;
                let env_var_name = http_factory.api_key_name();
                let api_key = crate::session::provider::resolve_api_key_with_preference(
                    provider_name,
                    env_var_name.as_deref(),
                    preferred_method,
                    &mut use_oauth_resolver,
                )
                .await?;

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
        pruned_config,
        pruned_config_str,
        pruned_keys,
        ..
    } = builder.prune_for_factory(factory.as_ref())?;

    // Trust implementation identity, not a logical alias or a custom factory's name.
    // Derive only scalar metadata while the final pruned Value is already available.
    let generation_settings = binding
        .implementation_id
        .as_deref()
        .and_then(|id| id.strip_prefix("oci://ghcr.io/querymt/"))
        .filter(|id| {
            id.strip_prefix(factory.name()).is_some_and(|suffix| {
                suffix.starts_with(':') || suffix.starts_with('@') || suffix.is_empty()
            })
        })
        .map(|_| {
            crate::agent::utils::genai::GenerationSettings::from_config(
                factory.name(),
                &pruned_config,
            )
        })
        .unwrap_or_default();

    let reasoning_effort = pruned_config
        .get("reasoning_effort")
        .and_then(|value| serde_json::from_value(value.clone()).ok());

    Ok(ResolvedProviderConfig {
        #[cfg(feature = "oauth")]
        builder_config: full_config,
        pruned_config_str,
        generation_settings,
        output_caps,
        reasoning_effort,
        pruned_keys,
        use_oauth_resolver,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::HTTPLLMProvider;
    use querymt::chat::{ChatOutput, Tool};
    use querymt::completion::{CompletionRequest, CompletionResponse};
    use querymt::error::LLMError;
    use querymt::plugin::host::{PluginRegistry, ProviderBinding};
    use querymt::plugin::{HTTPLLMProviderFactory, LLMProviderFactory};
    #[cfg(test)]
    use std::sync::Arc;

    #[derive(Default)]
    struct DummyHttpProvider {
        resolver: Option<Arc<dyn querymt::auth::ApiKeyResolver>>,
    }

    impl querymt::chat::http::HTTPChatProvider for DummyHttpProvider {
        fn prepare_chat_request(
            &self,
            _call: &querymt::chat::ChatCall<'_>,
            _stream: bool,
        ) -> Result<querymt::chat::http::PreparedChatRequest, LLMError> {
            unimplemented!()
        }

        fn parse_chat(
            &self,
            _resp: http::Response<Vec<u8>>,
            _context: serde_json::Value,
        ) -> Result<ChatOutput, LLMError> {
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
        fn key_resolver(&self) -> Option<&Arc<dyn querymt::auth::ApiKeyResolver>> {
            self.resolver.as_ref()
        }
        fn set_key_resolver(&mut self, resolver: Arc<dyn querymt::auth::ApiKeyResolver>) {
            self.resolver = Some(resolver);
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
            Ok(Box::new(DummyHttpProvider::default()))
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

    fn binding(registry: &PluginRegistry, factory: Arc<dyn LLMProviderFactory>) -> ProviderBinding {
        ProviderBinding {
            logical_name: "xiaomi".to_string(),
            factory,
            static_config: querymt::provider_config::provider_static_config_json(
                registry, "xiaomi",
            )
            .expect("static config"),
            implementation_id: None,
        }
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

        let binding = binding(&registry, factory.clone());
        let resolved =
            resolve_provider_config(&binding, &initial, ProviderConfigMode::CatalogListing)
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
            let binding = binding(&registry, factory.clone());
            let resolved =
                resolve_provider_config(&binding, &initial, ProviderConfigMode::CatalogListing)
                    .await
                    .expect("resolved config");

            assert_eq!(
                resolved.builder_config["base_url"],
                "https://override.invalid/v1"
            );
        }

        #[cfg(not(feature = "oauth"))]
        {
            let binding = binding(&registry, factory.clone());
            let _ = resolve_provider_config(&binding, &initial, ProviderConfigMode::CatalogListing)
                .await
                .expect("resolved config");
        }
    }

    #[tokio::test]
    async fn genai_settings_follow_binding_session_defaults_and_schema_pruning() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use opentelemetry::Value;
        let registry = registry_with_provider("providers = []");
        let factory = adapted_factory(
            Some("UNUSED_TEST_KEY"),
            r#"{"type":"object","properties":{"model":{"type":"string"},"api_key":{"type":"string"},"max_tokens":{"type":"integer"},"temperature":{"type":"number"},"reasoning_effort":{"type":"string"}},"additionalProperties":false}"#,
        );
        for binding_max in [None, Some(1234)] {
            let mut binding = binding(&registry, factory.clone());
            binding.logical_name = "anthropic".into();
            binding.implementation_id = Some("oci://ghcr.io/querymt/openai:latest".into());
            binding.static_config =
                serde_json::json!({"api_key":"SECRET_ARGUMENT", "temperature":0.25});
            if let Some(max) = binding_max {
                binding.static_config["max_tokens"] = max.into();
            }
            let params =
                serde_json::json!({"temperature":0.5, "top_p":0.75, "reasoning_effort":"high"});
            let resolved = resolve_provider_config(
                &binding,
                &LLMParams::new(),
                ProviderConfigMode::Runtime {
                    model: "claude-test",
                    params: Some(&params),
                    api_key_override: None,
                    session_id: "test",
                },
            )
            .await
            .unwrap();
            let config: serde_json::Value =
                serde_json::from_str(&resolved.pruned_config_str).unwrap();
            assert_eq!(config["temperature"], 0.5);
            assert!(config.get("top_p").is_none());
            let max = binding_max.unwrap_or(32_000);
            assert_eq!(config["max_tokens"], max);
            // Unknown model: 32k default for both; explicit config is its own ceiling.
            let expected = u32::try_from(max).unwrap();
            assert_eq!(
                resolved.output_caps,
                Some(OutputCaps {
                    initial: expected,
                    ceiling: expected,
                })
            );
            let (_, spans) = capture(async {
                let span = tracing::info_span!("resolved-settings");
                resolved.generation_settings.record(&span);
            })
            .await;
            assert_eq!(
                attr(&spans[0], "gen_ai.request.max_tokens"),
                Some(&Value::I64(max))
            );
            assert_eq!(
                attr(&spans[0], "gen_ai.request.temperature"),
                Some(&Value::F64(0.5))
            );
            assert!(attr(&spans[0], "gen_ai.request.top_p").is_none());
            assert_eq!(
                attr(&spans[0], "gen_ai.request.reasoning.level"),
                Some(&Value::from("high"))
            );
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_reasoning_effort_is_omitted_after_schema_pruning() {
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        let registry = registry_with_provider("providers = []");
        let factory = adapted_factory(None, r#"{"properties":{"model":{},"api_key":{}}}"#);
        let mut binding = binding(&registry, factory);
        binding.implementation_id = Some("oci://ghcr.io/querymt/openai:latest".into());
        binding.static_config =
            serde_json::json!({"api_key":"SECRET_ARGUMENT", "reasoning_effort":"high"});
        let resolved = resolve_provider_config(
            &binding,
            &LLMParams::new(),
            ProviderConfigMode::Runtime {
                model: "test",
                params: None,
                api_key_override: None,
                session_id: "test",
            },
        )
        .await
        .unwrap();
        let config: serde_json::Value = serde_json::from_str(&resolved.pruned_config_str).unwrap();
        assert!(config.get("reasoning_effort").is_none());
        // No max_tokens source for a non-Anthropic provider: nothing to escalate.
        assert_eq!(resolved.output_caps, None);
        let (_, spans) = capture(async {
            let span = tracing::info_span!("resolved-settings");
            resolved.generation_settings.record(&span);
        })
        .await;
        assert!(attr(&spans[0], "gen_ai.request.reasoning.level").is_none());
        assert_private(&spans);
    }

    #[tokio::test]
    async fn speech_mode_preserves_overrides_without_chat_defaults() {
        let registry = registry_with_provider(
            "[[providers]]\nname='xiaomi'\npath='dummy'\n[providers.config]\napi_key='static'\nbase_url='https://static.invalid/'\n",
        );
        let factory = adapted_factory(
            Some("UNUSED_SPEECH_KEY"),
            r#"{"properties":{"api_key":{},"model":{},"base_url":{},"max_tokens":{}}}"#,
        );
        let binding = binding(&registry, factory);
        let params = serde_json::json!({"base_url":"https://speech.invalid/"});
        let resolved = resolve_provider_config(
            &binding,
            &LLMParams::default(),
            ProviderConfigMode::Speech {
                model: "tts-1",
                params: Some(&params),
                api_key_override: Some("override"),
            },
        )
        .await
        .unwrap();
        let config: Value = serde_json::from_str(&resolved.pruned_config_str).unwrap();
        assert_eq!(config["model"], "tts-1");
        assert_eq!(config["api_key"], "override");
        assert_eq!(config["base_url"], "https://speech.invalid/");
        assert!(config.get("max_tokens").is_none());
        assert!(resolved.output_caps.is_none());
    }

    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn speech_provider_attaches_oauth_refresh_when_supported() {
        let binding = ProviderBinding {
            logical_name: "openai".into(),
            factory: adapted_factory(
                Some("UNUSED_SPEECH_KEY"),
                r#"{"properties":{"api_key":{},"model":{}}}"#,
            ),
            static_config: serde_json::json!({"api_key":"test"}),
            implementation_id: None,
        };
        let mut resolved = resolve_provider_config(
            &binding,
            &LLMParams::default(),
            ProviderConfigMode::Speech {
                model: "tts-1",
                params: None,
                api_key_override: None,
            },
        )
        .await
        .unwrap();
        resolved.use_oauth_resolver = true;
        let provider = resolved.build_provider(&binding).unwrap();
        assert_eq!(
            provider.key_resolver().expect("refresh resolver").current(),
            "test"
        );
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

        let binding = binding(&registry, factory);
        let resolved = resolve_provider_config(
            &binding,
            &initial,
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
