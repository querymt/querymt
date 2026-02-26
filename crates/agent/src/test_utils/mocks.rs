//! Mock implementations for testing

use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, IntentSnapshot, ProgressEntry, ProgressKind, Task,
    TaskStatus,
};
use crate::session::error::SessionResult;
use crate::session::store::{
    CustomModel, LLMConfig, Session, SessionExecutionConfig, SessionStore,
};
use async_trait::async_trait;
use mockall::mock;
use querymt::LLMParams;
use querymt::chat::{ChatMessage, ChatResponse, FinishReason, Tool};
use querymt::completion::{CompletionRequest, CompletionResponse};
use querymt::error::LLMError;
use querymt::plugin::host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin};
use querymt::plugin::{Fut, LLMProviderFactory};
use querymt::{LLMProvider, Usage};
use std::sync::Arc;
use tokio::sync::Mutex;

// ============================================================================
// MockSessionStore
// ============================================================================

mock! {
    pub SessionStore {}

    #[async_trait]
    impl SessionStore for SessionStore {
        async fn create_session(&self, name: Option<String>, cwd: Option<std::path::PathBuf>, parent_session_id: Option<String>, fork_origin: Option<ForkOrigin>) -> SessionResult<Session>;
        async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>>;
        async fn list_sessions(&self) -> SessionResult<Vec<Session>>;
        async fn delete_session(&self, session_id: &str) -> SessionResult<()>;
        async fn get_history(&self, session_id: &str) -> SessionResult<Vec<crate::model::AgentMessage>>;
        async fn get_effective_history(&self, session_id: &str) -> SessionResult<Vec<crate::model::AgentMessage>>;
        async fn add_message(&self, session_id: &str, message: crate::model::AgentMessage) -> SessionResult<()>;
        async fn fork_session(
            &self,
            source_session_id: &str,
            target_message_id: &str,
            fork_origin: ForkOrigin,
        ) -> SessionResult<String>;
        async fn create_or_get_llm_config(&self, input: &LLMParams) -> SessionResult<LLMConfig>;
        async fn get_llm_config(&self, id: i64) -> SessionResult<Option<LLMConfig>>;
        async fn get_session_llm_config(&self, session_id: &str) -> SessionResult<Option<LLMConfig>>;
        async fn set_session_llm_config(&self, session_id: &str, config_id: i64) -> SessionResult<()>;
        async fn set_session_provider_node_id<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            provider_node_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn get_session_provider_node_id<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<String>>;
        async fn set_session_execution_config<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            config: &'c SessionExecutionConfig,
        ) -> SessionResult<()>;
        async fn get_session_execution_config<'a, 'b>(
            &'a self,
            session_id: &'b str,
        ) -> SessionResult<Option<SessionExecutionConfig>>;
        async fn list_custom_models(&self, provider: &str) -> SessionResult<Vec<CustomModel>>;
        async fn get_custom_model(
            &self,
            provider: &str,
            model_id: &str,
        ) -> SessionResult<Option<CustomModel>>;
        async fn upsert_custom_model(&self, model: &CustomModel) -> SessionResult<()>;
        async fn delete_custom_model(&self, provider: &str, model_id: &str) -> SessionResult<()>;
        async fn set_current_intent_snapshot<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            snapshot_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn set_active_task<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<()>;
        async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>>;
        async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>>;
        async fn create_task(&self, task: Task) -> SessionResult<Task>;
        async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>>;
        async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>>;
        async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()>;
        async fn update_task(&self, task: Task) -> SessionResult<()>;
        async fn delete_task(&self, task_id: &str) -> SessionResult<()>;
        async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()>;
        async fn get_intent_snapshot(&self, snapshot_id: &str) -> SessionResult<Option<IntentSnapshot>>;
        async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>>;
        async fn get_current_intent_snapshot(&self, session_id: &str) -> SessionResult<Option<IntentSnapshot>>;
        async fn record_decision(&self, decision: Decision) -> SessionResult<()>;
        async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()>;
        async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>>;
        async fn list_decisions<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Decision>>;
        async fn list_alternatives<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Alternative>>;
        async fn update_decision_status(
            &self,
            decision_id: &str,
            status: DecisionStatus,
        ) -> SessionResult<()>;
        async fn update_alternative_status(
            &self,
            alternative_id: &str,
            status: AlternativeStatus,
        ) -> SessionResult<()>;
        async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()>;
        async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>>;
        async fn list_progress_entries<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<ProgressEntry>>;
        async fn list_progress_by_kind(
            &self,
            session_id: &str,
            kind: ProgressKind,
        ) -> SessionResult<Vec<ProgressEntry>>;
        async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()>;
        async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>>;
        async fn list_artifacts<'a, 'b, 'c>(
            &'a self,
            session_id: &'b str,
            task_id: Option<&'c str>,
        ) -> SessionResult<Vec<Artifact>>;
        async fn list_artifacts_by_kind(
            &self,
            session_id: &str,
            kind: &str,
        ) -> SessionResult<Vec<Artifact>>;
        async fn create_delegation(&self, delegation: Delegation) -> SessionResult<Delegation>;
        async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>>;
        async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>>;
        async fn update_delegation_status(
            &self,
            delegation_id: &str,
            status: DelegationStatus,
        ) -> SessionResult<()>;
        async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()>;
        async fn peek_revert_state(
            &self,
            session_id: &str,
        ) -> SessionResult<Option<crate::session::domain::RevertState>>;
        async fn push_revert_state(
            &self,
            session_id: &str,
            state: crate::session::domain::RevertState,
        ) -> SessionResult<()>;
        async fn pop_revert_state(
            &self,
            session_id: &str,
        ) -> SessionResult<Option<crate::session::domain::RevertState>>;
        async fn list_revert_states(
            &self,
            session_id: &str,
        ) -> SessionResult<Vec<crate::session::domain::RevertState>>;
        async fn clear_revert_states(&self, session_id: &str) -> SessionResult<()>;
        async fn delete_messages_after(
            &self,
            session_id: &str,
            message_id: &str,
        ) -> SessionResult<usize>;
        async fn mark_tool_results_compacted(
            &self,
            session_id: &str,
            call_ids: &[String],
        ) -> SessionResult<usize>;
    }
}

// ============================================================================
// MockLlmProvider
// ============================================================================

mock! {
    pub LlmProvider {}

    #[async_trait]
    impl querymt::chat::ChatProvider for LlmProvider {
        fn supports_streaming(&self) -> bool;
        async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError>;
        async fn chat_with_tools<'a, 'b, 'c>(
            &'a self,
            messages: &'b [ChatMessage],
            tools: Option<&'c [Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError>;
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
        ) -> Result<String, LLMError>;
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
    fn supports_streaming(&self) -> bool {
        false
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        let provider = self.inner.lock().await;
        provider.chat(messages).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let provider = self.inner.lock().await;
        provider.chat_with_tools(messages, tools).await
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

    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        let provider = self.inner.lock().await;
        provider.call_tool(name, args).await
    }
}

// ============================================================================
// TestProviderFactory - Factory for creating SharedLlmProvider
// ============================================================================

pub struct TestProviderFactory {
    pub provider: SharedLlmProvider,
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
// MockChatResponse - Mock implementation of ChatResponse
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

impl ChatResponse for MockChatResponse {
    fn text(&self) -> Option<String> {
        if self.text.is_empty() {
            None
        } else {
            Some(self.text.clone())
        }
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        if self.tool_calls.is_empty() {
            None
        } else {
            Some(self.tool_calls.clone())
        }
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        if !self.tool_calls.is_empty() {
            return Some(FinishReason::ToolCalls);
        } else if self.text.is_empty() {
            return Some(FinishReason::Stop);
        }
        None
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
    fn supports_streaming(&self) -> bool {
        false
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.received_messages.lock().await.push(messages.to_vec());

        let mut responses = self.responses.lock().await;
        match responses.pop_front() {
            Some(Ok(text)) => Ok(Box::new(MockChatResponse::text_only(&text))),
            Some(Err(e)) => Err(e),
            None => Err(LLMError::GenericError(
                "No more mock responses available".to_string(),
            )),
        }
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        // For compaction testing, we don't use tools
        self.chat(messages).await
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

    async fn call_tool(&self, _name: &str, _args: serde_json::Value) -> Result<String, LLMError> {
        Err(LLMError::NotImplemented(
            "MockCompactionProvider does not support tools".to_string(),
        ))
    }
}
