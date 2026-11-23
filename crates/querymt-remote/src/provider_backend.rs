use crate::peer_access::quota::QuotaEstimate;
use async_trait::async_trait;
use querymt::LLMProvider;
use querymt::chat::{ChatCall, estimate_chat_tokens};
use querymt::providers::{ModelInfo, providers_registry};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ProviderBuildRequest {
    pub provider_name: String,
    pub model: String,
    pub params: Option<Value>,
}

impl ProviderBuildRequest {
    pub fn new(provider_name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider_name: provider_name.into(),
            model: model.into(),
            params: None,
        }
    }

    pub fn with_params(mut self, params: Option<Value>) -> Self {
        self.params = params;
        self
    }
}

#[async_trait]
pub trait RemoteProviderBackend: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Optional host-side defaults that should be merged with request params
    /// before constructing a provider.
    fn host_default_params(&self, _provider_name: &str, _model: &str) -> Option<Value> {
        None
    }

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error>;

    /// Prepare the binding and estimate the request without dispatching it.
    async fn prepare_chat(
        &self,
        request: ProviderBuildRequest,
        call: &ChatCall<'_>,
    ) -> Result<(Arc<dyn LLMProvider>, QuotaEstimate), Self::Error> {
        let param = |key: &str| request.params.as_ref()?.get(key)?.as_u64();
        let configured_cap = param("max_tokens").and_then(|cap| u32::try_from(cap).ok());
        let system_tokens = configured_system_tokens(request.params.as_ref());
        let n_ctx = matches!(request.provider_name.as_str(), "llama_cpp" | "llama-cpp")
            .then(|| param("n_ctx"))
            .flatten();
        let provider_name = request.provider_name.clone();
        let model = request.model.clone();
        let provider = self.build_provider(request).await?;
        let output_cap = provider
            .capabilities()
            .options
            .retain_supported(call.options)
            .max_tokens
            .or(configured_cap);
        let estimate = QuotaEstimate::for_chat(
            &provider_name,
            &model,
            call,
            output_cap,
            n_ctx,
            None,
            system_tokens,
        );
        Ok((provider, estimate))
    }

    /// Resolve facts before the first chat request. Backends with generation
    /// defaults override this to report the settings of the provider they build.
    async fn contract_info(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<crate::ProviderContractInfo, Self::Error> {
        let provider = self.build_provider(request).await?;
        Ok(crate::ProviderContractInfo {
            item_aware_chat_version: Some(crate::ITEM_AWARE_CHAT_CONTRACT_VERSION),
            protocol_version: Some(crate::provider_protocol::MESH_PROTOCOL_VERSION),
            capabilities: provider.capabilities(),
            generation: Default::default(),
        })
    }
}

/// Estimate system text supplied separately in the effective provider configuration.
pub fn configured_system_tokens(params: Option<&Value>) -> usize {
    let Some(system) = params.and_then(|params| params.get("system")) else {
        return 0;
    };
    let text_tokens = |part: &Value| {
        part.as_str()
            .or_else(|| part.get("text").and_then(Value::as_str))
            .map_or(0, |text| text.len().div_ceil(4))
    };
    match system {
        Value::Array(parts) => parts.iter().map(text_tokens).sum(),
        part => text_tokens(part),
    }
}

impl QuotaEstimate {
    /// Estimate chat usage for a host-resolved binding, not as a hard bound.
    pub fn for_chat(
        provider: &str,
        model: &str,
        call: &ChatCall<'_>,
        output_cap: Option<u32>,
        n_ctx: Option<u64>,
        profile_fallback: Option<usize>,
        system_tokens: usize,
    ) -> Self {
        let registry = providers_registry().ok();
        let model = registry.as_ref().and_then(|r| r.get_model(provider, model));
        Self::from_call(
            call,
            model,
            output_cap,
            n_ctx,
            profile_fallback,
            system_tokens,
        )
    }

    fn from_call(
        call: &ChatCall<'_>,
        model: Option<&ModelInfo>,
        output_cap: Option<u32>,
        n_ctx: Option<u64>,
        profile_fallback: Option<usize>,
        system_tokens: usize,
    ) -> Self {
        // Attachments are charged through reported usage, not a separate allowance.
        let prompt = estimate_chat_tokens(call.messages, |_| 0).saturating_add(system_tokens);
        let tools = call.tools.and_then(|tools| serde_json::to_vec(tools).ok());
        let prompt = prompt.saturating_add(tools.map_or(0, |json| json.len().div_ceil(4)));
        let prompt_tokens = i64::try_from(prompt).unwrap_or(i64::MAX);
        let output = output_cap
            .map(u64::from)
            .or_else(|| model.and_then(ModelInfo::output_limit))
            .unwrap_or(32_000);
        let ceiling = n_ctx.filter(|n| *n > 0).or_else(|| match model {
            Some(model) => model.context_limit(),
            // models.dev has no entry, so the profile fallback is the only configured
            // context size we have. ContextMiddleware uses it for the same case.
            None => profile_fallback.and_then(|n| u64::try_from(n).ok()),
        });
        let total = prompt_tokens.saturating_add(i64::try_from(output).unwrap_or(i64::MAX));
        Self {
            reserved_tokens: ceiling
                .and_then(|cap| i64::try_from(cap).ok())
                .map_or(total, |cap| total.min(cap))
                .max(1),
            prompt_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{ChatCall, ChatOutput, ChatProvider};
    use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;
    use serde_json::json;

    #[derive(Debug, thiserror::Error)]
    #[error("backend error")]
    struct DummyError;

    struct DummyProvider;

    #[async_trait]
    impl ChatProvider for DummyProvider {
        async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    #[async_trait]
    impl CompletionProvider for DummyProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    #[async_trait]
    impl EmbeddingProvider for DummyProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }
    }

    impl querymt::LLMProvider for DummyProvider {}

    struct DummyBackend;

    #[async_trait]
    impl RemoteProviderBackend for DummyBackend {
        type Error = DummyError;

        fn host_default_params(&self, provider_name: &str, model: &str) -> Option<Value> {
            Some(json!({"provider": provider_name, "model": model, "temperature": 0.4}))
        }

        async fn build_provider(
            &self,
            request: ProviderBuildRequest,
        ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
            assert_eq!(request.provider_name, "demo");
            assert_eq!(request.model, "m1");
            assert_eq!(
                self.host_default_params(&request.provider_name, &request.model),
                Some(json!({"provider": "demo", "model": "m1", "temperature": 0.4}))
            );
            Ok(Arc::new(DummyProvider))
        }
    }

    #[test]
    fn reservation_uses_model_limits_and_the_effective_output_cap() {
        use querymt::chat::ChatMessage;
        let model: ModelInfo = serde_json::from_value(json!({
            "id": "model", "name": "Model", "limit": {"context": 60, "output": 40}
        }))
        .unwrap();
        let messages = [ChatMessage::user().text("abcdefgh").build()];
        let call = ChatCall::new(&messages);
        let estimate = |call: &ChatCall<'_>, cap, n_ctx| {
            QuotaEstimate::from_call(call, Some(&model), cap, n_ctx, Some(3), 0)
        };
        assert_eq!(estimate(&call, None, None).prompt_tokens, 6);
        assert_eq!(estimate(&call, None, None).reserved_tokens, 46);
        assert_eq!(estimate(&call, Some(100), None).reserved_tokens, 60);
        assert_eq!(estimate(&call, Some(100), Some(50)).reserved_tokens, 50);
        assert_eq!(estimate(&call, Some(20), None).reserved_tokens, 26);
    }

    #[test]
    fn reservation_uses_profile_fallback_only_for_a_model_missing_from_models_dev() {
        let call = ChatCall::new(&[]);
        assert_eq!(
            QuotaEstimate::from_call(&call, None, None, None, Some(500), 0).reserved_tokens,
            500
        );
        assert_eq!(
            QuotaEstimate::from_call(&call, None, None, None, None, 0).reserved_tokens,
            32_000
        );
        let model: ModelInfo =
            serde_json::from_value(json!({"id": "model", "name": "Model"})).unwrap();
        assert_eq!(
            QuotaEstimate::from_call(&call, Some(&model), None, None, Some(500), 0).reserved_tokens,
            32_000
        );
    }

    #[tokio::test]
    async fn standalone_backend_counts_configured_system_and_ignores_an_unsupported_cap() {
        use querymt::chat::{ChatMessage, GenerationOptions};
        let messages = [ChatMessage::user().text("abcdefgh").build()];
        let call = ChatCall::new(&messages).options(GenerationOptions::NONE.with_max_tokens(1));
        for (system, prompt_tokens) in [
            (json!("Be brief"), 8),
            (json!(["Be brief"]), 8),
            (
                json!([{"text": "Be brief", "cache": {"ephemeral": {"ttl_seconds": 1800}}}, "Use Markdown"]),
                11,
            ),
        ] {
            let request = ProviderBuildRequest::new("demo", "m1")
                .with_params(Some(json!({"max_tokens": 128, "system": system})));
            let (_, estimate) = DummyBackend.prepare_chat(request, &call).await.unwrap();
            assert_eq!(estimate.prompt_tokens, prompt_tokens);
            assert_eq!(estimate.reserved_tokens, 128 + prompt_tokens);
        }
    }

    #[test]
    fn provider_build_request_attaches_params() {
        let request =
            ProviderBuildRequest::new("demo", "m1").with_params(Some(json!({"temperature": 0.2})));

        assert_eq!(request.provider_name, "demo");
        assert_eq!(request.model, "m1");
        assert_eq!(request.params, Some(json!({"temperature": 0.2})));
    }

    #[test]
    fn backend_default_host_params_are_available() {
        let backend = DummyBackend;
        assert_eq!(
            backend.host_default_params("demo", "m1"),
            Some(json!({"provider": "demo", "model": "m1", "temperature": 0.4}))
        );
    }
}
