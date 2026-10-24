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

/// A validated fork isolation plan, resolved entirely BEFORE any
/// conversation copy so invalid or unhonorable requests create no fork.
struct ForkWorkspacePlan {
    source_cwd: std::path::PathBuf,
    options: crate::worktree::SessionCreationOptions,
    source_gate: Option<std::sync::Arc<crate::worktree::gate::CheckoutMutationGate>>,
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
            // Decide from checkout identity, not ownership: an explicitly
            // shared child (at any nesting depth) executes in a managed
            // checkout without owning a workspace binding, and its MCP
            // integrations must be validated against that checkout too.
            let owns_workspace = service
                .registry()
                .get_workspace_by_session(session_id)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
                .is_some();
            let in_managed_checkout = if owns_workspace {
                true
            } else if let Some(cwd) = cwd.as_deref()
                && let Ok(repo) = crate::worktree::identity::resolve_repository(cwd)
            {
                service
                    .registry()
                    .get_workspace_by_checkout(&repo.checkout.root)
                    .await
                    .map_err(|err| Error::internal_error().data(err.to_string()))?
                    .is_some()
            } else {
                false
            };
            if in_managed_checkout {
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
                "code": "workspace_service_unavailable",
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

        // The configured default baseline only fills in a choice the request
        // left open; the resolved mode is disclosed by the binding's baseline.
        let options =
            crate::worktree::apply_default_baseline(options, meta, service.default_baseline());

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
        if self
            .config
            .provider
            .history_store()
            .get_pending_fork_isolation(session_id)
            .await
            .map_err(|err| Error::internal_error().data(err.to_string()))?
            .is_some()
        {
            return Err(invalid_workspace_request(
                "fork isolation is pending; refusing local fallback before workspace creation completes",
            ));
        }
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
                        // A shared child may attach when its parent is the
                        // owner or itself a shared descendant of the owner
                        // (nested shared delegation P -> C -> D).
                        let permitted = match parent_session_id {
                            Some(parent) => {
                                self.session_shares_managed_checkout(&record, parent)
                                    .await?
                            }
                            None => false,
                        };
                        if !permitted {
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
        // Durable non-runnable isolation barrier for an isolated session,
        // written atomically with the session row. Until the session has
        // bound its owner AND started, a later load/resume (even one that
        // supplies the original unmanaged source cwd) is refused instead of
        // silently executing in the source checkout.
        let isolation_intent =
            workspace
                .as_ref()
                .map(|binding| crate::session::store::PendingForkIsolation {
                    source_session_id: String::new(),
                    source_cwd: requested_cwd
                        .clone()
                        .unwrap_or_else(|| binding.record.repository.origin_path.clone()),
                    options: crate::worktree::WorkspaceCreationOptions {
                        execution: crate::worktree::ExecutionMode::Worktree,
                        baseline: binding.record.requested_baseline_mode,
                        base_ref: binding.record.requested_base_ref.clone(),
                    },
                });
        let cwd = match &workspace {
            Some(binding) => Some(binding.effective_cwd.clone()),
            None => requested_cwd,
        };

        // Create session in database (heavy I/O); isolated sessions persist
        // the bound execution path as their cwd.
        let session_context = self
            .config
            .provider
            .create_session_with_isolation_intent(
                cwd.clone(),
                parent_session_id,
                &self.config.execution_config_snapshot(),
                isolation_intent,
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        let session_id = session_context.session().public_id.clone();

        // Persist the durable session-to-workspace association. The registry
        // enforces a single writable owner atomically, so a duplicate owner
        // attempt fails closed instead of sharing the checkout. A failed
        // binding is compensated: the never-owned workspace is marked
        // `Failed` (resources retained for explicit recovery/removal).
        if let Some(binding) = &workspace {
            let service = self.workspace_service().ok_or_else(|| {
                Error::internal_error().data(serde_json::json!({
                    "message": "workspace service disappeared during session creation",
                }))
            })?;
            if let Err(err) = service.bind_owner(&binding.record.id, &session_id).await {
                let _ = service
                    .mark_owner_binding_failed(
                        &binding.record.id,
                        &format!("owner binding failed; workspace retained unused: {err}"),
                    )
                    .await;
                return Err(worktree_error_to_acp(err));
            }
        }

        // Everything from here to a running actor can fail (MCP root
        // validation, MCP startup, actor spawn). A freshly bound workspace
        // that never reaches a running session must not stay Ready and
        // owned by a dead session: release it as failed (resources retained
        // for explicit recovery/removal).
        let started = async {
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

            // The session is bound and running: release the isolation
            // barrier. Any failure above leaves it in place, so the session
            // stays non-runnable instead of falling back to local execution.
            if workspace.is_some() {
                self.config
                    .provider
                    .history_store()
                    .complete_fork_isolation(&session_id)
                    .await
                    .map_err(|err| Error::internal_error().data(err.to_string()))?;
            }
            Ok::<_, Error>((resolved, materialization))
        }
        .await;
        let (resolved, materialization) = match started {
            Ok(started) => started,
            Err(err) => {
                if let Some(binding) = &workspace {
                    self.release_unstarted_workspace(binding, &err).await;
                }
                return Err(err);
            }
        };

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

    /// Whether `start_session_id` may run in the managed checkout of
    /// `record`: it is the workspace owner, or a (possibly nested) shared
    /// descendant of the owner whose ancestors all execute inside that
    /// checkout. Walks the parent chain (bounded) and fails closed on any
    /// missing link or an ancestor outside the checkout.
    async fn session_shares_managed_checkout(
        &self,
        record: &crate::worktree::WorkspaceRecord,
        start_session_id: &str,
    ) -> Result<bool, Error> {
        let store = self.config.provider.history_store();
        let mut candidate = start_session_id.to_string();
        for _ in 0..32 {
            if record.ownership.owner_session_id.as_deref() == Some(candidate.as_str()) {
                return Ok(true);
            }
            let Some(session) = store
                .get_session(&candidate)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
            else {
                return Ok(false);
            };
            let Some(parent) = session.parent_session_id else {
                return Ok(false);
            };
            let Some((public_id, parent_cwd)) = store
                .get_session_ref_by_internal_id(parent)
                .await
                .map_err(|err| Error::internal_error().data(err.to_string()))?
            else {
                return Ok(false);
            };
            if parent_cwd
                .as_deref()
                .is_none_or(|cwd| !cwd.starts_with(&record.checkout_path))
            {
                return Ok(false);
            }
            candidate = public_id;
        }
        Ok(false)
    }

    /// Compensate a workspace whose session failed to start after the owner
    /// was bound: detach the dead owner and mark the workspace `Failed` with
    /// a diagnostic. Checkout and branch are retained; nothing is deleted.
    async fn release_unstarted_workspace(&self, binding: &WorkspaceBinding, cause: &Error) {
        let Some(service) = self.workspace_service() else {
            return;
        };
        let _ = service.registry().detach_owner(&binding.record.id).await;
        let _ = service
            .mark_owner_binding_failed(
                &binding.record.id,
                &format!(
                    "session startup failed after workspace binding; workspace retained unused: {}",
                    cause.message
                ),
            )
            .await;
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

        if let Some(reg) = registry
            && let Some(existing) =
                crate::agent::session_registry::live_session(reg, &session_id).await
        {
            return Ok(PreparedSessionResult::AlreadyRegistered(existing));
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

        if let Some(reg) = registry
            && let Some(existing) =
                crate::agent::session_registry::live_session(reg, &session_id).await
        {
            return Ok(PreparedSessionResult::AlreadyRegistered(existing));
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

        // Resolve and validate the fork's isolation policy BEFORE any
        // conversation copy: invalid or unhonorable requests fail closed
        // here, creating no fork row and copying no history (F2). A
        // validated isolated fork persists its pending provisioning intent
        // during allocation, before any Git mutation.
        let fork_plan = self
            .plan_fork_isolation(
                &source_session_id,
                session.cwd.as_deref(),
                req.meta.as_ref(),
            )
            .await?;

        // Persist the startup barrier atomically with the conversation copy,
        // before allocation can fail or the process can be interrupted.
        let pending_intent =
            fork_plan
                .as_ref()
                .map(|plan| crate::session::store::PendingForkIsolation {
                    source_session_id: source_session_id.clone(),
                    source_cwd: plan.source_cwd.clone(),
                    options: plan.options.to_workspace_options(),
                });
        let new_session_id = self
            .config
            .provider
            .history_store()
            .fork_session_with_isolation_intent(
                &source_session_id,
                &target_message_id,
                crate::session::domain::ForkOrigin::User,
                pending_intent,
            )
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        // Allocate the fork's own workspace per the validated plan; local
        // forks stay pure message copies. Allocation persists the pending
        // isolation intent (a `Provisioning` workspace record) before any
        // Git mutation, then binds the fork as the single writable owner.
        let fork_workspace = match fork_plan {
            Some(plan) => Some(self.execute_fork_isolation(plan, &new_session_id).await?),
            None => None,
        };

        let response = if let Some(binding) = &fork_workspace {
            self.finalize_fork_isolation(&new_session_id, binding)
                .await?
        } else {
            ForkSessionResponse::new(new_session_id)
        };
        Ok(response)
    }

    /// Resolve and validate the fork's isolation policy BEFORE the
    /// conversation copy (F2).
    ///
    /// Returns `Ok(None)` for local forks of unmanaged sources (pure message
    /// copy, no new workspace). Invalid meta, invalid option combinations, a
    /// local request against a managed source, missing isolation support, or
    /// a source without a usable cwd all fail closed here — no fork row is
    /// created and no history is copied. A managed source inherits isolation
    /// when the fork requests none.
    async fn plan_fork_isolation(
        &self,
        source_session_id: &str,
        source_session_cwd: Option<&std::path::Path>,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Option<ForkWorkspacePlan>, Error> {
        let requested = match crate::worktree::fork_creation_options_from_meta(meta)
            .map_err(|message| invalid_workspace_request(&message))?
        {
            Some(options) => {
                options
                    .validate()
                    .map_err(|message| invalid_workspace_request(&message))?;
                Some(options)
            }
            None => None,
        };
        let source_binding = self
            .resolve_existing_workspace(source_session_id, None)
            .await?;

        let options = match requested {
            Some(options) => options,
            None if source_binding.is_some() => {
                // A managed source's fork stays isolated by default; there
                // is no legacy local checkout to fall back to.
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

        if self.workspace_service().is_none() {
            return Err(Error::internal_error().data(serde_json::json!({
                "message": "worktree isolation requested but no workspace service is \
                            configured; refusing to downgrade to shared execution",
            })));
        }

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
        // Isolated execution is local-filesystem-only in this version; fail
        // before copying any conversation state.
        crate::worktree::placement::ensure_local_filesystem_execution(&source_cwd)
            .map_err(worktree_error_to_acp)?;
        crate::worktree::resolve_repository(&source_cwd).map_err(worktree_error_to_acp)?;

        // Serialize baseline capture against cooperating writes when the
        // source is a live managed workspace.
        Ok(Some(ForkWorkspacePlan {
            source_cwd,
            options,
            source_gate: source_binding.as_ref().map(|b| b.mutation_gate.clone()),
        }))
    }

    /// Execute a validated fork isolation plan after the conversation copy:
    /// allocate the fork's own workspace through the same create path as
    /// fresh sessions — under its own durable path, never the source
    /// session's checkout — and bind the fork session as the single writable
    /// owner. Allocation persists the pending isolation intent (a
    /// `Provisioning` workspace record) before any Git mutation and leaves
    /// it explicitly failed and non-runnable if provisioning fails.
    ///
    /// A failed owner binding is compensated: the freshly allocated,
    /// never-owned workspace is marked `Failed` (resources retained for
    /// explicit recovery/removal) and the pending intent stays in place, so
    /// the fork can never surface as a runnable local session.
    async fn execute_fork_isolation(
        &self,
        plan: ForkWorkspacePlan,
        fork_session_id: &str,
    ) -> Result<WorkspaceBinding, Error> {
        let service = self.workspace_service().ok_or_else(|| {
            Error::internal_error().data(serde_json::json!({
                "message": "workspace service disappeared during fork creation",
            }))
        })?;
        let binding = service
            .allocate_for_fork(&plan.source_cwd, &plan.options, plan.source_gate.as_deref())
            .await
            .map_err(worktree_error_to_acp)?;

        // Persist the association: the fork session is the new workspace's
        // single writable owner. The message fork point was recorded by the
        // session store and remains independent of this association.
        if let Err(err) = service
            .bind_owner(&binding.record.id, fork_session_id)
            .await
        {
            let _ = service
                .mark_owner_binding_failed(
                    &binding.record.id,
                    &format!("fork owner binding failed; workspace retained unused: {err}"),
                )
                .await;
            return Err(worktree_error_to_acp(err));
        }

        Ok(binding)
    }

    /// Explicitly complete a pending isolated fork's startup (retry path for
    /// failed or interrupted fork creation).
    ///
    /// Refuses when the session has no pending isolation intent or no
    /// workspace service — a pending fork is never downgraded to local
    /// execution. On success the fork owns exactly one ready workspace, its
    /// row cwd is the bound checkout, and the barrier is released, so
    /// load/resume resolve inside the fork's own checkout, never the
    /// source's. The response discloses that files reflect the source's
    /// contents at recovery time, not at fork-request time.
    ///
    /// # Errors
    /// Returns an error when the intent is missing, the workspace service is
    /// unavailable, provisioning/owner binding fails, or the binding cannot
    /// be published (cwd persistence or barrier release). The pending intent
    /// survives every failure so the fork stays non-runnable and retryable.
    pub async fn recover_pending_fork(
        &self,
        session_id: &str,
    ) -> Result<ForkSessionResponse, Error> {
        let intent = self
            .config
            .provider
            .history_store()
            .get_pending_fork_isolation(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .ok_or_else(|| {
                invalid_workspace_request("session has no pending fork isolation to recover")
            })?;
        let service = self.workspace_service().ok_or_else(|| {
            Error::internal_error().data(serde_json::json!({
                "message": "worktree isolation pending but no workspace service is \
                            configured; refusing to downgrade to shared execution",
            }))
        })?;
        let binding = service
            .recover_fork_isolation(session_id, &intent)
            .await
            .map_err(worktree_error_to_acp)?;
        self.finalize_fork_isolation(session_id, &binding).await
    }

    /// Publish an isolated fork as runnable: persist the execution cwd,
    /// release the durable startup barrier, and build the disclosure
    /// response.
    ///
    /// The barrier release is the atomic publication point: SQLite validates
    /// in one transaction that the session row cwd equals the ready
    /// workspace's bound execution path before deleting the pending intent.
    /// Until that transaction commits, load/resume reject the session, so a
    /// fork interrupted anywhere before it can never run — in its own
    /// checkout or its source's.
    async fn finalize_fork_isolation(
        &self,
        fork_session_id: &str,
        binding: &WorkspaceBinding,
    ) -> Result<ForkSessionResponse, Error> {
        // Persist the fork's execution cwd consistently with its durable
        // workspace owner association: the session row, workspace binding,
        // session listing, load, and resume must all agree on the fork's
        // checkout with no external session-row rewrite. `fork_session`
        // copied the source's cwd into the fork row; an isolated fork must
        // not keep it. Fail closed — a fork that cannot record its own
        // checkout must never surface as runnable in the source checkout.
        self.config
            .provider
            .history_store()
            .set_session_cwd(fork_session_id, Some(binding.effective_cwd.clone()))
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({
                    "message": "isolated fork could not persist its execution cwd; \
                                refusing to expose a runnable fork in the source checkout",
                    "error": e.to_string(),
                }))
            })?;
        self.config
            .provider
            .history_store()
            .complete_fork_isolation(fork_session_id)
            .await
            .map_err(|err| Error::internal_error().data(err.to_string()))?;

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
        Ok(ForkSessionResponse::new(fork_session_id.to_string()).meta(meta.as_object().cloned()))
    }

    /// Finalize session after registration: emit events, register in DHT, initialize workspace index.
    ///
    /// This is called AFTER the session has been inserted into the registry.
    /// It does NOT hold the registry lock.
    pub async fn finalize_session(&self, prepared: &PreparedSession) -> Result<(), Error> {
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
        let mut slash_registry: Option<Arc<crate::slash_commands::SlashCommandRegistry>> = None;
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
            let permitted = self
                .session_shares_managed_checkout(&record, &session_id)
                .await?;
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
            // Managed sessions advertise and expand slash commands from
            // their bound checkout so catalog and execution agree on the
            // same workspace sources. Discovery config (global/configured
            // paths, scripts policy) follows the agent startup config.
            if let Some(slash_config) = self
                .config
                .slash_commands_config
                .as_ref()
                .filter(|config| config.enabled)
            {
                slash_registry = Some(Arc::new(
                    crate::slash_commands::SlashCommandRegistry::registry_for_workspace(
                        slash_config,
                        &repo.checkout.root,
                    ),
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
            // Only managed-checkout sessions run MCP servers inside the
            // bound workspace; legacy local sessions keep the agent
            // process working directory exactly as before.
            if activity.is_some() {
                cwd.as_deref()
            } else {
                None
            },
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
        if let Some(registry) = slash_registry {
            // Freshly created runtime, so this cannot already be initialized.
            let _ = runtime.slash_registry.set(registry);
        }
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
    let code = err.code();
    match &err {
        crate::worktree::WorktreeError::SessionCwdConflict { .. } => {
            Error::invalid_params().data(serde_json::json!({
                "code": code,
                "message": message,
            }))
        }
        _ => Error::internal_error().data(serde_json::json!({
            "code": code,
            "message": message,
        })),
    }
}

/// A malformed workspace request (options or missing cwd) is invalid params.
fn invalid_workspace_request(message: &str) -> Error {
    Error::invalid_params().data(serde_json::json!({
        "code": "invalid_options",
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
    use crate::worktree::{GixRepoOps, WorkspaceService};
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

    /// Registry wrapper failing the first N `attach_owner` calls to inject
    /// an owner-binding failure after allocation has already succeeded.
    /// Every other operation is forwarded verbatim.
    struct FlakyOwnerBinding {
        inner: Arc<dyn crate::worktree::WorkspaceRepository>,
        remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::worktree::WorkspaceRepository for FlakyOwnerBinding {
        async fn insert_workspace(
            &self,
            record: crate::worktree::WorkspaceRecord,
        ) -> crate::session::error::SessionResult<crate::worktree::WorkspaceRecord> {
            self.inner.insert_workspace(record).await
        }

        async fn get_workspace(
            &self,
            id: &crate::worktree::WorkspaceId,
        ) -> crate::session::error::SessionResult<Option<crate::worktree::WorkspaceRecord>>
        {
            self.inner.get_workspace(id).await
        }

        async fn get_workspace_by_checkout(
            &self,
            checkout_path: &std::path::Path,
        ) -> crate::session::error::SessionResult<Option<crate::worktree::WorkspaceRecord>>
        {
            self.inner.get_workspace_by_checkout(checkout_path).await
        }

        async fn get_workspace_by_session(
            &self,
            session_id: &str,
        ) -> crate::session::error::SessionResult<Option<crate::worktree::WorkspaceRecord>>
        {
            self.inner.get_workspace_by_session(session_id).await
        }

        async fn list_workspaces(
            &self,
        ) -> crate::session::error::SessionResult<Vec<crate::worktree::WorkspaceRecord>> {
            self.inner.list_workspaces().await
        }

        async fn list_workspaces_by_repository(
            &self,
            common_dir: &std::path::Path,
        ) -> crate::session::error::SessionResult<Vec<crate::worktree::WorkspaceRecord>> {
            self.inner.list_workspaces_by_repository(common_dir).await
        }

        async fn list_workspaces_by_batch(
            &self,
            batch_id: &str,
        ) -> crate::session::error::SessionResult<Vec<crate::worktree::WorkspaceRecord>> {
            self.inner.list_workspaces_by_batch(batch_id).await
        }

        async fn update_workspace(
            &self,
            record: crate::worktree::WorkspaceRecord,
        ) -> crate::session::error::SessionResult<()> {
            self.inner.update_workspace(record).await
        }

        async fn transition_lifecycle(
            &self,
            id: &crate::worktree::WorkspaceId,
            from: crate::worktree::WorkspaceLifecycle,
            to: crate::worktree::WorkspaceLifecycle,
        ) -> crate::session::error::SessionResult<bool> {
            self.inner.transition_lifecycle(id, from, to).await
        }

        async fn attach_owner(
            &self,
            id: &crate::worktree::WorkspaceId,
            owner_session_id: &str,
        ) -> crate::session::error::SessionResult<crate::worktree::WorkspaceRecord> {
            let consumed = self
                .remaining
                .try_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if consumed {
                return Err(crate::session::error::SessionError::Other(
                    "injected owner-binding failure".to_string(),
                ));
            }
            self.inner.attach_owner(id, owner_session_id).await
        }

        async fn detach_owner(
            &self,
            id: &crate::worktree::WorkspaceId,
        ) -> crate::session::error::SessionResult<crate::worktree::WorkspaceRecord> {
            self.inner.detach_owner(id).await
        }

        async fn bind_baseline(
            &self,
            id: &crate::worktree::WorkspaceId,
            baseline: &crate::worktree::Baseline,
        ) -> crate::session::error::SessionResult<()> {
            self.inner.bind_baseline(id, baseline).await
        }

        async fn record_provisioning_phase(
            &self,
            id: &crate::worktree::WorkspaceId,
            phase: crate::worktree::ProvisioningPhase,
        ) -> crate::session::error::SessionResult<bool> {
            self.inner.record_provisioning_phase(id, phase).await
        }

        async fn record_result(
            &self,
            id: &crate::worktree::WorkspaceId,
            provenance: &crate::worktree::ResultProvenance,
        ) -> crate::session::error::SessionResult<()> {
            self.inner.record_result(id, provenance).await
        }

        async fn list_results(
            &self,
            id: &crate::worktree::WorkspaceId,
        ) -> crate::session::error::SessionResult<Vec<crate::worktree::ResultProvenance>> {
            self.inner.list_results(id).await
        }
    }

    async fn worktree_fixture(configure_service: bool) -> WorktreeFixture {
        worktree_fixture_inner(configure_service, 0).await
    }

    /// Fixture whose workspace service fails the first `owner_failures`
    /// owner-binding attempts, injecting a crash/failure after allocation
    /// but before the owner is published.
    async fn worktree_fixture_with_flaky_owner(owner_failures: usize) -> WorktreeFixture {
        worktree_fixture_inner(true, owner_failures).await
    }

    async fn worktree_fixture_inner(
        configure_service: bool,
        owner_failures: usize,
    ) -> WorktreeFixture {
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
        let registry: Arc<dyn crate::worktree::WorkspaceRepository> = if owner_failures > 0 {
            Arc::new(FlakyOwnerBinding {
                inner: storage
                    .workspace_repository()
                    .expect("sqlite storage exposes a workspace repository"),
                remaining: std::sync::atomic::AtomicUsize::new(owner_failures),
            })
        } else {
            storage
                .workspace_repository()
                .expect("sqlite storage exposes a workspace repository")
        };
        let service = Arc::new(WorkspaceService::new(
            Arc::new(GixRepoOps::new()),
            registry,
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

    /// Shared (non-isolated) delegation nests: P -> C -> D all run in P's
    /// managed checkout. D's direct parent C is not the workspace owner, but
    /// C is a shared descendant of the owner, so D may attach. A session
    /// with no parent chain to the owner is still rejected.
    #[tokio::test]
    async fn nested_shared_delegation_attaches_through_the_owner_chain() {
        let fx = worktree_fixture(true).await;
        let owner = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .unwrap();
        let cwd = owner.cwd.clone().unwrap();
        let parent_meta = |parent: &str| {
            let mut meta = serde_json::Map::new();
            meta.insert("parent_session_id".into(), parent.to_string().into());
            meta
        };

        let child = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(cwd.clone()).meta(parent_meta(&owner.session_id)),
            )
            .await
            .expect("direct shared child attaches");
        let grandchild = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(cwd.clone()).meta(parent_meta(&child.session_id)),
            )
            .await
            .expect("nested shared grandchild attaches through its parent chain");
        assert_eq!(grandchild.cwd.as_deref(), Some(cwd.as_path()));

        // A parent that is unrelated to the owner chain stays rejected.
        let stranger = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        assert!(
            fx.materializer
                .prepare_new_session(
                    NewSessionRequest::new(cwd).meta(parent_meta(&stranger.session_id)),
                )
                .await
                .is_err(),
            "a non-descendant parent cannot attach to the managed checkout"
        );
    }

    /// An isolated session whose MCP server is pinned to the *original*
    /// checkout would silently operate on the wrong files, so startup is
    /// rejected. The workspace was already allocated and bound, so it must
    /// not stay Ready and owned by a session that never started: it is
    /// released as Failed (resources retained) with a diagnostic.
    /// A shared child owns no workspace binding but executes inside the
    /// owner's managed checkout. Its MCP integrations must be validated
    /// against that checkout exactly like the owner's: a server pinned to
    /// the original project must be rejected, at any nesting depth.
    #[tokio::test]
    async fn shared_children_validate_mcp_roots_inside_managed_checkouts() {
        let fx = worktree_fixture(true).await;
        let owner = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .unwrap();
        let cwd = owner.cwd.clone().unwrap();
        let parent_meta = |parent: &str| {
            let mut meta = serde_json::Map::new();
            meta.insert("parent_session_id".into(), parent.to_string().into());
            meta
        };
        let pinned = || {
            McpServer::Stdio(
                crate::acp::protocol::McpServerStdio::new("pinned", "/bin/false")
                    .args(vec![fx.repo.path().display().to_string()]),
            )
        };

        // Direct shared child with a fixed-root integration: rejected.
        let err = match fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(cwd.clone())
                    .meta(parent_meta(&owner.session_id))
                    .mcp_servers(vec![pinned()]),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("a shared child must not start a root pinned to the source checkout"),
        };
        assert!(
            format!("{err:?}").contains("different checkout"),
            "expected a root-mismatch diagnostic, got: {err:?}"
        );

        // Nested shared descendant: same rule through the ancestry chain.
        let child = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(cwd.clone()).meta(parent_meta(&owner.session_id)),
            )
            .await
            .expect("an unpinned shared child attaches");
        let nested_err = match fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(cwd.clone())
                    .meta(parent_meta(&child.session_id))
                    .mcp_servers(vec![pinned()]),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("a nested shared child must be validated too"),
        };
        assert!(
            format!("{nested_err:?}").contains("different checkout"),
            "{nested_err:?}"
        );

        // The owner's workspace is untouched by a rejected shared child.
        let records = fx
            .storage
            .workspace_repository()
            .unwrap()
            .list_workspaces()
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].lifecycle,
            crate::worktree::WorkspaceLifecycle::Ready
        );
    }

    #[tokio::test]
    async fn mcp_root_mismatch_rejects_startup_and_releases_the_workspace() {
        let fx = worktree_fixture(true).await;
        let pinned = McpServer::Stdio(
            crate::acp::protocol::McpServerStdio::new("pinned", "/bin/false")
                .args(vec![fx.repo.path().display().to_string()]),
        );
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf())
            .meta(isolation_meta())
            .mcp_servers(vec![pinned]);
        let err = match fx.materializer.prepare_new_session(req).await {
            Err(err) => err,
            Ok(_) => panic!("a root pinned to another checkout must be rejected"),
        };
        assert!(
            format!("{err:?}").contains("different checkout"),
            "expected a root-mismatch diagnostic, got: {err:?}"
        );

        let records = fx
            .storage
            .workspace_repository()
            .unwrap()
            .list_workspaces()
            .await
            .unwrap();
        assert_eq!(records.len(), 1, "the allocated workspace is retained");
        let record = &records[0];
        assert_eq!(
            record.lifecycle,
            crate::worktree::WorkspaceLifecycle::Failed,
            "an unstarted workspace must not stay Ready"
        );
        assert!(
            record.ownership.owner_session_id.is_none(),
            "no dead session may keep owning the workspace"
        );
        assert!(
            record
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("session startup failed")),
            "diagnostic: {:?}",
            record.last_error
        );
        assert!(
            record.checkout_path.is_dir(),
            "the checkout is retained for explicit recovery or removal"
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

    /// The single effective workspace/cwd resolution path supplies an
    /// isolated fork's own files and cwd to every consumer: the durable
    /// binding wins over the fork row's copied source cwd (restart included),
    /// and nested delegation is provisioned from the fork's contents — never
    /// the source's.
    #[tokio::test]
    async fn fork_workspace_supplies_its_own_cwd_and_files_to_consumers() {
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
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        // Diverge the source checkout before forking: the fork's fresh
        // working-state baseline includes this edit.
        std::fs::write(source_checkout.join("README.md"), "edited after creation\n").unwrap();

        // Isolated fork through the real fork path.
        let response = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    source_checkout.clone(),
                )
                .meta(isolation_meta()),
            )
            .await
            .expect("isolated fork prepares");
        let fork_id = response.session_id.to_string();

        let registry = fx.storage.workspace_repository().unwrap();
        let fork_record = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork bound to its own workspace");
        let fork_checkout = fork_record.checkout_path.clone();
        assert_ne!(fork_checkout, source_checkout);

        // The fork row's cwd is persisted consistently with its durable
        // binding by the fork path itself — no manual session-row rewrite.
        let fork_row = fx
            .storage
            .get_session(&fork_id)
            .await
            .unwrap()
            .expect("fork row exists");
        assert_eq!(
            fork_row.cwd.as_deref(),
            Some(fork_checkout.as_path()),
            "fork row cwd agrees with its workspace binding"
        );

        // Consumer: the single resolution path. The durable binding wins
        // over the stale row cwd.
        let resolved = fx
            .service
            .resolve_session_cwd(&fork_id, fork_row.cwd.clone())
            .await
            .expect("resolution succeeds for a ready binding");
        assert_eq!(resolved.as_deref(), Some(fork_checkout.as_path()));

        // The fork's contents now differ from its source.
        std::fs::write(fork_checkout.join("README.md"), "fork-only edit\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(source_checkout.join("README.md")).unwrap(),
            "edited after creation\n"
        );

        // Consumer: restart (load). The binding resolves the fork's own
        // checkout with its own files, and a request echoing the stale row
        // cwd is rejected instead of attaching the source checkout.
        {
            let load = LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new());
            let loaded = match fx
                .materializer
                .prepare_load_session(load, None)
                .await
                .unwrap()
            {
                PreparedSessionResult::Prepared(p) => p,
                PreparedSessionResult::AlreadyRegistered(_) => {
                    panic!("fork was never registered")
                }
            };
            assert_eq!(loaded.cwd.as_deref(), Some(fork_checkout.as_path()));
            assert_eq!(
                std::fs::read_to_string(loaded.cwd.expect("bound cwd").join("README.md")).unwrap(),
                "fork-only edit\n",
                "the fork supplies its own files after restart"
            );
        }
        let conflicting =
            LoadSessionRequest::new(SessionId::from(fork_id.clone()), source_checkout.clone());
        let err = match fx
            .materializer
            .prepare_load_session(conflicting, None)
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("stale fork row cwd must not attach the source checkout"),
        };
        assert!(
            format!("{err:?}").contains("conflicting cwd"),
            "diagnostic: {err:?}"
        );

        // Consumer: nested delegation. The fork delegates with the resolved
        // cwd; the child is provisioned from the FORK's contents, links to
        // the fork's workspace, and never reuses a parent checkout.
        let fork_gate = crate::worktree::CheckoutMutationGate::new(
            fx.service.storage_root(),
            &fork_record.checkout_path,
        )
        .expect("fork gate");
        let child = fx
            .service
            .allocate_for_delegation(
                &fork_id,
                Some(&fork_record),
                resolved.as_deref().expect("resolved cwd"),
                Some(&fork_gate),
                "nested-delegation",
                None,
            )
            .await
            .expect("nested delegate allocates");
        assert_ne!(child.record.checkout_path, fork_record.checkout_path);
        assert!(
            !child.record.checkout_path.starts_with(&source_checkout),
            "nested delegation never provisions from the source checkout"
        );
        assert_eq!(
            child
                .record
                .ownership
                .parent_workspace_id
                .as_ref()
                .map(|id| id.as_str()),
            Some(fork_record.id.as_str()),
            "nested delegation links to the fork's workspace"
        );
        assert_eq!(
            std::fs::read_to_string(child.effective_cwd.join("README.md")).unwrap(),
            "fork-only edit\n",
            "nested delegation inherits the fork's contents, not the source's"
        );
    }

    /// The fork path persists the fork's execution cwd consistently with its
    /// durable workspace owner association: the session row, workspace
    /// binding, disclosed/list metadata, load, and resume all agree on the
    /// fork checkout — established by the real fork path, with no manual
    /// `sessions.cwd` rewrite in test setup.
    #[tokio::test]
    async fn fork_persists_execution_cwd_consistently_with_its_workspace_binding() {
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
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;
        std::fs::write(source_checkout.join("README.md"), "edited after creation\n").unwrap();

        let response = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    source_checkout.clone(),
                )
                .meta(isolation_meta()),
            )
            .await
            .expect("isolated fork prepares");
        let fork_id = response.session_id.to_string();

        let registry = fx.storage.workspace_repository().unwrap();
        let binding = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork bound to its own workspace");
        let fork_checkout = binding.checkout_path.clone();
        assert_ne!(fork_checkout, source_checkout);
        let disclosed = workspace_meta_of(&response)["sessionCwd"]
            .as_str()
            .expect("disclosed session cwd")
            .to_string();

        // Row, binding, and disclosed metadata agree — persisted by the
        // real fork path.
        let stored = fx
            .storage
            .get_session(&fork_id)
            .await
            .unwrap()
            .expect("fork row exists");
        assert_eq!(stored.cwd.as_deref(), Some(fork_checkout.as_path()));
        assert!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(binding.session_cwd(), fork_checkout);
        assert_eq!(std::path::PathBuf::from(disclosed), fork_checkout);
        let listed = fx.storage.list_sessions().await.unwrap();
        let listed_fork = listed
            .iter()
            .find(|session| session.public_id == fork_id)
            .expect("fork appears in the session listing");
        assert_eq!(listed_fork.cwd.as_deref(), Some(fork_checkout.as_path()));

        // Load agrees with the persisted row/binding (restart).
        {
            let load = LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new());
            let loaded = match fx
                .materializer
                .prepare_load_session(load, None)
                .await
                .unwrap()
            {
                PreparedSessionResult::Prepared(p) => p,
                PreparedSessionResult::AlreadyRegistered(_) => {
                    panic!("fork was never registered")
                }
            };
            assert_eq!(loaded.cwd.as_deref(), Some(fork_checkout.as_path()));
        }

        // Resume agrees with the persisted row/binding.
        {
            let resume =
                ResumeSessionRequest::new(SessionId::from(fork_id.clone()), fork_checkout.clone());
            let resumed = match fx
                .materializer
                .prepare_resume_session(resume, None)
                .await
                .unwrap()
            {
                PreparedSessionResult::Prepared(p) => p,
                PreparedSessionResult::AlreadyRegistered(_) => {
                    panic!("fork was never registered")
                }
            };
            assert_eq!(resumed.cwd.as_deref(), Some(fork_checkout.as_path()));
        }

        // Resuming in the copied-source cwd is rejected: the fork owns its
        // own checkout.
        let conflicting =
            ResumeSessionRequest::new(SessionId::from(fork_id.clone()), source_checkout);
        let err = match fx
            .materializer
            .prepare_resume_session(conflicting, None)
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("fork must not resume in the source checkout"),
        };
        assert!(
            format!("{err:?}").contains("conflicting cwd"),
            "diagnostic: {err:?}"
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
        let sessions_before = fx.storage.list_sessions().await.unwrap().len();

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
        // The request failed closed BEFORE the conversation copy: no fork
        // row was created and the source history is untouched.
        assert_eq!(
            fx.storage.list_sessions().await.unwrap().len(),
            sessions_before,
            "an unhonorable fork request must not create a session row"
        );
    }

    /// An invalid fork request fails closed before any conversation copy:
    /// no fork row, no copied messages, and the source session untouched.
    #[tokio::test]
    async fn invalid_fork_request_copies_no_conversation() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local source prepares");
        let source_id = source.session_id.clone();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1", "msg-2"]).await;
        let sessions_before = fx.storage.list_sessions().await.unwrap().len();
        let source_messages = fx.storage.get_history(&source_id).await.unwrap().len();

        // Invalid option combination: current-working-tree baseline with an
        // explicit base ref.
        let mut meta = isolation_meta();
        meta.insert(
            "querymt".to_string(),
            serde_json::json!({
                crate::worktree::SESSION_WORKSPACE_META_KEY: {
                    "execution": "worktree",
                    "baseline": "current_working_tree",
                    "baseRef": "main",
                },
            }),
        );
        let err = match fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    fx.repo.path().to_path_buf(),
                )
                .meta(meta),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("invalid fork options must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("base ref"),
            "diagnostic: {err:?}"
        );
        assert_eq!(
            fx.storage.list_sessions().await.unwrap().len(),
            sessions_before,
            "an invalid fork request must not create a session row"
        );
        assert_eq!(
            fx.storage.get_history(&source_id).await.unwrap().len(),
            source_messages,
            "an invalid fork request must not copy history"
        );
    }

    /// Forking a managed source with an explicit local request fails closed
    /// before the copy: there is no legacy checkout to fall back to.
    #[tokio::test]
    async fn local_fork_of_managed_source_fails_before_copy() {
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
        let sessions_before = fx.storage.list_sessions().await.unwrap().len();

        let mut meta = isolation_meta();
        meta.insert(
            "querymt".to_string(),
            serde_json::json!({
                crate::worktree::SESSION_WORKSPACE_META_KEY: { "execution": "local" },
            }),
        );
        let err = match fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(
                    SessionId::from(source_id.clone()),
                    fx.repo.path().to_path_buf(),
                )
                .meta(meta),
            )
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("a local fork of a managed source must fail closed"),
        };
        assert!(
            format!("{err:?}").contains("requires an independent worktree"),
            "diagnostic: {err:?}"
        );
        assert_eq!(
            fx.storage.list_sessions().await.unwrap().len(),
            sessions_before,
            "the rejected fork must not create a session row"
        );
    }

    /// A copied isolated fork remains blocked even before a workspace record exists.
    #[tokio::test]
    async fn pending_fork_provisioning_is_not_runnable_as_local() {
        let fx = worktree_fixture(false).await;
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        let source_id = source.session_id.clone();
        let source_cwd = source.cwd.clone().unwrap();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        let intent = crate::session::store::PendingForkIsolation {
            source_session_id: source_id.clone(),
            source_cwd: source_cwd.clone(),
            options: crate::worktree::SessionCreationOptions::isolated_worktree(
                crate::worktree::BaselineMode::CurrentWorkingTree,
            )
            .to_workspace_options(),
        };
        // Stop at the actual copy transaction boundary, before any workspace
        // exists. Both history and intent must already be durable here.
        let fork_id = fx
            .storage
            .fork_session_with_isolation_intent(
                &source_id,
                "msg-1",
                crate::session::domain::ForkOrigin::User,
                Some(intent.clone()),
            )
            .await
            .unwrap();
        assert_eq!(fx.storage.get_history(&fork_id).await.unwrap().len(), 1);
        assert_eq!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap(),
            Some(intent)
        );
        assert!(
            fx.storage
                .workspace_repository()
                .unwrap()
                .get_workspace_by_session(&fork_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fx.storage.complete_fork_isolation(&fork_id).await.is_err(),
            "an unbound fork cannot release its startup barrier"
        );

        // A fresh materializer without an isolation service must still reject
        // the pending fork rather than treating it as a legacy local session.
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        let load = LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new());
        let err = match restarted.prepare_load_session(load, None).await {
            Err(err) => err,
            Ok(_) => panic!("a pending fork must not be loadable"),
        };
        assert!(
            format!("{err:?}").contains("fork isolation is pending"),
            "{err:?}"
        );
        let resume = ResumeSessionRequest::new(SessionId::from(fork_id.clone()), source_cwd);
        assert!(
            restarted
                .prepare_resume_session(resume, None)
                .await
                .is_err()
        );
        let listed = fx.storage.list_sessions().await.unwrap();
        assert!(listed.iter().any(|session| session.public_id == fork_id));
        assert!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn failed_fork_allocation_retains_pending_intent() {
        let fx = worktree_fixture(true).await;
        // Git cannot materialize an empty directory in the child checkout,
        // so allocation fails after the conversation copy has committed.
        let cwd = fx.repo.path().join("empty-session-cwd");
        std::fs::create_dir(&cwd).unwrap();
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(cwd.clone()))
            .await
            .unwrap();
        let source_id = source.session_id.clone();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;
        assert!(
            fx.materializer
                .prepare_fork_session(
                    ForkSessionRequest::new(SessionId::from(source_id.clone()), cwd)
                        .meta(isolation_meta()),
                )
                .await
                .is_err()
        );
        let sessions = fx.storage.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        let fork = sessions.iter().find(|s| s.public_id != source_id).unwrap();
        assert_eq!(
            fx.storage.get_history(&fork.public_id).await.unwrap().len(),
            1
        );
        assert!(
            fx.storage
                .get_pending_fork_isolation(&fork.public_id)
                .await
                .unwrap()
                .is_some()
        );
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        assert!(
            restarted
                .prepare_load_session(
                    LoadSessionRequest::new(
                        SessionId::from(fork.public_id.clone()),
                        PathBuf::new()
                    ),
                    None,
                )
                .await
                .is_err()
        );
    }

    /// Injected owner-binding failure after a successful allocation: the
    /// fork compensates the unused workspace to `Failed`, the pending intent
    /// survives, load/resume (including on a fresh materializer) refuse to
    /// run the fork anywhere — least of all the source checkout — and the
    /// listing exposes the pending-isolation state (11.4/11.5).
    #[tokio::test]
    async fn failed_isolated_new_session_cannot_resume_in_the_source_checkout() {
        let fx = worktree_fixture_with_flaky_owner(1).await;
        let source_cwd = fx.repo.path().to_path_buf();

        // The owner binding fails after the session row was persisted.
        let err = match fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(source_cwd.clone()).meta(isolation_meta()))
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("injected owner-binding failure must fail creation"),
        };
        assert!(
            format!("{err:?}").contains("injected owner-binding failure"),
            "{err:?}"
        );

        // The leftover row carries the durable non-runnable barrier.
        let sessions = fx.storage.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1, "the leftover session row exists");
        let session_id = sessions[0].public_id.clone();
        assert!(
            fx.storage
                .get_pending_fork_isolation(&session_id)
                .await
                .unwrap()
                .is_some(),
            "isolation intent must be persisted with the session row"
        );

        // Neither a resume supplying the original unmanaged source cwd nor a
        // load can run it: not in the source checkout, not anywhere.
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        let resume =
            ResumeSessionRequest::new(SessionId::from(session_id.clone()), source_cwd.clone());
        let resume_err = match restarted.prepare_resume_session(resume, None).await {
            Err(err) => err,
            Ok(_) => panic!("a failed isolated session must not resume in the source checkout"),
        };
        assert!(
            format!("{resume_err:?}").contains("fork isolation is pending"),
            "{resume_err:?}"
        );
        let load = LoadSessionRequest::new(SessionId::from(session_id), source_cwd);
        assert!(restarted.prepare_load_session(load, None).await.is_err());
    }

    #[tokio::test]
    async fn successful_isolated_new_session_releases_its_barrier() {
        let fx = worktree_fixture(true).await;
        let prepared = fx
            .materializer
            .prepare_new_session(
                NewSessionRequest::new(fx.repo.path().to_path_buf()).meta(isolation_meta()),
            )
            .await
            .expect("isolated session prepares");
        assert!(
            fx.storage
                .get_pending_fork_isolation(&prepared.session_id)
                .await
                .unwrap()
                .is_none(),
            "a started isolated session is runnable: no pending barrier"
        );
    }

    #[tokio::test]
    async fn injected_owner_binding_failure_compensates_and_stays_non_runnable() {
        let fx = worktree_fixture_with_flaky_owner(1).await;
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        let source_id = source.session_id.clone();
        let source_cwd = source.cwd.clone().unwrap();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        // The owner binding fails after the workspace was fully provisioned.
        let err = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(SessionId::from(source_id.clone()), source_cwd.clone())
                    .meta(isolation_meta()),
            )
            .await
            .expect_err("injected owner-binding failure must fail the fork");
        assert!(
            format!("{err:?}").contains("injected owner-binding failure"),
            "{err:?}"
        );

        // The fork row and its history exist and the intent is retained.
        let sessions = fx.storage.list_sessions().await.unwrap();
        let fork = sessions.iter().find(|s| s.public_id != source_id).unwrap();
        let fork_id = fork.public_id.clone();
        assert_eq!(fx.storage.get_history(&fork_id).await.unwrap().len(), 1);
        assert!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap()
                .is_some()
        );

        // Compensation: the provisioned-but-unused workspace is marked
        // Failed with a diagnostic and never owned.
        let registry = fx.storage.workspace_repository().unwrap();
        let records = registry.list_workspaces().await.unwrap();
        assert_eq!(records.len(), 1, "exactly the compensated workspace exists");
        let record = &records[0];
        assert_eq!(
            record.lifecycle,
            crate::worktree::WorkspaceLifecycle::Failed
        );
        assert!(record.ownership.owner_session_id.is_none());
        let diagnostic = record.last_error.clone().expect("compensation diagnostic");
        assert!(
            diagnostic.contains("fork owner binding failed"),
            "unexpected diagnostic: {diagnostic}"
        );

        // A fresh materializer must not run the pending fork: neither in its
        // own (failed) workspace nor in the source checkout.
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        let load = LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new());
        let load_err = match restarted.prepare_load_session(load, None).await {
            Err(err) => err,
            Ok(_) => panic!("a pending fork must not be loadable"),
        };
        assert!(
            format!("{load_err:?}").contains("fork isolation is pending"),
            "{load_err:?}"
        );
        let resume = ResumeSessionRequest::new(SessionId::from(fork_id.clone()), source_cwd);
        assert!(
            restarted
                .prepare_resume_session(resume, None)
                .await
                .is_err()
        );

        // The listing exposes the outstanding isolation state so consumers
        // never mistake the fork for a legacy local session.
        use crate::session::ViewStore as _;
        let view = fx.storage.get_session_list_view(None).await.unwrap();
        let item = view
            .groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .find(|item| item.session_id == fork_id)
            .expect("fork appears in the listing");
        assert!(
            item.pending_isolation,
            "fork listing must expose pending isolation"
        );
    }

    /// Simulated crash after owner binding but before the execution cwd and
    /// barrier release: the fork stays non-runnable, and explicit recovery
    /// reuses the existing binding — no duplicate owners — then publishes
    /// the fork inside its own checkout (11.4).
    #[tokio::test]
    async fn crash_after_owner_binding_is_recovered_without_duplicate_owners() {
        let fx = worktree_fixture(true).await;
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        let source_id = source.session_id.clone();
        let source_cwd = source.cwd.clone().unwrap();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        let intent = crate::session::store::PendingForkIsolation {
            source_session_id: source_id.clone(),
            source_cwd: source_cwd.clone(),
            options: crate::worktree::SessionCreationOptions::isolated_worktree(
                crate::worktree::BaselineMode::CurrentWorkingTree,
            )
            .to_workspace_options(),
        };
        let fork_id = fx
            .storage
            .fork_session_with_isolation_intent(
                &source_id,
                "msg-1",
                crate::session::domain::ForkOrigin::User,
                Some(intent.clone()),
            )
            .await
            .unwrap();
        // "Crash" here: allocation + binding succeeded, cwd/barrier did not.
        let service = fx.service.clone();
        let binding = service
            .allocate_for_fork(
                &source_cwd,
                &crate::worktree::SessionCreationOptions::isolated_worktree(
                    crate::worktree::BaselineMode::CurrentWorkingTree,
                ),
                None,
            )
            .await
            .unwrap();
        let bound_cwd = binding.effective_cwd.clone();
        let workspace_id = binding.record.id.as_str().to_string();
        service
            .bind_owner(&binding.record.id, &fork_id)
            .await
            .unwrap();
        assert_ne!(bound_cwd, source_cwd);

        // Not runnable before recovery: load/resume refuse (pending intent).
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        assert!(
            restarted
                .prepare_load_session(
                    LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new()),
                    None,
                )
                .await
                .is_err()
        );
        assert!(
            restarted
                .prepare_resume_session(ResumeSessionRequest::new(
                    SessionId::from(fork_id.clone()),
                    source_cwd.clone(),
                ), None)
                .await
                .is_err()
        );

        // Explicit recovery reuses the existing binding (no second workspace
        // is ever owned) and releases the barrier.
        let response = fx
            .materializer
            .recover_pending_fork(&fork_id)
            .await
            .expect("pending fork recovers");
        let meta = response
            .meta
            .as_ref()
            .and_then(|meta| meta.get("querymt"))
            .and_then(|querymt| querymt.get(crate::worktree::SESSION_WORKSPACE_META_KEY))
            .cloned()
            .expect("recovery discloses the workspace");
        assert_eq!(
            meta.get("workspaceId").and_then(|value| value.as_str()),
            Some(workspace_id.as_str()),
            "recovery must reuse the bound workspace, not allocate another"
        );
        assert_eq!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap(),
            None
        );

        // Exactly one workspace is owned by the fork, at the bound cwd.
        let registry = fx.storage.workspace_repository().unwrap();
        let owned = registry.get_workspace_by_session(&fork_id).await.unwrap();
        assert_eq!(
            owned.expect("fork owns a workspace").id.as_str(),
            workspace_id
        );
        let owned_count = registry
            .list_workspaces()
            .await
            .unwrap()
            .into_iter()
            .filter(|record| {
                record
                    .ownership
                    .owner_session_id
                    .as_deref()
                    .is_some_and(|owner| owner == fork_id)
            })
            .count();
        assert_eq!(owned_count, 1, "no duplicate owners across retries");

        // The barrier is gone and the fork loads inside its own checkout —
        // a retry of recovery is refused (nothing pending) and the loaded
        // session resolves the bound execution cwd.
        assert!(
            fx.materializer
                .recover_pending_fork(&fork_id)
                .await
                .is_err()
        );
        let reloaded = match restarted
            .prepare_load_session(
                LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new()),
                None,
            )
            .await
        {
            Ok(crate::agent::session_materializer::PreparedSessionResult::Prepared(prepared)) => {
                prepared
            }
            Ok(_) => panic!("unexpected non-prepared load result"),
            Err(err) => panic!("recovered fork loads on a fresh materializer: {err:?}"),
        };
        assert_eq!(reloaded.cwd.as_deref(), Some(bound_cwd.as_path()));
    }

    /// Full retry path after a compensated failure: the failed attempt's
    /// workspace stays retained and unowned, recovery allocates a fresh
    /// workspace for the same fork session (single owner), and the source
    /// checkout is never used (11.5).
    #[tokio::test]
    async fn failed_fork_creation_recovers_through_the_retry_path() {
        let fx = worktree_fixture_with_flaky_owner(1).await;
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        let source_id = source.session_id.clone();
        let source_cwd = source.cwd.clone().unwrap();
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        // First attempt fails at the owner binding and is compensated.
        assert!(
            fx.materializer
                .prepare_fork_session(
                    ForkSessionRequest::new(SessionId::from(source_id.clone()), source_cwd.clone())
                        .meta(isolation_meta()),
                )
                .await
                .is_err()
        );
        let sessions = fx.storage.list_sessions().await.unwrap();
        let fork = sessions.iter().find(|s| s.public_id != source_id).unwrap();
        let fork_id = fork.public_id.clone();

        // Retry through the explicit recovery surface.
        let response = fx
            .materializer
            .recover_pending_fork(&fork_id)
            .await
            .expect("retry recovers the pending fork");

        // The fork owns exactly one ready workspace — a fresh one; the
        // failed attempt's workspace is retained but never owned.
        let registry = fx.storage.workspace_repository().unwrap();
        let records = registry.list_workspaces().await.unwrap();
        let owned: Vec<_> = records
            .iter()
            .filter(|record| {
                record
                    .ownership
                    .owner_session_id
                    .as_deref()
                    .is_some_and(|owner| owner == fork_id)
            })
            .collect();
        assert_eq!(owned.len(), 1, "retries create no duplicate owners");
        assert_eq!(
            owned[0].lifecycle,
            crate::worktree::WorkspaceLifecycle::Ready
        );
        assert_eq!(
            records.len(),
            2,
            "the failed attempt's workspace stays as an explicit failed record"
        );
        assert!(
            records.iter().any(|record| record.lifecycle
                == crate::worktree::WorkspaceLifecycle::Failed
                && record.ownership.owner_session_id.is_none()),
            "the compensated record remains retained and unowned"
        );

        // The disclosure names the new workspace and the fork is runnable in
        // its own checkout afterwards (fresh materializer, restart-safe).
        let meta = response
            .meta
            .as_ref()
            .and_then(|meta| meta.get("querymt"))
            .and_then(|querymt| querymt.get(crate::worktree::SESSION_WORKSPACE_META_KEY))
            .cloned()
            .expect("recovery discloses the workspace");
        assert_eq!(
            meta.get("workspaceId").and_then(|value| value.as_str()),
            Some(owned[0].id.as_str())
        );
        assert_eq!(
            fx.storage
                .get_session(&fork_id)
                .await
                .unwrap()
                .expect("fork row exists")
                .cwd
                .as_deref(),
            Some(owned[0].session_cwd().as_path()),
            "the session row cwd is the fork's own checkout"
        );
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        let reloaded = match restarted
            .prepare_load_session(
                LoadSessionRequest::new(SessionId::from(fork_id.clone()), PathBuf::new()),
                None,
            )
            .await
        {
            Ok(crate::agent::session_materializer::PreparedSessionResult::Prepared(prepared)) => {
                prepared
            }
            Ok(_) => panic!("unexpected non-prepared load result"),
            Err(err) => panic!("recovered fork loads after restart: {err:?}"),
        };
        assert_eq!(
            reloaded.cwd.as_deref(),
            Some(owned[0].session_cwd().as_path()),
            "the fork resolves its own checkout, never the source's"
        );
    }

    /// Record a pre-turn snapshot (TurnSnapshotStart) in `session_id`'s
    /// history exactly as the runtime would, scoped to an explicit workspace.
    async fn record_pre_snapshot(
        storage: &SqliteStorage,
        backend: &crate::snapshot::GitSnapshotBackend,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> (String, String) {
        use crate::model::MessagePart;
        use crate::snapshot::SnapshotBackend as _;
        let snapshot_id = backend.track(cwd).await.expect("track pre snapshot");
        let turn_id = uuid::Uuid::new_v4().to_string();
        let msg = crate::model::AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: querymt::chat::ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotStart {
                turn_id: turn_id.clone(),
                snapshot_id: snapshot_id.clone(),
                workspace_root: Some(cwd.to_path_buf()),
                inherited: false,
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        storage
            .session_store()
            .add_message(session_id, msg)
            .await
            .expect("record pre snapshot");
        (turn_id, snapshot_id)
    }

    /// Record a post-turn snapshot (TurnSnapshotPatch) and return the changed
    /// paths, scoped to an explicit workspace.
    async fn record_post_snapshot(
        storage: &SqliteStorage,
        backend: &crate::snapshot::GitSnapshotBackend,
        session_id: &str,
        turn_id: &str,
        pre_snapshot: &str,
        cwd: &std::path::Path,
    ) -> Vec<String> {
        use crate::model::MessagePart;
        use crate::snapshot::SnapshotBackend as _;
        let post_snapshot = backend.track(cwd).await.expect("track post snapshot");
        let changed = backend
            .diff(cwd, &pre_snapshot.to_string(), &post_snapshot)
            .await
            .expect("diff post snapshot");
        let changed_strs: Vec<String> = changed
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let msg = crate::model::AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: querymt::chat::ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotPatch {
                turn_id: turn_id.to_string(),
                snapshot_id: post_snapshot,
                changed_paths: changed_strs.clone(),
                workspace_root: Some(cwd.to_path_buf()),
                inherited: false,
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        storage
            .session_store()
            .add_message(session_id, msg)
            .await
            .expect("record post snapshot");
        changed_strs
    }

    /// Parent/fork undo regression through the REAL fork creation path
    /// (11.6): parent undo excludes the isolated fork's snapshots despite
    /// source ancestry (the fork is a child session) while still aggregating
    /// compatible shared-checkout children; fork-local undo/redo works after
    /// a restart (fresh materializer over the same store plus a fresh
    /// backend over the same snapshot base).
    #[tokio::test]
    async fn parent_undo_excludes_isolated_fork_and_fork_undo_redo_works_after_restart() {
        let fx = worktree_fixture(true).await;
        let snapshot_base = tempfile::TempDir::new().unwrap();
        let backend = crate::snapshot::GitSnapshotBackend::with_snapshot_base(
            snapshot_base.path().to_path_buf(),
        );

        // Parent: a plain local session in the real repository.
        let source = fx
            .materializer
            .prepare_new_session(NewSessionRequest::new(fx.repo.path().to_path_buf()))
            .await
            .unwrap();
        let parent_id = source.session_id.clone();
        let parent_cwd = source.cwd.clone().unwrap();
        drop(source);
        seed_history(fx.storage.as_ref(), &parent_id, &["msg-1"]).await;

        // Real isolated fork through prepare_fork_session.
        let response = fx
            .materializer
            .prepare_fork_session(
                ForkSessionRequest::new(SessionId::from(parent_id.clone()), parent_cwd.clone())
                    .meta(isolation_meta()),
            )
            .await
            .unwrap();
        let fork_id = response.session_id.to_string();
        let registry = fx.storage.workspace_repository().unwrap();
        let fork_record = registry
            .get_workspace_by_session(&fork_id)
            .await
            .unwrap()
            .expect("fork bound to its own workspace");
        let fork_cwd = fork_record.session_cwd();
        assert_ne!(fork_cwd, parent_cwd);

        // A legacy shared child still bound to the parent's checkout.
        let shared_child = fx
            .storage
            .session_store()
            .create_session(
                None,
                Some(parent_cwd.clone()),
                Some(parent_id.clone()),
                Some(crate::session::domain::ForkOrigin::Delegation),
            )
            .await
            .unwrap()
            .public_id;

        // Parent turn (its own checkout).
        let (parent_turn, parent_pre) =
            record_pre_snapshot(fx.storage.as_ref(), &backend, &parent_id, &parent_cwd).await;
        std::fs::write(parent_cwd.join("README.md"), "parent-edit\n").unwrap();
        record_post_snapshot(
            fx.storage.as_ref(),
            &backend,
            &parent_id,
            &parent_turn,
            &parent_pre,
            &parent_cwd,
        )
        .await;

        // Shared child turn (same checkout).
        let (shared_turn, shared_pre) =
            record_pre_snapshot(fx.storage.as_ref(), &backend, &shared_child, &parent_cwd).await;
        std::fs::write(parent_cwd.join("README.md"), "shared-edit\n").unwrap();
        record_post_snapshot(
            fx.storage.as_ref(),
            &backend,
            &shared_child,
            &shared_turn,
            &shared_pre,
            &parent_cwd,
        )
        .await;

        // Fork turn (its OWN checkout).
        let (fork_turn, fork_pre) =
            record_pre_snapshot(fx.storage.as_ref(), &backend, &fork_id, &fork_cwd).await;
        std::fs::write(fork_cwd.join("README.md"), "fork-edit\n").unwrap();
        record_post_snapshot(
            fx.storage.as_ref(),
            &backend,
            &fork_id,
            &fork_turn,
            &fork_pre,
            &fork_cwd,
        )
        .await;

        assert_eq!(
            std::fs::read_to_string(parent_cwd.join("README.md")).unwrap(),
            "shared-edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(fork_cwd.join("README.md")).unwrap(),
            "fork-edit\n"
        );

        // Parent undo: succeeds despite the isolated fork existing and
        // aggregates only the parent's and the shared child's patches.
        let result = crate::agent::undo::undo_impl(
            &backend,
            &fx.materializer.config.provider,
            &parent_id,
            "msg-1",
            &parent_cwd,
        )
        .await
        .expect("parent undo succeeds with an isolated fork present");
        assert!(
            !result.reverted_files.is_empty(),
            "parent undo reverts the shared-checkout edits"
        );
        assert_eq!(
            std::fs::read_to_string(parent_cwd.join("README.md")).unwrap(),
            "hello\n",
            "parent undo reverts the shared-checkout edits to the original"
        );
        assert_eq!(
            std::fs::read_to_string(fork_cwd.join("README.md")).unwrap(),
            "fork-edit\n",
            "parent undo must not touch the isolated fork's workspace"
        );

        // Restart: a fresh materializer over the same store and a fresh
        // backend over the same snapshot base. Fork-local undo rewinds the
        // fork's OWN workspace to its baseline contents; redo reapplies it.
        let restarted = SessionMaterializer::new(fx.materializer.config.clone());
        let restarted_backend = crate::snapshot::GitSnapshotBackend::with_snapshot_base(
            snapshot_base.path().to_path_buf(),
        );
        // The fork's copied messages carry new public ids; target the
        // copied user message (first entry of the fork's history).
        let fork_target = fx
            .storage
            .get_history(&fork_id)
            .await
            .unwrap()
            .first()
            .expect("fork history has the copied message")
            .id
            .clone();
        let fork_undo = crate::agent::undo::undo_impl(
            &restarted_backend,
            &restarted.config.provider,
            &fork_id,
            &fork_target,
            &fork_cwd,
        )
        .await
        .expect("fork-local undo works after restart");
        assert!(!fork_undo.reverted_files.is_empty());
        assert_eq!(
            std::fs::read_to_string(fork_cwd.join("README.md")).unwrap(),
            "hello\n",
            "fork-local undo rewinds the fork's own workspace"
        );
        assert_eq!(
            std::fs::read_to_string(parent_cwd.join("README.md")).unwrap(),
            "hello\n",
            "fork-local undo does not touch the parent"
        );

        crate::agent::undo::redo_impl(
            &restarted_backend,
            &restarted.config.provider,
            &fork_id,
            &fork_cwd,
        )
        .await
        .expect("fork-local redo works after restart");
        assert_eq!(
            std::fs::read_to_string(fork_cwd.join("README.md")).unwrap(),
            "fork-edit\n",
            "fork-local redo reapplies the fork's own edit"
        );
    }

    /// A local fork of an unmanaged source retains its cwd and stays unmanaged.
    #[tokio::test]
    async fn legacy_local_fork_stays_local() {
        let fx = worktree_fixture(true).await;
        let req = NewSessionRequest::new(fx.repo.path().to_path_buf());
        let source = fx
            .materializer
            .prepare_new_session(req)
            .await
            .expect("local source prepares");
        let source_id = source.session_id.clone();
        let source_cwd = source.cwd.clone().expect("local source cwd");
        drop(source);
        seed_history(fx.storage.as_ref(), &source_id, &["msg-1"]).await;

        let response = fx
            .materializer
            .prepare_fork_session(ForkSessionRequest::new(
                SessionId::from(source_id.clone()),
                source_cwd.clone(),
            ))
            .await
            .expect("local fork prepares");
        let fork_id = response.session_id.to_string();

        let registry = fx.storage.workspace_repository().unwrap();
        assert!(
            registry
                .get_workspace_by_session(&fork_id)
                .await
                .unwrap()
                .is_none(),
            "a local fork creates no workspace record"
        );
        let fork_row = fx
            .storage
            .get_session(&fork_id)
            .await
            .unwrap()
            .expect("fork row exists");
        assert_eq!(
            fork_row.cwd.as_deref(),
            Some(source_cwd.as_path()),
            "a local fork keeps the copied row cwd"
        );

        // Loading keeps the original checkout without claiming managed isolation.
        let load = LoadSessionRequest::new(SessionId::from(fork_id.clone()), source_cwd.clone());
        let loaded = match fx
            .materializer
            .prepare_load_session(load, None)
            .await
            .unwrap()
        {
            PreparedSessionResult::Prepared(p) => p,
            PreparedSessionResult::AlreadyRegistered(_) => panic!("fork was never registered"),
        };
        assert_eq!(loaded.cwd.as_deref(), Some(source_cwd.as_path()));
        assert!(
            !loaded
                .runtime
                .managed_checkout
                .get()
                .copied()
                .unwrap_or(false)
        );
        assert!(
            fx.storage
                .get_pending_fork_isolation(&fork_id)
                .await
                .unwrap()
                .is_none()
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
