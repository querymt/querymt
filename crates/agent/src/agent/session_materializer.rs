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
use crate::worktree::{WorkspaceBinding, WorkspaceService};
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

        if let Some(service) = self.workspace_service() {
            let workspace = service
                .registry()
                .get_workspace_by_session(session_id)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?;
            if workspace.is_some() {
                self.config
                    .session_mcp_attachment_source
                    .validate_workspace_roots(&ctx, &server_configs, &connected_peers)
                    .await?;
            }
        }
        Ok(ResolvedSessionMcpInputs {
            server_configs,
            connected_peers,
        })
    }

    /// Managed worktree workspace service, when configured.
    fn workspace_service(&self) -> Option<Arc<WorkspaceService>> {
        self.config.workspace_service()
    }

    /// Resolve the workspace binding for a NEW session before any session,
    /// MCP, index, hook, or actor startup.
    ///
    /// Returns `None` for local execution (default/absent options), keeping
    /// existing behavior. Requested isolation fails closed when it cannot be
    /// honored; it never downgrades to shared execution.
    async fn resolve_new_session_workspace(
        &self,
        requested_cwd: &Option<PathBuf>,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Option<WorkspaceBinding>, Error> {
        let options = match crate::worktree::session_creation_options_from_meta(meta)
            .map_err(|message| invalid_workspace_request(&message))?
        {
            Some(options) => options,
            None => return Ok(None),
        };
        if !options.requests_isolation() {
            return Ok(None);
        }

        let Some(service) = self.workspace_service() else {
            return Err(Error::internal_error().data(serde_json::json!({
                "message": "worktree isolation requested but no workspace service is \
                            configured; refusing to downgrade to shared execution",
            })));
        };

        let Some(source_cwd) = requested_cwd.as_deref() else {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "worktree isolation requires an absolute source cwd \
                            inside the target repository",
            })));
        };

        // Allocation provisions the checkout and records the ready binding
        // durably; the caller persists the owner association right after the
        // session row exists.
        service
            .allocate_for_new_session(source_cwd, &options)
            .await
            .map_err(worktree_error_to_acp)
    }

    /// Resolve the persisted workspace binding for load/resume ahead of any
    /// caller cwd. Bound sessions always use their persisted execution path;
    /// conflicting cwd requests and missing checkouts fail closed. Legacy
    /// local sessions resolve to `None` and keep their existing behavior.
    async fn resolve_existing_workspace(
        &self,
        session_id: &str,
        requested_cwd: Option<&std::path::Path>,
    ) -> Result<Option<WorkspaceBinding>, Error> {
        let Some(service) = self.workspace_service() else {
            if let Some(registry) = self.config.storage.workspace_repository()
                && registry
                    .get_workspace_by_session(session_id)
                    .await
                    .map_err(|err| Error::internal_error().data(err.to_string()))?
                    .is_some()
            {
                return Err(invalid_workspace_request(
                    "persisted managed session requires a workspace service; refusing shared fallback",
                ));
            }
            return Ok(None);
        };
        service
            .resolve_existing_session(session_id, requested_cwd)
            .await
            .map_err(worktree_error_to_acp)
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
        // Keep the remote-feature materialization future off the caller's stack.
        Box::pin(self.prepare_new_session_with_workspace(req, None)).await
    }

    pub(crate) async fn prepare_new_session_with_workspace(
        &self,
        req: NewSessionRequest,
        preallocated: Option<WorkspaceBinding>,
    ) -> Result<PreparedSession, Error> {
        let requested_cwd = acp_cwd_to_optional(&req.cwd)?;

        let parent_session_id = req
            .meta
            .as_ref()
            .and_then(|m| m.get("parent_session_id"))
            .and_then(|v| v.as_str());

        // Resolve the workspace binding BEFORE creating the session row,
        // starting MCP, indexes, hooks, or the actor, so every consumer
        // materializes inside the bound checkout. Requested isolation fails
        // closed here; it never downgrades to shared execution.
        let workspace = match preallocated {
            Some(binding) => {
                if requested_cwd.as_deref() != Some(binding.effective_cwd.as_path())
                    || binding.record.ownership.parent_session_id.as_deref() != parent_session_id
                {
                    return Err(invalid_workspace_request(
                        "preallocated delegate workspace does not match its parent or cwd",
                    ));
                }
                Some(binding)
            }
            None => {
                self.resolve_new_session_workspace(&requested_cwd, req.meta.as_ref())
                    .await?
            }
        };
        if workspace.is_none()
            && self.workspace_service().is_none()
            && let (Some(registry), Some(cwd)) = (
                self.config.storage.workspace_repository(),
                requested_cwd.as_deref(),
            )
            && let Ok(repo) = crate::worktree::identity::resolve_repository(cwd)
            && registry
                .get_workspace_by_checkout(&repo.checkout.root)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
                .is_some()
        {
            return Err(invalid_workspace_request(
                "managed checkout requires a workspace service; refusing local fallback",
            ));
        }
        let shared_gate = if workspace.is_none() {
            if let (Some(service), Some(cwd)) = (self.workspace_service(), requested_cwd.as_deref())
            {
                if let Ok(repo) = crate::worktree::identity::resolve_repository(cwd) {
                    let record = service
                        .registry()
                        .get_workspace_by_checkout(&repo.checkout.root)
                        .await
                        .map_err(|err| Error::internal_error().data(err.to_string()))?;
                    if let Some(record) = record {
                        if parent_session_id.is_none()
                            || record.ownership.owner_session_id.as_deref() != parent_session_id
                        {
                            return Err(invalid_workspace_request(
                                "managed checkout cannot be attached to another writable session; create a fresh worktree",
                            ));
                        }
                        Some(Arc::new(
                            crate::worktree::CheckoutMutationGate::new(
                                service.storage_root(),
                                &record.checkout_path,
                            )
                            .map_err(worktree_error_to_acp)?,
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let cwd = match &workspace {
            Some(binding) => Some(binding.effective_cwd.clone()),
            None => requested_cwd,
        };

        // Create session in database (heavy I/O); isolated sessions persist
        // the bound execution path as their cwd.
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

        // Persist the durable session-to-workspace association. The registry
        // enforces a single writable owner atomically, so a duplicate owner
        // attempt fails closed instead of sharing the checkout.
        if let Some(binding) = &workspace {
            let service = self.workspace_service().ok_or_else(|| {
                Error::internal_error().data(serde_json::json!({
                    "message": "workspace service disappeared during session creation",
                }))
            })?;
            service
                .bind_owner(&binding.record.id, &session_id)
                .await
                .map_err(worktree_error_to_acp)?;
        }

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
                workspace
                    .as_ref()
                    .map(|binding| binding.mutation_gate.clone())
                    .or(shared_gate),
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
        // Resolve the persisted workspace binding ahead of any caller cwd:
        // bound sessions always use their bound execution path, conflicting
        // cwd requests and missing checkouts fail closed.
        let workspace = self
            .resolve_existing_workspace(&session_id, request_cwd.as_deref())
            .await?;
        let cwd = match &workspace {
            Some(binding) => Some(binding.effective_cwd.clone()),
            None => session.cwd.or(request_cwd),
        };

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
                workspace
                    .as_ref()
                    .map(|binding| binding.mutation_gate.clone()),
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

        // Resolve the persisted workspace binding ahead of any caller cwd:
        // resume of a bound session always uses the persisted execution
        // path, conflicting cwd requests and missing checkouts fail closed.
        let workspace = self
            .resolve_existing_workspace(&session_id, cwd.as_deref())
            .await?;
        let cwd = match &workspace {
            Some(binding) => Some(binding.effective_cwd.clone()),
            None => cwd,
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
                workspace
                    .as_ref()
                    .map(|binding| binding.mutation_gate.clone()),
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
        let session = self
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

        // Fork session in DB (heavy I/O). This durably records the
        // historical message fork point separately from any workspace
        // association (which is created below, only for isolated forks).
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

        // Allocate the fork's own workspace when the fork requests
        // isolation; local forks stay pure message copies.
        let fork_workspace = self
            .resolve_fork_workspace(
                &source_session_id,
                session.cwd.as_deref(),
                req.meta.as_ref(),
                &new_session_id,
            )
            .await?;

        let mut response = ForkSessionResponse::new(new_session_id);
        if let Some(binding) = &fork_workspace {
            // Disclose the fresh working state: files reflect the source's
            // current contents, not the historical filesystem state at the
            // message fork point.
            let meta = serde_json::json!({
                crate::acp::shared::QUERYMT_META_NAMESPACE: {
                    crate::worktree::SESSION_WORKSPACE_META_KEY: {
                        "execution": "worktree",
                        "workspaceId": binding.record.id.as_str(),
                        "sessionCwd": binding.effective_cwd.display().to_string(),
                        "baseline": binding.record.baseline.as_ref().map(|b| b.mode.to_string()),
                        "disclosure": "fresh-working-state",
                        "message": "This fork runs in a fresh isolated worktree whose files \
                                   reflect the source repository's current contents, not the \
                                   historical filesystem state at the fork point.",
                    },
                },
            });
            response = response.meta(meta.as_object().cloned());
        }
        Ok(response)
    }

    /// Allocate the fork's own workspace when the fork requests isolation.
    ///
    /// Returns `Ok(None)` for local forks (pure message copy, no new
    /// workspace). The fork's checkout is provisioned through the same
    /// create path as fresh sessions, under its own durable path — never
    /// the source session's checkout — and becomes owned by the fork
    /// session as its single writable owner.
    async fn resolve_fork_workspace(
        &self,
        source_session_id: &str,
        source_session_cwd: Option<&std::path::Path>,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
        fork_session_id: &str,
    ) -> Result<Option<WorkspaceBinding>, Error> {
        let source_binding = self
            .resolve_existing_workspace(source_session_id, None)
            .await?;
        let options = match crate::worktree::fork_creation_options_from_meta(meta)
            .map_err(|message| invalid_workspace_request(&message))?
        {
            Some(options) => options,
            None if source_binding.is_some() => {
                crate::worktree::SessionCreationOptions::isolated_worktree(
                    crate::worktree::BaselineMode::CurrentWorkingTree,
                )
            }
            None => return Ok(None),
        };
        if !options.requests_isolation() {
            if source_binding.is_some() {
                return Err(invalid_workspace_request(
                    "forking a managed session requires an independent worktree",
                ));
            }
            return Ok(None);
        }

        let Some(service) = self.workspace_service() else {
            return Err(Error::internal_error().data(serde_json::json!({
                "message": "worktree isolation requested but no workspace service is \
                            configured; refusing to downgrade to shared execution",
            })));
        };

        // The source's execution cwd is its bound checkout when the source
        // itself is workspace-bound, otherwise its stored session cwd.
        let source_cwd = match &source_binding {
            Some(binding) => binding.effective_cwd.clone(),
            None => source_session_cwd
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| {
                    invalid_workspace_request(
                        "isolated fork requires the source session to have a working \
                     directory inside a Git repository",
                    )
                })?,
        };
        // Serialize baseline capture against cooperating writes when the
        // source is a live managed workspace.
        let source_gate = source_binding.as_ref().map(|b| b.mutation_gate.clone());

        let binding = service
            .allocate_for_fork(&source_cwd, &options, source_gate.as_deref())
            .await
            .map_err(worktree_error_to_acp)?;

        // Persist the association: the fork session is the new workspace's
        // single writable owner. The message fork point was recorded by the
        // session store and remains independent of this association.
        service
            .bind_owner(&binding.record.id, fork_session_id)
            .await
            .map_err(worktree_error_to_acp)?;

        Ok(Some(binding))
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

        if let Some(service) = self.workspace_service()
            && let Some(record) = service
                .registry()
                .get_workspace_by_session(&prepared.session_id)
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?
        {
            let workspace = crate::session::projection::SessionWorkspaceView {
                workspace_id: record.id.to_string(),
                repository_id: record.repository.common_dir.display().to_string(),
                project_name: record.repository.project_name.clone(),
                checkout_path: record.checkout_path.display().to_string(),
                branch: record.branch.as_str().to_string(),
                baseline_mode: record
                    .baseline
                    .as_ref()
                    .map(|b| b.mode.to_string())
                    .unwrap_or_else(|| "commit".to_string()),
                baseline_commit: record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
                lifecycle: record.lifecycle.to_string(),
                last_error: record.last_error.clone(),
            };
            self.config
                .emit_event_persisted(
                    &prepared.session_id,
                    AgentEventKind::SessionWorkspaceBound {
                        workspace,
                        cwd: prepared.cwd.clone(),
                    },
                )
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
        }

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
        let mcp_servers = prepared
            .mcp_servers
            .iter()
            .map(crate::config::McpServerConfig::from_acp)
            .map(crate::events::McpServerInfo::from)
            .collect();
        self.config.emit_event(
            &prepared.session_id,
            crate::events::AgentEventKind::SessionConfigured {
                cwd: prepared.cwd.clone(),
                mcp_servers,
                limits: self.config.get_session_limits(),
            },
        );

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
        mutation_gate: Option<Arc<crate::worktree::gate::CheckoutMutationGate>>,
    ) -> Result<SessionMaterialization, Error> {
        let mut mutation_gate = mutation_gate;
        let mut activity = None;
        if let (Some(service), Some(cwd)) = (self.workspace_service(), cwd.as_deref())
            && let Ok(repo) = crate::worktree::identity::resolve_repository(cwd)
            && let Some(record) = service
                .registry()
                .get_workspace_by_checkout(&repo.checkout.root)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
        {
            let lease = crate::worktree::gate::CheckoutActivityLease::acquire(
                service.storage_root(),
                &repo.checkout.root,
                false,
            )
            .map_err(worktree_error_to_acp)?;
            let record = service
                .registry()
                .get_workspace(&record.id)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
                .ok_or_else(|| invalid_workspace_request("workspace disappeared"))?;
            if record.lifecycle != crate::worktree::WorkspaceLifecycle::Ready {
                return Err(invalid_workspace_request(
                    "managed workspace is no longer ready; refusing actor startup",
                ));
            }
            let mut candidate = session_id.clone();
            let mut permitted = false;
            for _ in 0..32 {
                if record.ownership.owner_session_id.as_deref() == Some(candidate.as_str()) {
                    permitted = true;
                    break;
                }
                let Some(session) = self
                    .config
                    .provider
                    .history_store()
                    .get_session(&candidate)
                    .await
                    .map_err(|err| Error::internal_error().data(err.to_string()))?
                else {
                    break;
                };
                let Some(parent) = session.parent_session_id else {
                    break;
                };
                let Some((public_id, parent_cwd)) = self
                    .config
                    .provider
                    .history_store()
                    .get_session_ref_by_internal_id(parent)
                    .await
                    .map_err(|err| Error::internal_error().data(err.to_string()))?
                else {
                    break;
                };
                if parent_cwd
                    .as_deref()
                    .is_none_or(|cwd| !cwd.starts_with(&record.checkout_path))
                {
                    break;
                }
                candidate = public_id;
            }
            if !permitted {
                return Err(invalid_workspace_request(
                    "managed checkout is owned by another session; create a fresh worktree",
                ));
            }
            if mutation_gate.is_none() {
                mutation_gate = Some(Arc::new(
                    crate::worktree::CheckoutMutationGate::new(
                        service.storage_root(),
                        &record.checkout_path,
                    )
                    .map_err(worktree_error_to_acp)?,
                ));
            }
            activity = Some(lease);
        }
        if mutation_gate.is_some() && activity.is_none() {
            return Err(invalid_workspace_request(
                "managed checkout identity is unavailable; refusing actor startup",
            ));
        }
        let tool_state = crate::agent::core::McpToolState::empty();
        let mcp_services = crate::agent::protocol::build_mcp_state(
            mcp_servers,
            self.config.pending_elicitations(),
            self.config.event_sink.clone(),
            session_id.clone(),
            &crate::agent::mcp::agent_implementation(),
            tool_state.clone(),
            cwd.as_deref(),
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

        let initial_reasoning_effort = self.config.provider.initial_config().reasoning_effort;
        let reasoning_effort = match self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&session_id)
            .await
        {
            Ok(Some(config)) => match config
                .params
                .as_ref()
                .and_then(|params| params.get("reasoning_effort"))
            {
                Some(value) => match serde_json::from_value(value.clone()) {
                    Ok(effort) => Some(effort),
                    Err(err) => {
                        tracing::warn!(
                            session_id,
                            error = %err,
                            "invalid persisted reasoning effort; using initial config"
                        );
                        initial_reasoning_effort
                    }
                },
                None => None,
            },
            Ok(None) => {
                tracing::warn!(
                    session_id,
                    "session LLM config missing; using initial reasoning config"
                );
                initial_reasoning_effort
            }
            Err(err) => {
                tracing::warn!(
                    session_id,
                    error = %err,
                    "failed to load persisted reasoning effort; using initial config"
                );
                initial_reasoning_effort
            }
        };
        let control_state = match self
            .config
            .provider
            .history_store()
            .get_session_control(&session_id)
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = %error,
                    "failed to load persisted session control; rebuilding from session state"
                );
                None
            }
        };
        let runtime = SessionRuntime::new(cwd.clone(), mcp_services, tool_state);
        let _ = runtime.managed_checkout.set(mutation_gate.is_some());
        if let Some(activity) = activity {
            let _ = runtime.workspace_activity.set(activity);
        }
        let mutation_gate = match mutation_gate {
            Some(gate) => Some(gate),
            None => match (self.workspace_service(), cwd.as_deref()) {
                (Some(service), Some(cwd)) => {
                    match crate::worktree::identity::resolve_repository(cwd) {
                        Ok(repo) => Some(Arc::new(
                            crate::worktree::CheckoutMutationGate::new(
                                service.storage_root(),
                                &repo.checkout.root,
                            )
                            .map_err(worktree_error_to_acp)?,
                        )),
                        Err(_) => None,
                    }
                }
                _ => None,
            },
        };
        if let Some(gate) = mutation_gate {
            // Freshly created runtime, so this cannot already be initialized.
            let _ = runtime.mutation_gate.set(gate);
        }
        #[cfg(feature = "remote")]
        let mut actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone())
            .with_reasoning_effort(reasoning_effort)
            .with_mesh(if _options.attach_mesh_handle {
                // Use the mesh handle from the materializer's shared state
                self.mesh()
            } else {
                None
            });
        #[cfg(not(feature = "remote"))]
        let mut actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone())
            .with_reasoning_effort(reasoning_effort);
        if let Some(control_state) = control_state {
            actor = actor.with_control_state(control_state);
        }
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

/// Map worktree failures to ACP errors. A conflicting cwd request is the
/// caller's mistake (invalid params); missing or failed workspaces and
/// allocation failures are server-side diagnostics.
fn worktree_error_to_acp(err: crate::worktree::WorktreeError) -> Error {
    let message = err.to_string();
    match &err {
        crate::worktree::WorktreeError::SessionCwdConflict { .. } => {
            Error::invalid_params().data(serde_json::json!({
                "message": message,
            }))
        }
        _ => Error::internal_error().data(serde_json::json!({
            "message": message,
        })),
    }
}

/// A malformed workspace request (options or missing cwd) is invalid params.
fn invalid_workspace_request(message: &str) -> Error {
    Error::invalid_params().data(serde_json::json!({
        "message": message,
    }))
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
        let factory = Arc::new(TestProviderFactory::new(shared));
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

/// Workspace binding for new/load/resume (git-worktree-workspaces).
///
/// Exercises the real allocator against a real Git repository through the
/// same `WorkspaceService` a configured agent would use, so the fail-closed
/// paths (missing checkout, conflicting cwd, duplicate writable owner) are
/// observed end to end.
#[cfg(test)]
mod worktree_binding_tests {
    use super::*;
    use crate::acp::protocol::SessionId;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::session::backend::StorageBackend;
    use crate::session::provider::SessionProvider;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::SessionStore as _;
    use crate::test_utils::{
        MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
    };
    use crate::tools::context::ToolContext as _;
    use crate::worktree::test_support::{commit_files, init_repo, materialize};
    use crate::worktree::{CliGit, WorkspaceService};
    use querymt::LLMParams;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct WorktreeFixture {
        materializer: SessionMaterializer,
        storage: Arc<SqliteStorage>,
        service: Arc<WorkspaceService>,
        repo: tempfile::TempDir,
        storage_root: tempfile::TempDir,
    }

    async fn worktree_fixture(configure_service: bool) -> WorktreeFixture {
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let provider_inner = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider_inner,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, _plugin_dir) = mock_plugin_registry(factory).expect("registry");
        let provider = Arc::new(SessionProvider::new(
            Arc::new(plugin_registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        ));

        let repo = init_repo();
        commit_files(repo.path(), "initial", &[("README.md", "hello\n")]);
        materialize(repo.path(), &[("README.md", "hello\n")]);

        let storage_root = tempfile::TempDir::new().unwrap();
        let service = Arc::new(WorkspaceService::new(
            Arc::new(CliGit::new()),
            storage
                .workspace_repository()
                .expect("sqlite storage exposes a workspace repository"),
            storage_root.path(),
        ));
        let mut builder =
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .with_tool_policy(ToolPolicy::ProviderOnly);
        if configure_service {
            builder = builder.with_workspace_service(service.clone());
        }
        let materializer = SessionMaterializer::new(Arc::new(builder.build()));
        WorktreeFixture {
            materializer,
            storage,
            service,
            repo,
            storage_root,
        }
    }

    fn worktree_meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt".to_string(),
            serde_json::json!({ crate::worktree::SESSION_WORKSPACE_META_KEY: value }),
        );
        meta
    }

    fn isolation_meta() -> serde_json::Map<String, serde_json::Value> {
        worktree_meta(serde_json::json!({ "execution": "worktree" }))
    }

    #[tokio::test]
    async fn isolated_new_session_binds_the_worktree_cwd_and_persists_the_association() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");

        // The bound execution cwd lives under durable storage, not the source.
        let bound = prepared.cwd.clone().expect("bound cwd");
        assert!(
            bound.starts_with(fx.storage_root.path()),
            "bound cwd {bound:?} must live under the durable storage root"
        );
        assert_eq!(prepared.runtime.cwd.as_deref(), Some(bound.as_path()));

        // The association is persisted with this session as its owner.
        let record = fx
            .storage
            .workspace_repository()
            .unwrap()
            .get_workspace_by_session(&prepared.session_id)
            .await
            .expect("registry readable")
            .expect("workspace association persisted");
        assert_eq!(record.session_cwd(), bound);
        assert_eq!(
            record.ownership.owner_session_id.as_deref(),
            Some(prepared.session_id.as_str())
        );

        // The checkout-scoped mutation gate is installed on the runtime.
        assert!(prepared.runtime.mutation_gate.get().is_some());
    }

    #[tokio::test]
    async fn isolated_new_session_can_include_current_working_tree_changes() {
        let fx = worktree_fixture(true).await;
        // Dirty the source: a tracked edit plus a non-ignored untracked file.
        std::fs::write(fx.repo.path().join("README.md"), "dirty\n").unwrap();
        std::fs::write(fx.repo.path().join("notes.txt"), "untracked\n").unwrap();

        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(worktree_meta(
            serde_json::json!({
                "execution": "worktree",
                "includeLocalChanges": true,
            }),
        ));
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");

        let bound = prepared.cwd.clone().expect("bound cwd");
        assert_eq!(
            std::fs::read_to_string(bound.join("README.md")).unwrap(),
            "dirty\n",
            "tracked working contents must be included"
        );
        assert_eq!(
            std::fs::read_to_string(bound.join("notes.txt")).unwrap(),
            "untracked\n",
            "non-ignored untracked files must be included"
        );
        // The source checkout keeps its own dirty state untouched.
        assert_eq!(
            std::fs::read_to_string(fx.repo.path().join("README.md")).unwrap(),
            "dirty\n"
        );
    }

    #[tokio::test]
    async fn requested_isolation_fails_closed_without_a_workspace_service() {
        let fx = worktree_fixture(false).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let err = match fx.materializer.prepare_new_session(req).await {
            Err(err) => err,
            Ok(_) => panic!("isolation without a service must fail closed"),
        };
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("no workspace service"),
            "diagnostic: {rendered}"
        );
    }

    #[tokio::test]
    async fn load_uses_the_persisted_binding_and_rejects_conflicting_cwd() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");
        let session_id = prepared.session_id.clone();
        let bound = prepared.cwd.clone().expect("bound cwd");

        // Loading without a cwd resolves the persisted bound path. The
        // prepared session is dropped before the next request: load/resume
        // single-flight holds the per-session guard until the PreparedSession
        // is dropped, and this test never registers it.
        let reloaded_cwd = {
            let load = LoadSessionRequest::new(SessionId::from(session_id.clone()), PathBuf::new());
            match fx
                .materializer
                .prepare_load_session(load, None)
                .await
                .unwrap()
            {
                PreparedSessionResult::Prepared(p) => p,
                PreparedSessionResult::AlreadyRegistered(_) => {
                    panic!("session was never registered")
                }
            }
            .cwd
            .expect("bound cwd")
        };
        assert_eq!(reloaded_cwd, bound);

        // A conflicting cwd request fails closed.
        let conflicting = LoadSessionRequest::new(
            SessionId::from(session_id.clone()),
            fx.repo.path().to_path_buf(),
        );
        let err = match fx
            .materializer
            .prepare_load_session(conflicting, None)
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("conflicting cwd must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("conflicting cwd"),
            "diagnostic: {err:?}"
        );
    }

    #[tokio::test]
    async fn resume_uses_the_persisted_binding_and_rejects_conflicting_cwd() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");
        let session_id = prepared.session_id.clone();
        let bound = prepared.cwd.clone().expect("bound cwd");

        // Resume with the bound path succeeds and keeps the bound cwd. The
        // prepared session is dropped before the next request so the
        // single-flight guard does not block it.
        {
            let resume =
                ResumeSessionRequest::new(SessionId::from(session_id.clone()), bound.clone());
            let resumed = match fx
                .materializer
                .prepare_resume_session(resume, None)
                .await
                .unwrap()
            {
                PreparedSessionResult::Prepared(p) => p,
                PreparedSessionResult::AlreadyRegistered(_) => {
                    panic!("session was never registered")
                }
            };
            assert_eq!(resumed.cwd.as_deref(), Some(bound.as_path()));
            assert!(resumed.runtime.mutation_gate.get().is_some());
        }

        // Resume with the original source cwd is rejected.
        let conflicting = ResumeSessionRequest::new(
            SessionId::from(session_id.clone()),
            fx.repo.path().to_path_buf(),
        );
        let err = match fx
            .materializer
            .prepare_resume_session(conflicting, None)
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("conflicting cwd must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("conflicting cwd"),
            "diagnostic: {err:?}"
        );
    }

    #[tokio::test]
    async fn missing_checkout_fails_closed_on_load_and_resume() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");
        let session_id = prepared.session_id.clone();

        let record = fx
            .storage
            .workspace_repository()
            .unwrap()
            .get_workspace_by_session(&session_id)
            .await
            .unwrap()
            .expect("workspace persisted");
        // Simulate external deletion of the checkout.
        std::fs::remove_dir_all(&record.checkout_path).unwrap();

        let load = LoadSessionRequest::new(SessionId::from(session_id.clone()), PathBuf::new());
        let err = match fx.materializer.prepare_load_session(load, None).await {
            Err(err) => err,
            Ok(_) => panic!("missing checkout must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("missing"),
            "diagnostic: {err:?}"
        );

        // The lifecycle now durably records the missing checkout, so resume
        // also refuses attachment instead of falling back.
        let resume = ResumeSessionRequest::new(SessionId::from(session_id.clone()), PathBuf::new());
        let err = match fx.materializer.prepare_resume_session(resume, None).await {
            Err(err) => err,
            Ok(_) => panic!("missing checkout must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("cannot be attached"),
            "diagnostic: {err:?}"
        );
    }

    #[tokio::test]
    async fn local_requests_preserve_existing_behavior() {
        let fx = worktree_fixture(true).await;

        // No workspace meta at all.
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local session prepares");
        assert!(prepared.runtime.mutation_gate.get().is_some());
        assert_eq!(prepared.runtime.managed_checkout.get(), Some(&false));
        let association = fx
            .storage
            .workspace_repository()
            .unwrap()
            .get_workspace_by_session(&prepared.session_id)
            .await
            .unwrap();
        assert!(association.is_none(), "local sessions stay unassociated");

        // Explicit local execution meta.
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf())
            .meta(worktree_meta(serde_json::json!({ "execution": "local" })));
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local session prepares");
        assert!(prepared.runtime.mutation_gate.get().is_some());
        assert_eq!(prepared.runtime.managed_checkout.get(), Some(&false));
    }

    #[tokio::test]
    async fn local_creation_cannot_attach_to_owned_managed_checkout() {
        let fx = worktree_fixture(true).await;
        let owner = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .unwrap();
        let cwd = owner.cwd.clone().unwrap();
        assert!(
            fx.materializer
                .prepare_new_session(NewSessionRequest::new(cwd.clone()))
                .await
                .is_err()
        );
        std::fs::create_dir(cwd.join("sub")).unwrap();
        assert!(
            fx.materializer
                .prepare_new_session(NewSessionRequest::new(cwd.join("sub")))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn preallocated_delegate_is_bound_before_runtime_startup() {
        let fx = worktree_fixture(true).await;
        let parent = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .unwrap();
        let parent_record = fx
            .service
            .registry()
            .get_workspace_by_session(&parent.session_id)
            .await
            .unwrap()
            .unwrap();
        let child = fx
            .service
            .allocate_for_delegation(
                &parent.session_id,
                Some(&parent_record),
                parent.cwd.as_deref().unwrap(),
                parent.runtime.mutation_gate.get().map(Arc::as_ref),
                "delegate",
                Some("batch".into()),
            )
            .await
            .unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("parent_session_id".into(), parent.session_id.clone().into());
        let prepared = fx
            .materializer
            .prepare_new_session_with_workspace(
                NewSessionRequest::new(child.effective_cwd.clone()).meta(meta),
                Some(child.clone()),
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            prepared.runtime.mutation_gate.get().unwrap(),
            &child.mutation_gate
        ));
        let bound = fx
            .service
            .registry()
            .get_workspace_by_session(&prepared.session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.id, child.record.id);
        assert_eq!(
            prepared.runtime.cwd.as_deref(),
            Some(child.effective_cwd.as_path())
        );
    }

    #[tokio::test]
    async fn duplicate_writable_owner_fails_closed() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let prepared = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated session prepares");

        let record = fx
            .storage
            .workspace_repository()
            .unwrap()
            .get_workspace_by_session(&prepared.session_id)
            .await
            .unwrap()
            .expect("workspace persisted");

        // A second session cannot attach as a second writable owner; the
        // registry's atomic CAS keeps exactly one owner.
        let err = fx
            .service
            .bind_owner(&record.id, "other-session")
            .await
            .expect_err("second writable owner must be rejected");
        assert!(err.to_string().contains("already owned"), "error: {err}");
    }

    fn user_message(session_id: &str, id: &str) -> crate::model::AgentMessage {
        let mut message =
            crate::model::AgentMessage::new(session_id.to_string(), querymt::chat::ChatRole::User);
        message.id = id.to_string();
        message
    }

    async fn seed_history(storage: &SqliteStorage, session_id: &str, ids: &[&str]) {
        for id in ids {
            storage
                .add_message(session_id, user_message(session_id, id))
                .await
                .expect("history seeded");
        }
    }

    fn workspace_meta_of(
        response: &crate::acp::protocol::ForkSessionResponse,
    ) -> serde_json::Value {
        let meta = response
            .meta
            .as_ref()
            .expect("isolated fork discloses workspace meta");
        meta[crate::acp::shared::QUERYMT_META_NAMESPACE]
            [crate::worktree::SESSION_WORKSPACE_META_KEY]
            .clone()
    }

    #[tokio::test]
    async fn isolated_fork_allocates_its_own_workspace_with_current_contents() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated source prepares");
        let source_id = source.session_id.clone();
        let source_checkout = source.cwd.clone().expect("source bound cwd");
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1", "msg-2"]).await;

        // The source workspace's CURRENT working state diverges from its
        // baseline after creation.
        std::fs::write(source_checkout.join("README.md"), "edited after creation\n").unwrap();
        std::fs::write(source_checkout.join("notes2.txt"), "untracked now\n").unwrap();

        // Fork at the HISTORICAL message point msg-1 while requesting
        // isolation: the message fork point and the workspace are recorded
        // separately, and the checkout reflects current contents.
        let mut meta = isolation_meta();
        meta.insert(
            "querymt".to_string(),
            serde_json::json!({
                "message_id": "msg-1",
                crate::worktree::SESSION_WORKSPACE_META_KEY: { "execution": "worktree" },
            }),
        );
        let response = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    source_checkout.clone(),
                )
                .meta(meta),
            )
            .await
            .expect("isolated fork prepares");
        let fork_id = response.session_id.to_string();

        // Disclosure: fresh working state, not historical filesystem state.
        let workspace_meta = workspace_meta_of(&response);
        assert_eq!(workspace_meta["execution"], "worktree");
        assert_eq!(workspace_meta["disclosure"], "fresh-working-state");
        assert!(
            workspace_meta["message"]
                .as_str()
                .unwrap()
                .contains("current contents")
        );

        // The fork owns its own workspace; paths never overlap the source's.
        let registry = fx.storage.workspace_repository().unwrap();
        let source_record = registry
            .get_workspace_by_session(&source_id)
            .await
            .unwrap()
            .expect("source still bound");
        let fork_record = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork bound to its own workspace");
        assert_ne!(source_record.id, fork_record.id);
        assert_ne!(source_record.checkout_path, fork_record.checkout_path);
        assert_eq!(
            fork_record.ownership.owner_session_id.as_deref(),
            Some(fork_id.as_str())
        );
        assert_eq!(
            source_record.ownership.owner_session_id.as_deref(),
            Some(source_id.as_str()),
            "the source keeps its own workspace"
        );

        // The fork's files reflect the source's CURRENT contents (tracked
        // edit plus non-ignored untracked file), not the historical state
        // at the msg-1 fork point.
        assert_eq!(
            std::fs::read_to_string(fork_record.checkout_path.join("README.md")).unwrap(),
            "edited after creation\n"
        );
        assert!(fork_record.checkout_path.join("notes2.txt").exists());

        // The historical message fork point is recorded on the fork row
        // (internal message reference + fork origin), separately from the
        // workspace association above.
        let fork_row = fx
            .storage
            .get_session(&fork_id)
            .await
            .unwrap()
            .expect("fork row exists");
        assert!(
            fork_row.fork_point_ref.is_some(),
            "the message fork point is recorded separately from the workspace"
        );
        assert_eq!(
            fork_row.fork_origin,
            Some(crate::session::domain::ForkOrigin::User)
        );
    }

    #[tokio::test]
    async fn isolated_fork_from_local_source_uses_the_source_checkout() {
        let fx = worktree_fixture(true).await;
        // Local source session in the fixture repository.
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local source prepares");
        let source_id = source.session_id.clone();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        // The repository's current working contents.
        std::fs::write(fx.repo.path().join("scratch.txt"), "local edit\n").unwrap();

        let response = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    fx.repo.path().to_path_buf(),
                )
                .meta(isolation_meta()),
            )
            .await
            .expect("isolated fork prepares");
        let fork_id = response.session_id.to_string();

        let registry = fx.storage.workspace_repository().unwrap();
        assert!(
            registry
                .get_workspace_by_session(&source_id)
                .await
                .unwrap()
                .is_none(),
            "the local source stays unassociated"
        );
        let fork_record = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork bound");
        assert!(
            fork_record
                .checkout_path
                .starts_with(fx.storage_root.path()),
            "fork checkout lives under durable storage"
        );
        assert_eq!(
            std::fs::read_to_string(fork_record.checkout_path.join("scratch.txt")).unwrap(),
            "local edit\n",
            "fork discloses the source's current contents"
        );
    }

    #[tokio::test]
    async fn isolated_fork_inherits_isolation_without_metadata() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("isolated source prepares");
        let source_id = source.session_id.clone();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        let response = fx
            .materializer
            .prepare_fork_session(ForkSessionRequest::new(
                SessionId::from(source_id.clone()),
                fx.repo.path().to_path_buf(),
            ))
            .await
            .expect("local fork prepares");
        let fork_id = response.session_id.to_string();

        assert!(response.meta.is_some(), "inherited isolation is disclosed");
        let registry = fx.storage.workspace_repository().unwrap();
        let fork_record = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork receives its own workspace");
        let source_record = registry
            .get_workspace_by_session(&source_id)
            .await
            .unwrap()
            .expect("source unchanged");
        assert_eq!(
            source_record.ownership.owner_session_id.as_deref(),
            Some(source_id.as_str())
        );
        assert_ne!(fork_record.checkout_path, source_record.checkout_path);
        let fork_row = fx
            .storage
            .get_session(&fork_id)
            .await
            .unwrap()
            .expect("message copy exists");
        assert!(
            fork_row.fork_point_ref.is_some(),
            "a local fork still records its message fork point"
        );
    }

    #[tokio::test]
    async fn isolated_fork_without_a_workspace_service_fails_closed() {
        let fx = worktree_fixture(false).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local source prepares");
        let source_id = source.session_id.clone();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        let err = match fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    fx.repo.path().to_path_buf(),
                )
                .meta(isolation_meta()),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("isolation without a service must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("no workspace service"),
            "diagnostic: {err:?}"
        );
    }

    #[tokio::test]
    async fn two_sessions_resolve_identical_relative_paths_differently() {
        let fx = worktree_fixture(true).await;
        // Two independent isolated sessions over the same repository.
        let session_a = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .expect("session a prepares");
        let session_b = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .expect("session b prepares");

        let cwd_a = session_a.cwd.clone().expect("session a bound cwd");
        let cwd_b = session_b.cwd.clone().expect("session b bound cwd");
        assert_ne!(cwd_a, cwd_b, "workspaces never share an execution path");

        // Identical relative paths resolve inside each session's own
        // workspace and observe only its contents.
        std::fs::write(cwd_a.join("notes.txt"), "workspace a\n").unwrap();
        std::fs::write(cwd_b.join("notes.txt"), "workspace b\n").unwrap();

        let ctx_a = crate::tools::context_impl::AgentToolContext::basic(
            session_a.session_id.clone(),
            Some(cwd_a),
        );
        let ctx_b = crate::tools::context_impl::AgentToolContext::basic(
            session_b.session_id.clone(),
            Some(cwd_b),
        );
        let resolved_a = ctx_a.resolve_path("notes.txt").unwrap();
        let resolved_b = ctx_b.resolve_path("notes.txt").unwrap();
        assert_ne!(resolved_a, resolved_b);
        assert_eq!(
            std::fs::read_to_string(resolved_a).unwrap(),
            "workspace a\n"
        );
        assert_eq!(
            std::fs::read_to_string(resolved_b).unwrap(),
            "workspace b\n"
        );

        // Neither session's relative view touched the source checkout.
        assert!(!fx.repo.path().join("notes.txt").exists());
    }

    #[tokio::test]
    async fn indexes_are_scoped_per_workspace_not_repository() {
        let fx = worktree_fixture(true).await;
        let session_a = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .expect("session a prepares");
        let session_b = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .expect("session b prepares");
        let cwd_a = session_a.cwd.clone().expect("session a bound cwd");
        let cwd_b = session_b.cwd.clone().expect("session b bound cwd");

        // A linked worktree's `.git` file marks the checkout itself as the
        // workspace root, so each session's index resolves to its own root
        // even though both checkouts share one repository identity.
        let root_a = crate::index::resolve_workspace_root(&cwd_a);
        let root_b = crate::index::resolve_workspace_root(&cwd_b);
        assert_ne!(
            root_a, root_b,
            "per-workspace indexes must not share a workspace root"
        );

        // The index manager keys actors by root: distinct roots get
        // distinct workspace indexes (never shared by common repository).
        let manager = crate::index::WorkspaceIndexManagerActor::new(
            crate::index::WorkspaceIndexManagerConfig::default(),
        );
        let handle_a = crate::index::get_or_create_workspace_with_timeout(&manager, root_a.clone())
            .await
            .expect("index a");
        let handle_b = crate::index::get_or_create_workspace_with_timeout(&manager, root_b.clone())
            .await
            .expect("index b");
        // The initial index snapshot carries its workspace root.
        let index_a = handle_a.file_index().expect("index a initialized");
        let index_b = handle_b.file_index().expect("index b initialized");
        assert_eq!(index_a.root, root_a);
        assert_eq!(index_b.root, root_b);
        assert_ne!(index_a.root, index_b.root);
    }
}
