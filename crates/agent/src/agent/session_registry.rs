//! Session registry - manages session actors.
//!
//! Lives on the server layer. Not an actor — just a plain data structure
//! protected by a mutex (acceptable: only accessed for routing, not during execution).

use crate::acp::cwd::acp_cwd_to_optional;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::remote::SessionActorRef;
use crate::agent::session_actor::SessionActor;
use crate::error::AgentError;
use crate::events::AgentEventKind;
use agent_client_protocol::schema::{
    Error, ListSessionsRequest, ListSessionsResponse, McpServer, NewSessionRequest,
    NewSessionResponse, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionInfo, SessionMode, SessionModeState,
};
use kameo::actor::{ActorRef, Spawn};
use rmcp::RoleClient;
use rmcp::service::Peer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A pre-connected MCP peer that should be merged into session tool state.
///
/// The string is the MCP server name used for adapter metadata and diagnostics.
/// The peer is already initialized (has completed MCP handshake) and can be
/// reused across multiple sessions without re-initializing.
pub type PreconnectedMcpPeer = (String, Peer<RoleClient>);

fn all_session_modes() -> Vec<SessionMode> {
    vec![
        SessionMode::new("build", "Build").description("Full read/write mode"),
        SessionMode::new("plan", "Plan").description("Read-only planning mode"),
        SessionMode::new("review", "Review").description("Read-only review mode"),
    ]
}

fn mode_state(mode: AgentMode) -> SessionModeState {
    SessionModeState::new(mode.as_str(), all_session_modes())
}

/// Build the full set of session configuration options for the given mode and reasoning effort.
///
/// This is the single source of truth for config option shape — used by session creation,
/// set_session_config_option responses, and config_option_update notifications.
pub(crate) fn config_options(
    mode: AgentMode,
    reasoning_effort: Option<querymt::chat::ReasoningEffort>,
) -> Vec<SessionConfigOption> {
    let effort_value = reasoning_effort
        .map(|e| e.to_string())
        .unwrap_or_else(|| "auto".to_string());
    vec![
        SessionConfigOption::select(
            "mode",
            "Session Mode",
            mode.as_str(),
            vec![
                SessionConfigSelectOption::new("build", "Build")
                    .description("Full read/write mode"),
                SessionConfigSelectOption::new("plan", "Plan")
                    .description("Read-only planning mode"),
                SessionConfigSelectOption::new("review", "Review")
                    .description("Read-only review mode"),
            ],
        )
        .description("Controls how the agent operates for this session")
        .category(SessionConfigOptionCategory::Mode),
        SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            effort_value,
            vec![
                SessionConfigSelectOption::new("auto", "Auto")
                    .description("Use model-specific defaults"),
                SessionConfigSelectOption::new("low", "Low")
                    .description("Minimal thinking, fastest responses"),
                SessionConfigSelectOption::new("medium", "Medium").description("Balanced thinking"),
                SessionConfigSelectOption::new("high", "High").description("Thorough thinking"),
                SessionConfigSelectOption::new("max", "Max")
                    .description("Deepest thinking, highest budget"),
            ],
        )
        .description("Controls reasoning depth for this session")
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]
}

/// Controls how a materialized session actor is integrated with remote infra.
pub struct SessionMaterializationOptions {
    pub attach_mesh_handle: bool,
    pub register_in_dht: bool,
}

/// Manages session actors. Lives on the server layer.
pub struct SessionMaterialization {
    pub actor_ref: ActorRef<SessionActor>,
    pub runtime: Arc<SessionRuntime>,
}

pub struct SessionRegistry {
    pub config: Arc<AgentConfig>,
    sessions: HashMap<String, SessionActorRef>,
    local_actor_refs: HashMap<String, ActorRef<SessionActor>>,
    /// Tracks the `(relay_actor_id, relay_dht_name)` spawned for each remote
    /// session so that `detach_remote_session` can send `UnsubscribeEvents`
    /// with both values.
    #[cfg(feature = "remote")]
    relay_actor_ids: HashMap<String, (u64, String)>,
    /// Mesh handle for cleaning up re-registration closures when sessions
    /// are removed (Phase 4 of Bug 1 fix).
    #[cfg(feature = "remote")]
    mesh: Option<crate::agent::remote::MeshHandle>,
    /// Client bridge for workspace queries and notifications. Set once the ACP
    /// connection is established. Propagated to new session actors via `SetBridge`
    /// so that tools like `language_query` can access the client's language server.
    bridge: Option<crate::acp::client_bridge::ClientBridgeSender>,
}

impl SessionRegistry {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
            local_actor_refs: HashMap::new(),
            #[cfg(feature = "remote")]
            relay_actor_ids: HashMap::new(),
            #[cfg(feature = "remote")]
            mesh: None,
            bridge: None,
        }
    }

    /// Set the client bridge for ACP communication.
    ///
    /// When set, newly created sessions will receive the bridge via `SetBridge`
    /// so that session actors can send notifications and tools like
    /// `language_query` can access the client's language server.
    pub fn set_bridge(&mut self, bridge: crate::acp::client_bridge::ClientBridgeSender) {
        self.bridge = Some(bridge);
    }

    /// Set the mesh handle so that `remove()` and `detach_remote_session()`
    /// can deregister actors from the re-registration map.
    ///
    /// When a mesh handle is provided, all existing **local** sessions in the
    /// registry are registered in the DHT so that remote peers can discover
    /// and attach to them.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&mut self, mesh: Option<crate::agent::remote::MeshHandle>) {
        // Register all existing local sessions in DHT so remote peers can attach.
        if let Some(ref mesh) = mesh {
            for (session_id, actor_ref) in &self.local_actor_refs {
                let dht_name = crate::agent::remote::dht_name::session(session_id);
                let mesh = mesh.clone();
                let actor_ref = actor_ref.clone();
                tokio::spawn(async move {
                    mesh.register_actor(actor_ref, dht_name).await;
                });
            }
        }
        self.mesh = mesh;
    }

    /// Merge MCP servers from the agent config with any client-supplied servers.
    ///
    /// Config servers act as defaults; client-supplied servers with the same name
    /// take precedence (client wins).
    fn merged_mcp_servers(&self, req_servers: &[McpServer]) -> Vec<McpServer> {
        // Start with config servers converted to ACP format.
        let mut merged: Vec<McpServer> =
            self.config.mcp_servers.iter().map(|s| s.to_acp()).collect();

        // For each client-supplied server, replace any config server with the same
        // name or append it if not present.
        for req_server in req_servers {
            let req_name = match req_server {
                McpServer::Stdio(s) => s.name.as_str(),
                McpServer::Http(s) => s.name.as_str(),
                _ => continue,
            };
            if let Some(pos) = merged.iter().position(|s| match s {
                McpServer::Stdio(cs) => cs.name == req_name,
                McpServer::Http(cs) => cs.name == req_name,
                _ => false,
            }) {
                merged[pos] = req_server.clone();
            } else {
                merged.push(req_server.clone());
            }
        }

        merged
    }

    /// Get a reference to the session actor for routing.
    pub fn get(&self, session_id: &str) -> Option<&SessionActorRef> {
        self.sessions.get(session_id)
    }

    /// Insert a pre-spawned session actor into the registry.
    ///
    /// Accepts anything that converts into a `SessionActorRef`, including
    /// a bare `ActorRef<SessionActor>` (via the `From` impl).
    pub fn insert(&mut self, session_id: String, actor_ref: impl Into<SessionActorRef>) {
        self.sessions.insert(session_id.clone(), actor_ref.into());
        self.local_actor_refs.remove(&session_id);
    }

    pub fn local_actor_ref(&self, session_id: &str) -> Option<&ActorRef<SessionActor>> {
        self.local_actor_refs.get(session_id)
    }

    pub async fn materialize_session_actor(
        &mut self,
        session_id: String,
        cwd: Option<PathBuf>,
        mcp_servers: &[McpServer],
        initialize_fork: bool,
        options: &mut SessionMaterializationOptions,
    ) -> Result<SessionMaterialization, Error> {
        let merged_mcp = self.merged_mcp_servers(mcp_servers);
        let tool_state = crate::agent::core::McpToolState::empty();
        let mcp_services = crate::agent::protocol::build_mcp_state(
            &merged_mcp,
            self.config.pending_elicitations(),
            self.config.event_sink.clone(),
            session_id.clone(),
            &crate::agent::mcp::agent_implementation(),
            tool_state.clone(),
        )
        .await?;

        if initialize_fork {
            crate::session::runtime::SessionForkHelper::initialize_fork(
                self.config.provider.history_store(),
                &session_id,
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        }

        let runtime = SessionRuntime::new(cwd.clone(), mcp_services, tool_state);
        #[cfg(feature = "remote")]
        let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone())
            .with_mesh(if options.attach_mesh_handle {
                self.mesh.clone()
            } else {
                None
            });
        #[cfg(not(feature = "remote"))]
        let actor = { SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone()) };
        let actor_ref = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::from(actor_ref.clone());

        if let Some(ref bridge) = self.bridge
            && let Err(e) = session_ref.set_bridge(bridge.clone()).await
        {
            log::warn!(
                "Session {}: failed to set bridge on session actor: {}",
                session_id,
                e
            );
        }

        self.sessions
            .insert(session_id.clone(), session_ref.clone());
        self.local_actor_refs
            .insert(session_id.clone(), actor_ref.clone());

        #[cfg(feature = "remote")]
        if options.register_in_dht
            && let Some(ref mesh) = self.mesh
        {
            let dht_name = crate::agent::remote::dht_name::session(&session_id);
            let mesh = mesh.clone();
            let actor_ref = actor_ref.clone();
            tokio::spawn(async move {
                mesh.register_actor(actor_ref, dht_name).await;
            });
        }

        Ok(SessionMaterialization { actor_ref, runtime })
    }

    /// Remove a session actor from the registry.
    ///
    /// Also deregisters the session's re-registration closure from the mesh
    /// (if available) so dead actors don't accumulate (Phase 4 of Bug 1 fix).
    pub fn remove(&mut self, session_id: &str) -> Option<SessionActorRef> {
        #[cfg(feature = "remote")]
        if let Some(ref mesh) = self.mesh {
            let session_dht_name = crate::agent::remote::dht_name::session(session_id);
            mesh.deregister_actor(&session_dht_name);
        }
        self.local_actor_refs.remove(session_id);
        self.sessions.remove(session_id)
    }

    /// List all session IDs in the registry.
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Return `(session_id, peer_label)` pairs for all remote sessions.
    ///
    /// Used by the UI session-list handler to include remote sessions in the
    /// session picker alongside local (persisted) sessions.
    #[cfg(feature = "remote")]
    pub fn remote_sessions(&self) -> Vec<(String, String)> {
        self.sessions
            .iter()
            .filter_map(|(id, r)| match r {
                SessionActorRef::Remote { peer_label, .. } => {
                    Some((id.clone(), peer_label.clone()))
                }
                SessionActorRef::Local(_) => None,
            })
            .collect()
    }

    /// Attach a remote session to this registry.
    ///
    /// Wraps the remote actor ref in a `SessionActorRef::Remote`, spawns a local
    /// `EventRelayActor`, registers it in the swarm (via `into_remote_ref`), and
    /// sends `SubscribeEvents` to the remote `SessionActor` so events stream back.
    ///
    /// # Event relay
    ///
    /// The `EventRelayActor` is spawned locally and its `ActorId` is sent to the
    /// remote session via `SubscribeEvents`. The remote handler constructs a
    /// `RemoteActorRef<EventRelayActor>` from that id (requires swarm to be
    /// bootstrapped — Phase 6). Until then the relay is spawned but the
    /// `SubscribeEvents` call returns Ok without installing the forwarder.
    ///
    /// # Returns
    ///
    /// The `SessionActorRef::Remote` for the attached session.
    #[cfg(feature = "remote")]
    pub async fn attach_remote_session(
        &mut self,
        session_id: String,
        remote_ref: kameo::actor::RemoteActorRef<SessionActor>,
        peer_label: String,
        mesh: Option<crate::agent::remote::MeshHandle>,
        remote_node_id: Option<String>,
    ) -> SessionActorRef {
        log::debug!(
            "attach_remote_session: called for session_id={} peer='{}' \
             (registry currently has {} session(s))",
            session_id,
            peer_label,
            self.sessions.len(),
        );
        use crate::agent::remote::{EventRelayActor, SessionActorRef};
        use kameo::actor::Spawn;

        // 1. Wrap in SessionActorRef::Remote
        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_ref,
            peer_label: peer_label.clone(),
        };

        // 2. Spawn a local EventRelayActor for this session.
        //    It persists durable remote events to the journal and publishes to fanout.
        let relay_actor = EventRelayActor::new(self.config.event_sink.clone(), peer_label.clone());
        let relay_ref = EventRelayActor::spawn(relay_actor);
        let relay_id = relay_ref.id().sequence_id();

        // 3. Register the relay in REMOTE_REGISTRY + DHT so the remote
        //    SessionActor can look it up by name and install an EventForwarder.
        //    Use peer-scoped name so multiple peers can attach to the same
        //    session without overwriting each other's relay (Bug 3 fix).
        let mesh_active = mesh.is_some();
        let relay_dht_name = if let Some(ref mesh) = mesh {
            let name = crate::agent::remote::dht_name::event_relay(&session_id, mesh.peer_id());
            mesh.register_actor(relay_ref.clone(), name.clone()).await;
            name
        } else {
            log::debug!(
                "attach_remote_session: no mesh, DHT registration skipped for relay (session {})",
                session_id
            );
            // Fallback name when there is no mesh (shouldn't happen in practice
            // but keeps the type system happy).
            format!("event_relay::{}::local", session_id)
        };

        // 4. Send SubscribeEvents to the remote session.
        //    The remote SubscribeEvents handler uses mesh.lookup_actor to find
        //    the relay and install an EventForwarder on its EventBus.
        if let Err(e) = session_ref
            .subscribe_events(relay_id, relay_dht_name.clone())
            .await
        {
            log::warn!(
                "attach_remote_session: SubscribeEvents failed for {} (event relay may not be active): {}",
                session_id,
                e
            );
        }

        // 5. Insert into registry and track relay id + dht name for later cleanup.
        self.sessions
            .insert(session_id.clone(), session_ref.clone());
        self.relay_actor_ids
            .insert(session_id.clone(), (relay_id, relay_dht_name));

        // Persist a bookmark so this remote session survives server restart.
        if let Some(node_id) = remote_node_id {
            let title = self
                .config
                .provider
                .history_store()
                .get_session(&session_id)
                .await
                .ok()
                .flatten()
                .and_then(|session| session.name);
            let bookmark = crate::session::store::RemoteSessionBookmark {
                session_id: session_id.clone(),
                node_id,
                peer_label: peer_label.clone(),
                cwd: None,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                title,
            };
            let store = self.config.provider.history_store();
            tokio::spawn(async move {
                if let Err(e) = store.save_remote_session_bookmark(&bookmark).await {
                    log::warn!("Failed to persist remote session bookmark: {}", e);
                }
            });
        }

        log::info!(
            "Attached remote session {} from {} (relay_actor_id={}, event relay {})",
            session_id,
            peer_label,
            relay_id,
            if mesh_active {
                "active"
            } else {
                "pending mesh bootstrap"
            }
        );

        session_ref
    }

    #[cfg(feature = "remote")]
    async fn detach_remote_session_inner(
        &mut self,
        session_id: &str,
        preserve_bookmark: bool,
    ) -> Option<SessionActorRef> {
        // Send UnsubscribeEvents before removing so the remote forwarder is aborted.
        if let (Some(session_ref), Some((relay_id, relay_dht_name))) = (
            self.sessions.get(session_id),
            self.relay_actor_ids.get(session_id).cloned(),
        ) && session_ref.is_remote()
        {
            if let Err(e) = session_ref
                .unsubscribe_events(relay_id, relay_dht_name.clone())
                .await
            {
                log::warn!(
                    "detach_remote_session: UnsubscribeEvents failed for {} (relay_actor_id={}, relay_dht_name={}): {}",
                    session_id,
                    relay_id,
                    relay_dht_name,
                    e
                );
            } else {
                log::info!(
                    "detach_remote_session: sent UnsubscribeEvents for {} (relay_actor_id={}, relay_dht_name={})",
                    session_id,
                    relay_id,
                    relay_dht_name,
                );
            }
        }

        // Deregister the session and relay actors from the re-registration map
        // so dead closures don't accumulate (Phase 4 of Bug 1 fix).
        if let Some(ref mesh) = self.mesh {
            let session_dht_name = crate::agent::remote::dht_name::session(session_id);
            mesh.deregister_actor(&session_dht_name);

            if let Some((_, relay_name)) = self.relay_actor_ids.get(session_id) {
                mesh.deregister_actor(relay_name);
            }
        }

        if !preserve_bookmark {
            let store = self.config.provider.history_store();
            let sid = session_id.to_string();
            tokio::spawn(async move {
                if let Err(e) = store.remove_remote_session_bookmark(&sid).await {
                    log::warn!("Failed to remove remote session bookmark {}: {}", sid, e);
                }
            });
        }

        self.relay_actor_ids.remove(session_id);
        self.local_actor_refs.remove(session_id);
        self.sessions.remove(session_id)
    }

    /// Detach a remote session: send `UnsubscribeEvents` to stop the remote
    /// `EventForwarder`, then remove the session from the registry.
    ///
    /// This is the counterpart to [`attach_remote_session`](Self::attach_remote_session).
    /// Call this instead of bare `remove()` for remote sessions so the
    /// forwarder task on the remote node is properly cleaned up.
    ///
    /// For local sessions (or if the session is not in the registry) this
    /// falls back to a plain `remove()`.
    #[cfg(feature = "remote")]
    pub async fn detach_remote_session(&mut self, session_id: &str) -> Option<SessionActorRef> {
        self.detach_remote_session_inner(session_id, false).await
    }

    /// Remove a remote session runtime from local tracking but keep its bookmark
    /// so the stopped session remains visible and resumable.
    #[cfg(feature = "remote")]
    pub async fn detach_remote_session_preserve_bookmark(
        &mut self,
        session_id: &str,
    ) -> Option<SessionActorRef> {
        self.detach_remote_session_inner(session_id, true).await
    }

    /// Create a new session: build runtime, spawn SessionActor, return session_id.
    pub async fn new_session(
        &mut self,
        req: NewSessionRequest,
    ) -> Result<NewSessionResponse, Error> {
        self.new_session_with_preconnected(req, Vec::new()).await
    }

    /// Create a new session and merge tools from already-connected MCP peers.
    ///
    /// Used by mobile FFI clients that manage MCP transport lifetimes externally
    /// (for example iOS/Android pipe transports) and want those tools available in
    /// each newly created session. The peers are already initialized and can be
    /// reused across sessions without re-initializing.
    pub async fn new_session_with_preconnected(
        &mut self,
        req: NewSessionRequest,
        preconnected_peers: Vec<PreconnectedMcpPeer>,
    ) -> Result<NewSessionResponse, Error> {
        let cwd = acp_cwd_to_optional(&req.cwd)?;

        let parent_session_id = req
            .meta
            .as_ref()
            .and_then(|m| m.get("parent_session_id"))
            .and_then(|v| v.as_str());

        let session_context = self
            .config
            .provider
            .create_session(
                cwd.clone(),
                parent_session_id,
                &self.config.execution_config_snapshot(),
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        let session_id = session_context.session().public_id.clone();

        let materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd.clone(),
                &req.mcp_servers,
                parent_session_id.is_some(),
                &mut SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;
        let runtime = materialization.runtime;

        // Merge tools from already-connected MCP peers (e.g. mobile pipe transports).
        crate::agent::protocol::merge_preconnected_mcp_peers(
            runtime.mcp_tool_state.clone(),
            &preconnected_peers,
        )
        .await?;

        self.config
            .emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        // Emit initial provider configuration
        if let Ok(Some(llm_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&session_id)
            .await
        {
            let context_limit =
                crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                    .and_then(|m| m.context_limit());
            self.config.emit_event(
                &session_id,
                crate::events::AgentEventKind::ProviderChanged {
                    provider: llm_config.provider.clone(),
                    model: llm_config.model.clone(),
                    config_id: llm_config.id,
                    context_limit,
                    provider_node_id: None,
                },
            );
        }

        // Background: initialize workspace index (only if the path exists on this machine)
        if let Some(ref cwd_path) = cwd {
            if cwd_path.exists() {
                let manager_actor = self.config.workspace_manager_actor.clone();
                let runtime_clone = runtime.clone();
                let cwd_owned = cwd_path.clone();
                tokio::spawn(async move {
                    let root = crate::index::resolve_workspace_root(&cwd_owned);
                    match manager_actor.ask(crate::index::GetOrCreate { root }).await {
                        Ok(handle) => {
                            let _ = runtime_clone.workspace_handle.set(handle);
                        }
                        Err(e) => log::warn!("Failed to initialize workspace index: {}", e),
                    }
                });
            } else {
                log::debug!(
                    "SessionRegistry: cwd {:?} does not exist, skipping workspace index",
                    cwd_path
                );
            }
        }

        // Emit SessionConfigured
        let mcp_configs: Vec<crate::config::McpServerConfig> = req
            .mcp_servers
            .iter()
            .map(crate::config::McpServerConfig::from_acp)
            .collect();
        self.config.emit_event(
            &session_id,
            crate::events::AgentEventKind::SessionConfigured {
                cwd,
                mcp_servers: mcp_configs,
                limits: self.config.get_session_limits(),
            },
        );

        let current_mode = {
            let session_ref = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| {
                    Error::internal_error().data("session actor missing after creation")
                })?
                .clone();
            session_ref.get_mode().await.map_err(Error::from)?
        };

        Ok(NewSessionResponse::new(session_id)
            .modes(mode_state(current_mode))
            .config_options(config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    /// Load an existing session: validate it exists, build runtime, spawn SessionActor.
    pub async fn load_session(
        &mut self,
        req: agent_client_protocol::schema::LoadSessionRequest,
    ) -> Result<agent_client_protocol::schema::LoadSessionResponse, Error> {
        self.load_session_with_preconnected(req, Vec::new()).await
    }

    /// Load an existing session and merge tools from already-connected MCP peers.
    pub async fn load_session_with_preconnected(
        &mut self,
        req: agent_client_protocol::schema::LoadSessionRequest,
        preconnected_peers: Vec<PreconnectedMcpPeer>,
    ) -> Result<agent_client_protocol::schema::LoadSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        let attached_remote = self
            .sessions
            .get(&session_id)
            .is_some_and(|session_ref| session_ref.is_remote());

        if attached_remote {
            let current_mode = {
                let session_ref = self
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| {
                        Error::internal_error().data("remote session missing after attach")
                    })?
                    .clone();
                session_ref.get_mode().await.map_err(Error::from)?
            };

            return Ok(agent_client_protocol::schema::LoadSessionResponse::new()
                .modes(mode_state(current_mode))
                .config_options(config_options(
                    current_mode,
                    **self.config.default_reasoning_effort.load(),
                )));
        }

        let _session = self
            .config
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
            })?;

        let cwd = acp_cwd_to_optional(&req.cwd)?;

        let materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd,
                &req.mcp_servers,
                false,
                &mut SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;

        // Merge tools from already-connected MCP peers (e.g. mobile pipe transports).
        crate::agent::protocol::merge_preconnected_mcp_peers(
            materialization.runtime.mcp_tool_state.clone(),
            &preconnected_peers,
        )
        .await?;

        self.config
            .emit_event(&session_id, AgentEventKind::SessionCreated);

        // Emit initial provider configuration so UI can display context limits
        if let Ok(Some(llm_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&session_id)
            .await
        {
            let context_limit =
                crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                    .and_then(|m| m.context_limit());
            self.config.emit_event(
                &session_id,
                crate::events::AgentEventKind::ProviderChanged {
                    provider: llm_config.provider.clone(),
                    model: llm_config.model.clone(),
                    config_id: llm_config.id,
                    context_limit,
                    provider_node_id: None,
                },
            );
        }

        let current_mode = {
            let session_ref = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::internal_error().data("session actor missing after load"))?
                .clone();
            session_ref.get_mode().await.map_err(Error::from)?
        };

        Ok(agent_client_protocol::schema::LoadSessionResponse::new()
            .modes(mode_state(current_mode))
            .config_options(config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    /// Fork an existing session at the latest message.
    pub async fn fork_session(
        &self,
        req: agent_client_protocol::schema::ForkSessionRequest,
    ) -> Result<agent_client_protocol::schema::ForkSessionResponse, Error> {
        let source_session_id = req.session_id.to_string();

        let _session = self
            .config
            .provider
            .history_store()
            .get_session(&source_session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "source session not found",
                    "session_id": source_session_id,
                }))
            })?;

        let history = self
            .config
            .provider
            .history_store()
            .get_history(&source_session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        let target_message_id = history
            .last()
            .map(|msg| msg.id.clone())
            .ok_or_else(|| Error::from(AgentError::EmptySessionFork))?;

        let new_session_id = self
            .config
            .provider
            .history_store()
            .fork_session(
                &source_session_id,
                &target_message_id,
                crate::session::domain::ForkOrigin::User,
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        Ok(agent_client_protocol::schema::ForkSessionResponse::new(
            new_session_id,
        ))
    }

    /// Resume an existing session without history replay.
    pub async fn resume_session(
        &mut self,
        req: agent_client_protocol::schema::ResumeSessionRequest,
    ) -> Result<agent_client_protocol::schema::ResumeSessionResponse, Error> {
        let session_id = req.session_id.to_string();
        let _session = self
            .config
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
            })?;

        let cwd = if req.cwd.as_os_str().is_empty() {
            None
        } else {
            if !req.cwd.is_absolute() {
                return Err(Error::invalid_params().data(serde_json::json!({
                    "message": "cwd must be an absolute path",
                    "cwd": req.cwd.display().to_string(),
                })));
            }
            Some(req.cwd.clone())
        };

        let _materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd,
                &req.mcp_servers,
                false,
                &mut SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;

        self.config
            .emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        let current_mode = {
            let session_ref = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::internal_error().data("session actor missing after resume"))?
                .clone();
            session_ref.get_mode().await.map_err(Error::from)?
        };

        Ok(agent_client_protocol::schema::ResumeSessionResponse::new()
            .modes(mode_state(current_mode))
            .config_options(config_options(
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )))
    }

    /// List all sessions (queries the store, not the actors).
    pub async fn list_sessions(
        &self,
        req: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, Error> {
        let sessions = self
            .config
            .provider
            .history_store()
            .list_sessions()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        let session_infos: Vec<SessionInfo> = sessions
            .into_iter()
            .map(|s| {
                let mut info = SessionInfo::new(
                    agent_client_protocol::schema::SessionId::from(s.public_id),
                    std::path::PathBuf::new(),
                );
                if let Some(name) = s.name {
                    info.title = Some(name);
                }
                if let Some(updated_at) = s.updated_at {
                    info.updated_at = Some(
                        updated_at
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                    );
                }
                info
            })
            .collect();

        let filtered_infos = if let Some(_cwd) = req.cwd {
            // TODO: Filter by cwd once we store cwd per session
            session_infos
        } else {
            session_infos
        };

        let start_idx = req
            .cursor
            .as_ref()
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = 100;
        let end_idx = (start_idx + limit).min(filtered_infos.len());
        let paginated = filtered_infos[start_idx..end_idx].to_vec();
        let next_cursor = if end_idx < filtered_infos.len() {
            Some(end_idx.to_string())
        } else {
            None
        };

        Ok(ListSessionsResponse::new(paginated).next_cursor(next_cursor))
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::agent::session_actor::SessionActor;
    use crate::session::backend::StorageBackend;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── Fixture ──────────────────────────────────────────────────────────────

    struct RegistryFixture {
        registry: SessionRegistry,
        _temp_dir: tempfile::TempDir,
    }

    impl RegistryFixture {
        async fn new() -> Self {
            let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
            let shared = SharedLlmProvider {
                inner: provider.clone(),
                tools: vec![].into_boxed_slice(),
            };
            let factory = Arc::new(TestProviderFactory { provider: shared });
            let (plugin_registry, temp_dir) =
                mock_plugin_registry(factory).expect("plugin registry");

            let mut store = MockSessionStore::new();
            let llm_config = mock_llm_config();
            let session = mock_session("test-session");
            store
                .expect_get_session()
                .returning(move |_| Ok(Some(session.clone())))
                .times(0..);
            store
                .expect_get_session_llm_config()
                .returning(move |_| Ok(Some(llm_config.clone())))
                .times(0..);
            store
                .expect_list_sessions()
                .returning(|| Ok(vec![]))
                .times(0..);

            let store: Arc<dyn SessionStore> = Arc::new(store);
            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store"),
            );

            let config = Arc::new(
                AgentConfigBuilder::new(
                    Arc::new(plugin_registry),
                    store.clone(),
                    storage.event_journal(),
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
            );

            Self {
                registry: SessionRegistry::new(config),
                _temp_dir: temp_dir,
            }
        }

        fn spawn_actor(&self) -> kameo::actor::ActorRef<SessionActor> {
            let runtime = crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            );
            let actor = SessionActor::new(
                self.registry.config.clone(),
                "test-session".to_string(),
                runtime,
            );
            SessionActor::spawn(actor)
        }
    }

    // ── Unit tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_new_registry_is_empty() {
        let f = RegistryFixture::new().await;
        assert!(f.registry.is_empty());
        assert_eq!(f.registry.len(), 0);
        assert!(f.registry.session_ids().is_empty());
    }

    #[test]
    fn test_mode_state_contains_all_supported_modes() {
        let state = mode_state(AgentMode::Build);
        assert_eq!(state.current_mode_id.0.as_ref(), "build");
        assert_eq!(state.available_modes.len(), 3);
    }

    #[test]
    fn test_config_options_include_mode_selector() {
        let options = config_options(AgentMode::Review, None);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id.0.as_ref(), "mode");

        let select = match &options[0].kind {
            agent_client_protocol::schema::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select mode option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "review");

        // Verify reasoning effort option is present
        assert_eq!(options[1].id.0.as_ref(), "reasoning_effort");
        assert_eq!(
            options[1].category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );
    }

    #[tokio::test]
    async fn test_get_nonexistent_session_returns_none() {
        let f = RegistryFixture::new().await;
        assert!(f.registry.get("no-such-session").is_none());
    }

    #[tokio::test]
    async fn test_insert_and_get_session() {
        let mut f = RegistryFixture::new().await;
        let actor_ref = f.spawn_actor();
        f.registry.insert("sess-1".to_string(), actor_ref);

        assert!(!f.registry.is_empty());
        assert_eq!(f.registry.len(), 1);
        assert!(f.registry.get("sess-1").is_some());
        assert!(f.registry.get("sess-2").is_none());
    }

    #[tokio::test]
    async fn test_insert_multiple_sessions() {
        let mut f = RegistryFixture::new().await;
        for i in 0..5 {
            let actor_ref = f.spawn_actor();
            f.registry.insert(format!("sess-{i}"), actor_ref);
        }
        assert_eq!(f.registry.len(), 5);
        let ids = f.registry.session_ids();
        assert_eq!(ids.len(), 5);
        for i in 0..5 {
            assert!(ids.contains(&format!("sess-{i}")));
        }
    }

    #[tokio::test]
    async fn test_remove_existing_session() {
        let mut f = RegistryFixture::new().await;
        let actor_ref = f.spawn_actor();
        f.registry.insert("sess-1".to_string(), actor_ref);
        assert_eq!(f.registry.len(), 1);

        let removed = f.registry.remove("sess-1");
        assert!(removed.is_some());
        assert!(f.registry.is_empty());
    }

    #[tokio::test]
    async fn test_remove_nonexistent_session_returns_none() {
        let mut f = RegistryFixture::new().await;
        let removed = f.registry.remove("no-such-session");
        assert!(removed.is_none());
        assert!(f.registry.is_empty());
    }

    #[tokio::test]
    async fn test_overwrite_existing_session_id() {
        let mut f = RegistryFixture::new().await;
        let a1 = f.spawn_actor();
        let a2 = f.spawn_actor();
        f.registry.insert("sess-1".to_string(), a1);
        f.registry.insert("sess-1".to_string(), a2);
        // Still one entry, not two
        assert_eq!(f.registry.len(), 1);
        assert!(f.registry.get("sess-1").is_some());
    }

    #[tokio::test]
    async fn test_session_ids_reflects_inserts_and_removes() {
        let mut f = RegistryFixture::new().await;
        let a1 = f.spawn_actor();
        let a2 = f.spawn_actor();
        f.registry.insert("alpha".to_string(), a1);
        f.registry.insert("beta".to_string(), a2);

        let ids = f.registry.session_ids();
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));

        f.registry.remove("alpha");
        let ids = f.registry.session_ids();
        assert!(!ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
    }

    #[tokio::test]
    async fn test_list_sessions_empty_store() {
        let f = RegistryFixture::new().await;
        let req = ListSessionsRequest::new();
        let resp = f.registry.list_sessions(req).await.expect("list_sessions");
        assert!(resp.sessions.is_empty());
    }

    #[tokio::test]
    async fn test_fork_session_empty_history_fails() {
        let _f = RegistryFixture::new().await;

        // Override: get_history returns empty vec, get_session returns Some
        let mut store2 = MockSessionStore::new();
        let session = mock_session("source-session");
        store2
            .expect_get_session()
            .returning(move |_| Ok(Some(session.clone())))
            .times(0..);
        store2
            .expect_get_history()
            .returning(|_| Ok(vec![]))
            .times(0..);
        store2
            .expect_get_session_llm_config()
            .returning(|_| Ok(None))
            .times(0..);
        store2
            .expect_list_sessions()
            .returning(|| Ok(vec![]))
            .times(0..);

        let store2: Arc<dyn SessionStore> = Arc::new(store2);
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory { provider: shared });
        let (plugin_registry, _temp) = mock_plugin_registry(factory).expect("registry");
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(plugin_registry),
                store2.clone(),
                storage.event_journal(),
                LLMParams::new().provider("mock").model("mock-model"),
            )
            .build(),
        );
        let registry = SessionRegistry::new(config);

        let req = agent_client_protocol::schema::ForkSessionRequest::new(
            agent_client_protocol::schema::SessionId::from("source-session".to_string()),
            std::path::PathBuf::from("/tmp"),
        );
        let result = registry.fork_session(req).await;
        // Should fail with EmptySessionFork since history is empty
        assert!(result.is_err(), "expected error for empty session fork");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("empty")
                || err.code == agent_client_protocol::ErrorCode::InternalError,
            "unexpected error: {}",
            err
        );
    }

    // ── merged_mcp_servers tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn merged_mcp_servers_no_config_no_request() {
        let f = RegistryFixture::new().await;
        let merged = f.registry.merged_mcp_servers(&[]);
        assert!(merged.is_empty());
    }

    #[tokio::test]
    async fn merged_mcp_servers_config_only() {
        let mut f = RegistryFixture::new().await;
        // Inject an MCP server via config
        Arc::get_mut(&mut f.registry.config)
            .expect("single owner")
            .mcp_servers
            .push(crate::config::McpServerConfig::Http {
                name: "config-server".to_string(),
                url: "https://mcp.example.com/mcp".to_string(),
                headers: std::collections::HashMap::new(),
            });

        let merged = f.registry.merged_mcp_servers(&[]);
        assert_eq!(merged.len(), 1);
        assert!(matches!(&merged[0], McpServer::Http(s) if s.name == "config-server"));
    }

    #[tokio::test]
    async fn merged_mcp_servers_request_only() {
        let f = RegistryFixture::new().await;
        let req_server = agent_client_protocol::schema::McpServerHttp::new(
            "req-server".to_string(),
            "https://req.example.com/mcp".to_string(),
        );
        let req_servers = vec![McpServer::Http(req_server)];

        let merged = f.registry.merged_mcp_servers(&req_servers);
        assert_eq!(merged.len(), 1);
        assert!(matches!(&merged[0], McpServer::Http(s) if s.name == "req-server"));
    }

    #[tokio::test]
    async fn merged_mcp_servers_request_overrides_config_by_name() {
        let mut f = RegistryFixture::new().await;
        Arc::get_mut(&mut f.registry.config)
            .expect("single owner")
            .mcp_servers
            .push(crate::config::McpServerConfig::Http {
                name: "shared-name".to_string(),
                url: "https://config.example.com/mcp".to_string(),
                headers: std::collections::HashMap::new(),
            });

        // Request provides a different URL for the same name — should win.
        let req_server = agent_client_protocol::schema::McpServerHttp::new(
            "shared-name".to_string(),
            "https://override.example.com/mcp".to_string(),
        );
        let req_servers = vec![McpServer::Http(req_server)];

        let merged = f.registry.merged_mcp_servers(&req_servers);
        // Still one entry, not two
        assert_eq!(merged.len(), 1);
        assert!(
            matches!(&merged[0], McpServer::Http(s) if s.url == "https://override.example.com/mcp"),
            "expected the request server to override the config server"
        );
    }

    #[tokio::test]
    async fn merged_mcp_servers_both_different_names() {
        let mut f = RegistryFixture::new().await;
        Arc::get_mut(&mut f.registry.config)
            .expect("single owner")
            .mcp_servers
            .push(crate::config::McpServerConfig::Http {
                name: "config-server".to_string(),
                url: "https://config.example.com/mcp".to_string(),
                headers: std::collections::HashMap::new(),
            });

        let req_server = agent_client_protocol::schema::McpServerHttp::new(
            "req-server".to_string(),
            "https://req.example.com/mcp".to_string(),
        );
        let req_servers = vec![McpServer::Http(req_server)];

        let merged = f.registry.merged_mcp_servers(&req_servers);
        assert_eq!(merged.len(), 2);
        let names: Vec<&str> = merged
            .iter()
            .map(|s| match s {
                McpServer::Http(h) => h.name.as_str(),
                McpServer::Stdio(s) => s.name.as_str(),
                _ => "unknown",
            })
            .collect();
        assert!(names.contains(&"config-server"));
        assert!(names.contains(&"req-server"));
    }
}
