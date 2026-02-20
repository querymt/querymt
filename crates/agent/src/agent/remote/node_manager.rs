//! Remote node manager — handles session lifecycle requests from remote peers.
//!
//! `RemoteNodeManager` is a kameo actor that runs on every node in the mesh.
//! Remote peers send it messages to create, list, or destroy sessions on this
//! node. The local dashboard sends these messages to nodes it has discovered.
//!
//! The actor and its messages are only available with the `remote` feature.
//! The `RemoteSessionInfo` and `NodeInfo` data types are always available
//! (needed for UI serialization regardless of feature).

use serde::{Deserialize, Serialize};

/// Metadata about a session available on a remote node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionInfo {
    /// Session public ID (same format as local sessions)
    pub session_id: String,
    /// kameo ActorId of the SessionActor on the remote node (raw u64)
    pub actor_id: u64,
    /// Working directory on the remote machine (if set)
    pub cwd: Option<String>,
    /// Unix timestamp when the session was created
    pub created_at: i64,
    /// Session title/name, if set
    pub title: Option<String>,
    /// Human-readable label of the peer that owns this session
    pub peer_label: String,
}

/// Metadata about a remote node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
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
    CreateRemoteSession, CreateRemoteSessionResponse, DestroyRemoteSession, GetNodeInfo,
    ListAvailableModels, ListRemoteSessions, RemoteNodeManager,
};

#[cfg(feature = "remote")]
mod remote_impl {
    use super::{AvailableModel, NodeInfo, RemoteSessionInfo};
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::core::SessionRuntime;
    use crate::agent::remote::mesh::MeshHandle;
    use crate::agent::session_actor::SessionActor;
    use crate::agent::session_registry::SessionRegistry;
    use crate::error::AgentError;
    use kameo::Actor;
    use kameo::actor::Spawn;
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

    /// Response from `CreateRemoteSession`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CreateRemoteSessionResponse {
        /// The new session's public ID
        pub session_id: String,
        /// The `SessionActor`'s kameo ActorId (raw u64 for serialization)
        pub actor_id: u64,
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

    /// Get metadata about this node.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct GetNodeInfo;

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
        type Reply = Result<CreateRemoteSessionResponse, AgentError>;

        async fn handle(
            &mut self,
            msg: CreateRemoteSession,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let cwd_path: Option<PathBuf> = msg.cwd.as_ref().map(PathBuf::from);

            // Validate cwd if provided
            if let Some(ref path) = cwd_path
                && !path.is_absolute()
            {
                return Err(AgentError::Internal(format!(
                    "cwd must be an absolute path, got: {}",
                    path.display()
                )));
            }

            // Create session via provider
            let session_context = self
                .config
                .provider
                .create_session(
                    cwd_path.clone(),
                    None, // no parent
                    &self.config.execution_config_snapshot(),
                )
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;

            let session_id = session_context.session().public_id.clone();

            // Build a minimal runtime (no MCP for now — remote MCP is future work)
            let runtime = SessionRuntime::new(
                cwd_path.clone(),
                HashMap::new(), // mcp_services
                HashMap::new(), // mcp_tools
                Vec::new(),     // mcp_tool_defs
            );

            // Spawn the session actor, giving it access to the mesh handle so its
            // SubscribeEvents handler can do DHT lookups without a global flag.
            let actor = SessionActor::new(self.config.clone(), session_id.clone(), runtime.clone())
                .with_mesh(self.mesh.clone());
            let actor_ref = SessionActor::spawn(actor);

            let actor_id_raw = actor_ref.id().sequence_id();

            // Register in REMOTE_REGISTRY + Kademlia DHT so remote peers can
            // address the session actor by name.
            if let Some(ref mesh) = self.mesh {
                mesh.register_actor(actor_ref.clone(), format!("session::{}", session_id))
                    .await;
            } else {
                log::debug!(
                    "RemoteNodeManager: no mesh, skipping DHT registration for session {}",
                    session_id
                );
            }

            // Insert into local registry
            {
                let mut registry = self.registry.lock().await;
                registry.insert(session_id.clone(), actor_ref);
            }

            // Track metadata
            let created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.session_meta
                .insert(session_id.clone(), (created_at, msg.cwd.clone()));

            // Emit SessionCreated event
            self.config
                .emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

            // Emit ProviderChanged so the UI knows which model is active and
            // so prompt execution can find the LLM config (parity with
            // SessionRegistry::new_session).
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

            // Background: initialize workspace index (only if the path exists on this node)
            if let Some(ref cwd_path) = cwd_path {
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
                        "RemoteNodeManager: cwd {:?} does not exist on this node, skipping workspace index",
                        cwd_path
                    );
                }
            }

            log::info!(
                "RemoteNodeManager: created session {} (actor_id={})",
                session_id,
                actor_id_raw
            );

            Ok(CreateRemoteSessionResponse {
                session_id,
                actor_id: actor_id_raw,
            })
        }
    }

    impl Message<ListRemoteSessions> for RemoteNodeManager {
        type Reply = Result<Vec<RemoteSessionInfo>, AgentError>;

        async fn handle(
            &mut self,
            _msg: ListRemoteSessions,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let registry = self.registry.lock().await;
            let session_ids = registry.session_ids();

            let hostname = get_hostname();

            let mut infos = Vec::new();
            for sid in session_ids {
                let (created_at, cwd) = self.session_meta.get(&sid).cloned().unwrap_or((0, None));

                let actor_id = registry
                    .get(&sid)
                    .and_then(|r| match r {
                        crate::agent::remote::SessionActorRef::Local(ar) => {
                            Some(ar.id().sequence_id())
                        }
                        #[cfg(feature = "remote")]
                        crate::agent::remote::SessionActorRef::Remote { .. } => None,
                    })
                    .unwrap_or(0);

                infos.push(RemoteSessionInfo {
                    session_id: sid,
                    actor_id,
                    cwd,
                    created_at,
                    title: None,
                    peer_label: hostname.clone(),
                });
            }

            Ok(infos)
        }
    }

    impl Message<DestroyRemoteSession> for RemoteNodeManager {
        type Reply = Result<(), AgentError>;

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
                let _ = session_ref.shutdown().await;
                self.session_meta.remove(&msg.session_id);
                log::info!("RemoteNodeManager: destroyed session {}", msg.session_id);
                Ok(())
            } else {
                Err(AgentError::SessionNotFound {
                    session_id: msg.session_id.clone(),
                })
            }
        }
    }

    impl Message<GetNodeInfo> for RemoteNodeManager {
        type Reply = Result<NodeInfo, AgentError>;

        async fn handle(
            &mut self,
            _msg: GetNodeInfo,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let hostname = get_hostname();

            let active_sessions = {
                let registry = self.registry.lock().await;
                registry.len()
            };

            Ok(NodeInfo {
                hostname,
                capabilities: vec!["shell".to_string(), "filesystem".to_string()],
                active_sessions,
            })
        }
    }

    impl Message<ListAvailableModels> for RemoteNodeManager {
        type Reply = Result<Vec<AvailableModel>, AgentError>;

        async fn handle(
            &mut self,
            _msg: ListAvailableModels,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            let registry = self.config.provider.plugin_registry();
            registry.load_all_plugins().await;

            let mut models = Vec::new();

            for factory in registry.list() {
                let provider_name = factory.name().to_string();

                // For HTTP providers, require a valid API key
                let has_credentials = if let Some(http_factory) = factory.as_http() {
                    if let Some(api_key_name) = http_factory.api_key_name() {
                        // Check OAuth token first
                        #[cfg(feature = "oauth")]
                        {
                            if crate::auth::get_or_refresh_token(&provider_name)
                                .await
                                .is_ok()
                            {
                                true
                            } else {
                                std::env::var(api_key_name).is_ok()
                            }
                        }
                        #[cfg(not(feature = "oauth"))]
                        {
                            std::env::var(api_key_name).is_ok()
                        }
                    } else {
                        // No API key required for this HTTP provider
                        true
                    }
                } else {
                    // Non-HTTP provider (e.g., local llama-cpp) — always available
                    true
                };

                if !has_credentials {
                    continue;
                }

                // Resolve config for listing
                let cfg = if let Some(http_factory) = factory.as_http() {
                    let api_key = if let Some(api_key_name) = http_factory.api_key_name() {
                        #[cfg(feature = "oauth")]
                        {
                            crate::auth::get_or_refresh_token(&provider_name)
                                .await
                                .ok()
                                .or_else(|| std::env::var(api_key_name).ok())
                        }
                        #[cfg(not(feature = "oauth"))]
                        {
                            std::env::var(api_key_name).ok()
                        }
                    } else {
                        None
                    };

                    if let Some(key) = api_key {
                        serde_json::json!({"api_key": key}).to_string()
                    } else {
                        "{}".to_string()
                    }
                } else {
                    "{}".to_string()
                };

                match factory.list_models(&cfg).await {
                    Ok(model_list) => {
                        for model in model_list {
                            models.push(AvailableModel {
                                provider: provider_name.clone(),
                                model,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "ListAvailableModels: failed to list models for {}: {}",
                            provider_name,
                            e
                        );
                    }
                }
            }

            Ok(models)
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
        ListRemoteSessions,
        "querymt::ListRemoteSessions",
        REG_LIST_REMOTE_SESSIONS
    );
    remote_node_msg_impl!(
        DestroyRemoteSession,
        "querymt::DestroyRemoteSession",
        REG_DESTROY_REMOTE_SESSION
    );
    remote_node_msg_impl!(GetNodeInfo, "querymt::GetNodeInfo", REG_GET_NODE_INFO);
    remote_node_msg_impl!(
        ListAvailableModels,
        "querymt::ListAvailableModels",
        REG_LIST_AVAILABLE_MODELS
    );
}
