//! Mock implementations for testing

use async_trait::async_trait;
use mockall::mock;
use querymt::chat::{ChatCall, ChatCapabilities, ChatMessage, ChatOutput, FinishReason, Tool};
use querymt::completion::{CompletionRequest, CompletionResponse};
use querymt::error::LLMError;
use querymt::plugin::host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin};
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
pub use querymt_agent_core::test_support::MockSessionStore;
use std::sync::Arc;
use tokio::sync::Mutex;

// ============================================================================
// MockLlmProvider
// ============================================================================

/// Mock-friendly view of a chat call.
///
/// `mockall` cannot match `&ChatCall<'_>`, so expectations are set on this
/// flattened shape (`expect_chat` for tool-less calls, `expect_chat_with_tools`
/// otherwise) and [`MockLlmProvider`]'s `ChatProvider` impl adapts to it.
#[async_trait]
pub trait MockedChat {
    fn supports_streaming(&self) -> bool;
    async fn chat(&self, messages: &[ChatMessage]) -> Result<ChatOutput, LLMError>;
    async fn chat_with_tools<'a, 'b, 'c>(
        &'a self,
        messages: &'b [ChatMessage],
        tools: Option<&'c [Tool]>,
    ) -> Result<ChatOutput, LLMError>;
}

/// Routes a [`ChatCall`] to the matching [`MockedChat`] expectation.
async fn route_mocked_chat<M: MockedChat + Sync>(
    mock: &M,
    call: &ChatCall<'_>,
) -> Result<ChatOutput, LLMError> {
    match call.tools {
        Some(_) => mock.chat_with_tools(call.messages, call.tools).await,
        None => mock.chat(call.messages).await,
    }
}

mock! {
    pub LlmProvider {}

    #[async_trait]
    impl MockedChat for LlmProvider {
        fn supports_streaming(&self) -> bool;
        async fn chat(&self, messages: &[ChatMessage]) -> Result<ChatOutput, LLMError>;
        async fn chat_with_tools<'a, 'b, 'c>(
            &'a self,
            messages: &'b [ChatMessage],
            tools: Option<&'c [Tool]>,
        ) -> Result<ChatOutput, LLMError>;
    }

    #[async_trait]
    impl querymt::completion::CompletionProvider for LlmProvider {
        async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError>;
    }

    #[async_trait]
    impl querymt::embedding::EmbeddingProvider for LlmProvider {
        async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError>;
    }

    #[async_trait]
    impl LLMProvider for LlmProvider {
        fn tools<'a>(&'a self) -> Option<&'a [Tool]>;
        async fn call_tool<'a, 'b>(
            &'a self,
            name: &'b str,
            args: serde_json::Value,
        ) -> Result<Vec<querymt::chat::ToolResultPart>, LLMError>;
    }
}

#[async_trait]
impl querymt::chat::ChatProvider for MockLlmProvider {
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE.with_streaming(MockedChat::supports_streaming(self))
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        route_mocked_chat(self, call).await
    }
}

// ============================================================================
// SharedLlmProvider - Thread-safe wrapper around MockLlmProvider
// ============================================================================

#[derive(Clone)]
pub struct SharedLlmProvider {
    pub inner: Arc<Mutex<MockLlmProvider>>,
    pub tools: Box<[Tool]>,
}

impl SharedLlmProvider {
    pub fn new(mock: MockLlmProvider, tools: Vec<Tool>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(mock)),
            tools: tools.into_boxed_slice(),
        }
    }
}

#[async_trait]
impl querymt::chat::ChatProvider for SharedLlmProvider {
    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let provider = self.inner.lock().await;
        route_mocked_chat(&*provider, call).await
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for SharedLlmProvider {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let provider = self.inner.lock().await;
        provider.complete(req).await
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for SharedLlmProvider {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let provider = self.inner.lock().await;
        provider.embed(input).await
    }
}

#[async_trait]
impl LLMProvider for SharedLlmProvider {
    fn tools(&self) -> Option<&[Tool]> {
        if self.tools.is_empty() {
            None
        } else {
            Some(&self.tools)
        }
    }

    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Vec<querymt::chat::ToolResultPart>, LLMError> {
        let provider = self.inner.lock().await;
        provider.call_tool(name, args).await
    }
}

// ============================================================================
// TestProviderFactory - Factory for creating SharedLlmProvider
// ============================================================================

pub struct TestProviderFactory {
    pub provider: SharedLlmProvider,
    /// When set, this factory exposes [`querymt::plugin::http::HTTPLLMProviderFactory`]
    /// so auth mutations can resolve an `api_key_name`.
    pub api_key_name: Option<String>,
}

impl TestProviderFactory {
    pub fn new(provider: SharedLlmProvider) -> Self {
        Self {
            provider,
            api_key_name: None,
        }
    }

    pub fn with_api_key_name(mut self, name: impl Into<String>) -> Self {
        self.api_key_name = Some(name.into());
        self
    }
}

impl querymt::plugin::http::HTTPLLMProviderFactory for TestProviderFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn api_key_name(&self) -> Option<String> {
        self.api_key_name.clone()
    }

    fn config_schema(&self) -> String {
        "{}".to_string()
    }

    fn list_models_request(&self, _cfg: &str) -> Result<http::Request<Vec<u8>>, LLMError> {
        Err(LLMError::InvalidRequest(
            "test factory does not support HTTP list_models".into(),
        ))
    }

    fn parse_list_models(&self, _resp: http::Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Ok(vec!["mock-model".to_string()])
    }

    fn from_config(&self, _cfg: &str) -> Result<Box<dyn querymt::HTTPLLMProvider>, LLMError> {
        Err(LLMError::InvalidRequest(
            "test factory does not support HTTP from_config".into(),
        ))
    }
}

impl LLMProviderFactory for TestProviderFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn config_schema(&self) -> String {
        "{}".to_string()
    }

    fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        Ok(Box::new(self.provider.clone()))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async { Ok(vec!["mock-model".to_string()]) })
    }

    fn as_http(&self) -> Option<&dyn querymt::plugin::http::HTTPLLMProviderFactory> {
        // Only advertise HTTP when a key name is configured so existing
        // non-HTTP mock tests keep their previous factory path.
        if self.api_key_name.is_some() {
            Some(self)
        } else {
            None
        }
    }
}

/// A factory named `mock` whose provider can never be built: for tests of what
/// a run does when its provider fails at start.
pub struct UnbuildableProviderFactory;

impl LLMProviderFactory for UnbuildableProviderFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn config_schema(&self) -> String {
        "{}".to_string()
    }

    fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        Err(LLMError::InvalidRequest("cannot build the provider".into()))
    }

    fn list_models<'a>(&'a self, _cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        Box::pin(async { Ok(vec!["mock-model".to_string()]) })
    }
}

// ============================================================================
// TestPluginLoader - Plugin loader for tests
// ============================================================================

pub struct TestPluginLoader {
    pub factory: Arc<dyn LLMProviderFactory>,
}

#[async_trait]
impl PluginLoader for TestPluginLoader {
    fn supported_type(&self) -> PluginType {
        PluginType::Wasm
    }

    async fn load_plugin(
        &self,
        _plugin: ProviderPlugin,
        _plugin_cfg: &ProviderConfig,
    ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
        Ok(self.factory.clone())
    }
}

// ============================================================================
// MockChatResponse - mock projection source for tests
// ============================================================================

#[derive(Debug)]
pub struct MockChatResponse {
    pub text: String,
    pub tool_calls: Vec<querymt::ToolCall>,
    pub usage: Option<Usage>,
}

impl MockChatResponse {
    pub fn text_only(text: &str) -> Self {
        Self {
            text: text.to_string(),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            }),
        }
    }

    pub fn with_tools(text: &str, tool_calls: Vec<querymt::ToolCall>) -> Self {
        Self {
            text: text.to_string(),
            tool_calls,
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            }),
        }
    }
}

impl std::fmt::Display for MockChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl From<MockChatResponse> for ChatOutput {
    fn from(response: MockChatResponse) -> Self {
        let text = if response.text.is_empty() {
            None
        } else {
            Some(response.text.clone())
        };

        let tool_calls = if response.tool_calls.is_empty() {
            None
        } else {
            Some(response.tool_calls.clone())
        };

        let finish_reason = if !response.tool_calls.is_empty() {
            Some(FinishReason::ToolCalls)
        } else if response.text.is_empty() {
            Some(FinishReason::Stop)
        } else {
            None
        };

        ChatOutput::from_projections(None, text, tool_calls, response.usage, finish_reason)
    }
}

// ============================================================================
// MockCompactionProvider - for testing SessionCompaction
// ============================================================================

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Mock ChatProvider for testing compaction - queues predetermined responses
#[derive(Clone)]
pub struct MockCompactionProvider {
    responses: Arc<Mutex<VecDeque<Result<String, LLMError>>>>,
    received_messages: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    call_count: Arc<AtomicUsize>,
}

impl MockCompactionProvider {
    /// Create a new mock provider with a queue of responses
    pub fn new(responses: Vec<Result<String, LLMError>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            received_messages: Arc::new(Mutex::new(Vec::new())),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Create with a single successful response
    pub fn with_summary(summary: &str) -> Self {
        Self::new(vec![Ok(summary.to_string())])
    }

    /// Create with a single error response
    pub fn with_error(error: LLMError) -> Self {
        Self::new(vec![Err(error)])
    }

    /// Get all messages that were sent to this provider (async version)
    pub async fn get_received_messages_async(&self) -> Vec<Vec<ChatMessage>> {
        self.received_messages.lock().await.clone()
    }

    /// Get the number of times chat() was called
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Reset call count and messages (for reusing provider in multiple tests)
    pub async fn reset(&self) {
        self.call_count.store(0, Ordering::SeqCst);
        self.received_messages.lock().await.clear();
    }
}

#[async_trait]
impl querymt::chat::ChatProvider for MockCompactionProvider {
    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.received_messages
            .lock()
            .await
            .push(call.messages.to_vec());

        let mut responses = self.responses.lock().await;
        match responses.pop_front() {
            Some(Ok(text)) => Ok(MockChatResponse::text_only(&text).into()),
            Some(Err(e)) => Err(e),
            None => Err(LLMError::GenericError(
                "No more mock responses available".to_string(),
            )),
        }
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for MockCompactionProvider {
    async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "MockCompactionProvider does not support completions".to_string(),
        ))
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for MockCompactionProvider {
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "MockCompactionProvider does not support embeddings".to_string(),
        ))
    }
}

#[async_trait]
impl LLMProvider for MockCompactionProvider {
    fn tools(&self) -> Option<&[Tool]> {
        None
    }

    async fn call_tool(
        &self,
        _name: &str,
        _args: serde_json::Value,
    ) -> Result<Vec<querymt::chat::ToolResultPart>, LLMError> {
        Err(LLMError::NotImplemented(
            "MockCompactionProvider does not support tools".to_string(),
        ))
    }
}
