use crate::{
    chat::{BasicChatProvider, ChatMessage, ChatResponse, ToolChatProvider},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    outbound::call_outbound,
    HTTPLLMProvider, LLMProvider, Tool,
};
use async_trait::async_trait;
use std::sync::Arc;
use tracing::instrument;

pub struct LLMProviderFromHTTP {
    inner: Arc<dyn HTTPLLMProvider>,
}

impl LLMProviderFromHTTP {
    pub fn new(inner: Arc<dyn HTTPLLMProvider>) -> Self {
        Self { inner }
    }

    async fn do_chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let req = self
            .inner
            .chat_request(messages, tools)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        self.inner.parse_chat(resp)
    }
}

#[async_trait]
impl BasicChatProvider for LLMProviderFromHTTP {
    #[instrument(name = "http_adapter.chat", skip_all)]
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        // no tools by default
        self.do_chat(messages, None).await
    }
}

#[async_trait]
impl ToolChatProvider for LLMProviderFromHTTP {
    #[instrument(name = "http_adapter.chat_with_tools", skip_all)]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.do_chat(messages, tools).await
    }
}

#[async_trait]
impl EmbeddingProvider for LLMProviderFromHTTP {
    #[instrument(name = "http_adapter.embed", skip_all)]
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let req = self.inner.embed_request(&inputs)?;
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_embed(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }
}

#[async_trait]
impl CompletionProvider for LLMProviderFromHTTP {
    #[instrument(name = "http_adapter.complete", skip_all)]
    async fn complete(&self, req_obj: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let req = self.inner.complete_request(req_obj)?;
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_complete(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }
}

impl LLMProvider for LLMProviderFromHTTP {
    fn tools(&self) -> Option<&[Tool]> {
        self.inner.tools()
    }
}
