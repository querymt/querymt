//! Single agent implementation

use super::callbacks::EventCallbacksState;
use super::quorum::QuorumBuilder;
use super::session::AgentSession;
use super::utils::{default_registry, latest_assistant_message, to_absolute_path};
use crate::acp::AcpTransport;
use crate::acp::stdio::serve_stdio;
use crate::acp::websocket::serve_websocket;
use crate::agent::builder::AgentBuilderExt;
use crate::agent::core::{QueryMTAgent, SnapshotPolicy, ToolPolicy};
use crate::config::SingleAgentConfig;
use crate::events::AgentEvent;
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::session::backend::StorageBackend;
use crate::session::projection::ViewStore;
use crate::session::sqlite_storage::SqliteStorage;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use querymt::LLMParams;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct AgentBuilder {
    pub(super) llm_config: Option<LLMParams>,
    pub(super) tools: Vec<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) snapshot_policy: SnapshotPolicy,
    pub(super) db_path: Option<PathBuf>,
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            llm_config: None,
            tools: Vec::new(),
            cwd: None,
            snapshot_policy: SnapshotPolicy::Diff,
            db_path: None,
        }
    }

    // Helper that lazily initializes LLMParams
    fn with_llm<F>(mut self, f: F) -> Self
    where
        F: FnOnce(LLMParams) -> LLMParams,
    {
        let cfg = self.llm_config.take().unwrap_or_default();
        self.llm_config = Some(f(cfg));
        self
    }

    pub fn provider(self, name: impl Into<String>, model: impl Into<String>) -> Self {
        self.with_llm(|c| c.provider(name).model(model))
    }

    pub fn api_key(self, key: impl Into<String>) -> Self {
        self.with_llm(|c| c.api_key(key))
    }

    pub fn system(self, prompt: impl Into<String>) -> Self {
        self.with_llm(|c| c.system(prompt))
    }

    pub fn parameter<K: Into<String>>(self, key: K, value: impl Into<serde_json::Value>) -> Self {
        self.with_llm(|c| c.parameter(key, value.into()))
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    pub fn tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    pub async fn build(self) -> Result<Agent> {
        let snapshot_policy = self.snapshot_policy;
        let cwd = if let Some(path) = self.cwd {
            Some(to_absolute_path(path)?)
        } else {
            None
        };

        let llm_config = self
            .llm_config
            .ok_or_else(|| anyhow!("LLM configuration is required (call .provider() first)"))?;

        let registry = Arc::new(default_registry().await?);
        let backend =
            SqliteStorage::connect(self.db_path.unwrap_or_else(|| PathBuf::from(":memory:")))
                .await?;
        let mut agent = QueryMTAgent::new(registry, backend.session_store(), llm_config)
            .with_snapshot_policy(snapshot_policy);
        agent.add_observer(backend.event_observer());

        if !self.tools.is_empty() {
            agent = agent
                .with_tool_policy(ToolPolicy::BuiltInOnly)
                .with_allowed_tools(self.tools.clone());
        }

        let view_store = backend
            .view_store()
            .expect("SqliteStorage always provides ViewStore");

        Ok(Agent {
            inner: Arc::new(agent),
            view_store,
            default_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
        })
    }
}

pub struct Agent {
    pub(super) inner: Arc<QueryMTAgent>,
    #[cfg_attr(not(feature = "dashboard"), allow(dead_code))]
    pub(super) view_store: Arc<dyn ViewStore>,
    default_session_id: Arc<Mutex<Option<String>>>,
    pub(super) cwd: Option<PathBuf>,
    callbacks: Arc<EventCallbacksState>,
}

impl Agent {
    pub fn single() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn multi() -> QuorumBuilder {
        QuorumBuilder::new()
    }

    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_default_session().await?;
        self.chat_with_session(&session_id, prompt).await
    }

    pub async fn chat_session(&self) -> Result<AgentSession> {
        let session_id = self.create_session().await?;
        Ok(AgentSession::new(self.inner.clone(), session_id))
    }

    pub async fn set_provider(&self, provider: &str, model: &str) -> Result<()> {
        let session_id = self.ensure_default_session().await?;
        self.inner
            .set_provider(&session_id, provider, model)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub async fn set_llm_config(&self, config: LLMParams) -> Result<()> {
        let session_id = self.ensure_default_session().await?;
        self.inner
            .set_llm_config(&session_id, config)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(())
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.inner.subscribe_events()
    }

    pub fn on_tool_call<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_tool_complete<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_message<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_delegation<F>(&self, callback: F) -> &Self
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    pub fn on_error<F>(&self, callback: F) -> &Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
        self
    }

    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        AgentServer::new(self.inner.clone(), self.view_store.clone())
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Transports
    /// - `"stdio"` - Use stdin/stdout for JSON-RPC communication (for subprocess spawning)
    /// - `"ws://host:port"` - Start a WebSocket server (not yet implemented, use .dashboard() instead)
    ///
    /// # Example
    /// ```rust,no_run
    /// use querymt_agent::prelude::*;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let agent = Agent::single()
    ///         .provider("anthropic", "claude-sonnet-4-20250514")
    ///         .cwd("/tmp")
    ///         .tools(["read_file", "write_file"])
    ///         .build()
    ///         .await?;
    ///     
    ///     agent.acp("stdio").await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Graceful Shutdown
    /// The server handles SIGTERM and SIGINT (Ctrl+C) for graceful shutdown.
    pub async fn acp<T>(&self, transport: T) -> Result<()>
    where
        T: TryInto<AcpTransport>,
        T::Error: std::fmt::Display,
    {
        let transport = transport
            .try_into()
            .map_err(|e| anyhow!("Invalid transport: {}", e))?;

        match transport {
            AcpTransport::Stdio => serve_stdio(self.inner.clone())
                .await
                .map_err(|e| anyhow!("ACP stdio error: {}", e)),
            AcpTransport::WebSocket(addr) => serve_websocket(self.inner.clone(), &addr)
                .await
                .map_err(|e| anyhow!("ACP websocket error: {}", e)),
        }
    }

    pub fn inner(&self) -> Arc<QueryMTAgent> {
        self.inner.clone()
    }

    async fn ensure_default_session(&self) -> Result<String> {
        if let Some(existing) = self.default_session_id.lock().unwrap().clone() {
            return Ok(existing);
        }
        let session_id = self.create_session().await?;
        *self.default_session_id.lock().unwrap() = Some(session_id.clone());
        Ok(session_id)
    }

    pub(super) async fn create_session(&self) -> Result<String> {
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = self
            .inner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(response.session_id.to_string())
    }

    async fn chat_with_session(&self, session_id: &str, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.inner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .inner
            .provider
            .history_store()
            .get_history(session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    /// Build an Agent from a single agent config
    pub async fn from_single_config(config: SingleAgentConfig) -> Result<Self> {
        let mut builder = AgentBuilder::new()
            .provider(config.agent.provider, config.agent.model)
            .tools(config.agent.tools);

        if let Some(api_key) = config.agent.api_key {
            builder = builder.api_key(api_key);
        }
        if let Some(system) = config.agent.system {
            builder = builder.system(system);
        }
        if let Some(params) = config.agent.parameters {
            for (key, value) in params {
                builder = builder.parameter(key, value);
            }
        }
        if let Some(cwd) = config.agent.cwd {
            builder.cwd = Some(cwd);
        }
        if let Some(db) = config.agent.db {
            builder.db_path = Some(db);
        }

        builder.build().await
    }
}

#[async_trait]
impl ChatRunner for Agent {
    async fn chat(&self, prompt: &str) -> Result<String> {
        Agent::chat(self, prompt).await
    }

    async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        let session = Agent::chat_session(self).await?;
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        Agent::subscribe(self)
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.subscribe_events());
    }

    #[cfg(feature = "dashboard")]
    fn dashboard(&self) -> AgentServer {
        Agent::dashboard(self)
    }
}
