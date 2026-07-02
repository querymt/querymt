//! Session Materializer - handles heavy session creation/loading work.
//!
//! This service extracts the expensive async work (DB I/O, MCP startup, actor spawning)
//! from SessionRegistry so that the registry lock is only held for fast in-memory map operations.
//!
//! ## Architecture
//!
//! The materialization process follows a 3-phase pattern:
//!
//! 1. **Prepare**: All heavy work (DB queries, MCP startup, actor spawn) happens here
//!    WITHOUT holding the registry lock.
//! 2. **Register**: Registry lock is acquired ONLY to insert/remove from in-memory maps.
//!    This phase should complete in microseconds.
//! 3. **Finalize**: Post-registration work (DHT registration, event emission) happens here
//!    WITHOUT holding the registry lock.
//!
//! This ensures that session creation/loading/resuming never blocks other registry operations
//! like prompt lookup, session listing, or model setting.

use crate::acp::cwd::acp_cwd_to_optional;
use crate::acp::protocol::{
    Error, ForkSessionRequest, ForkSessionResponse, LoadSessionRequest, McpServer,
    NewSessionRequest, ResumeSessionRequest,
};
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SessionRuntime;
use crate::agent::remote::SessionActorRef;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::scoped_session;
use crate::agent::session_actor::SessionActor;
use crate::agent::session_mcp::{
    ConnectedMcpPeer, SessionMaterializationKind, SessionMcpAttachmentContext,
};
use crate::agent::session_registry::{
    SessionMaterialization, SessionMaterializationOptions, SessionRegistry,
};
use crate::events::AgentEventKind;
use kameo::actor::{ActorRef, Spawn};
#[cfg(feature = "remote")]
use querymt_remote::MeshRuntimeHandle;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Prepared session data ready to be registered in the SessionRegistry.
///
/// Contains everything needed to insert into the registry's in-memory maps,
/// but hasn't been registered yet.
struct SessionSingleFlightGuard {
    session_id: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    inflight: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    _guard: OwnedMutexGuard<()>,
}

impl Drop for SessionSingleFlightGuard {
    fn drop(&mut self) {
        // Remove the per-session lock once the last materialization waiter finishes.
        if Arc::strong_count(&self.lock) == 3 {
            let mut inflight = self.inflight.lock().unwrap();
            if inflight
                .get(&self.session_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.lock))
            {
                inflight.remove(&self.session_id);
            }
        }
    }
}

pub struct PreparedSession {
    /// The public session ID
    pub session_id: String,
    /// The session actor reference for routing
    pub actor_ref: ActorRef<SessionActor>,
    /// The session runtime for execution
    pub runtime: Arc<SessionRuntime>,
    /// MCP servers used (for config event)
    pub mcp_servers: Vec<McpServer>,
    /// Working directory (for config event)
    pub cwd: Option<PathBuf>,
    /// Whether to register in DHT (for remote sessions)
    pub register_in_dht: bool,
    /// Keeps the per-session single-flight guard alive until the caller finishes
    /// registration/finalization.
    _single_flight_guard: Option<SessionSingleFlightGuard>,
}

pub enum PreparedSessionResult {
    Prepared(PreparedSession),
    AlreadyRegistered(SessionActorRef),
}

/// Session Materializer - extracts heavy session creation/loading work from SessionRegistry.
///
/// This service is responsible for the expensive async operations that were previously
/// done while holding the registry lock:
/// - Database queries and session creation
/// - MCP server initialization
/// - Actor spawning and bridge setup
/// - Workspace index initialization
///
/// The registry lock is only acquired for fast in-memory map insertions.
///
/// ## Single-Flight Guarantee
///
/// The materializer provides a per-session single-flight mechanism to prevent
/// concurrent materialization of the same session ID. This is critical for:
/// - Preventing duplicate SessionActors for the same session
/// - Preventing leaked actors when concurrent loads race
/// - Ensuring idempotent load/resume behavior
pub struct SessionMaterializer {
    config: Arc<AgentConfig>,
    /// Per-session locks preventing concurrent materialization of the same session ID.
    /// Different session IDs proceed independently.
    inflight: Arc<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Mesh handle for remote operations (DHT registration, remote actor export).
    #[cfg(feature = "remote")]
    mesh: parking_lot::RwLock<Option<crate::agent::remote::MeshHandle>>,
}

/// Resolved MCP server configs and connected peers after consulting
/// the runtime attachment source and merging with request servers.
struct ResolvedSessionMcpInputs {
    /// The full list of MCP server configs (config + request + attached
    /// `ServerConfig` attachments).
    server_configs: Vec<McpServer>,
    /// Connected MCP peers that should be merged into runtime tool state
    /// after the actor has been spawned.
    connected_peers: Vec<ConnectedMcpPeer>,
}

impl SessionMaterializer {
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self {
            config,
            inflight: Arc::new(StdMutex::new(HashMap::new())),
            #[cfg(feature = "remote")]
            mesh: parking_lot::RwLock::new(None),
        }
    }

    /// Set the mesh handle for remote operations.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.write() = Some(mesh);
    }

    #[cfg(feature = "remote")]
    pub fn clear_mesh(&self) {
        *self.mesh.write() = None;
    }

    /// Get the current mesh handle (if any).
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.read().clone()
    }

    /// Acquire a per-session single-flight lock.
    ///
    /// Returns a guard that must be held during the entire materialization.
    /// This prevents concurrent materialization of the same session ID.
    async fn acquire_session_lock(&self, session_id: &str) -> SessionSingleFlightGuard {
        let lock = {
            let mut inflight = self.inflight.lock().unwrap();
            inflight
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let guard = lock.clone().lock_owned().await;
        SessionSingleFlightGuard {
            session_id: session_id.to_string(),
            lock,
            inflight: self.inflight.clone(),
            _guard: guard,
        }
    }

    // ── Internal: Resolve MCP inputs ───────────────────────────────────────

    async fn resolve_mcp_inputs(
        &self,
        session_id: &str,
        cwd: &Option<PathBuf>,
        kind: SessionMaterializationKind,
        request_mcp_servers: &[McpServer],
    ) -> Result<ResolvedSessionMcpInputs, Error> {
        // Resolve attachments from the runtime attachment source.
        let ctx = SessionMcpAttachmentContext {
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            kind,
        };
        let attachments = self
            .config
            .session_mcp_attachment_source
            .attachments(&ctx)
            .await?;

        let (attached_servers, connected_peers) =
            crate::agent::session_mcp::split_attachments(attachments);

        // Build the full server config list: config + request + attached servers.
        let mut server_configs = self
            .config
            .mcp_servers
            .iter()
            .map(|s| s.to_acp())
            .collect::<Vec<_>>();

        for req_server in request_mcp_servers {
            let req_name = match req_server {
                McpServer::Stdio(s) => s.name.as_str(),
                McpServer::Http(s) => s.name.as_str(),
                _ => continue,
            };
            if let Some(pos) = server_configs.iter().position(|s| match s {
                McpServer::Stdio(cs) => cs.name == req_name,
                McpServer::Http(cs) => cs.name == req_name,
                _ => false,
            }) {
                server_configs[pos] = req_server.clone();
            } else {
                server_configs.push(req_server.clone());
            }
        }

        for attached in attached_servers {
            let name = match &attached {
                McpServer::Stdio(s) => s.name.as_str(),
                McpServer::Http(s) => s.name.as_str(),
                _ => continue,
            };
            if !server_configs.iter().any(|s| match s {
                McpServer::Stdio(cs) => cs.name == name,
                McpServer::Http(cs) => cs.name == name,
                _ => false,
            }) {
                server_configs.push(attached);
            }
        }

        Ok(ResolvedSessionMcpInputs {
            server_configs,
            connected_peers,
        })
    }

    // ── Public materialization ─────────────────────────────────────────────

    /// Prepare a new session: create in DB, initialize MCP, spawn actor.
    ///
    /// This does NOT hold the registry lock. After this returns, call
    /// `register_session()` to insert into the registry.
    pub async fn prepare_new_session(
        &self,
        req: NewSessionRequest,
    ) -> Result<PreparedSession, Error> {
        let cwd = acp_cwd_to_optional(&req.cwd)?;

        let parent_session_id = req
            .meta
            .as_ref()
            .and_then(|m| m.get("parent_session_id"))
            .and_then(|v| v.as_str());

        // Create session in database (heavy I/O)
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

        // Resolve MCP inputs (config + request + runtime attachments)
        let resolved = self
            .resolve_mcp_inputs(
                &session_id,
                &cwd,
                SessionMaterializationKind::New,
                &req.mcp_servers,
            )
            .await?;

        // Materialize session actor (heavy: MCP startup, actor spawn)
        let materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd.clone(),
                &resolved.server_configs,
                parent_session_id.is_some(),
                &SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;

        // Merge connected MCP peers (from runtime attachments)
        crate::agent::protocol::merge_connected_mcp_peers(
            materialization.runtime.mcp_tool_state.clone(),
            &resolved.connected_peers,
        )
        .await?;

        Ok(PreparedSession {
            session_id,
            actor_ref: materialization.actor_ref,
            runtime: materialization.runtime,
            mcp_servers: resolved.server_configs,
            cwd,
            register_in_dht: true,
            _single_flight_guard: None,
        })
    }

    /// Prepare to load an existing session: validate, initialize MCP, spawn actor.
    ///
    /// This does NOT hold the registry lock. After this returns, the caller must
    /// register/finalize the prepared session before dropping it.
    ///
    /// Uses single-flight mechanism to prevent duplicate materialization of the same session.
    /// The returned `PreparedSession` keeps the per-session guard alive so the caller can
    /// safely register and finalize before concurrent waiters continue.
    pub async fn prepare_load_session(
        &self,
        req: LoadSessionRequest,
        registry: Option<&Mutex<SessionRegistry>>,
    ) -> Result<PreparedSessionResult, Error> {
        let session_id = req.session_id.to_string();

        let single_flight_guard = self.acquire_session_lock(&session_id).await;

        if let Some(reg) = registry {
            let registry_guard = reg.lock().await;
            if let Some(existing) = registry_guard.get(&session_id).cloned() {
                return Ok(PreparedSessionResult::AlreadyRegistered(existing));
            }
        }

        // Validate session exists in DB (heavy I/O) and prefer its persisted cwd.
        let session = self
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

        let request_cwd = acp_cwd_to_optional(&req.cwd)?;
        let cwd = session.cwd.or(request_cwd);

        // Resolve MCP inputs (config + request + runtime attachments)
        let resolved = self
            .resolve_mcp_inputs(
                &session_id,
                &cwd,
                SessionMaterializationKind::Load,
                &req.mcp_servers,
            )
            .await?;

        // Materialize session actor (heavy: MCP startup, actor spawn)
        let materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd.clone(),
                &resolved.server_configs,
                false,
                &SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;

        // Merge connected MCP peers (from runtime attachments)
        crate::agent::protocol::merge_connected_mcp_peers(
            materialization.runtime.mcp_tool_state.clone(),
            &resolved.connected_peers,
        )
        .await?;

        let prepared = PreparedSession {
            session_id,
            actor_ref: materialization.actor_ref,
            runtime: materialization.runtime,
            mcp_servers: resolved.server_configs,
            cwd,
            register_in_dht: true,
            _single_flight_guard: Some(single_flight_guard),
        };

        Ok(PreparedSessionResult::Prepared(prepared))
    }

    /// Prepare to resume an existing session: validate, optionally initialize MCP, spawn actor.
    ///
    /// This does NOT hold the registry lock. After this returns, the caller must
    /// register/finalize the prepared session before dropping it.
    ///
    /// Uses single-flight mechanism to prevent duplicate materialization of the same session.
    /// The returned `PreparedSession` keeps the per-session guard alive so the caller can
    /// safely register and finalize before concurrent waiters continue.
    pub async fn prepare_resume_session(
        &self,
        req: ResumeSessionRequest,
        registry: Option<&Mutex<SessionRegistry>>,
    ) -> Result<PreparedSessionResult, Error> {
        let session_id = req.session_id.to_string();

        let single_flight_guard = self.acquire_session_lock(&session_id).await;

        if let Some(reg) = registry {
            let registry_guard = reg.lock().await;
            if let Some(existing) = registry_guard.get(&session_id).cloned() {
                return Ok(PreparedSessionResult::AlreadyRegistered(existing));
            }
        }

        // Validate session exists in DB (heavy I/O)
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

        // Resolve MCP inputs (config + request + runtime attachments)
        let resolved = self
            .resolve_mcp_inputs(
                &session_id,
                &cwd,
                SessionMaterializationKind::Resume,
                &req.mcp_servers,
            )
            .await?;

        // Materialize session actor (heavy: MCP startup, actor spawn)
        let materialization = self
            .materialize_session_actor(
                session_id.clone(),
                cwd.clone(),
                &resolved.server_configs,
                false,
                &SessionMaterializationOptions {
                    attach_mesh_handle: true,
                    register_in_dht: true,
                },
            )
            .await?;

        // Merge connected MCP peers (from runtime attachments)
        crate::agent::protocol::merge_connected_mcp_peers(
            materialization.runtime.mcp_tool_state.clone(),
            &resolved.connected_peers,
        )
        .await?;

        let prepared = PreparedSession {
            session_id,
            actor_ref: materialization.actor_ref,
            runtime: materialization.runtime,
            mcp_servers: resolved.server_configs,
            cwd: cwd.clone(),
            register_in_dht: true,
            _single_flight_guard: Some(single_flight_guard),
        };

        Ok(PreparedSessionResult::Prepared(prepared))
    }

    /// Prepare to fork a session: create fork in DB.
    ///
    /// This does NOT hold the registry lock. After this returns, the caller
    /// can use the returned session ID to load the forked session.
    ///
    /// Native ACP has no fork-point field, so QueryMT clients may pass
    /// `_meta.querymt.message_id` (or `messageId`) to fork at a historical
    /// message boundary. Without that hint, this follows ACP's latest-state
    /// behavior by forking at the last stored message.
    pub async fn prepare_fork_session(
        &self,
        req: ForkSessionRequest,
    ) -> Result<ForkSessionResponse, Error> {
        let source_session_id = req.session_id.to_string();

        // Validate source session exists (heavy I/O)
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

        // Get history to find last message (heavy I/O)
        let history = self
            .config
            .provider
            .history_store()
            .get_history(&source_session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        let target_message_id = fork_message_id_from_meta(req.meta.as_ref())
            .map(ToOwned::to_owned)
            .or_else(|| history.last().map(|msg| msg.id.clone()))
            .ok_or(crate::error::AgentError::EmptySessionFork)?;

        // Fork session in DB (heavy I/O)
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

        Ok(ForkSessionResponse::new(new_session_id))
    }

    /// Finalize session after registration: emit events, set bridge, register in DHT, initialize workspace index.
    ///
    /// This is called AFTER the session has been inserted into the registry.
    /// It does NOT hold the registry lock.
    pub async fn finalize_session(
        &self,
        prepared: &PreparedSession,
        bridge: Option<crate::acp::client_bridge::ClientBridgeSender>,
    ) -> Result<(), Error> {
        // Set bridge on the session actor if available.
        // This is done here instead of in register_prepared_session to avoid
        // holding the registry lock during the async actor call.
        if let Some(ref bridge_sender) = bridge {
            let session_ref =
                crate::agent::remote::SessionActorRef::from(prepared.actor_ref.clone());
            if let Err(e) = session_ref.set_bridge(bridge_sender.clone()).await {
                log::warn!(
                    "Session {}: failed to set bridge on session actor: {}",
                    prepared.session_id,
                    e
                );
            }
        }

        // Register in DHT if requested (for remote sessions)
        #[cfg(feature = "remote")]
        if prepared.register_in_dht
            && let Some(mesh) = self.mesh()
        {
            let runtime = MeshRuntimeHandle::from(mesh.clone());
            for scope in runtime.active_scopes() {
                let dht_name = scoped_session(&scope, &prepared.session_id);
                let runtime = runtime.clone();
                let actor_ref = prepared.actor_ref.clone();
                tokio::spawn(async move {
                    runtime.register_actor(actor_ref, dht_name).await;
                });
            }
        }

        // Emit SessionCreated before later setup events so callers that await finalization
        // can observe session creation deterministically.
        self.config
            .emit_event_persisted(&prepared.session_id, AgentEventKind::SessionCreated)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        if let Ok(Some(llm_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&prepared.session_id)
            .await
        {
            let permission_mode = {
                let mode = *self
                    .config
                    .default_mode
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                crate::hooks::permission_mode_label(mode).to_string()
            };
            let hook_result = self
                .config
                .hooks
                .run_session_start(crate::hooks::SessionStartRequest {
                    session_id: prepared.session_id.clone(),
                    cwd: prepared.cwd.clone(),
                    model: llm_config.model.clone(),
                    permission_mode,
                    source: "new_session".to_string(),
                })
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
            for notice in hook_result.notices {
                self.config.emit_event(
                    &prepared.session_id,
                    crate::events::AgentEventKind::HookNotice {
                        event_name: notice.event_name,
                        message: notice.message,
                        is_error: notice.is_error,
                    },
                );
            }
            if !hook_result.additional_contexts.is_empty() {
                log::debug!(
                    "Session {}: ignoring {} session-start hook additional_context item(s) for MVP",
                    prepared.session_id,
                    hook_result.additional_contexts.len()
                );
            }
            if let Some(stop_reason) = hook_result.stop_reason {
                log::warn!(
                    "Session {}: session-start hook requested stop: {}",
                    prepared.session_id,
                    stop_reason
                );
            }
        }

        // Emit initial provider configuration
        if let Ok(Some(llm_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&prepared.session_id)
            .await
        {
            let context_limit =
                crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                    .and_then(|m| m.context_limit());
            self.config.emit_event(
                &prepared.session_id,
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
        if let Some(ref cwd_path) = prepared.cwd {
            if cwd_path.exists() {
                let manager_actor = self.config.workspace_manager_actor.clone();
                let runtime_clone = prepared.runtime.clone();
                let cwd_owned = cwd_path.clone();
                tokio::spawn(async move {
                    let root = crate::index::resolve_workspace_root(&cwd_owned);
                    match crate::index::get_or_create_workspace_with_timeout(&manager_actor, root)
                        .await
                    {
                        Ok(handle) => {
                            let _ = runtime_clone.workspace_handle.set(handle);
                        }
                        Err(e) => log::warn!("Failed to initialize workspace index: {}", e),
                    }
                });
            } else {
                log::debug!(
                    "SessionMaterializer: cwd {:?} does not exist, skipping workspace index",
                    cwd_path
                );
            }
        }

        // Emit SessionConfigured
        let mcp_configs: Vec<crate::config::McpServerConfig> = prepared
            .mcp_servers
            .iter()
            .map(crate::config::McpServerConfig::from_acp)
            .collect();
        self.config.emit_event(
            &prepared.session_id,
            crate::events::AgentEventKind::SessionConfigured {
                cwd: prepared.cwd.clone(),
                mcp_servers: mcp_configs,
                limits: self.config.get_session_limits(),
            },
        );

        // Publish available slash commands via ACP bridge
        if !self.config.slash_command_registry.is_empty()
            && let Some(bridge_sender) = bridge
        {
            let notification = crate::slash_commands::acp::build_commands_notification(
                &prepared.session_id,
                &self.config.slash_command_registry,
            );
            let bridge = bridge_sender.clone();
            tokio::spawn(async move {
                if let Err(e) = bridge.notify(notification).await {
                    log::debug!("Failed to publish available commands: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Internal: Materialize a session actor.
    ///
    /// This is the core heavy work that was previously in SessionRegistry::materialize_session_actor().
    async fn materialize_session_actor(
        &self,
        session_id: String,
        cwd: Option<PathBuf>,
        mcp_servers: &[McpServer],
        initialize_fork: bool,
        _options: &SessionMaterializationOptions,
    ) -> Result<SessionMaterialization, Error> {
        let tool_state = crate::agent::core::McpToolState::empty();
        let mcp_services = crate::agent::protocol::build_mcp_state(
            mcp_servers,
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
            .with_mesh(if _options.attach_mesh_handle {
                // Use the mesh handle from the materializer's shared state
                self.mesh()
            } else {
                None
            });
        #[cfg(not(feature = "remote"))]
        let actor = { SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone()) };
        let actor_ref = SessionActor::spawn(actor);

        // Bridge will be set during registration if available
        // We defer this to avoid needing the registry lock

        Ok(SessionMaterialization { actor_ref, runtime })
    }
}

fn fork_message_id_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<&str> {
    let querymt = meta?.get("querymt")?;
    querymt
        .get("message_id")
        .or_else(|| querymt.get("messageId"))
        .and_then(serde_json::Value::as_str)
        .filter(|message_id| !message_id.is_empty())
}

#[cfg(test)]
mod fork_meta_tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::model::AgentMessage;
    use crate::session::backend::StorageBackend;
    use crate::session::domain::ForkOrigin;
    use crate::session::provider::SessionProvider;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use querymt::LLMParams;
    use querymt::chat::ChatRole;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn message(id: &str, session_id: &str) -> AgentMessage {
        let mut message = AgentMessage::new(session_id.to_string(), ChatRole::User);
        message.id = id.to_string();
        message
    }

    async fn materializer_with_store(store: MockSessionStore) -> SessionMaterializer {
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
        let store: Arc<dyn SessionStore> = Arc::new(store);
        let provider = Arc::new(SessionProvider::new(
            Arc::new(plugin_registry),
            store,
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
        );
        SessionMaterializer::new(config)
    }

    #[tokio::test]
    async fn prepare_fork_session_uses_querymt_message_id_meta() {
        let source_session_id = "source-session".to_string();
        let history_session_id = source_session_id.clone();
        let fork_source_id = source_session_id.clone();
        let session = mock_session(&source_session_id);
        let llm_config = mock_llm_config();
        let mut store = MockSessionStore::new();
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session.clone())))
            .times(1);
        store
            .expect_get_history()
            .returning(move |_| {
                Ok(vec![
                    message("msg-1", &history_session_id),
                    message("msg-2", &history_session_id),
                ])
            })
            .times(1);
        store
            .expect_fork_session()
            .withf(move |source, target, origin| {
                source == fork_source_id && target == "msg-1" && *origin == ForkOrigin::User
            })
            .returning(|_, _, _| Ok("forked-session".to_string()))
            .times(1);
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(llm_config.clone())))
            .times(0..);
        store
            .expect_list_sessions()
            .returning(|| Ok(vec![]))
            .times(0..);

        let materializer = materializer_with_store(store).await;
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt".to_string(),
            serde_json::json!({ "message_id": "msg-1" }),
        );
        let req = ForkSessionRequest::new(
            crate::acp::protocol::SessionId::from(source_session_id),
            std::path::PathBuf::from("/tmp"),
        )
        .meta(meta);

        let response = materializer
            .prepare_fork_session(req)
            .await
            .expect("fork session");
        assert_eq!(response.session_id.to_string(), "forked-session");
    }

    #[tokio::test]
    async fn prepare_fork_session_without_meta_uses_latest_message() {
        let source_session_id = "source-session".to_string();
        let history_session_id = source_session_id.clone();
        let fork_source_id = source_session_id.clone();
        let session = mock_session(&source_session_id);
        let llm_config = mock_llm_config();
        let mut store = MockSessionStore::new();
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session.clone())))
            .times(1);
        store
            .expect_get_history()
            .returning(move |_| {
                Ok(vec![
                    message("msg-1", &history_session_id),
                    message("msg-2", &history_session_id),
                ])
            })
            .times(1);
        store
            .expect_fork_session()
            .withf(move |source, target, origin| {
                source == fork_source_id && target == "msg-2" && *origin == ForkOrigin::User
            })
            .returning(|_, _, _| Ok("forked-session".to_string()))
            .times(1);
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(llm_config.clone())))
            .times(0..);
        store
            .expect_list_sessions()
            .returning(|| Ok(vec![]))
            .times(0..);

        let materializer = materializer_with_store(store).await;
        let req = ForkSessionRequest::new(
            crate::acp::protocol::SessionId::from(source_session_id),
            std::path::PathBuf::from("/tmp"),
        );

        let response = materializer
            .prepare_fork_session(req)
            .await
            .expect("fork session");
        assert_eq!(response.session_id.to_string(), "forked-session");
    }

    #[test]
    fn fork_message_id_from_meta_accepts_snake_and_camel_case() {
        let mut snake = serde_json::Map::new();
        snake.insert(
            "querymt".to_string(),
            serde_json::json!({ "message_id": "msg-1" }),
        );
        assert_eq!(fork_message_id_from_meta(Some(&snake)), Some("msg-1"));

        let mut camel = serde_json::Map::new();
        camel.insert(
            "querymt".to_string(),
            serde_json::json!({ "messageId": "msg-2" }),
        );
        assert_eq!(fork_message_id_from_meta(Some(&camel)), Some("msg-2"));
    }
}
