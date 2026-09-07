//! Session registry - manages session actors.
//!
//! Lives on the server layer. Not an actor — just a plain data structure
//! protected by a mutex (acceptable: only accessed for routing, not during execution).

use crate::acp::protocol::{
    Error, ListSessionsRequest, ListSessionsResponse, McpServer, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionInfo, SessionMode,
    SessionModeState,
};
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime};
use crate::agent::remote::SessionActorRef;
#[cfg(feature = "remote")]
use crate::agent::remote::event_relay::RemoteSessionDisconnect;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::{MeshScopeId, scoped_event_relay, scoped_session};
use crate::agent::session_actor::SessionActor;
use crate::error::AgentError;
use crate::profiles::ProfileMetadata;
use kameo::actor::{ActorRef, Spawn};
#[cfg(feature = "remote")]
use querymt_remote::MeshRuntimeHandle;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "remote")]
use tokio::sync::mpsc;

#[cfg(feature = "remote")]
pub(crate) fn select_relay_scope(
    active_scopes: &[MeshScopeId],
    preferred_scope: Option<MeshScopeId>,
) -> MeshScopeId {
    preferred_scope
        .filter(|scope| active_scopes.contains(scope))
        .or_else(|| active_scopes.first().cloned())
        .unwrap_or_else(MeshScopeId::lan_default)
}

pub fn all_session_modes() -> Vec<SessionMode> {
    vec![
        SessionMode::new("build", "Build").description("Full read/write mode"),
        SessionMode::new("plan", "Plan").description("Read-only planning mode"),
        SessionMode::new("review", "Review").description("Read-only review mode"),
    ]
}

pub fn mode_state(mode: AgentMode) -> SessionModeState {
    SessionModeState::new(mode.as_str(), all_session_modes())
}

/// Build the full set of session configuration options for the given mode and reasoning effort.
///
/// This is the single source of truth for config option shape — used by session creation,
/// set_session_config_option responses, and config_option_update notifications.
pub fn config_options(
    mode: AgentMode,
    reasoning_effort: Option<querymt::chat::ReasoningEffort>,
) -> Vec<SessionConfigOption> {
    config_options_with_profiles(mode, reasoning_effort, None, &[])
}

pub fn config_options_with_profiles(
    mode: AgentMode,
    reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    current_profile_id: Option<&str>,
    profiles: &[ProfileMetadata],
) -> Vec<SessionConfigOption> {
    let effort_value = reasoning_effort
        .map(|e| e.to_string())
        .unwrap_or_else(|| "auto".to_string());
    let mut options = Vec::new();

    if !profiles.is_empty() {
        let profile_value = current_profile_id
            .map(str::to_string)
            .unwrap_or_else(|| profiles[0].id.clone());
        let profile_options: Vec<SessionConfigSelectOption> = profiles
            .iter()
            .map(|profile| {
                let mut option =
                    SessionConfigSelectOption::new(profile.id.clone(), profile.name.clone());
                if let Some(description) = profile.description.as_deref() {
                    option = option.description(description);
                }
                option
            })
            .collect();
        options.push(
            SessionConfigOption::select("profile", "Profile", profile_value, profile_options)
                .description("Controls the active profile for this session"),
        );
    }

    options.extend([
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
    ]);

    options
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

#[cfg(feature = "remote")]
#[derive(Clone)]
struct RemoteRelayRegistration {
    relay_actor_id: u64,
    relay_dht_name: String,
    registered_relay_names: Vec<String>,
    relay_ref: ActorRef<crate::agent::remote::EventRelayActor>,
    linked: bool,
    mesh: Option<crate::agent::remote::MeshHandle>,
    matched_scope: Option<MeshScopeId>,
}

/// Read-only metadata for the currently installed remote attachment.
#[cfg(feature = "remote")]
#[derive(Clone)]
pub(crate) struct RemoteAttachmentSnapshot {
    pub(crate) session_ref: SessionActorRef,
    pub(crate) attachment_id: u64,
    pub(crate) remote_actor_id: u64,
    pub(crate) node_id: Option<String>,
    pub(crate) peer_label: String,
    pub(crate) matched_scope: Option<MeshScopeId>,
}

/// Fully prepared control + relay attachment which has not yet been published
/// in the registry. Preparation performs all remote work before commit.
#[cfg(feature = "remote")]
pub(crate) struct PreparedRemoteAttachment {
    armed: bool,
    session_id: String,
    session_ref: SessionActorRef,
    relay_actor_id: u64,
    relay_dht_name: String,
    registered_relay_names: Vec<String>,
    relay_ref: ActorRef<crate::agent::remote::EventRelayActor>,
    linked: bool,
    mesh: Option<crate::agent::remote::MeshHandle>,
    matched_scope: Option<MeshScopeId>,
}

/// Complete installed attachment returned when registry ownership is removed.
/// Cleanup is deliberately asynchronous and must happen after releasing the
/// registry lock.
#[cfg(feature = "remote")]
pub(crate) struct InstalledRemoteAttachment {
    pub(crate) session_id: String,
    pub(crate) session_ref: SessionActorRef,
    pub(crate) attachment_id: u64,
    pub(crate) remote_actor_id: u64,
    relay_dht_name: String,
    registered_relay_names: Vec<String>,
    relay_ref: ActorRef<crate::agent::remote::EventRelayActor>,
    linked: bool,
    mesh: Option<crate::agent::remote::MeshHandle>,
    matched_scope: Option<MeshScopeId>,
}

#[cfg(feature = "remote")]
#[derive(Debug)]
pub(crate) struct RemoteAttachmentInstallConflict {
    pub(crate) expected_attachment_id: Option<u64>,
    pub(crate) current_attachment_id: Option<u64>,
}

#[cfg(feature = "remote")]
#[derive(Clone)]
pub(crate) struct RemoteAttachmentPrepareContext {
    pub(crate) event_sink: Arc<crate::event_sink::EventSink>,
    pub(crate) disconnect_tx: Option<mpsc::UnboundedSender<RemoteSessionDisconnect>>,
}

pub struct SessionRegistry {
    pub config: Arc<AgentConfig>,
    sessions: HashMap<String, SessionActorRef>,
    local_actor_refs: HashMap<String, ActorRef<SessionActor>>,
    /// Tracks the per-session relay actor so attach/detach can manage
    /// event forwarding and remote actor links together.
    #[cfg(feature = "remote")]
    relay_actor_ids: HashMap<String, RemoteRelayRegistration>,
    /// Mesh handle for cleaning up re-registration closures when sessions
    /// are removed (Phase 4 of Bug 1 fix).
    #[cfg(feature = "remote")]
    mesh: Option<crate::agent::remote::MeshHandle>,
    #[cfg(feature = "remote")]
    remote_disconnect_tx: Option<mpsc::UnboundedSender<RemoteSessionDisconnect>>,
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
            #[cfg(feature = "remote")]
            remote_disconnect_tx: None,
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

    #[cfg(feature = "remote")]
    pub(crate) fn set_remote_disconnect_tx(
        &mut self,
        tx: mpsc::UnboundedSender<RemoteSessionDisconnect>,
    ) {
        self.remote_disconnect_tx = Some(tx);
    }

    #[cfg(feature = "remote")]
    pub(crate) fn remote_attachment_prepare_context(&self) -> RemoteAttachmentPrepareContext {
        RemoteAttachmentPrepareContext {
            event_sink: self.config.event_sink.clone(),
            disconnect_tx: self.remote_disconnect_tx.clone(),
        }
    }

    /// Set the mesh handle so that `remove()` and `detach_remote_session()`
    /// can deregister actors from the re-registration map.
    ///
    /// When a mesh handle is provided, all existing **local** sessions in the
    /// registry are registered in the DHT so that remote peers can discover
    /// and attach to them.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&mut self, mesh: Option<crate::agent::remote::MeshHandle>) {
        // Register all existing local sessions in each active scope so remote peers can attach.
        if let Some(ref mesh) = mesh {
            let runtime = MeshRuntimeHandle::from(mesh.clone());
            let scopes = runtime.active_scopes();
            for (session_id, actor_ref) in &self.local_actor_refs {
                for scope in &scopes {
                    let dht_name = scoped_session(scope, session_id);
                    let runtime = runtime.clone();
                    let actor_ref = actor_ref.clone();
                    tokio::spawn(async move {
                        runtime.register_actor(actor_ref, dht_name).await;
                    });
                }
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

    /// Clone actor refs for loaded sessions without awaiting while the registry is locked.
    pub(crate) fn get_many<'a>(
        &self,
        session_ids: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, SessionActorRef> {
        session_ids
            .into_iter()
            .filter_map(|id| {
                self.sessions
                    .get(id)
                    .cloned()
                    .map(|actor| (id.to_string(), actor))
            })
            .collect()
    }

    /// Insert a pre-spawned session actor into the registry.
    ///
    /// Accepts anything that converts into a `SessionActorRef`, including
    /// a bare `ActorRef<SessionActor>` (via the `From` impl).
    pub fn insert(&mut self, session_id: String, actor_ref: impl Into<SessionActorRef>) {
        self.sessions.insert(session_id.clone(), actor_ref.into());
        self.local_actor_refs.remove(&session_id);
    }

    /// Register a prepared session actor into the registry (fast, map-only operation).
    ///
    /// This is the "Register" phase of the 3-phase materialization pattern.
    /// It performs ONLY in-memory HashMap operations and should complete in microseconds.
    ///
    /// This does NOT:
    /// - Query the database
    /// - Initialize MCP servers
    /// - Spawn actors (already done in Prepare phase)
    /// - Register with DHT (done in Finalize phase)
    /// - Emit events (done in Finalize phase)
    /// - Set bridge (done in Finalize phase to avoid holding lock during await)
    ///
    /// # Arguments
    ///
    /// * `prepared` - The prepared session from `SessionMaterializer::prepare_*()` methods
    ///
    /// # Returns
    ///
    /// The `SessionActorRef` for routing to the session actor.
    pub async fn register_prepared_session(
        &mut self,
        prepared: &crate::agent::session_materializer::PreparedSession,
    ) -> SessionActorRef {
        let session_ref = SessionActorRef::from(prepared.actor_ref.clone());

        // Insert into in-memory maps (microseconds)
        // This is the ONLY operation that should happen under the lock.
        // Bridge setup is deferred to finalize_session to avoid holding
        // the lock during an async actor call.
        self.sessions
            .insert(prepared.session_id.clone(), session_ref.clone());
        self.local_actor_refs
            .insert(prepared.session_id.clone(), prepared.actor_ref.clone());

        session_ref
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
            let runtime = MeshRuntimeHandle::from(mesh.clone());
            for scope in runtime.active_scopes() {
                let dht_name = scoped_session(&scope, &session_id);
                let runtime = runtime.clone();
                let actor_ref = actor_ref.clone();
                tokio::spawn(async move {
                    runtime.register_actor(actor_ref, dht_name).await;
                });
            }
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
            let runtime = MeshRuntimeHandle::from(mesh.clone());
            for scope in runtime.active_scopes() {
                let session_dht_name = scoped_session(&scope, session_id);
                runtime.deregister_actor(&session_dht_name);
            }
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

    /// Return `(session_id, peer_label, remote_node_id)` tuples for all remote sessions.
    ///
    /// Used by the UI session-list handler to include remote sessions in the
    /// session picker alongside local (persisted) sessions.
    #[cfg(feature = "remote")]
    pub fn remote_sessions(&self) -> Vec<(String, String, Option<String>)> {
        self.sessions
            .iter()
            .filter_map(|(id, r)| match r {
                SessionActorRef::Remote {
                    peer_label,
                    remote_node_id,
                    ..
                } => Some((id.clone(), peer_label.clone(), remote_node_id.clone())),
                SessionActorRef::Local(_) => None,
            })
            .collect()
    }

    /// Attachment generation (relay actor id) of the currently installed
    /// remote attachment, if any. Used by the connection coordinator for
    /// generation-aware invalidation (plan §2/§3).
    #[cfg(feature = "remote")]
    pub fn remote_attachment_id(&self, session_id: &str) -> Option<u64> {
        self.relay_actor_ids
            .get(session_id)
            .map(|registration| registration.relay_actor_id)
    }

    /// Snapshot the complete routing identity of the current attachment.
    #[cfg(feature = "remote")]
    pub(crate) fn remote_attachment(&self, session_id: &str) -> Option<RemoteAttachmentSnapshot> {
        let session_ref = self.sessions.get(session_id)?.clone();
        let registration = self.relay_actor_ids.get(session_id)?;
        let SessionActorRef::Remote {
            actor_ref,
            peer_label,
            remote_node_id,
        } = &session_ref
        else {
            return None;
        };
        let remote_actor_id = actor_ref.id().sequence_id();
        let node_id = remote_node_id.clone();
        let peer_label = peer_label.clone();
        Some(RemoteAttachmentSnapshot {
            session_ref,
            attachment_id: registration.relay_actor_id,
            remote_actor_id,
            node_id,
            peer_label,
            matched_scope: registration.matched_scope.clone(),
        })
    }

    /// Atomically install a prepared attachment if the generation observed by
    /// the preparer is still current. `None` means no attachment was observed.
    #[cfg(feature = "remote")]
    pub(crate) fn install_remote_attachment(
        &mut self,
        mut prepared: PreparedRemoteAttachment,
        expected_attachment_id: Option<u64>,
    ) -> Result<
        Option<InstalledRemoteAttachment>,
        Box<(PreparedRemoteAttachment, RemoteAttachmentInstallConflict)>,
    > {
        let current_attachment_id = self.remote_attachment_id(&prepared.session_id);
        if current_attachment_id != expected_attachment_id
            || self
                .sessions
                .get(&prepared.session_id)
                .is_some_and(|session_ref| !session_ref.is_remote())
        {
            return Err(Box::new((
                prepared,
                RemoteAttachmentInstallConflict {
                    expected_attachment_id,
                    current_attachment_id,
                },
            )));
        }

        let session_id = prepared.session_id.clone();
        let old = self.take_remote_attachment_if_current(&session_id, expected_attachment_id);
        self.sessions
            .insert(session_id.clone(), prepared.session_ref.clone());
        self.local_actor_refs.remove(&session_id);
        self.relay_actor_ids.insert(
            session_id,
            RemoteRelayRegistration {
                relay_actor_id: prepared.relay_actor_id,
                relay_dht_name: prepared.relay_dht_name.clone(),
                registered_relay_names: prepared.registered_relay_names.clone(),
                relay_ref: prepared.relay_ref.clone(),
                linked: prepared.linked,
                mesh: prepared.mesh.clone(),
                matched_scope: prepared.matched_scope.clone(),
            },
        );
        prepared.armed = false;
        Ok(old)
    }

    /// Remove an attachment only if it still matches the supplied generation.
    #[cfg(feature = "remote")]
    pub(crate) fn invalidate_remote_attachment_if_current(
        &mut self,
        session_id: &str,
        attachment_id: u64,
    ) -> Option<InstalledRemoteAttachment> {
        self.take_remote_attachment_if_current(session_id, Some(attachment_id))
    }

    /// Remove the current remote attachment from registry ownership.
    #[cfg(feature = "remote")]
    pub(crate) fn take_remote_attachment(
        &mut self,
        session_id: &str,
    ) -> Option<InstalledRemoteAttachment> {
        let attachment_id = self.remote_attachment_id(session_id)?;
        self.take_remote_attachment_if_current(session_id, Some(attachment_id))
    }

    #[cfg(feature = "remote")]
    fn take_remote_attachment_if_current(
        &mut self,
        session_id: &str,
        expected_attachment_id: Option<u64>,
    ) -> Option<InstalledRemoteAttachment> {
        if self.remote_attachment_id(session_id) != expected_attachment_id {
            return None;
        }
        let session_ref = self.sessions.get(session_id)?.clone();
        let registration = self.relay_actor_ids.remove(session_id)?;
        self.sessions.remove(session_id);
        self.local_actor_refs.remove(session_id);
        let remote_actor_id = match &session_ref {
            SessionActorRef::Remote { actor_ref, .. } => actor_ref.id().sequence_id(),
            SessionActorRef::Local(_) => 0,
        };
        Some(InstalledRemoteAttachment {
            session_id: session_id.to_string(),
            session_ref,
            attachment_id: registration.relay_actor_id,
            remote_actor_id,
            relay_dht_name: registration.relay_dht_name,
            registered_relay_names: registration.registered_relay_names,
            relay_ref: registration.relay_ref,
            linked: registration.linked,
            mesh: registration.mesh,
            matched_scope: registration.matched_scope,
        })
    }

    /// Fork an existing session at the latest message.
    pub async fn fork_session(
        &self,
        req: crate::acp::protocol::ForkSessionRequest,
    ) -> Result<crate::acp::protocol::ForkSessionResponse, Error> {
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

        Ok(crate::acp::protocol::ForkSessionResponse::new(
            new_session_id,
        ))
    }

    /// Resume an existing session without history replay.
    pub async fn resume_session(
        &mut self,
        req: crate::acp::protocol::ResumeSessionRequest,
    ) -> Result<crate::acp::protocol::ResumeSessionResponse, Error> {
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

        // If a local actor already exists for this session, reuse it instead
        // of spawning a duplicate.
        let already_materialized = self.local_actor_ref(&session_id).is_some();

        if !already_materialized {
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
        }

        let session_ref = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::internal_error().data("session actor missing after resume"))?
            .clone();
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
        let reasoning_effort = match session_ref.get_reasoning_effort().await {
            Ok(effort) => effort,
            Err(err) => {
                tracing::warn!(
                    session_id,
                    error = %err,
                    "failed to get session reasoning effort; using initial config"
                );
                self.config.provider.initial_config().reasoning_effort
            }
        };

        Ok(crate::acp::protocol::ResumeSessionResponse::new()
            .modes(mode_state(current_mode))
            .config_options(config_options(current_mode, reasoning_effort)))
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
                    crate::acp::protocol::SessionId::from(s.public_id),
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

#[cfg(feature = "remote")]
impl PreparedRemoteAttachment {
    pub(crate) fn session_ref(&self) -> &SessionActorRef {
        &self.session_ref
    }

    pub(crate) fn attachment_id(&self) -> u64 {
        self.relay_actor_id
    }
}

#[cfg(feature = "remote")]
impl Drop for PreparedRemoteAttachment {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(mesh) = &self.mesh {
            let runtime = MeshRuntimeHandle::from(mesh.clone());
            for name in &self.registered_relay_names {
                runtime.deregister_actor(name);
            }
        }
        self.relay_ref.kill();
    }
}

/// Prepare a complete remote attachment without holding the registry lock.
/// The caller must either install the returned value or abort it.
#[cfg(feature = "remote")]
pub(crate) async fn prepare_remote_attachment(
    context: RemoteAttachmentPrepareContext,
    session_id: String,
    remote_ref: kameo::actor::RemoteActorRef<SessionActor>,
    peer_label: String,
    mesh: Option<crate::agent::remote::MeshHandle>,
    preferred_scope: Option<MeshScopeId>,
    remote_node_id: Option<String>,
) -> Result<PreparedRemoteAttachment, AgentError> {
    use crate::agent::remote::EventRelayActor;

    let session_ref = SessionActorRef::Remote {
        actor_ref: remote_ref.clone(),
        peer_label: peer_label.clone(),
        remote_node_id: remote_node_id.clone(),
    };
    let relay_ref = EventRelayActor::spawn(EventRelayActor::new(
        context.event_sink,
        session_id.clone(),
        peer_label,
        remote_node_id,
        context.disconnect_tx,
    ));
    let relay_actor_id = relay_ref.id().sequence_id();
    let mut candidate = PreparedRemoteAttachment {
        armed: true,
        session_id,
        session_ref,
        relay_actor_id,
        relay_dht_name: String::new(),
        registered_relay_names: Vec::new(),
        relay_ref,
        linked: false,
        mesh,
        matched_scope: None,
    };

    if let Some(mesh) = candidate.mesh.clone() {
        let runtime = MeshRuntimeHandle::from(mesh.clone());
        let active_scopes = runtime.active_scopes();
        let selected_scope = select_relay_scope(&active_scopes, preferred_scope);
        candidate.relay_dht_name = format!(
            "{}::{}",
            scoped_event_relay(&selected_scope, &candidate.session_id, mesh.peer_id()),
            candidate.relay_actor_id
        );
        candidate.matched_scope = Some(selected_scope);
        candidate
            .registered_relay_names
            .reserve(active_scopes.len());
        for scope in &active_scopes {
            // A generation-specific name lets an old attachment be cleaned up
            // after commit without deregistering the replacement relay.
            let name = format!(
                "{}::{}",
                scoped_event_relay(scope, &candidate.session_id, mesh.peer_id()),
                candidate.relay_actor_id
            );
            candidate.registered_relay_names.push(name.clone());
            runtime
                .register_actor(candidate.relay_ref.clone(), name)
                .await;
        }
    } else {
        candidate.relay_dht_name = format!(
            "event_relay::{}::local::{}",
            candidate.session_id, candidate.relay_actor_id
        );
    }

    if let Err(error) = candidate.relay_ref.link_remote(&remote_ref).await {
        abort_prepared_remote_attachment(candidate).await;
        return Err(AgentError::RemoteActor(format!(
            "failed to link event relay to remote session: {error}"
        )));
    }
    candidate.linked = true;

    let relay_remote_ref = candidate.relay_ref.into_remote_ref().await;
    if let Err(error) = candidate
        .session_ref
        .subscribe_events_direct(
            candidate.relay_actor_id,
            relay_remote_ref,
            candidate.relay_dht_name.clone(),
        )
        .await
    {
        log::warn!(
            "remote attachment event subscription failed for {} (attachment_id={}): {}",
            candidate.session_id,
            candidate.relay_actor_id,
            error
        );
        abort_prepared_remote_attachment(candidate).await;
        return Err(AgentError::RemoteActor(format!(
            "failed to subscribe remote session events: {error}"
        )));
    }

    Ok(candidate)
}

/// Tear down a candidate which never became registry-owned.
#[cfg(feature = "remote")]
pub(crate) async fn abort_prepared_remote_attachment(mut candidate: PreparedRemoteAttachment) {
    cleanup_attachment_parts(candidate.cleanup_parts(), true).await;
    candidate.armed = false;
}

/// Clean up a removed/replaced attachment without touching registry state or
/// durable bookmark identity.
#[cfg(feature = "remote")]
pub(crate) async fn cleanup_installed_remote_attachment(
    attachment: InstalledRemoteAttachment,
    notify_remote: bool,
) {
    let matched_scope = attachment.matched_scope.clone();
    cleanup_attachment_parts(attachment.cleanup_parts(), notify_remote).await;
    log::debug!(
        "remote attachment resources released for {} (scope={:?})",
        attachment.session_id,
        matched_scope
    );
}

/// Cleanup-relevant view of an attachment, shared by prepared (never
/// installed) and installed attachments so the cleanup routine takes a single
/// borrowed argument instead of eight positional ones.
#[cfg(feature = "remote")]
struct RemoteAttachmentCleanupParts<'a> {
    session_id: &'a str,
    session_ref: &'a SessionActorRef,
    attachment_id: u64,
    relay_dht_name: &'a str,
    registered_relay_names: &'a [String],
    relay_ref: &'a ActorRef<crate::agent::remote::EventRelayActor>,
    linked: bool,
    mesh: Option<&'a crate::agent::remote::MeshHandle>,
}

#[cfg(feature = "remote")]
impl PreparedRemoteAttachment {
    fn cleanup_parts(&self) -> RemoteAttachmentCleanupParts<'_> {
        RemoteAttachmentCleanupParts {
            session_id: &self.session_id,
            session_ref: &self.session_ref,
            attachment_id: self.relay_actor_id,
            relay_dht_name: &self.relay_dht_name,
            registered_relay_names: &self.registered_relay_names,
            relay_ref: &self.relay_ref,
            linked: self.linked,
            mesh: self.mesh.as_ref(),
        }
    }
}

#[cfg(feature = "remote")]
impl InstalledRemoteAttachment {
    fn cleanup_parts(&self) -> RemoteAttachmentCleanupParts<'_> {
        RemoteAttachmentCleanupParts {
            session_id: &self.session_id,
            session_ref: &self.session_ref,
            attachment_id: self.attachment_id,
            relay_dht_name: &self.relay_dht_name,
            registered_relay_names: &self.registered_relay_names,
            relay_ref: &self.relay_ref,
            linked: self.linked,
            mesh: self.mesh.as_ref(),
        }
    }
}

#[cfg(feature = "remote")]
async fn cleanup_attachment_parts(
    parts: RemoteAttachmentCleanupParts<'_>,
    notify_remote: bool,
) {
    let RemoteAttachmentCleanupParts {
        session_id,
        session_ref,
        attachment_id,
        relay_dht_name,
        registered_relay_names,
        relay_ref,
        linked,
        mesh,
    } = parts;
    if notify_remote {
        if let Err(error) = session_ref
            .unsubscribe_events(attachment_id, relay_dht_name.to_string())
            .await
        {
            log::warn!(
                "remote attachment cleanup: unsubscribe failed for {} (attachment_id={}): {}",
                session_id,
                attachment_id,
                error
            );
        }
        if linked
            && let SessionActorRef::Remote { actor_ref, .. } = session_ref
            && let Err(error) = relay_ref.unlink_remote(actor_ref).await
        {
            log::warn!(
                "remote attachment cleanup: unlink failed for {} (attachment_id={}): {}",
                session_id,
                attachment_id,
                error
            );
        }
    }

    if let Some(mesh) = mesh {
        let runtime = MeshRuntimeHandle::from(mesh.clone());
        for name in registered_relay_names {
            runtime.deregister_actor(name);
        }
    }
    relay_ref.kill();
    let (remote_actor_id, node_id, peer_label) = match session_ref {
        SessionActorRef::Remote {
            actor_ref,
            remote_node_id,
            peer_label,
        } => (
            actor_ref.id().sequence_id(),
            remote_node_id.as_deref().unwrap_or(""),
            peer_label.as_str(),
        ),
        SessionActorRef::Local(_) => (0, "", ""),
    };
    log::info!(
        "remote attachment cleanup complete for {} (attachment_id={}, remote_actor_id={}, node_id={}, peer_label={})",
        session_id,
        attachment_id,
        remote_actor_id,
        node_id,
        peer_label,
    );
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
            let factory = Arc::new(TestProviderFactory::new(shared));
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

            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store"),
            );

            let config = Arc::new(
                AgentConfigBuilder::new(
                    Arc::new(plugin_registry),
                    storage.clone(),
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
            crate::acp::protocol::SessionConfigKind::Select(select) => select,
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
        let store: Arc<dyn SessionStore> = Arc::new(store2);
        let provider = Arc::new(crate::session::provider::SessionProvider::new(
            Arc::new(plugin_registry),
            store,
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .build(),
        );

        let registry = SessionRegistry::new(config);

        let req = crate::acp::protocol::ForkSessionRequest::new(
            crate::acp::protocol::SessionId::from("source-session".to_string()),
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
        let req_server = crate::acp::protocol::McpServerHttp::new(
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
        let req_server = crate::acp::protocol::McpServerHttp::new(
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

        let req_server = crate::acp::protocol::McpServerHttp::new(
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

    #[cfg(feature = "remote")]
    #[test]
    fn select_relay_scope_uses_preferred_scope_when_present() {
        let scopes = vec![
            MeshScopeId::lan_default(),
            MeshScopeId::Iroh {
                mesh_id: "team-a".to_string(),
            },
        ];
        let selected = select_relay_scope(
            &scopes,
            Some(MeshScopeId::Iroh {
                mesh_id: "team-a".to_string(),
            }),
        );
        assert_eq!(
            selected,
            MeshScopeId::Iroh {
                mesh_id: "team-a".to_string()
            }
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn select_relay_scope_falls_back_to_first_scope_when_preferred_missing() {
        let scopes = vec![
            MeshScopeId::lan_default(),
            MeshScopeId::Iroh {
                mesh_id: "team-a".to_string(),
            },
        ];
        let selected = select_relay_scope(
            &scopes,
            Some(MeshScopeId::Iroh {
                mesh_id: "other".to_string(),
            }),
        );
        assert_eq!(selected, MeshScopeId::lan_default());
    }

    #[cfg(feature = "remote")]
    fn install_test_registration(fixture: &mut RegistryFixture, session_id: &str) -> u64 {
        let actor_ref = fixture.spawn_actor();
        fixture
            .registry
            .sessions
            .insert(session_id.to_string(), SessionActorRef::Local(actor_ref));
        let relay_ref = crate::agent::remote::EventRelayActor::spawn(
            crate::agent::remote::EventRelayActor::new(
                fixture.registry.config.event_sink.clone(),
                session_id.to_string(),
                "peer".to_string(),
                None,
                None,
            ),
        );
        let relay_actor_id = relay_ref.id().sequence_id();
        fixture.registry.relay_actor_ids.insert(
            session_id.to_string(),
            RemoteRelayRegistration {
                relay_actor_id,
                relay_dht_name: format!("relay::{session_id}::{relay_actor_id}"),
                registered_relay_names: Vec::new(),
                relay_ref,
                linked: false,
                mesh: None,
                matched_scope: None,
            },
        );
        relay_actor_id
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn invalidate_remote_attachment_ignores_stale_generation() {
        let mut fixture = RegistryFixture::new().await;
        let current = install_test_registration(&mut fixture, "remote-session");

        let invalidated = fixture
            .registry
            .invalidate_remote_attachment_if_current("remote-session", current + 1);

        assert!(invalidated.is_none());
        assert_eq!(
            fixture.registry.remote_attachment_id("remote-session"),
            Some(current)
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn invalidate_remote_attachment_removes_matching_generation_atomically() {
        let mut fixture = RegistryFixture::new().await;
        let current = install_test_registration(&mut fixture, "remote-session");

        let invalidated = fixture
            .registry
            .invalidate_remote_attachment_if_current("remote-session", current)
            .expect("matching generation should be removed");

        assert_eq!(invalidated.attachment_id, current);
        assert!(fixture.registry.get("remote-session").is_none());
        assert!(
            fixture
                .registry
                .remote_attachment_id("remote-session")
                .is_none()
        );
        cleanup_installed_remote_attachment(invalidated, false).await;
    }
}
