//! Remote node manager — handles session lifecycle requests from remote peers.
//!
//! `RemoteNodeManager` is a kameo actor that runs on every node in the mesh.
//! Remote peers send it messages to create, list, or destroy sessions on this
//! node. The local dashboard sends these messages to nodes it has discovered.
//!
//! The actor and its messages are only available with the `remote` feature.
//! The `RemoteSessionInfo` and `NodeInfo` data types are always available
//! (needed for UI serialization regardless of feature).

use crate::agent::remote::NodeId;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

/// Metadata about a session available on a remote node.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionInfo {
    /// Session public ID (same format as local sessions)
    pub session_id: String,
    /// kameo ActorId of the SessionActor on the remote node (raw u64)
    #[typeshare(serialized_as = "number")]
    pub actor_id: u64,
    /// Working directory on the remote machine (if set)
    pub cwd: Option<String>,
    /// Unix timestamp when the session was created
    #[typeshare(serialized_as = "number")]
    pub created_at: i64,
    /// Session title/name, if set
    pub title: Option<String>,
    /// Human-readable label of the peer that owns this session
    pub peer_label: String,
    /// High-level runtime lifecycle state for UI summaries.
    pub runtime_state: Option<String>,
}

/// Paginated response for listing sessions on a remote node.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRemoteSessionsResponse {
    /// Sessions in the requested page.
    pub sessions: Vec<RemoteSessionInfo>,
    /// Offset for the next page; `None` when there are no more pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    /// Total sessions across all pages.
    #[typeshare(serialized_as = "number")]
    pub total_count: u32,
}

/// Metadata about a remote node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Stable mesh node identifier (PeerId-backed).
    pub node_id: NodeId,
    /// Human-readable hostname
    pub hostname: String,
    /// Node capabilities (e.g., "shell", "filesystem", "gpu")
    pub capabilities: Vec<String>,
    /// Number of active sessions
    pub active_sessions: usize,
}

/// A single available model advertised by a mesh node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableModel {
    pub provider: String,
    pub model: String,
}

// ── Remote-only actor and messages ────────────────────────────────────────────

#[cfg(feature = "remote")]
pub use remote_impl::{
    AdmissionRequest, AdmissionResponse, CreateRemoteSession, CreateRemoteSessionResponse,
    ForkRemoteSession, ForkRemoteSessionResponse, GetNodeInfo, ListAvailableModels,
    ListRemoteSessions, RemoteNodeManager, RemoteNodeManagerState, ResumeRemoteSession,
    SessionHandoff, StopRemoteSessionRuntime,
};

#[cfg(feature = "remote")]
mod remote_impl {
    use super::{AvailableModel, ListRemoteSessionsResponse, NodeInfo, RemoteSessionInfo};
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::remote::NodeId;
    use crate::agent::remote::mesh::MeshHandle;
    use crate::agent::session_actor::SessionActor;
    use crate::agent::session_registry::SessionRegistry;
    use crate::error::AgentError;
    use futures_util::FutureExt;
    use kameo::Actor;
    use kameo::message::{Context, Message};
    use kameo::remote::_internal;
    use serde::{Deserialize, Serialize};

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── Messages ──────────────────────────────────────────────────────────────

    /// Create a new session on this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CreateRemoteSession {
        /// Working directory on the remote machine (optional; string for cross-OS)
        pub cwd: Option<String>,
    }

    /// Wire-safe handoff returned from remote create/fork lifecycle RPCs.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum SessionHandoff {
        /// Immediate direct capability for the newly materialized session.
        DirectRemote {
            session_ref: kameo::actor::RemoteActorRef<SessionActor>,
        },
        /// No direct capability was available, but the caller can attach via lookup.
        LookupOnly,
        /// The session was created, but this environment cannot provide any remote attach path.
        NoAttachPath,
    }

    impl SessionHandoff {
        pub fn direct(session_ref: kameo::actor::RemoteActorRef<SessionActor>) -> Self {
            Self::DirectRemote { session_ref }
        }

        pub fn lookup_only() -> Self {
            Self::LookupOnly
        }

        pub fn no_attach_path() -> Self {
            Self::NoAttachPath
        }
    }

    /// Response from `CreateRemoteSession`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CreateRemoteSessionResponse {
        /// The new session's public ID.
        pub session_id: String,
        /// Wire-safe handoff for immediate attach or lookup fallback.
        pub handoff: SessionHandoff,
        /// Working directory on the remote machine, if known.
        pub cwd: Option<String>,
        /// Session title/name, if known.
        pub title: Option<String>,
        /// Unix timestamp when the session was created.
        pub created_at: i64,
    }

    /// Fork an existing session on this node at a specific message boundary.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ForkRemoteSession {
        pub source_session_id: String,
        pub message_id: String,
    }

    /// Response from `ForkRemoteSession`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ForkRemoteSessionResponse {
        pub session_id: String,
        pub handoff: SessionHandoff,
        pub cwd: Option<String>,
        pub title: Option<String>,
        pub created_at: i64,
    }

    /// List sessions active on this node, with pagination.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListRemoteSessions {
        /// Number of sessions to skip (default 0).
        #[serde(default)]
        pub offset: Option<u32>,
        /// Maximum sessions to return (default 20, clamped to 1..100).
        #[serde(default)]
        pub limit: Option<u32>,
    }

    /// List all provider/model pairs this node can serve (has valid credentials for).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListAvailableModels;

    /// Shut down the live runtime actor for a session on this node.
    ///
    /// This removes the session actor from the in-memory registry and
    /// deregisters it from the mesh DHT, but **does not** delete the
    /// persisted session history from SQLite. The session can later be
    /// re-materialized via `ResumeRemoteSession`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StopRemoteSessionRuntime {
        pub session_id: String,
    }

    /// Re-materialize a persisted session on this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ResumeRemoteSession {
        pub session_id: String,
    }

    /// Get metadata about this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct GetNodeInfo;

    /// Admission request sent by a joining peer immediately after connection.
    ///
    /// Two variants:
    /// - `Invite` — first join: presents the `invite_id` from the signed grant.
    ///   The host calls `InviteStore::admit_peer`, consumes one use, and returns
    ///   a signed `MembershipToken`.
    /// - `Token` — reconnect: presents a previously-issued `MembershipToken`.
    ///   Any mesh node can verify this — no invite store access required.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum AdmissionRequest {
        /// First join via an invite grant.
        Invite {
            /// The `invite_id` field from the `SignedInviteGrant`.
            invite_id: String,
            /// The joiner's own PeerId (self-declared; baked into the token on success).
            peer_id: String,
        },
        /// Reconnect using a previously issued membership token.
        Token {
            membership_token: crate::agent::remote::invite::MembershipToken,
            /// The reconnecting peer's PeerId — must match `membership_token.peer_id`.
            peer_id: String,
        },
    }

    /// Response to an `AdmissionRequest`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum AdmissionResponse {
        /// First join succeeded — here is the membership token for future reconnects.
        Admitted {
            membership_token: crate::agent::remote::invite::MembershipToken,
            /// `PeerId` strings of all currently connected mesh peers (excluding
            /// the joiner).  The joiner should dial each of these to form a full
            /// mesh instead of staying in a star topology with only the inviter.
            existing_peers: Vec<String>,
        },
        /// Reconnect accepted — token is still valid.
        Readmitted {
            /// `PeerId` strings of all currently connected mesh peers.
            existing_peers: Vec<String>,
        },
        /// Admission denied.
        Rejected { reason: String },
    }

    // ── Shared State ──────────────────────────────────────────────────────────

    /// Shared mutable state for `RemoteNodeManager` that can be safely accessed
    /// from delegated (spawned) tasks.
    ///
    /// This state is separated from the actor itself to enable the DelegatedReply
    /// Type alias for session metadata storage.
    /// Maps session_id to (created_at_timestamp, optional_cwd).
    pub type SessionMetaMap = Arc<Mutex<HashMap<String, (i64, Option<String>)>>>;

    /// pattern: heavyweight operations (create/resume/fork) spawn background tasks
    /// that capture cheap clones of this shared state, keeping the actor mailbox
    /// responsive for lightweight control-plane messages (GetNodeInfo, ListAvailableModels).
    #[derive(Clone)]
    pub struct RemoteNodeManagerState {
        /// Configuration and provider access.
        pub config: Arc<AgentConfig>,
        /// Local session registry — we manage sessions via this.
        pub registry: Arc<Mutex<SessionRegistry>>,
        /// Session materializer for heavy async work (DB, MCP, actor spawn).
        /// Separates expensive operations from registry lock to keep control plane fast.
        pub session_materializer: Arc<crate::agent::session_materializer::SessionMaterializer>,
        /// Tracks created_at timestamps and cwds per session_id.
        pub session_meta: SessionMetaMap,
        /// Mesh handle for DHT registration of newly created sessions.
        pub mesh: Option<MeshHandle>,
        /// Per-session async locks preventing concurrent materialization of the
        /// same `session_id`.  Different sessions are independent.
        pub materialization_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
        /// Cached model inventory for fast model listing without blocking the actor mailbox.
        pub model_inventory: crate::model_inventory::ModelInventory,
    }

    // ── Actor ─────────────────────────────────────────────────────────────────

    /// Per-node actor that manages session lifecycle for remote peers.
    ///
    /// Every node in the mesh runs one `RemoteNodeManager`. Remote peers send
    /// it `CreateRemoteSession` / `ListRemoteSessions` / etc. to interact with
    /// sessions on this machine.
    ///
    /// Heavy operations (create/resume/fork) use the DelegatedReply pattern:
    /// they spawn background tasks that capture cheap clones of `shared_state`,
    /// keeping the actor mailbox responsive for lightweight control-plane messages.
    #[derive(Actor)]
    pub struct RemoteNodeManager {
        /// Shared state accessible from spawned tasks.
        shared_state: RemoteNodeManagerState,
        /// Optional fixed node name returned by `GetNodeInfo`.
        ///
        /// When `Some`, `GetNodeInfo` returns this value directly as `hostname`
        /// instead of calling `get_hostname()`. Useful in tests (deterministic,
        /// parallel-safe) and in future config-driven deployments where the
        /// operator wants a stable name independent of the OS hostname.
        node_name: Option<String>,
    }

    impl RemoteNodeManager {
        /// Create a new `RemoteNodeManager`.
        pub fn new(
            config: Arc<AgentConfig>,
            registry: Arc<Mutex<SessionRegistry>>,
            mesh: Option<MeshHandle>,
        ) -> Self {
            let model_inventory = crate::model_inventory::ModelInventory::new(config.clone());
            let session_materializer = Arc::new(
                crate::agent::session_materializer::SessionMaterializer::new(config.clone()),
            );

            // Set mesh on model inventory and session materializer if provided
            #[cfg(feature = "remote")]
            if let Some(ref mesh_handle) = mesh {
                model_inventory.set_mesh(mesh_handle.clone());
                session_materializer.set_mesh(mesh_handle.clone());
            }

            let shared_state = RemoteNodeManagerState {
                config,
                registry,
                session_materializer,
                session_meta: Arc::new(Mutex::new(HashMap::new())),
                mesh,
                materialization_locks: Arc::new(Mutex::new(HashMap::new())),
                model_inventory,
            };

            // Prewarm model inventory in the background so first remote model
            // advertisements are less likely to return an indistinguishable empty snapshot.
            let inventory = shared_state.model_inventory.clone();
            tokio::spawn(async move {
                let _ = inventory.trigger_refresh().await;
            });

            Self {
                shared_state,
                node_name: None,
            }
        }

        /// Override the name returned by `GetNodeInfo` instead of reading the
        /// OS hostname.  Returns `self` for easy chaining:
        ///
        /// ```rust,ignore
        /// let nm = RemoteNodeManager::new(config, registry, mesh)
        ///     .with_node_name("bob".to_string());
        /// ```
        pub fn with_node_name(mut self, name: String) -> Self {
            self.node_name = Some(name);
            self
        }
    }

    impl RemoteNodeManagerState {
        /// Build a `SessionHandoff` from an already-spawned local actor ref.
        async fn handoff_for_local_actor(
            &self,
            session_id: &str,
            actor_ref: kameo::actor::ActorRef<SessionActor>,
        ) -> SessionHandoff {
            let actor_id_raw = actor_ref.id().sequence_id();
            log::info!(
                "RemoteNodeManager: reusing already-materialized session {} (actor_id={})",
                session_id,
                actor_id_raw,
            );

            match std::panic::AssertUnwindSafe(async { actor_ref.into_remote_ref().await })
                .catch_unwind()
                .await
            {
                Ok(remote_ref) => SessionHandoff::direct(remote_ref),
                Err(_) if self.mesh.is_some() => {
                    log::warn!(
                        "RemoteNodeManager: direct remote export unavailable for existing session {}; returning lookup-only handoff",
                        session_id
                    );
                    SessionHandoff::lookup_only()
                }
                Err(_) => {
                    log::warn!(
                        "RemoteNodeManager: direct remote export unavailable for existing session {} and no mesh is active; returning no-attach-path handoff",
                        session_id
                    );
                    SessionHandoff::no_attach_path()
                }
            }
        }

        /// Materialize a remote session from a spawned task.
        ///
        /// This method is called from background tasks spawned by DelegatedReply
        /// handlers. It uses the shared state to perform the heavy materialization
        /// work without blocking the actor mailbox.
        ///
        /// Uses the 3-phase pattern:
        /// 1. Prepare (NO lock): DB creation, MCP init, actor spawn
        /// 2. Register (lock held): Insert into in-memory maps (microseconds)
        /// 3. Finalize (NO lock): DHT registration, event emission
        pub async fn materialize_remote_session(
            &self,
            session_id: String,
            cwd_path: Option<PathBuf>,
            cwd: Option<String>,
            title: Option<String>,
            created_at: i64,
        ) -> Result<SessionHandoff, AgentError> {
            // ── Per-session single-flight lock ──────────────────────────────
            // Prevents two concurrent callers from materializing the same
            // session_id into separate SessionActor instances.  Different
            // session IDs proceed independently.
            let session_lock = {
                let mut locks = self.materialization_locks.lock().await;
                locks
                    .entry(session_id.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone()
            };
            let _guard = session_lock.lock().await;

            // ── Idempotency check ───────────────────────────────────────────
            // After acquiring the per-session lock, check whether another
            // caller already materialized this session.
            {
                let registry = self.registry.lock().await;
                if let Some(existing) = registry.local_actor_ref(&session_id).cloned() {
                    drop(registry);
                    let handoff = self.handoff_for_local_actor(&session_id, existing).await;
                    self.cleanup_materialization_lock(&session_id, &session_lock)
                        .await;
                    return Ok(handoff);
                }
            }

            // ── Phase 1: Prepare (NO lock held) ─────────────────────────────
            // Initialize MCP, spawn actor for EXISTING session.
            // This is the heavy work that should NOT hold the registry lock.
            let req = agent_client_protocol::schema::LoadSessionRequest::new(
                agent_client_protocol::schema::SessionId::from(session_id.clone()),
                cwd_path.clone().unwrap_or_default(),
            );

            let (prepared, actor_ref) = match self
                .session_materializer
                .prepare_load_session(req, vec![], Some(&self.registry))
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?
            {
                crate::agent::session_materializer::PreparedSessionResult::Prepared(prepared) => {
                    let actor_ref = prepared.actor_ref.clone();
                    (Some(prepared), actor_ref)
                }
                crate::agent::session_materializer::PreparedSessionResult::AlreadyRegistered(
                    session_ref,
                ) => (
                    None,
                    match session_ref {
                        crate::agent::remote::SessionActorRef::Local(actor_ref) => actor_ref,
                        #[cfg(feature = "remote")]
                        crate::agent::remote::SessionActorRef::Remote { .. } => {
                            return Err(AgentError::Internal(format!(
                                "Session {} was registered without a local actor",
                                session_id
                            )));
                        }
                    },
                ),
            };

            let actor_id_raw = actor_ref.id().sequence_id();

            tracing::Span::current()
                .record("session_id", &session_id)
                .record("actor_id", actor_id_raw);

            if let Some(prepared) = prepared.as_ref() {
                // ── Phase 2: Register (lock held for microseconds) ──────────────
                {
                    let mut registry = self.registry.lock().await;
                    registry.register_prepared_session(prepared).await;
                }

                // ── Phase 3: Finalize (NO lock held) ────────────────────────────
                self.session_materializer
                    .finalize_session(prepared, None)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?;
            }

            {
                let mut session_meta = self.session_meta.lock().await;
                session_meta.insert(session_id.clone(), (created_at, cwd.clone()));
            }

            let handoff = self
                .handoff_for_local_actor(&session_id, actor_ref.clone())
                .await;

            log::info!(
                "RemoteNodeManager: materialized session {} (actor_id={}, title={:?})",
                session_id,
                actor_id_raw,
                title
            );

            drop(_guard);
            self.cleanup_materialization_lock(&session_id, &session_lock)
                .await;

            Ok(handoff)
        }

        /// Best-effort cleanup of a per-session lock map entry when no other
        /// task is waiting on it.
        async fn cleanup_materialization_lock(
            &self,
            session_id: &str,
            lock: &Arc<tokio::sync::Mutex<()>>,
        ) {
            let mut locks = self.materialization_locks.lock().await;
            if Arc::strong_count(lock) == 2 {
                locks.remove(session_id);
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Get the local hostname using `gethostname` syscall via std.
    fn get_hostname() -> String {
        // Try environment variable first (allows override)
        if let Ok(h) = std::env::var("HOSTNAME")
            && !h.is_empty()
        {
            return h;
        }
        // Fall back to running `hostname`
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    // ── Message handlers ──────────────────────────────────────────────────────

    impl Message<CreateRemoteSession> for RemoteNodeManager {
        type Reply = kameo::reply::DelegatedReply<Result<CreateRemoteSessionResponse, AgentError>>;

        #[tracing::instrument(
            name = "remote.node_manager.create_session",
            skip(self, ctx),
            fields(
                cwd = msg.cwd.as_deref().unwrap_or("<none>"),
                session_id = tracing::field::Empty,
                actor_id = tracing::field::Empty,
            )
        )]
        async fn handle(
            &mut self,
            msg: CreateRemoteSession,
            ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let cwd_path: Option<PathBuf> = msg.cwd.as_ref().map(PathBuf::from);

            // Capture shared state for the spawned task
            let shared_state = self.shared_state.clone();

            // Spawn the work as a background task
            ctx.spawn(async move {
                if let Some(ref path) = cwd_path
                    && !path.is_absolute()
                {
                    return Err(AgentError::Internal(format!(
                        "cwd must be an absolute path, got: {}",
                        path.display()
                    )));
                }

                let session_context = shared_state
                    .config
                    .provider
                    .create_session(
                        cwd_path.clone(),
                        None,
                        &shared_state.config.execution_config_snapshot(),
                    )
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?;

                let session = session_context.session();
                let session_id = session.public_id.clone();
                let title = session.name.clone();
                let created_at = session
                    .created_at
                    .map(|ts| ts.unix_timestamp())
                    .unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                    });

                tracing::Span::current().record("session_id", &session_id);

                let handoff = shared_state
                    .materialize_remote_session(
                        session_id.clone(),
                        cwd_path,
                        msg.cwd.clone(),
                        title.clone(),
                        created_at,
                    )
                    .await?;

                Ok(CreateRemoteSessionResponse {
                    session_id,
                    handoff,
                    cwd: msg.cwd,
                    title,
                    created_at,
                })
            })
        }
    }

    impl Message<ForkRemoteSession> for RemoteNodeManager {
        type Reply = kameo::reply::DelegatedReply<Result<ForkRemoteSessionResponse, AgentError>>;

        #[tracing::instrument(
            name = "remote.node_manager.fork_session",
            skip(self, ctx),
            fields(
                source_session_id = %msg.source_session_id,
                message_id = %msg.message_id,
                session_id = tracing::field::Empty,
                actor_id = tracing::field::Empty,
            )
        )]
        async fn handle(
            &mut self,
            msg: ForkRemoteSession,
            ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            // Capture shared state for the spawned task
            let shared_state = self.shared_state.clone();

            // Spawn the heavy work as a background task
            ctx.spawn(async move {
                let session = shared_state
                    .config
                    .provider
                    .history_store()
                    .get_session(&msg.source_session_id)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?
                    .ok_or_else(|| AgentError::SessionNotFound {
                        session_id: msg.source_session_id.clone(),
                    })?;

                let forked_session_id = shared_state
                    .config
                    .provider
                    .history_store()
                    .fork_session(
                        &msg.source_session_id,
                        &msg.message_id,
                        crate::session::domain::ForkOrigin::User,
                    )
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?;

                let forked_session = shared_state
                    .config
                    .provider
                    .history_store()
                    .get_session(&forked_session_id)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?
                    .ok_or_else(|| AgentError::SessionNotFound {
                        session_id: forked_session_id.clone(),
                    })?;

                let cwd_path = forked_session.cwd.clone();
                let cwd = cwd_path.as_ref().map(|path| path.display().to_string());
                let title = forked_session.name.clone().or(session.name.clone());
                let created_at = forked_session
                    .created_at
                    .map(|ts| ts.unix_timestamp())
                    .unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                    });

                tracing::Span::current().record("session_id", &forked_session_id);

                let handoff = shared_state
                    .materialize_remote_session(
                        forked_session_id.clone(),
                        cwd_path,
                        cwd.clone(),
                        title.clone(),
                        created_at,
                    )
                    .await?;

                Ok(ForkRemoteSessionResponse {
                    session_id: forked_session_id,
                    handoff,
                    cwd,
                    title,
                    created_at,
                })
            })
        }
    }

    impl Message<ListRemoteSessions> for RemoteNodeManager {
        type Reply = kameo::reply::DelegatedReply<Result<ListRemoteSessionsResponse, AgentError>>;

        #[tracing::instrument(
            name = "remote.node_manager.list_sessions",
            skip(self, ctx),
            fields(count = tracing::field::Empty, total = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            msg: ListRemoteSessions,
            ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            use crate::agent::messages::SessionRuntimeStatus;

            let shared_state = self.shared_state.clone();
            let hostname = self.node_name.clone().unwrap_or_else(get_hostname);

            ctx.spawn(async move {
                let offset = msg.offset.unwrap_or(0) as usize;
                let limit = msg.limit.unwrap_or(20).clamp(1, 100) as usize;

                let store = shared_state.config.provider.history_store();
                let all_sessions = store.list_sessions().await.map_err(|e| {
                    AgentError::Internal(format!("Failed to list persisted sessions: {}", e))
                })?;

                let mut sorted = all_sessions;
                sorted.sort_by(|a, b| {
                    b.updated_at
                        .cmp(&a.updated_at)
                        .then_with(|| b.public_id.cmp(&a.public_id))
                });

                let total_count = sorted.len() as u32;
                let page: Vec<_> = sorted.into_iter().skip(offset).take(limit).collect();
                let page_len = page.len();

                let registry_live: std::collections::HashMap<
                    String,
                    crate::agent::remote::SessionActorRef,
                > = {
                    let registry = shared_state.registry.lock().await;
                    page.iter()
                        .filter_map(|s| {
                            registry
                                .get(&s.public_id)
                                .map(|r| (s.public_id.clone(), r.clone()))
                        })
                        .collect()
                };

                let mut infos = Vec::with_capacity(page_len);
                for session in &page {
                    let title = store
                        .get_initial_intent_snapshot(&session.public_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|snapshot| {
                            querymt_utils::str_utils::truncate_with_ellipsis(&snapshot.summary, 80)
                        })
                        .or_else(|| session.name.clone());

                    let created_at = session.created_at.map(|t| t.unix_timestamp()).unwrap_or(0);

                    let (actor_id, runtime_state) = match registry_live.get(&session.public_id) {
                        Some(session_ref) => {
                            let aid = match session_ref {
                                crate::agent::remote::SessionActorRef::Local(ar) => {
                                    ar.id().sequence_id()
                                }
                                #[cfg(feature = "remote")]
                                crate::agent::remote::SessionActorRef::Remote { .. } => 0,
                            };
                            let state = match tokio::time::timeout(
                                std::time::Duration::from_millis(200),
                                session_ref.get_runtime_status(),
                            )
                            .await
                            {
                                Ok(Ok(SessionRuntimeStatus::Idle)) => "idle".to_string(),
                                Ok(Ok(
                                    SessionRuntimeStatus::Running
                                    | SessionRuntimeStatus::CancelRequested,
                                )) => "busy".to_string(),
                                Ok(Err(_)) | Err(_) => "active".to_string(),
                            };
                            (aid, Some(state))
                        }
                        None => (0, Some("persisted".to_string())),
                    };

                    infos.push(RemoteSessionInfo {
                        session_id: session.public_id.clone(),
                        actor_id,
                        cwd: session.cwd.as_ref().map(|p| p.display().to_string()),
                        created_at,
                        title,
                        peer_label: hostname.clone(),
                        runtime_state,
                    });
                }

                let next_offset = if ((offset + page_len) as u32) < total_count {
                    Some((offset + page_len) as u32)
                } else {
                    None
                };

                tracing::Span::current()
                    .record("count", infos.len())
                    .record("total", total_count);

                Ok(ListRemoteSessionsResponse {
                    sessions: infos,
                    next_offset,
                    total_count,
                })
            })
        }
    }

    impl Message<StopRemoteSessionRuntime> for RemoteNodeManager {
        type Reply = Result<(), AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.stop_session_runtime",
            skip(self, _ctx),
            fields(session_id = %msg.session_id, found = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            msg: StopRemoteSessionRuntime,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let session_ref = {
                let mut registry = self.shared_state.registry.lock().await;
                registry.remove(&msg.session_id)
            };

            if let Some(session_ref) = session_ref {
                tracing::Span::current().record("found", true);

                // Bound shutdown latency so stop requests cannot hang forever
                // if an actor is unresponsive.
                let shutdown_timeout = std::time::Duration::from_secs(2);
                match tokio::time::timeout(shutdown_timeout, session_ref.shutdown()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!(
                            "RemoteNodeManager: shutdown error for session {}: {}",
                            msg.session_id,
                            e
                        );
                    }
                    Err(_) => {
                        log::warn!(
                            "RemoteNodeManager: shutdown timed out for session {} after {:?}",
                            msg.session_id,
                            shutdown_timeout
                        );
                    }
                }

                // Ensure this session's re-registration closure is removed from the
                // mesh handle so repeated create/stop cycles don't leak entries.
                if let Some(ref mesh) = self.shared_state.mesh {
                    let dht_name = crate::agent::remote::dht_name::session(&msg.session_id);
                    mesh.deregister_actor(&dht_name);
                }

                log::info!(
                    "RemoteNodeManager: stopped runtime for session {}",
                    msg.session_id
                );
                Ok(())
            } else {
                tracing::Span::current().record("found", false);
                Err(AgentError::SessionNotFound {
                    session_id: msg.session_id.clone(),
                })
            }
        }
    }

    impl Message<ResumeRemoteSession> for RemoteNodeManager {
        type Reply = kameo::reply::DelegatedReply<Result<CreateRemoteSessionResponse, AgentError>>;

        #[tracing::instrument(
            name = "remote.node_manager.resume_session",
            skip(self, ctx),
            fields(session_id = %msg.session_id)
        )]
        async fn handle(
            &mut self,
            msg: ResumeRemoteSession,
            ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            // Capture shared state for the spawned task
            let shared_state = self.shared_state.clone();

            // Spawn the heavy work as a background task
            ctx.spawn(async move {
                let session = shared_state
                    .config
                    .provider
                    .history_store()
                    .get_session(&msg.session_id)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?
                    .ok_or_else(|| AgentError::SessionNotFound {
                        session_id: msg.session_id.clone(),
                    })?;

                let cwd_path = session.cwd.clone();
                let cwd = cwd_path.as_ref().map(|p| p.display().to_string());
                let title = session.name.clone();
                let created_at = session
                    .created_at
                    .map(|ts| ts.unix_timestamp())
                    .unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                    });

                let handoff = shared_state
                    .materialize_remote_session(
                        msg.session_id.clone(),
                        cwd_path,
                        cwd.clone(),
                        title.clone(),
                        created_at,
                    )
                    .await?;

                Ok(CreateRemoteSessionResponse {
                    session_id: msg.session_id,
                    handoff,
                    cwd,
                    title,
                    created_at,
                })
            })
        }
    }

    impl Message<GetNodeInfo> for RemoteNodeManager {
        type Reply = Result<NodeInfo, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.get_node_info",
            skip(self, _ctx),
            fields(hostname = tracing::field::Empty, active_sessions = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            _msg: GetNodeInfo,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let hostname = self.node_name.clone().unwrap_or_else(get_hostname);

            let active_sessions = {
                let registry = self.shared_state.registry.lock().await;
                registry.len()
            };

            tracing::Span::current()
                .record("hostname", &hostname)
                .record("active_sessions", active_sessions);

            let node_id = self
                .shared_state
                .mesh
                .as_ref()
                .map(|m| NodeId::from_peer_id(*m.peer_id()))
                .ok_or(AgentError::MeshNotBootstrapped)?;

            Ok(NodeInfo {
                node_id,
                hostname,
                capabilities: vec!["shell".to_string(), "filesystem".to_string()],
                active_sessions,
            })
        }
    }

    impl Message<ListAvailableModels> for RemoteNodeManager {
        type Reply = Result<Vec<AvailableModel>, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.list_models",
            skip(self, _ctx),
            fields(model_count = tracing::field::Empty, cache_hit = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            _msg: ListAvailableModels,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            // Use the cached model inventory instead of synchronously enumerating models.
            // This keeps the actor mailbox responsive even when providers are slow.
            let (local_models, meta) = self.shared_state.model_inventory.get_snapshot().await;

            // Prewarming on actor startup handles the first-query empty snapshot case.
            // If cache is stale, refresh in the background and return the latest snapshot.
            if meta.is_stale && !meta.refresh_in_progress {
                let inventory = self.shared_state.model_inventory.clone();
                tokio::spawn(async move {
                    let _ = inventory.trigger_refresh().await;
                });
            }

            let local_only: Vec<AvailableModel> = local_models
                .into_iter()
                .filter(|entry| entry.node_id.is_none())
                .map(|entry| AvailableModel {
                    provider: entry.provider,
                    model: entry.model,
                })
                .collect();

            tracing::Span::current().record("model_count", local_only.len());
            tracing::Span::current().record("cache_hit", !meta.is_stale);
            Ok(local_only)
        }
    }

    impl Message<AdmissionRequest> for RemoteNodeManager {
        type Reply = Result<AdmissionResponse, String>;

        #[tracing::instrument(
            name = "remote.node_manager.admission",
            skip(self, _ctx),
            fields(variant = tracing::field::Empty, peer_id = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            msg: AdmissionRequest,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            use crate::agent::remote::invite::InviteStore;

            match msg {
                // ── First join: consume the invite, issue a membership token ──
                AdmissionRequest::Invite { invite_id, peer_id } => {
                    tracing::Span::current()
                        .record("variant", "Invite")
                        .record("peer_id", &peer_id);

                    let Some(ref mesh) = self.shared_state.mesh else {
                        return Ok(AdmissionResponse::Rejected {
                            reason: "host has no mesh handle".to_string(),
                        });
                    };

                    let Some(store_arc) = mesh.invite_store() else {
                        return Ok(AdmissionResponse::Rejected {
                            reason: "host has no invite store".to_string(),
                        });
                    };

                    let keypair = mesh.keypair().clone();
                    let mesh_name: Option<String> = None;

                    let result = store_arc.write().admit_peer(
                        &invite_id,
                        &peer_id,
                        &keypair,
                        mesh_name.as_deref(),
                    );

                    match result {
                        Ok(token) => {
                            log::info!(
                                "AdmissionRequest::Invite: admitted peer {} via invite {}",
                                peer_id,
                                invite_id
                            );
                            let existing_peers: Vec<String> = mesh
                                .known_peer_ids()
                                .iter()
                                .filter(|pid| pid.to_string() != peer_id)
                                .map(|pid| pid.to_string())
                                .collect();
                            Ok(AdmissionResponse::Admitted {
                                membership_token: token,
                                existing_peers,
                            })
                        }
                        Err(e) => {
                            log::warn!(
                                "AdmissionRequest::Invite: rejected peer {} ({})",
                                peer_id,
                                e
                            );
                            Ok(AdmissionResponse::Rejected {
                                reason: e.to_string(),
                            })
                        }
                    }
                }

                // ── Reconnect: verify the self-contained membership token ──────
                AdmissionRequest::Token {
                    membership_token,
                    peer_id,
                } => {
                    tracing::Span::current()
                        .record("variant", "Token")
                        .record("peer_id", &peer_id);

                    if membership_token.peer_id != peer_id {
                        log::warn!(
                            "AdmissionRequest::Token: peer_id mismatch: token={} request={}",
                            membership_token.peer_id,
                            peer_id
                        );
                        return Ok(AdmissionResponse::Rejected {
                            reason: "peer_id does not match membership token".to_string(),
                        });
                    }

                    // Pure crypto — any mesh node can verify, no store access needed.
                    match InviteStore::verify_membership_token(&membership_token) {
                        Ok(()) => {
                            log::info!(
                                "AdmissionRequest::Token: readmitted peer {} (admitted_by={})",
                                peer_id,
                                membership_token.admitted_by
                            );
                            let existing_peers: Vec<String> = self
                                .shared_state
                                .mesh
                                .as_ref()
                                .map(|m| {
                                    m.known_peer_ids()
                                        .iter()
                                        .filter(|pid| pid.to_string() != peer_id)
                                        .map(|pid| pid.to_string())
                                        .collect()
                                })
                                .unwrap_or_default();
                            Ok(AdmissionResponse::Readmitted { existing_peers })
                        }
                        Err(e) => {
                            log::warn!(
                                "AdmissionRequest::Token: rejected peer {} ({})",
                                peer_id,
                                e
                            );
                            Ok(AdmissionResponse::Rejected {
                                reason: e.to_string(),
                            })
                        }
                    }
                }
            }
        }
    }

    // ── RemoteActor + RemoteMessage registrations ─────────────────────────────

    impl kameo::remote::RemoteActor for RemoteNodeManager {
        const REMOTE_ID: &'static str = "querymt::RemoteNodeManager";
    }

    #[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
    #[linkme(crate = _internal::linkme)]
    static REMOTE_NODE_MANAGER_REG: (&'static str, _internal::RemoteActorFns) = (
        <RemoteNodeManager as kameo::remote::RemoteActor>::REMOTE_ID,
        _internal::RemoteActorFns {
            link: (|actor_id, sibling_id, sibling_remote_id| {
                Box::pin(_internal::link::<RemoteNodeManager>(
                    actor_id,
                    sibling_id,
                    sibling_remote_id,
                ))
            }) as _internal::RemoteLinkFn,
            unlink: (|actor_id, sibling_id| {
                Box::pin(_internal::unlink::<RemoteNodeManager>(actor_id, sibling_id))
            }) as _internal::RemoteUnlinkFn,
            signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
                Box::pin(_internal::signal_link_died::<RemoteNodeManager>(
                    dead_actor_id,
                    notified_actor_id,
                    stop_reason,
                ))
            }) as _internal::RemoteSignalLinkDiedFn,
        },
    );

    macro_rules! remote_node_msg_impl {
        ($msg_ty:ty, $remote_id:expr, $static_name:ident) => {
            impl kameo::remote::RemoteMessage<$msg_ty> for RemoteNodeManager {
                const REMOTE_ID: &'static str = $remote_id;
            }

            #[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
            #[linkme(crate = _internal::linkme)]
            static $static_name: (
                _internal::RemoteMessageRegistrationID<'static>,
                _internal::RemoteMessageFns,
            ) = (
                _internal::RemoteMessageRegistrationID {
                    actor_remote_id: <RemoteNodeManager as kameo::remote::RemoteActor>::REMOTE_ID,
                    message_remote_id: <RemoteNodeManager as kameo::remote::RemoteMessage<
                        $msg_ty,
                    >>::REMOTE_ID,
                },
                _internal::RemoteMessageFns {
                    ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
                        Box::pin(_internal::ask::<RemoteNodeManager, $msg_ty>(
                            actor_id,
                            msg,
                            mailbox_timeout,
                            reply_timeout,
                        ))
                    }) as _internal::RemoteAskFn,
                    try_ask: (|actor_id, msg, reply_timeout| {
                        Box::pin(_internal::try_ask::<RemoteNodeManager, $msg_ty>(
                            actor_id,
                            msg,
                            reply_timeout,
                        ))
                    }) as _internal::RemoteTryAskFn,
                    tell: (|actor_id, msg, mailbox_timeout| {
                        Box::pin(_internal::tell::<RemoteNodeManager, $msg_ty>(
                            actor_id,
                            msg,
                            mailbox_timeout,
                        ))
                    }) as _internal::RemoteTellFn,
                    try_tell: (|actor_id, msg| {
                        Box::pin(_internal::try_tell::<RemoteNodeManager, $msg_ty>(
                            actor_id, msg,
                        ))
                    }) as _internal::RemoteTryTellFn,
                },
            );
        };
    }

    remote_node_msg_impl!(
        CreateRemoteSession,
        "querymt::CreateRemoteSession",
        REG_CREATE_REMOTE_SESSION
    );
    remote_node_msg_impl!(
        ForkRemoteSession,
        "querymt::ForkRemoteSession",
        REG_FORK_REMOTE_SESSION
    );
    remote_node_msg_impl!(
        ListRemoteSessions,
        "querymt::ListRemoteSessions",
        REG_LIST_REMOTE_SESSIONS
    );
    remote_node_msg_impl!(
        StopRemoteSessionRuntime,
        "querymt::StopRemoteSessionRuntime",
        REG_STOP_REMOTE_SESSION_RUNTIME
    );
    remote_node_msg_impl!(
        ResumeRemoteSession,
        "querymt::ResumeRemoteSession",
        REG_RESUME_REMOTE_SESSION
    );
    remote_node_msg_impl!(GetNodeInfo, "querymt::GetNodeInfo", REG_GET_NODE_INFO);
    remote_node_msg_impl!(
        ListAvailableModels,
        "querymt::ListAvailableModels",
        REG_LIST_AVAILABLE_MODELS
    );
    remote_node_msg_impl!(
        AdmissionRequest,
        "querymt::AdmissionRequest",
        REG_ADMISSION_REQUEST
    );
}
