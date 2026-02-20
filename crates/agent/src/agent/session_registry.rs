//! Session registry - manages session actors.
//!
//! Lives on the server layer. Not an actor — just a plain data structure
//! protected by a mutex (acceptable: only accessed for routing, not during execution).

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SessionRuntime;
use crate::agent::remote::SessionActorRef;
use crate::agent::session_actor::SessionActor;
use crate::error::AgentError;
use agent_client_protocol::{
    Error, ListSessionsRequest, ListSessionsResponse, NewSessionRequest, NewSessionResponse,
    SessionInfo,
};
use kameo::actor::Spawn;
use std::collections::HashMap;
use std::sync::Arc;

/// Manages session actors. Lives on the server layer.
pub struct SessionRegistry {
    pub config: Arc<AgentConfig>,
    sessions: HashMap<String, SessionActorRef>,
}

impl SessionRegistry {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
        }
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
        self.sessions.insert(session_id, actor_ref.into());
    }

    /// Remove a session actor from the registry.
    pub fn remove(&mut self, session_id: &str) -> Option<SessionActorRef> {
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
        //    It republishes remote events on the local event bus.
        let relay_actor = EventRelayActor::new(self.config.event_bus.clone(), peer_label.clone());
        let relay_ref = EventRelayActor::spawn(relay_actor);
        let relay_id = relay_ref.id().sequence_id();

        // 3. Register the relay in REMOTE_REGISTRY + DHT so the remote
        //    SessionActor can look it up by name and install an EventForwarder.
        let mesh_active = mesh.is_some();
        if let Some(ref mesh) = mesh {
            mesh.register_actor(relay_ref.clone(), format!("event_relay::{}", session_id))
                .await;
        } else {
            log::debug!(
                "attach_remote_session: no mesh, DHT registration skipped for relay (session {})",
                session_id
            );
        }

        // 4. Send SubscribeEvents to the remote session.
        //    The remote SubscribeEvents handler uses mesh.lookup_actor to find
        //    the relay and install an EventForwarder on its EventBus.
        if let Err(e) = session_ref.subscribe_events(relay_id).await {
            log::warn!(
                "attach_remote_session: SubscribeEvents failed for {} (event relay may not be active): {}",
                session_id,
                e
            );
        }

        // 5. Insert into registry
        self.sessions
            .insert(session_id.clone(), session_ref.clone());

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

    /// Create a new session: build runtime, spawn SessionActor, return session_id.
    pub async fn new_session(
        &mut self,
        req: NewSessionRequest,
    ) -> Result<NewSessionResponse, Error> {
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

        // Build MCP state
        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &req.mcp_servers,
            self.config.pending_elicitations(),
            self.config.event_bus.clone(),
            session_id.clone(),
        )
        .await?;

        // Initialize fork if parent_session_id was provided
        if parent_session_id.is_some() {
            crate::session::runtime::SessionForkHelper::initialize_fork(
                self.config.provider.history_store(),
                &session_id,
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        }

        let runtime = SessionRuntime::new(cwd.clone(), mcp_services, mcp_tools, mcp_tool_defs);

        // Spawn the session actor
        let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone());
        let actor_ref = SessionActor::spawn(actor);
        self.sessions.insert(session_id.clone(), actor_ref.into());

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
                    provider_node: None,
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

        Ok(NewSessionResponse::new(session_id))
    }

    /// Load an existing session: validate it exists, build runtime, spawn SessionActor.
    pub async fn load_session(
        &mut self,
        req: agent_client_protocol::LoadSessionRequest,
    ) -> Result<agent_client_protocol::LoadSessionResponse, Error> {
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

        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &req.mcp_servers,
            self.config.pending_elicitations(),
            self.config.event_bus.clone(),
            session_id.clone(),
        )
        .await?;

        let runtime = SessionRuntime::new(cwd, mcp_services, mcp_tools, mcp_tool_defs);

        let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime);
        let actor_ref = SessionActor::spawn(actor);
        self.sessions.insert(session_id.clone(), actor_ref.into());

        // Stream full history to client
        // TODO: Implement full-fidelity history streaming with SessionUpdate notifications
        // For now, we'll return success without streaming history
        self.config
            .emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

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
                    provider_node: None,
                },
            );
        }

        Ok(agent_client_protocol::LoadSessionResponse::new())
    }

    /// Fork an existing session at the latest message.
    pub async fn fork_session(
        &self,
        req: agent_client_protocol::ForkSessionRequest,
    ) -> Result<agent_client_protocol::ForkSessionResponse, Error> {
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

        Ok(agent_client_protocol::ForkSessionResponse::new(
            new_session_id,
        ))
    }

    /// Resume an existing session without history replay.
    pub async fn resume_session(
        &mut self,
        req: agent_client_protocol::ResumeSessionRequest,
    ) -> Result<agent_client_protocol::ResumeSessionResponse, Error> {
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

        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &req.mcp_servers,
            self.config.pending_elicitations(),
            self.config.event_bus.clone(),
            session_id.clone(),
        )
        .await?;

        let runtime = SessionRuntime::new(cwd, mcp_services, mcp_tools, mcp_tool_defs);

        let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime);
        let actor_ref = SessionActor::spawn(actor);
        self.sessions.insert(session_id.clone(), actor_ref.into());

        self.config
            .emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        Ok(agent_client_protocol::ResumeSessionResponse::new())
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
                    agent_client_protocol::SessionId::from(s.public_id),
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
