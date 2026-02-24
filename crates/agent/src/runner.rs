//! Unified chat runner interface for agents
//!
//! Provides a common trait for both single agents and multi-agent quorums.

use crate::config::{Config, ConfigSource, load_config};
use crate::events::EventEnvelope;
#[cfg(feature = "dashboard")]
use crate::server::AgentServer;
use crate::simple::{Agent, Quorum};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;

/// Unified interface for chat operations
///
/// Implemented by both `Agent` and `Quorum` to provide a consistent API.
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

    /// Get the web dashboard server
    #[cfg(feature = "dashboard")]
    fn dashboard(&self) -> AgentServer;
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

/// A chat session for maintaining conversation context
#[async_trait]
pub trait ChatSession: Send + Sync {
    /// Get the session ID
    fn id(&self) -> &str;

    /// Send a chat message within this session
    async fn chat(&self, prompt: &str) -> Result<String>;

    /// Register a callback for tool call events in this session
    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>);

    /// Register a callback for tool completion events in this session
    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for message events in this session
    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>);

    /// Register a callback for error events in this session
    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>);
}

/// Extension trait for convenient callback registration
pub trait ChatSessionExt: ChatSession {
    fn on_tool_call(&self, callback: impl Fn(String, Value) + Send + Sync + 'static) {
        self.on_tool_call_boxed(Box::new(callback));
    }

    fn on_tool_complete(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_tool_complete_boxed(Box::new(callback));
    }

    fn on_message(&self, callback: impl Fn(String, String) + Send + Sync + 'static) {
        self.on_message_boxed(Box::new(callback));
    }

    fn on_error(&self, callback: impl Fn(String) + Send + Sync + 'static) {
        self.on_error_boxed(Box::new(callback));
    }
}

// Blanket implementation
impl<T: ChatSession + ?Sized> ChatSessionExt for T {}

/// Unified runner for both single agents and multi-agent quorums.
///
/// This enum provides a concrete type that supports both Send and !Send
/// async methods, unlike the ChatRunner trait which requires Send bounds.
///
/// Use this type when you need access to all agent functionality including
/// ACP transport setup which uses !Send LocalSet internally.
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
/// // Start ACP stdio server (not possible with Box<dyn ChatRunner>)
/// runner.acp("stdio").await?;
/// # Ok(())
/// # }
/// ```
pub enum AgentRunner {
    /// Single agent runner
    Single(Agent),
    /// Multi-agent quorum runner
    Multi(Quorum),
}

impl AgentRunner {
    /// Send a chat message and get a response
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        match self {
            AgentRunner::Single(agent) => agent.chat(prompt).await,
            AgentRunner::Multi(quorum) => quorum.chat(prompt).await,
        }
    }

    /// Create a new chat session for maintaining conversation history
    pub async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        match self {
            AgentRunner::Single(agent) => ChatRunner::chat_session(agent).await,
            AgentRunner::Multi(quorum) => ChatRunner::chat_session(quorum).await,
        }
    }

    /// Start an ACP server with the specified transport
    ///
    /// # Arguments
    /// * `transport` - Either "stdio" for stdin/stdout, or "ip:port" for WebSocket
    ///
    /// # Note
    /// This method returns a !Send future because stdio transport uses LocalSet internally.
    /// This is the primary advantage of AgentRunner over Box<dyn ChatRunner>.
    pub async fn acp(&self, transport: &str) -> Result<()> {
        match self {
            AgentRunner::Single(agent) => agent.acp(transport).await,
            AgentRunner::Multi(quorum) => quorum.acp(transport).await,
        }
    }

    /// Get the web dashboard server
    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        match self {
            AgentRunner::Single(agent) => agent.dashboard(),
            AgentRunner::Multi(quorum) => quorum.dashboard(),
        }
    }

    /// Subscribe to agent events (tool calls, messages, etc.)
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        match self {
            AgentRunner::Single(agent) => ChatRunner::subscribe(agent),
            AgentRunner::Multi(quorum) => ChatRunner::subscribe(quorum),
        }
    }

    /// Register a callback for tool call events
    pub fn on_tool_call(&self, callback: impl Fn(String, Value) + Send + Sync + 'static) -> &Self {
        match self {
            AgentRunner::Single(agent) => {
                agent.on_tool_call(callback);
            }
            AgentRunner::Multi(quorum) => {
                quorum.on_tool_call(callback);
            }
        }
        self
    }

    /// Register a callback for tool completion events
    pub fn on_tool_complete(
        &self,
        callback: impl Fn(String, String) + Send + Sync + 'static,
    ) -> &Self {
        match self {
            AgentRunner::Single(agent) => {
                agent.on_tool_complete(callback);
            }
            AgentRunner::Multi(quorum) => {
                quorum.on_tool_complete(callback);
            }
        }
        self
    }

    /// Register a callback for message events
    pub fn on_message(&self, callback: impl Fn(String, String) + Send + Sync + 'static) -> &Self {
        match self {
            AgentRunner::Single(agent) => {
                agent.on_message(callback);
            }
            AgentRunner::Multi(quorum) => {
                quorum.on_message(callback);
            }
        }
        self
    }

    /// Register a callback for delegation events
    pub fn on_delegation(
        &self,
        callback: impl Fn(String, String) + Send + Sync + 'static,
    ) -> &Self {
        match self {
            AgentRunner::Single(agent) => {
                agent.on_delegation(callback);
            }
            AgentRunner::Multi(quorum) => {
                quorum.on_delegation(callback);
            }
        }
        self
    }

    /// Register a callback for error events
    pub fn on_error(&self, callback: impl Fn(String) + Send + Sync + 'static) -> &Self {
        match self {
            AgentRunner::Single(agent) => {
                agent.on_error(callback);
            }
            AgentRunner::Multi(quorum) => {
                quorum.on_error(callback);
            }
        }
        self
    }

    /// Access the underlying `AgentHandle` for advanced use cases.
    ///
    /// For `Single` runners, this is the agent's handle.
    /// For `Multi` (quorum) runners, this is the planner's handle.
    ///
    /// The handle provides direct access to the session registry, event bus,
    /// and agent config — useful for integrating with the kameo mesh
    /// (e.g., bootstrapping `RemoteNodeManager` with `--mesh`).
    pub fn handle(&self) -> std::sync::Arc<crate::agent::AgentHandle> {
        match self {
            AgentRunner::Single(agent) => agent.handle(),
            AgentRunner::Multi(quorum) => quorum.handle(),
        }
    }

    /// Get a reference to the inner Agent if this is a Single runner
    pub fn as_agent(&self) -> Option<&Agent> {
        match self {
            AgentRunner::Single(agent) => Some(agent),
            AgentRunner::Multi(_) => None,
        }
    }

    /// Get a reference to the inner Quorum if this is a Multi runner
    pub fn as_quorum(&self) -> Option<&Quorum> {
        match self {
            AgentRunner::Single(_) => None,
            AgentRunner::Multi(quorum) => Some(quorum),
        }
    }

    /// Convert into the inner Agent if this is a Single runner
    pub fn into_agent(self) -> Option<Agent> {
        match self {
            AgentRunner::Single(agent) => Some(agent),
            AgentRunner::Multi(_) => None,
        }
    }

    /// Convert into the inner Quorum if this is a Multi runner
    pub fn into_quorum(self) -> Option<Quorum> {
        match self {
            AgentRunner::Single(_) => None,
            AgentRunner::Multi(quorum) => Some(quorum),
        }
    }
}

/// Convert AgentRunner to a trait object for backwards compatibility
///
/// This allows migration from code expecting `Box<dyn ChatRunner>` to code
/// using `AgentRunner`. Note that the `.acp()` method will not be available
/// through the trait object.
impl From<AgentRunner> for Box<dyn ChatRunner> {
    fn from(runner: AgentRunner) -> Self {
        match runner {
            AgentRunner::Single(agent) => Box::new(agent),
            AgentRunner::Multi(quorum) => Box::new(quorum),
        }
    }
}

/// Load an agent or quorum from configuration source.
///
/// Automatically detects whether the config is for a single agent or multi-agent quorum.
/// Returns an `AgentRunner` which provides access to all agent functionality including
/// the `.acp()` method for starting ACP servers.
///
/// `source` accepts either a config file path or inline TOML (`ConfigSource::Toml`).
/// Inline TOML must contain fully inlined system prompts (no `{ file = ... }` entries).
///
/// ## Phase 7: Config-Driven Mesh Bootstrap
///
/// When the `remote` feature is enabled and the TOML config contains:
///
/// ```toml
/// [mesh]
/// enabled = true
/// listen = "/ip4/0.0.0.0/tcp/9000"
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
/// 3. Pre-populate the agent registry with `AgentInfo` for each `[[remote_agents]]` entry,
///    backed by a `RemoteAgentStub` that routes delegation calls via the mesh.
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

    match config {
        Config::Single(single_config) => {
            // Phase 7: bootstrap mesh from config if enabled (remote feature only).
            #[cfg(feature = "remote")]
            if single_config.mesh.enabled {
                use crate::agent::remote::remote_setup::setup_mesh_from_config;
                use std::sync::Arc;

                log::info!("Phase 7: mesh.enabled = true in config, bootstrapping mesh...");
                match setup_mesh_from_config(
                    &single_config.mesh,
                    &single_config.remote_agents,
                    None, // RemoteNodeManager will be spawned by coder_agent or caller
                    None, // ProviderHostActor will be spawned after AgentConfig is built
                )
                .await
                {
                    Ok(result) => {
                        use crate::delegation::AgentRegistry as _;
                        log::info!(
                            "Phase 7: mesh bootstrapped, {} remote agent(s) registered",
                            result.registry.list_agents().len()
                        );
                        let auto_fallback = single_config.mesh.auto_fallback;
                        let agent = Agent::from_single_config_with_registry(
                            single_config,
                            Some(Arc::new(result.registry)),
                            Some(result.mesh),
                            auto_fallback,
                        )
                        .await?;
                        return Ok(AgentRunner::Single(agent));
                    }
                    Err(e) => {
                        log::warn!(
                            "Phase 7: mesh bootstrap failed: {}; continuing without mesh",
                            e
                        );
                    }
                }
            }

            let agent = Agent::from_single_config(single_config).await?;
            Ok(AgentRunner::Single(agent))
        }
        Config::Multi(quorum_config) => {
            // Phase 7: bootstrap mesh from config if enabled (remote feature only).
            #[cfg(feature = "remote")]
            if quorum_config.mesh.enabled {
                use crate::agent::remote::remote_setup::setup_mesh_from_config;
                use std::sync::Arc;

                log::info!("Phase 7: mesh.enabled = true in config, bootstrapping mesh...");
                match setup_mesh_from_config(
                    &quorum_config.mesh,
                    &quorum_config.remote_agents,
                    None,
                    None, // ProviderHostActor will be spawned after AgentConfig is built
                )
                .await
                {
                    Ok(result) => {
                        use crate::delegation::AgentRegistry;
                        log::info!(
                            "Phase 7: mesh bootstrapped, {} remote agent(s) registered",
                            result.registry.list_agents().len()
                        );
                        let auto_fallback = quorum_config.mesh.auto_fallback;
                        let quorum = Quorum::from_quorum_config_with_registry(
                            quorum_config,
                            Some(Arc::new(result.registry)),
                            Some(result.mesh),
                            auto_fallback,
                        )
                        .await?;
                        return Ok(AgentRunner::Multi(quorum));
                    }
                    Err(e) => {
                        log::warn!(
                            "Phase 7: mesh bootstrap failed: {}; continuing without mesh",
                            e
                        );
                    }
                }
            }

            let quorum = Quorum::from_quorum_config(quorum_config).await?;
            Ok(AgentRunner::Multi(quorum))
        }
    }
}
