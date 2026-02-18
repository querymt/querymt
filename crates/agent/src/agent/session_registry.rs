//! Session registry - manages session actors.
//!
//! Lives on the server layer. Not an actor — just a plain data structure
//! protected by a mutex (acceptable: only accessed for routing, not during execution).

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SessionRuntime;
use crate::agent::session_actor::SessionActor;
use agent_client_protocol::{
    Error, ListSessionsRequest, ListSessionsResponse, NewSessionRequest, NewSessionResponse,
    SessionInfo,
};
use kameo::actor::{ActorRef, Spawn};
use std::collections::HashMap;
use std::sync::Arc;

/// Manages session actors. Lives on the server layer.
pub struct SessionRegistry {
    pub config: Arc<AgentConfig>,
    sessions: HashMap<String, ActorRef<SessionActor>>,
}

impl SessionRegistry {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            sessions: HashMap::new(),
        }
    }

    /// Get a reference to the session actor for routing.
    pub fn get(&self, session_id: &str) -> Option<&ActorRef<SessionActor>> {
        self.sessions.get(session_id)
    }

    /// Insert a pre-spawned session actor into the registry.
    pub fn insert(&mut self, session_id: String, actor_ref: ActorRef<SessionActor>) {
        self.sessions.insert(session_id, actor_ref);
    }

    /// Remove a session actor from the registry.
    pub fn remove(&mut self, session_id: &str) -> Option<ActorRef<SessionActor>> {
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
            .map_err(|e| Error::new(-32000, e.to_string()))?;
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
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        }

        let runtime = SessionRuntime::new(cwd.clone(), mcp_services, mcp_tools, mcp_tool_defs);

        // Spawn the session actor
        let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone());
        let actor_ref = SessionActor::spawn(actor);
        self.sessions.insert(session_id.clone(), actor_ref);

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
                },
            );
        }

        // Background: initialize workspace index
        if let Some(cwd_path) = cwd.clone() {
            let manager_actor = self.config.workspace_manager_actor.clone();
            let runtime_clone = runtime.clone();
            tokio::spawn(async move {
                let root = crate::index::resolve_workspace_root(&cwd_path);
                match manager_actor.ask(crate::index::GetOrCreate { root }).await {
                    Ok(handle) => {
                        let _ = runtime_clone.workspace_handle.set(handle);
                    }
                    Err(e) => log::warn!("Failed to initialize workspace index: {}", e),
                }
            });
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
            .map_err(|e| Error::new(-32000, e.to_string()))?
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
        self.sessions.insert(session_id.clone(), actor_ref);

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
            .map_err(|e| Error::new(-32000, e.to_string()))?
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
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        let target_message_id = history
            .last()
            .map(|msg| msg.id.clone())
            .ok_or_else(|| Error::new(-32000, "cannot fork empty session"))?;

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
            .map_err(|e| Error::new(-32000, e.to_string()))?;

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
            .map_err(|e| Error::new(-32000, e.to_string()))?
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
        self.sessions.insert(session_id.clone(), actor_ref);

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
            .map_err(|e| Error::new(-32000, e.to_string()))?;

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
