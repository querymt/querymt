//! AgentHandle facade — the public replacement for QueryMTAgent.
//!
//! This lightweight struct bundles shared config, the kameo session registry,
//! and connection-level mutable state. It is NOT an actor — just a convenient
//! bundle that consumers hold instead of `Arc<QueryMTAgent>`.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, ClientState};
use crate::agent::session_registry::SessionRegistry;
use crate::delegation::AgentRegistry;
use crate::event_bus::EventBus;
use crate::events::{AgentEvent, EventObserver};
use crate::index::WorkspaceIndexManagerActor;
use crate::middleware::CompositeDriver;
use crate::send_agent::SendAgent;
use crate::session::store::LLMConfig;
use crate::tools::ToolRegistry;
use agent_client_protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Client, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SetSessionModelRequest, SetSessionModelResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use querymt::LLMParams;
use std::any::Any;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, broadcast};

/// Lightweight facade replacing `Arc<QueryMTAgent>` for all consumers.
///
/// Holds shared config, the kameo session registry, and connection-level
/// mutable state. Not an actor — just a convenient bundle.
pub struct AgentHandle {
    pub config: Arc<AgentConfig>,
    pub registry: Arc<Mutex<SessionRegistry>>,

    // Connection-level mutable state
    pub client_state: Arc<StdMutex<Option<ClientState>>>,
    pub client: Arc<StdMutex<Option<Arc<dyn Client + Send + Sync>>>>,
    pub bridge: Arc<StdMutex<Option<ClientBridgeSender>>>,

    // Mutable default mode (UI "set agent mode" → affects new sessions)
    pub default_mode: StdMutex<AgentMode>,

    /// Handle to the kameo mesh swarm, set after `bootstrap_mesh()` succeeds.
    /// `None` in local-only mode. Wrapped in a `Mutex` for interior mutability
    /// so startup code can set it on the shared `Arc<AgentHandle>`.
    #[cfg(feature = "remote")]
    pub mesh: StdMutex<Option<crate::agent::remote::MeshHandle>>,
}

impl AgentHandle {
    /// Construct an `AgentHandle` from a shared `AgentConfig`.
    ///
    /// This is the canonical way to create an `AgentHandle` after building
    /// an `AgentConfig` via `AgentConfigBuilder::build()`.
    pub fn from_config(config: Arc<AgentConfig>) -> Self {
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        Self {
            config,
            registry,
            client_state: Arc::new(StdMutex::new(None)),
            client: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            default_mode: StdMutex::new(crate::agent::core::AgentMode::Build),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
        }
    }

    /// Subscribes to agent events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.config.event_bus.subscribe()
    }

    /// Adds an event observer after agent creation.
    pub fn add_observer(&self, observer: Arc<dyn EventObserver>) {
        self.config.event_bus.add_observer(observer);
    }

    /// Access the underlying event bus.
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.config.event_bus.clone()
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    /// Access the tool registry for built-in tool execution.
    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.config.tool_registry_arc()
    }

    /// Access the pending elicitations map for resolving tool and MCP server elicitation requests.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.config.pending_elicitations()
    }

    /// Access the workspace manager actor ref.
    pub fn workspace_manager_actor(&self) -> ActorRef<WorkspaceIndexManagerActor> {
        self.config.workspace_manager_actor()
    }

    /// Sets the client for protocol communication.
    pub fn set_client(&self, client: Arc<dyn Client + Send + Sync>) {
        if let Ok(mut handle) = self.client.lock() {
            *handle = Some(client);
        }
    }

    /// Sets the client bridge for ACP stdio communication.
    pub fn set_bridge(&self, bridge: ClientBridgeSender) {
        if let Ok(mut handle) = self.bridge.lock() {
            *handle = Some(bridge);
        }
    }

    /// Emits an event for external observers.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    /// Gracefully shutdown the agent and all background tasks.
    pub async fn shutdown(&self) {
        log::info!("AgentHandle: Starting graceful shutdown");

        // Shutdown event bus (abort all observer tasks)
        self.config.event_bus.shutdown().await;

        // Wait briefly for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        log::info!("AgentHandle: Shutdown complete");
    }

    /// Switch provider and model for a session (simple form)
    pub async fn set_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), Error> {
        // Preserve the system prompt when switching models
        let system_prompt = self.get_session_system_prompt(session_id).await;

        let mut config = LLMParams::new().provider(provider).model(model);

        // Add system prompt to config
        for prompt_part in system_prompt {
            config = config.system(prompt_part);
        }

        self.set_llm_config(session_id, config).await
    }

    /// Helper method to extract system prompt from current session config
    async fn get_session_system_prompt(&self, session_id: &str) -> Vec<String> {
        // Try to get the current session's LLM config
        if let Ok(Some(current_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
        {
            // Try to extract system prompt from params JSON
            if let Some(params) = &current_config.params
                && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
            {
                // Parse the array of strings
                let mut system_parts = Vec::new();
                for item in system_array {
                    if let Some(s) = item.as_str() {
                        system_parts.push(s.to_string());
                    }
                }
                if !system_parts.is_empty() {
                    return system_parts;
                }
            }
        }

        // Fall back to initial_config system prompt
        self.config.provider.initial_config().system.clone()
    }

    /// Switch provider configuration for a session (advanced form)
    pub async fn set_llm_config(&self, session_id: &str, config: LLMParams) -> Result<(), Error> {
        use crate::error::AgentError;
        let provider_name = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::from(AgentError::ProviderRequired))?;

        if self
            .config
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(Error::from(AgentError::UnknownProvider {
                name: provider_name.clone(),
            }));
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(session_id, llm_config.id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        // Fetch context limit from model info
        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.emit_event(
            session_id,
            crate::events::AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
                provider_node: None,
            },
        );
        Ok(())
    }

    /// Get current LLM config for a session
    pub async fn get_session_llm_config(
        &self,
        session_id: &str,
    ) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Creates a CompositeDriver from the configured middleware drivers
    pub fn create_driver(&self) -> CompositeDriver {
        self.config.create_driver()
    }

    /// Returns the session limits from configured middleware
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.config.get_session_limits()
    }

    /// Builds delegation metadata for ACP AgentCapabilities._meta field
    pub fn build_delegation_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        self.config.build_delegation_meta()
    }

    /// Undo: revert filesystem to state at the given message_id.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.undo(message_id.to_string()).await
    }

    /// Redo: re-apply the next change in the redo stack.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn redo(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::undo::RedoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.redo().await
    }

    // ── Remote session management (requires `remote` feature) ─────────────────

    /// List discovered peers in the kameo mesh.
    ///
    /// Looks up all `RemoteNodeManager` instances registered under
    /// `"node_manager"` in the Kademlia DHT and calls `GetNodeInfo` on each.
    /// Requires a bootstrapped swarm (`--mesh` flag).
    ///
    /// Without a swarm or with no peers, returns an empty list.
    /// Returns a clone of the `MeshHandle` if the mesh is active.
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Activate the mesh by storing the `MeshHandle` returned by `bootstrap_mesh()`.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = Some(mesh);
    }

    #[cfg(feature = "remote")]
    pub async fn list_remote_nodes(&self) -> Vec<crate::agent::remote::NodeInfo> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::StreamExt;

        let Some(mesh) = self.mesh() else {
            log::debug!("list_remote_nodes: mesh not bootstrapped");
            return Vec::new();
        };

        let local_peer_id = *mesh.peer_id();
        let mut stream = mesh.lookup_all_actors::<RemoteNodeManager>("node_manager");
        let mut nodes = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(node_manager_ref) => {
                    // Skip our own node — it is present in the DHT but is not
                    // a remote peer from the local user's perspective.
                    let peer_id = node_manager_ref.id().peer_id().copied();
                    if peer_id == Some(local_peer_id) {
                        log::debug!("list_remote_nodes: skipping local node");
                        continue;
                    }

                    // Skip peers that mDNS has expired but whose DHT records
                    // haven't been purged yet.
                    if let Some(pid) = peer_id {
                        if !mesh.is_peer_alive(&pid) {
                            log::debug!(
                                "list_remote_nodes: skipping stale DHT record for peer {pid}"
                            );
                            continue;
                        }
                    }

                    // Use a timeout to avoid blocking on a dead peer whose DHT
                    // record somehow survived the known_peers filter.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                    )
                    .await
                    {
                        Ok(Ok(info)) => {
                            log::debug!("list_remote_nodes: found node '{}'", info.hostname);
                            nodes.push(info);
                        }
                        Ok(Err(e)) => {
                            log::warn!("list_remote_nodes: GetNodeInfo failed: {}", e)
                        }
                        Err(_) => {
                            log::warn!(
                                "list_remote_nodes: GetNodeInfo timed out for peer {:?}",
                                peer_id
                            );
                        }
                    }
                }
                Err(e) => log::warn!("list_remote_nodes: lookup error: {}", e),
            }
        }

        nodes
    }

    /// Find a `RemoteNodeManager` by its node label (hostname).
    ///
    /// Iterates all registered `RemoteNodeManager` actors via DHT lookup and
    /// returns the one whose `GetNodeInfo.hostname` matches the given label.
    #[cfg(feature = "remote")]
    pub async fn find_node_manager(
        &self,
        label: &str,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::StreamExt;

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        let local_peer_id = *mesh.peer_id();
        let mut stream = mesh.lookup_all_actors::<RemoteNodeManager>("node_manager");

        while let Some(result) = stream.next().await {
            match result {
                Ok(node_manager_ref) => {
                    // Never return our own node manager — the caller is looking
                    // for a genuinely remote peer.
                    let peer_id = node_manager_ref.id().peer_id().copied();
                    if peer_id == Some(local_peer_id) {
                        continue;
                    }
                    // Skip stale DHT records for expired peers.
                    if let Some(pid) = peer_id {
                        if !mesh.is_peer_alive(&pid) {
                            log::debug!(
                                "find_node_manager: skipping stale DHT record for peer {pid}"
                            );
                            continue;
                        }
                    }
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                    )
                    .await
                    {
                        Ok(Ok(info)) if info.hostname == label => {
                            return Ok(node_manager_ref);
                        }
                        Ok(Ok(_)) => continue,
                        Ok(Err(e)) => {
                            log::warn!("find_node_manager: GetNodeInfo failed: {}", e);
                            continue;
                        }
                        Err(_) => {
                            log::warn!(
                                "find_node_manager: GetNodeInfo timed out for peer {:?}",
                                peer_id
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    log::warn!("find_node_manager: lookup error: {}", e);
                    continue;
                }
            }
        }

        Err(agent_client_protocol::Error::from(
            AgentError::RemoteSessionNotFound {
                details: format!(
                    "Remote node '{}' not found in the mesh. Available nodes can be listed via list_remote_nodes.",
                    label
                ),
            },
        ))
    }

    /// List sessions on a specific remote node.
    ///
    /// Sends `ListRemoteSessions` to the `RemoteNodeManager` registered under
    /// `node_manager_name` in the Kademlia DHT.
    ///
    /// Requires a bootstrapped swarm (Phase 6). Returns an error if the node
    /// is not reachable or has no registered `RemoteNodeManager`.
    #[cfg(feature = "remote")]
    pub async fn list_remote_sessions(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
    ) -> Result<Vec<crate::agent::remote::RemoteSessionInfo>, agent_client_protocol::Error> {
        use crate::agent::remote::ListRemoteSessions;
        use crate::error::AgentError;
        node_manager_ref
            .ask(&ListRemoteSessions)
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    /// Create a session on a remote node and attach it to the local registry.
    ///
    /// 1. Sends `CreateRemoteSession` to the remote `RemoteNodeManager`
    /// 2. Gets back `(session_id, actor_id)` for the new remote `SessionActor`
    /// 3. Looks up `RemoteActorRef<SessionActor>` by the session-scoped DHT name
    ///    that `RemoteNodeManager` registered under `"session::{session_id}"`.
    /// 4. Calls `attach_remote_session` on the local registry to insert it
    ///    and set up event relay
    ///
    /// Returns the new session's ID and a `SessionActorRef` for it.
    #[cfg(feature = "remote")]
    pub async fn create_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        peer_label: String,
        cwd: Option<String>,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), agent_client_protocol::Error> {
        use crate::agent::remote::CreateRemoteSession;
        use crate::agent::session_actor::SessionActor;

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        let resp = node_manager_ref
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| {
                agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string()))
            })?;

        let session_id = resp.session_id.clone();

        // Resolve the RemoteActorRef<SessionActor> via DHT lookup.
        // RemoteNodeManager registers the session under "session::{session_id}"
        // in its CreateRemoteSession handler once the swarm is up.
        let dht_name = format!("session::{}", session_id);
        let remote_session_ref = mesh
            .lookup_actor::<SessionActor>(dht_name.clone())
            .await
            .map_err(|e| {
                agent_client_protocol::Error::from(AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })
            })?
            .ok_or_else(|| {
                agent_client_protocol::Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} (actor_id={}) not found in DHT under '{}'; \
                     remote node may not have registered it yet",
                        session_id, resp.actor_id, dht_name
                    ),
                })
            })?;

        // Attach to local registry (spawns EventRelayActor, sends SubscribeEvents)
        let mut registry = self.registry.lock().await;
        let session_actor_ref = registry
            .attach_remote_session(
                session_id.clone(),
                remote_session_ref,
                peer_label,
                Some(mesh),
            )
            .await;

        log::info!(
            "create_remote_session: attached {} from DHT lookup '{}'",
            session_id,
            dht_name
        );

        Ok((session_id, session_actor_ref))
    }

    /// Attach an existing remote session (already has a `RemoteActorRef`) to
    /// the local registry.
    ///
    /// This is the lower-level entry point used when the caller already has a
    /// `RemoteActorRef<SessionActor>` (e.g., obtained via swarm lookup after
    /// Phase 6 bootstrap).
    #[cfg(feature = "remote")]
    pub async fn attach_remote_session(
        &self,
        session_id: String,
        remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        peer_label: String,
    ) -> crate::agent::remote::SessionActorRef {
        let mesh = self.mesh();
        let mut registry = self.registry.lock().await;
        registry
            .attach_remote_session(session_id, remote_ref, peer_label, mesh)
            .await
    }
}

/// SendAgent implementation for AgentHandle
///
/// All methods delegate to either the kameo session registry or the shared config.
/// This replaces the `impl SendAgent for QueryMTAgent` from protocol.rs.
#[async_trait]
impl SendAgent for AgentHandle {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        use agent_client_protocol::{
            AgentCapabilities, Implementation, McpCapabilities, PromptCapabilities, ProtocolVersion,
        };

        let protocol_version = if req.protocol_version <= ProtocolVersion::LATEST {
            req.protocol_version
        } else {
            ProtocolVersion::LATEST
        };

        if let Ok(mut state) = self.client_state.lock() {
            *state = Some(ClientState {
                protocol_version: protocol_version.clone(),
                client_capabilities: req.client_capabilities.clone(),
                client_info: req.client_info.clone(),
                authenticated: false,
            });
        }

        let auth_methods = self.config.auth_methods.clone();

        let mut capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true));

        // Add delegation metadata if agent registry is available
        if let Some(delegation_meta) = self.build_delegation_meta() {
            capabilities = capabilities.meta(delegation_meta);
        }

        Ok(InitializeResponse::new(protocol_version)
            .agent_capabilities(capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("querymt-agent", env!("CARGO_PKG_VERSION"))
                    .title("QueryMT Agent"),
            ))
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        let auth_methods = &self.config.auth_methods;

        if !auth_methods.is_empty() && !auth_methods.iter().any(|m| m.id == req.method_id) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown auth method",
                "methodId": req.method_id.to_string(),
            })));
        }

        if let Ok(mut state) = self.client_state.lock()
            && let Some(state) = state.as_mut()
        {
            state.authenticated = true;
        }
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        // Auth check stays on AgentHandle (connection-level concern)
        if let Ok(state) = self.client_state.lock()
            && let Some(state) = state.as_ref()
        {
            let auth_required = !self.config.auth_methods.is_empty();

            if auth_required && !state.authenticated {
                return Err(Error::auth_required());
            }
        }

        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.new_session(req).await
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = session_ref {
            let _ = session_ref.cancel().await;
        } else {
            log::warn!(
                "Cancel requested for session {} but not found in registry",
                session_id
            );
        }
        Ok(())
    }

    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.load_session(req).await
    }

    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        // Delegate to kameo SessionRegistry
        let registry = self.registry.lock().await;
        registry.list_sessions(req).await
    }

    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let registry = self.registry.lock().await;
        registry.fork_session(req).await
    }

    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.resume_session(req).await
    }

    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_session_model(req).await
    }

    async fn ext_method(&self, _req: ExtRequest) -> Result<ExtResponse, Error> {
        // Return empty response - extensions not yet implemented
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| Error::from(crate::error::AgentError::Serialization(e.to_string())))?;
        Ok(ExtResponse::new(Arc::from(raw_value)))
    }

    async fn ext_notification(&self, _notif: ExtNotification) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
