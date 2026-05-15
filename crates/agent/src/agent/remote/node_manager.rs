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
    DestroyRemoteSession, ForkRemoteSession, ForkRemoteSessionResponse, GetNodeInfo,
    ListAvailableModels, ListRemoteSessions, RemoteNodeManager, ResumeRemoteSession,
    SessionHandoff,
};

#[cfg(feature = "remote")]
mod remote_impl {
    use super::{AvailableModel, NodeInfo, RemoteSessionInfo};
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::remote::NodeId;
    use crate::agent::remote::mesh::MeshHandle;
    use crate::agent::session_actor::SessionActor;
    use crate::agent::session_registry::{SessionMaterializationOptions, SessionRegistry};
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
    use tracing::Instrument;

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

    /// List sessions active on this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListRemoteSessions;

    /// List all provider/model pairs this node can serve (has valid credentials for).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ListAvailableModels;

    /// Destroy (shutdown) a session on this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DestroyRemoteSession {
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

    // ── Actor ─────────────────────────────────────────────────────────────────

    /// Per-node actor that manages session lifecycle for remote peers.
    ///
    /// Every node in the mesh runs one `RemoteNodeManager`. Remote peers send
    /// it `CreateRemoteSession` / `ListRemoteSessions` / etc. to interact with
    /// sessions on this machine.
    #[derive(Actor)]
    pub struct RemoteNodeManager {
        config: Arc<AgentConfig>,
        /// Local session registry — we manage sessions via this.
        registry: Arc<Mutex<SessionRegistry>>,
        /// Tracks created_at timestamps and cwds per session_id.
        session_meta: HashMap<String, (i64, Option<String>)>,
        /// Mesh handle for DHT registration of newly created sessions.
        mesh: Option<MeshHandle>,
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
            Self {
                config,
                registry,
                session_meta: HashMap::new(),
                mesh,
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

        async fn materialize_remote_session(
            &mut self,
            session_id: String,
            cwd_path: Option<PathBuf>,
            cwd: Option<String>,
            title: Option<String>,
            created_at: i64,
        ) -> Result<SessionHandoff, AgentError> {
            let materialization = {
                let mut registry = self.registry.lock().await;
                registry.set_mesh(self.mesh.clone());
                registry
                    .materialize_session_actor(
                        session_id.clone(),
                        cwd_path.clone(),
                        &[],
                        false,
                        SessionMaterializationOptions {
                            attach_mesh_handle: true,
                            register_in_dht: true,
                        },
                    )
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?
            };
            let actor_ref = materialization.actor_ref;
            let runtime = materialization.runtime;
            let actor_id_raw = actor_ref.id().sequence_id();

            tracing::Span::current()
                .record("session_id", &session_id)
                .record("actor_id", actor_id_raw);

            self.session_meta
                .insert(session_id.clone(), (created_at, cwd.clone()));

            if let Err(e) = self
                .config
                .emit_event_persisted(&session_id, crate::events::AgentEventKind::SessionCreated)
                .await
            {
                log::warn!(
                    "RemoteNodeManager: failed to emit SessionCreated for {}: {}",
                    session_id,
                    e
                );
            }

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

            if let Some(ref cwd_path) = cwd_path {
                if cwd_path.exists() {
                    let manager_actor = self.config.workspace_manager_actor.clone();
                    let runtime_clone = runtime.clone();
                    let cwd_owned = cwd_path.clone();
                    let config_clone = self.config.clone();
                    let session_id_for_index = session_id.clone();
                    let index_span = tracing::info_span!(
                        "remote.node_manager.init_workspace_index",
                        session_id = %session_id,
                        cwd = %cwd_owned.display(),
                    );
                    tokio::spawn(
                        async move {
                            let root = crate::index::resolve_workspace_root(&cwd_owned);
                            match manager_actor
                                .ask(crate::index::GetOrCreate { root: root.clone() })
                                .await
                            {
                                Ok(handle) => {
                                    let _ = runtime_clone.workspace_handle.set(handle);
                                    config_clone.emit_event(
                                        &session_id_for_index,
                                        crate::events::AgentEventKind::WorkspaceIndexReady {
                                            workspace_root: root.display().to_string(),
                                        },
                                    );
                                }
                                Err(e) => {
                                    log::warn!("Failed to initialize workspace index: {}", e)
                                }
                            }
                        }
                        .instrument(index_span),
                    );
                } else {
                    log::debug!(
                        "RemoteNodeManager: cwd {:?} does not exist on this node, skipping workspace index",
                        cwd_path
                    );
                }
            }

            let handoff = match std::panic::AssertUnwindSafe(async {
                actor_ref.into_remote_ref().await
            })
            .catch_unwind()
            .await
            {
                Ok(remote_ref) => SessionHandoff::direct(remote_ref),
                Err(_) if self.mesh.is_some() => {
                    log::warn!(
                        "RemoteNodeManager: direct remote export unavailable for session {}; returning lookup-only handoff",
                        session_id
                    );
                    SessionHandoff::lookup_only()
                }
                Err(_) => {
                    log::warn!(
                        "RemoteNodeManager: direct remote export unavailable for session {} and no mesh is active; returning no-attach-path handoff",
                        session_id
                    );
                    SessionHandoff::no_attach_path()
                }
            };

            log::info!(
                "RemoteNodeManager: materialized session {} (actor_id={}, title={:?})",
                session_id,
                actor_id_raw,
                title
            );

            Ok(handoff)
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
        type Reply = Result<CreateRemoteSessionResponse, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.create_session",
            skip(self, _ctx),
            fields(
                cwd = msg.cwd.as_deref().unwrap_or("<none>"),
                session_id = tracing::field::Empty,
                actor_id = tracing::field::Empty,
            )
        )]
        async fn handle(
            &mut self,
            msg: CreateRemoteSession,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let cwd_path: Option<PathBuf> = msg.cwd.as_ref().map(PathBuf::from);

            if let Some(ref path) = cwd_path
                && !path.is_absolute()
            {
                return Err(AgentError::Internal(format!(
                    "cwd must be an absolute path, got: {}",
                    path.display()
                )));
            }

            let session_context = self
                .config
                .provider
                .create_session(
                    cwd_path.clone(),
                    None,
                    &self.config.execution_config_snapshot(),
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

            let handoff = self
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
        }
    }

    impl Message<ForkRemoteSession> for RemoteNodeManager {
        type Reply = Result<ForkRemoteSessionResponse, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.fork_session",
            skip(self, _ctx),
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
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let session = self
                .config
                .provider
                .history_store()
                .get_session(&msg.source_session_id)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?
                .ok_or_else(|| AgentError::SessionNotFound {
                    session_id: msg.source_session_id.clone(),
                })?;

            let forked_session_id = self
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

            let forked_session = self
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

            let handoff = self
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
        }
    }

    impl Message<ListRemoteSessions> for RemoteNodeManager {
        type Reply = Result<Vec<RemoteSessionInfo>, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.list_sessions",
            skip(self, _ctx),
            fields(count = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            _msg: ListRemoteSessions,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            use crate::agent::messages::SessionRuntimeStatus;

            let registry = self.registry.lock().await;
            let session_ids = registry.session_ids();

            let hostname = get_hostname();
            let store = self.config.provider.history_store();

            let mut infos = Vec::new();
            for sid in session_ids {
                // Prefer metadata from session_meta (for remotely-created sessions),
                // but fall back to querying the session store for locally-created sessions.
                let (created_at, cwd, session_name) = if let Some((ts, meta_cwd)) =
                    self.session_meta.get(&sid)
                {
                    (*ts, meta_cwd.clone(), None)
                } else {
                    match store.get_session(&sid).await {
                        Ok(Some(session)) => {
                            let ts = session.created_at.map(|t| t.unix_timestamp()).unwrap_or(0);
                            let cwd = session.cwd.map(|p| p.display().to_string());
                            let session_name = session.name.clone();
                            (ts, cwd, session_name)
                        }
                        _ => (0, None, None),
                    }
                };

                // Match local session-list title behavior: use the initial intent summary
                // (truncated) as the display title, then fall back to session.name.
                let title = match store.get_initial_intent_snapshot(&sid).await {
                    Ok(Some(snapshot)) => Some(querymt_utils::str_utils::truncate_with_ellipsis(
                        &snapshot.summary,
                        80,
                    )),
                    _ => session_name,
                };

                let session_ref = registry.get(&sid).cloned();
                let actor_id = match session_ref.as_ref() {
                    Some(crate::agent::remote::SessionActorRef::Local(ar)) => ar.id().sequence_id(),
                    #[cfg(feature = "remote")]
                    Some(crate::agent::remote::SessionActorRef::Remote { .. }) | None => 0,
                    #[cfg(not(feature = "remote"))]
                    None => 0,
                };
                let runtime_state = match session_ref {
                    Some(session_ref) => match tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        session_ref.get_runtime_status(),
                    )
                    .await
                    {
                        Ok(Ok(SessionRuntimeStatus::Idle)) => Some("idle".to_string()),
                        Ok(Ok(
                            SessionRuntimeStatus::Running | SessionRuntimeStatus::CancelRequested,
                        )) => Some("busy".to_string()),
                        Ok(Err(e)) => {
                            log::debug!(
                                "RemoteNodeManager: failed to query runtime status for {}: {}",
                                sid,
                                e
                            );
                            Some("active".to_string())
                        }
                        Err(_) => Some("active".to_string()),
                    },
                    None => Some("active".to_string()),
                };

                infos.push(RemoteSessionInfo {
                    session_id: sid,
                    actor_id,
                    cwd,
                    created_at,
                    title,
                    peer_label: hostname.clone(),
                    runtime_state,
                });
            }

            tracing::Span::current().record("count", infos.len());
            Ok(infos)
        }
    }

    impl Message<DestroyRemoteSession> for RemoteNodeManager {
        type Reply = Result<(), AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.destroy_session",
            skip(self, _ctx),
            fields(session_id = %msg.session_id, found = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            msg: DestroyRemoteSession,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let session_ref = {
                let mut registry = self.registry.lock().await;
                registry.remove(&msg.session_id)
            };

            if let Some(session_ref) = session_ref {
                tracing::Span::current().record("found", true);

                // Bound shutdown latency so destroy requests cannot hang forever
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
                // mesh handle so repeated create/destroy cycles don't leak entries.
                if let Some(ref mesh) = self.mesh {
                    let dht_name = crate::agent::remote::dht_name::session(&msg.session_id);
                    mesh.deregister_actor(&dht_name);
                }

                log::info!(
                    "RemoteNodeManager: destroyed runtime for session {}",
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
        type Reply = Result<CreateRemoteSessionResponse, AgentError>;

        #[tracing::instrument(
            name = "remote.node_manager.resume_session",
            skip(self, _ctx),
            fields(session_id = %msg.session_id)
        )]
        async fn handle(
            &mut self,
            msg: ResumeRemoteSession,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let session = self
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

            let handoff = self
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
                let registry = self.registry.lock().await;
                registry.len()
            };

            tracing::Span::current()
                .record("hostname", &hostname)
                .record("active_sessions", active_sessions);

            let node_id = self
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
            fields(model_count = tracing::field::Empty)
        )]
        async fn handle(
            &mut self,
            _msg: ListAvailableModels,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            // Delegate to the shared local enumerator from model_registry.
            let local_models = crate::model_registry::enumerate_local_models(&self.config).await;

            // Convert ModelEntry -> AvailableModel for the mesh response payload.
            let models: Vec<AvailableModel> = local_models
                .into_iter()
                .map(|entry| AvailableModel {
                    provider: entry.provider,
                    model: entry.model,
                })
                .collect();

            tracing::Span::current().record("model_count", models.len());
            Ok(models)
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

                    let Some(ref mesh) = self.mesh else {
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
        DestroyRemoteSession,
        "querymt::DestroyRemoteSession",
        REG_DESTROY_REMOTE_SESSION
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
