//! Unified chat runner interface for agents
//!
//! Provides a common trait for both single agents and multi-agent quorums.

use crate::api::Agent;
use crate::config::{Config, ConfigSource, load_config};
use crate::events::EventEnvelope;
#[cfg(feature = "dashboard")]
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
/// Since `Agent` now handles both single and multi-agent configurations,
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

    /// Get the web dashboard server
    #[cfg(feature = "dashboard")]
    pub fn dashboard(&self) -> AgentServer {
        self.0.dashboard()
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

    /// Returns true if this is a multi-agent (quorum) runner.
    pub fn is_multi(&self) -> bool {
        self.0.is_multi()
    }
}

/// Convert AgentRunner to a trait object for backwards compatibility.
impl From<AgentRunner> for Box<dyn ChatRunner> {
    fn from(runner: AgentRunner) -> Self {
        Box::new(runner.0)
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
/// ## Config-Driven Mesh Bootstrap
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
///    backed by a `RemoteAgentHandle` that routes delegation calls via the mesh.
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
            // bootstrap mesh from config if enabled (remote feature only).
            #[cfg(feature = "remote")]
            if single_config.mesh.enabled {
                use crate::agent::remote::remote_setup::setup_mesh_from_config;
                use std::sync::Arc;

                log::info!("mesh.enabled = true in config, bootstrapping mesh...");
                match setup_mesh_from_config(
                    &single_config.mesh,
                    &single_config.remote_agents,
                    None, // spawned below after AgentConfig is built
                    None, // spawned below after AgentConfig is built
                )
                .await
                {
                    Ok(result) => {
                        use crate::delegation::AgentRegistry as _;
                        log::info!(
                            "mesh bootstrapped, {} remote agent(s) registered",
                            result.registry.list_agents().len()
                        );
                        let auto_fallback = single_config.mesh.auto_fallback;
                        let agent = Agent::from_single_config_with_registry(
                            single_config,
                            Some(Arc::new(result.registry)),
                            Some(result.mesh.clone()),
                            auto_fallback,
                        )
                        .await?;
                        // Now that we have an AgentConfig, spawn and register the
                        // RemoteNodeManager and ProviderHostActor so remote peers can
                        // discover this node in the DHT and create sessions here.
                        spawn_and_register_mesh_actors(&agent.handle(), &result.mesh).await;
                        return Ok(AgentRunner::new(agent));
                    }
                    Err(e) => {
                        log::warn!("mesh bootstrap failed: {}; continuing without mesh", e);
                    }
                }
            }

            let agent = Agent::from_single_config(single_config).await?;
            Ok(AgentRunner::new(agent))
        }
        Config::Multi(quorum_config) => {
            // bootstrap mesh from config if enabled (remote feature only).
            #[cfg(feature = "remote")]
            if quorum_config.mesh.enabled {
                use crate::agent::remote::remote_setup::setup_mesh_from_config;
                use std::sync::Arc;

                log::info!("mesh.enabled = true in config, bootstrapping mesh...");
                match setup_mesh_from_config(
                    &quorum_config.mesh,
                    &quorum_config.remote_agents,
                    None, // spawned below after AgentConfig is built
                    None, // spawned below after AgentConfig is built
                )
                .await
                {
                    Ok(result) => {
                        use crate::delegation::AgentRegistry;
                        log::info!(
                            "mesh bootstrapped, {} remote agent(s) registered",
                            result.registry.list_agents().len()
                        );
                        let auto_fallback = quorum_config.mesh.auto_fallback;
                        let agent = Agent::from_quorum_config_with_registry(
                            quorum_config,
                            Some(Arc::new(result.registry)),
                            Some(result.mesh.clone()),
                            auto_fallback,
                        )
                        .await?;
                        // Now that we have an AgentConfig, spawn and register the
                        // RemoteNodeManager and ProviderHostActor so remote peers can
                        // discover this node in the DHT and create sessions here.
                        spawn_and_register_mesh_actors(&agent.handle(), &result.mesh).await;
                        return Ok(AgentRunner::new(agent));
                    }
                    Err(e) => {
                        log::warn!("mesh bootstrap failed: {}; continuing without mesh", e);
                    }
                }
            }

            let agent = Agent::from_quorum_config(quorum_config).await?;
            Ok(AgentRunner::new(agent))
        }
    }
}

/// Spawn a `RemoteNodeManager` and a `ProviderHostActor` for this node and
/// register them in the kameo DHT so remote peers can discover and use them.
///
/// This is called by `from_config` immediately after the agent/quorum is built,
/// once an `AgentConfig` is available. It replicates the registration that
/// `coder_agent --mesh` does manually, making mesh-enabled configs self-contained.
#[cfg(feature = "remote")]
async fn spawn_and_register_mesh_actors(
    handle: &crate::agent::LocalAgentHandle,
    mesh: &crate::agent::remote::MeshHandle,
) {
    use crate::agent::remote::ProviderHostActor;
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::dht_name;
    use kameo::actor::Spawn;

    // ── RemoteNodeManager ────────────────────────────────────────────────────
    let node_manager = RemoteNodeManager::new(
        handle.config.clone(),
        handle.registry.clone(),
        Some(mesh.clone()),
    );
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    // Register under the global name (for lookup_all_actors / list_remote_nodes).
    mesh.register_actor(node_manager_ref.clone(), dht_name::NODE_MANAGER)
        .await;
    log::info!(
        "RemoteNodeManager registered in DHT as '{}'",
        dht_name::NODE_MANAGER
    );

    // Register under the per-peer name for O(1) direct lookup by peer_id.
    // This is the name resolve_peer_node_id looks up — it must be present for
    // peer delegates to route to this node.
    let per_peer_name = dht_name::node_manager_for_peer(mesh.peer_id());
    mesh.register_actor(node_manager_ref, per_peer_name.clone())
        .await;
    log::info!(
        "RemoteNodeManager also registered in DHT as '{}'",
        per_peer_name
    );

    // ── ProviderHostActor ────────────────────────────────────────────────────
    let provider_host = ProviderHostActor::new(handle.config.clone());
    let provider_host_ref = ProviderHostActor::spawn(provider_host);
    let ph_dht_name = dht_name::provider_host(mesh.peer_id());
    mesh.register_actor(provider_host_ref, ph_dht_name.clone())
        .await;
    log::info!("ProviderHostActor registered in DHT as '{}'", ph_dht_name);
}
