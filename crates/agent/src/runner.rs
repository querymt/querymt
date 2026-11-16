//! Unified chat runner interface for agents
//!
//! Provides a common trait for agents, with or without subagents.

use crate::api::{Agent, AgentInfra, AgentProfiles, ProfileRuntimeHandle};
use crate::config::{Config, ConfigSource, load_config};
use crate::events::EventEnvelope;
#[cfg(feature = "api")]
use crate::server::AgentServer;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;

/// Unified interface for chat operations
///
/// Implemented by `Agent` to provide a consistent API.
#[async_trait]
pub trait ChatRunner: Send + Sync {
    /// Send a chat message and get a response
    async fn chat(&self, prompt: &str) -> Result<String>;

    /// Create a new chat session for maintaining conversation history
    async fn chat_session(&self) -> Result<Box<dyn ChatSession>>;

    /// Subscribe to agent events (tool calls, messages, etc.)
    fn subscribe(&self) -> broadcast::Receiver<EventEnvelope>;

    /// Register a callback for tool call events (boxed version)
    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>);

    /// Register a callback for tool completion events (boxed version)
    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for message events (boxed version)
    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for delegation events (boxed version)
    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for error events (boxed version)
    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>);

    /// Get the HTTP server
    #[cfg(feature = "api")]
    fn server(&self) -> AgentServer;
}

/// Extension trait for convenient callback registration  
pub trait ChatRunnerExt: ChatRunner {
    fn on_tool_call(&self, callback: impl Fn(String, Value) + Send + Sync + 'static) {
        self.on_tool_call_boxed(Box::new(callback));
    }

    fn on_tool_complete(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_complete_boxed(Box::new(callback));
    }

    fn on_message(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_message_boxed(Box::new(callback));
    }

    fn on_delegation(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_delegation_boxed(Box::new(callback));
    }

    fn on_error(&self, callback: impl Fn(String) + Send + Sync + 'static) {
        self.on_error_boxed(Box::new(callback));
    }
}

// Blanket implementation
impl<T: ChatRunner + ?Sized> ChatRunnerExt for T {}

pub use crate::chat_session::{ChatSession, ChatSessionExt};

/// Unified runner for agents, with or without subagents.
///
/// `AgentRunner` is a thin wrapper around `Agent` for backward compatibility.
///
/// # Examples
///
/// ```no_run
/// use querymt_agent::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// // Load from config
/// let runner = from_config("agent.toml").await?;
///
/// // Use chat functionality
/// let response = runner.chat("Hello!").await?;
///
/// // Start ACP stdio server
/// runner.acp("stdio").await?;
/// # Ok(())
/// # }
/// ```
pub struct AgentRunner(Agent);

impl AgentRunner {
    /// Create a new runner from an `Agent`.
    pub fn new(agent: Agent) -> Self {
        Self(agent)
    }

    /// Send a chat message and get a response
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        self.0.chat(prompt).await
    }

    /// Create a new chat session for maintaining conversation history
    pub async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        ChatRunner::chat_session(&self.0).await
    }

    /// Start an ACP server with the specified transport
    pub async fn acp(&self, transport: &str) -> Result<()> {
        self.0.acp(transport).await
    }

    /// Get the HTTP server
    #[cfg(feature = "api")]
    pub fn server(&self) -> AgentServer {
        self.0.server()
    }

    /// Subscribe to agent events (tool calls, messages, etc.)
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.0.subscribe()
    }

    /// Register a callback for tool call events
    pub fn on_tool_call(&self, callback: impl Fn(String, Value) + Send + Sync + 'static) -> &Self {
        self.0.on_tool_call(callback);
        self
    }

    /// Register a callback for tool completion events
    pub fn on_tool_complete(
        &self,
        callback: impl Fn(String, String) + Send + Sync + 'static,
    ) -> &Self {
        self.0.on_tool_complete(callback);
        self
    }

    /// Register a callback for message events
    pub fn on_message(&self, callback: impl Fn(String, String) + Send + Sync + 'static) -> &Self {
        self.0.on_message(callback);
        self
    }

    /// Register a callback for delegation events
    pub fn on_delegation(
        &self,
        callback: impl Fn(String, String) + Send + Sync + 'static,
    ) -> &Self {
        self.0.on_delegation(callback);
        self
    }

    /// Register a callback for error events
    pub fn on_error(&self, callback: impl Fn(String) + Send + Sync + 'static) -> &Self {
        self.0.on_error(callback);
        self
    }

    pub fn with_profiles(mut self, profiles: AgentProfiles) -> Self {
        self.0 = self.0.with_profiles(profiles);
        self
    }

    pub fn profiles(&self) -> Option<ProfileRuntimeHandle> {
        self.0.profiles()
    }

    pub async fn shutdown(&self) {
        self.0.shutdown().await;
    }

    /// Access the underlying `AgentHandle` for advanced use cases.
    pub fn handle(&self) -> std::sync::Arc<crate::agent::LocalAgentHandle> {
        self.0.handle()
    }

    /// Get a reference to the inner Agent.
    pub fn as_agent(&self) -> &Agent {
        &self.0
    }

    /// Convert into the inner Agent.
    pub fn into_agent(self) -> Agent {
        self.0
    }

    /// Returns true if the agent has subagents it can delegate to.
    pub fn has_subagents(&self) -> bool {
        self.0.has_subagents()
    }

    #[deprecated(note = "use `has_subagents()`")]
    pub fn is_multi(&self) -> bool {
        self.has_subagents()
    }
}

/// Convert AgentRunner to a trait object for backwards compatibility.
impl From<AgentRunner> for Box<dyn ChatRunner> {
    fn from(runner: AgentRunner) -> Self {
        Box::new(runner.0)
    }
}

/// Load an agent from a configuration source.
///
/// Accepts `[agent]` (with optional `[[delegates]]`) and the deprecated `[quorum]` shape.
/// Returns an `AgentRunner` which provides access to all agent functionality including
/// the `.acp()` method for starting ACP servers.
///
/// `source` accepts either a config file path or inline TOML (`ConfigSource::Toml`).
/// Inline TOML must contain fully inlined system prompts (no `{ file = ... }` entries).
///
/// ## Config-Driven Mesh Bootstrap
///
/// When the `remote` feature is enabled and the TOML config contains:
///
/// ```toml
/// [mesh]
/// enabled = true
/// listen = "/ip4/0.0.0.0/tcp/0"
/// discovery = "mdns"
///
/// [[remote_agents]]
/// id = "gpu-coder"
/// name = "GPU Coder"
/// peer = "dev-gpu"
/// capabilities = ["shell", "gpu"]
/// ```
///
/// `from_config` will automatically:
/// 1. Bootstrap the kameo libp2p swarm.
/// 2. Register this node as a `RemoteNodeManager` in the DHT.
/// 3. List each `[[remote_agents]]` entry as a roster member. The peer is resolved when a
///    delegation needs it, not at startup.
///
/// # Example
///
/// ```no_run
/// use querymt_agent::config::ConfigSource;
/// use querymt_agent::prelude::*;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let runner = from_config("agent.toml").await?;
///
/// // Or from inline TOML
/// let inline = r#"[agent]
/// provider = \"openai\"
/// model = \"gpt-4.1-mini\"
/// tools = []
/// "#;
/// let _runner = from_config(ConfigSource::Toml(inline.to_string())).await?;
///
/// // Chat functionality
/// let response = runner.chat("Hello!").await?;
/// println!("{}", response);
///
/// // Start ACP stdio server
/// runner.acp("stdio").await?;
/// # Ok(())
/// # }
/// ```
pub async fn from_config(source: impl Into<ConfigSource>) -> Result<AgentRunner> {
    let config = load_config(source).await?;
    build_runner_from_config(config, None).await
}

/// Build a runner from config while reusing caller-provided infrastructure.
pub async fn from_config_with_infra(
    source: impl Into<ConfigSource>,
    infra: AgentInfra,
) -> Result<AgentRunner> {
    let config = load_config(source).await?;
    build_runner_from_config(config, Some(infra)).await
}

/// Build a runner from an already-loaded config while reusing caller-provided infrastructure.
pub async fn from_config_value_with_infra(
    config: Config,
    infra: AgentInfra,
) -> Result<AgentRunner> {
    build_runner_from_config(config, Some(infra)).await
}

async fn build_runner_from_config(
    config: Config,
    infra: Option<AgentInfra>,
) -> Result<AgentRunner> {
    let mut builder = Agent::builder_from_config_value(config, None)?;
    if let Some(infra) = infra {
        builder = builder.infra(infra);
    }
    Ok(AgentRunner::new(builder.build().await?))
}
