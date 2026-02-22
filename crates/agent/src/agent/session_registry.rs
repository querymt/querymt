//! Session registry - manages session actors.
//!
//! Lives on the server layer. Not an actor — just a plain data structure
//! protected by a mutex (acceptable: only accessed for routing, not during execution).

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::remote::SessionActorRef;
use crate::agent::session_actor::SessionActor;
use crate::error::AgentError;
use agent_client_protocol::{
    Error, ListSessionsRequest, ListSessionsResponse, McpServer, NewSessionRequest,
    NewSessionResponse, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionInfo, SessionMode, SessionModeState,
};
use kameo::actor::Spawn;
use std::collections::HashMap;
use std::sync::Arc;

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

fn config_options(mode: AgentMode) -> Vec<SessionConfigOption> {
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
    ]
}

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
                McpServer::Sse(s) => s.name.as_str(),
                _ => continue,
            };
            if let Some(pos) = merged.iter().position(|s| match s {
                McpServer::Stdio(cs) => cs.name == req_name,
                McpServer::Http(cs) => cs.name == req_name,
                McpServer::Sse(cs) => cs.name == req_name,
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

        // Build MCP state: merge config-level MCP servers with any client-supplied ones.
        let merged_mcp = self.merged_mcp_servers(&req.mcp_servers);
        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &merged_mcp,
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
            .config_options(config_options(current_mode)))
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

        let merged_mcp = self.merged_mcp_servers(&req.mcp_servers);
        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &merged_mcp,
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

        let current_mode = {
            let session_ref = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::internal_error().data("session actor missing after load"))?
                .clone();
            session_ref.get_mode().await.map_err(Error::from)?
        };

        Ok(agent_client_protocol::LoadSessionResponse::new()
            .modes(mode_state(current_mode))
            .config_options(config_options(current_mode)))
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

        let merged_mcp = self.merged_mcp_servers(&req.mcp_servers);
        let (mcp_services, mcp_tools, mcp_tool_defs) = crate::agent::protocol::build_mcp_state(
            &merged_mcp,
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

        let current_mode = {
            let session_ref = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| Error::internal_error().data("session actor missing after resume"))?
                .clone();
            session_ref.get_mode().await.map_err(Error::from)?
        };

        Ok(agent_client_protocol::ResumeSessionResponse::new()
            .modes(mode_state(current_mode))
            .config_options(config_options(current_mode)))
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

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::core::{
        AgentMode, DelegationContextConfig, DelegationContextTiming, SnapshotPolicy, ToolConfig,
        ToolPolicy,
    };
    use crate::agent::session_actor::SessionActor;
    use crate::config::RuntimeExecutionPolicy;
    use crate::delegation::DefaultAgentRegistry;
    use crate::event_bus::EventBus;
    use crate::index::{WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig};
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use crate::tools::ToolRegistry;
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use std::collections::{HashMap, HashSet};
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
            let provider_ctx = Arc::new(crate::session::provider::SessionProvider::new(
                Arc::new(plugin_registry),
                store.clone(),
                LLMParams::new().provider("mock").model("mock-model"),
            ));

            let config = Arc::new(AgentConfig {
                provider: provider_ctx,
                event_bus: Arc::new(EventBus::new()),
                agent_registry: Arc::new(DefaultAgentRegistry::new()),
                workspace_manager_actor: WorkspaceIndexManagerActor::new(
                    WorkspaceIndexManagerConfig::default(),
                ),
                default_mode: Arc::new(std::sync::Mutex::new(AgentMode::Build)),
                tool_config: ToolConfig {
                    policy: ToolPolicy::ProviderOnly,
                    ..ToolConfig::default()
                },
                tool_registry: ToolRegistry::new(),
                middleware_drivers: Vec::new(),
                auth_methods: Vec::new(),
                max_steps: None,
                snapshot_policy: SnapshotPolicy::None,
                assume_mutating: true,
                mutating_tools: HashSet::new(),
                max_prompt_bytes: None,
                execution_timeout_secs: 300,
                delegation_wait_policy: crate::config::DelegationWaitPolicy::default(),
                delegation_wait_timeout_secs: 120,
                delegation_cancel_grace_secs: 5,
                execution_policy: RuntimeExecutionPolicy::default(),
                compaction: crate::session::compaction::SessionCompaction::new(),
                snapshot_backend: None,
                snapshot_gc_config: crate::snapshot::GcConfig::default(),
                delegation_context_config: DelegationContextConfig {
                    timing: DelegationContextTiming::FirstTurnOnly,
                    auto_inject: true,
                },
                pending_elicitations: Arc::new(Mutex::new(HashMap::new())),
                mcp_servers: Vec::new(),
            });

            Self {
                registry: SessionRegistry::new(config),
                _temp_dir: temp_dir,
            }
        }

        fn spawn_actor(&self) -> kameo::actor::ActorRef<SessionActor> {
            let runtime = crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                HashMap::new(),
                vec![],
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
        let options = config_options(AgentMode::Review);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id.0.as_ref(), "mode");

        let select = match &options[0].kind {
            agent_client_protocol::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select mode option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "review");
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
        let provider_ctx = Arc::new(crate::session::provider::SessionProvider::new(
            Arc::new(plugin_registry),
            store2.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(AgentConfig {
            provider: provider_ctx,
            event_bus: Arc::new(EventBus::new()),
            agent_registry: Arc::new(DefaultAgentRegistry::new()),
            workspace_manager_actor: WorkspaceIndexManagerActor::new(
                WorkspaceIndexManagerConfig::default(),
            ),
            default_mode: Arc::new(std::sync::Mutex::new(AgentMode::Build)),
            tool_config: ToolConfig::default(),
            tool_registry: ToolRegistry::new(),
            middleware_drivers: Vec::new(),
            auth_methods: Vec::new(),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            execution_timeout_secs: 300,
            delegation_wait_policy: crate::config::DelegationWaitPolicy::default(),
            delegation_wait_timeout_secs: 120,
            delegation_cancel_grace_secs: 5,
            execution_policy: RuntimeExecutionPolicy::default(),
            compaction: crate::session::compaction::SessionCompaction::new(),
            snapshot_backend: None,
            snapshot_gc_config: crate::snapshot::GcConfig::default(),
            delegation_context_config: DelegationContextConfig {
                timing: DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            pending_elicitations: Arc::new(Mutex::new(HashMap::new())),
            mcp_servers: Vec::new(),
        });
        let registry = SessionRegistry::new(config);

        let req = agent_client_protocol::ForkSessionRequest::new(
            agent_client_protocol::SessionId::from("source-session".to_string()),
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
        let req_server = agent_client_protocol::McpServerHttp::new(
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
        let req_server = agent_client_protocol::McpServerHttp::new(
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

        let req_server = agent_client_protocol::McpServerHttp::new(
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
                McpServer::Sse(s) => s.name.as_str(),
                _ => "unknown",
            })
            .collect();
        assert!(names.contains(&"config-server"));
        assert!(names.contains(&"req-server"));
    }
}
