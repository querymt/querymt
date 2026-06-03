use async_trait::async_trait;
use querymt::LLMProvider;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, Tool};
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
        fn supports_streaming(&self) -> bool {
            false
        }

        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            Err(LLMError::NotImplemented("dummy".into()))
        }

        async fn chat_stream_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures_util::Stream<Item = Result<querymt::chat::StreamChunk, LLMError>>
                        + Send,
                >,
            >,
            LLMError,
        > {
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
