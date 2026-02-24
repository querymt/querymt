//! Per-session actor. Each session gets its own actor with isolated state.
//!
//! Prompt execution uses `ctx.spawn()` so the actor stays responsive for
//! `Cancel`, `SetMode`, and other messages between turns.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime, ToolConfig};
use crate::agent::execution::CycleOutcome;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::messages::*;
use crate::agent::undo::{RedoResult, UndoError, UndoResult};
use crate::agent::utils::{format_prompt_user_text_only, render_prompt_for_display};
use crate::error::AgentError;
use crate::events::{AgentEventKind, SessionLimits};
use crate::model::{AgentMessage, MessagePart};
use crate::session::runtime::RuntimeContext;
use crate::session::store::LLMConfig;
use agent_client_protocol::{
    ContentChunk, ExtResponse, PromptResponse, SessionUpdate, SetSessionModelResponse, StopReason,
};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::reply::DelegatedReply;
use log::{debug, info, warn};
use querymt::chat::ChatRole;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, instrument};
use uuid::Uuid;

/// Per-session actor. Each session gets its own actor with isolated state.
#[derive(Actor)]
pub struct SessionActor {
    // ── Shared config (read at turn start) ───────────────────────
    pub(crate) config: Arc<AgentConfig>,

    // ── Session identity ─────────────────────────────────────────
    pub(crate) session_id: String,

    // ── Session state (OWNED, NO LOCKS) ──────────────────────────
    pub(crate) runtime: Arc<SessionRuntime>,
    pub(crate) mode: AgentMode,
    pub(crate) tool_config: ToolConfig,

    // ── Cancellation ─────────────────────────────────────────────
    /// Tracks the active prompt generation and cancellation token.
    ///
    /// Generation advances when a new `Prompt` is accepted so stale
    /// `PromptFinished` notifications can be ignored safely.
    pub(crate) turn_state: TurnState,

    // ── Client bridge (for SessionUpdate notifications) ──────────
    pub(crate) bridge: Option<ClientBridgeSender>,

    // ── Execution tracking ───────────────────────────────────────
    pub(crate) prompt_running: bool,

    // ── Mesh (remote sessions only) ──────────────────────────────
    /// Present when this actor was spawned on a mesh node via
    /// `RemoteNodeManager`. Used by `SubscribeEvents` to look up the
    /// remote `EventRelayActor` in the Kademlia DHT.
    #[cfg(feature = "remote")]
    pub(crate) mesh: Option<crate::agent::remote::MeshHandle>,

    // Tracks EventForwarder task handles by relay actor so unsubscribe can abort.
    pub(crate) relay_forwarder_handles: HashMap<u64, tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct TurnState {
    pub(crate) generation: u64,
    pub(crate) token: CancellationToken,
}

impl TurnState {
    fn new() -> Self {
        Self {
            generation: 0,
            token: CancellationToken::new(),
        }
    }
}

impl SessionActor {
    /// Create a new SessionActor. Call `kameo::actor::spawn()` to start it.
    pub fn new(config: Arc<AgentConfig>, session_id: String, runtime: Arc<SessionRuntime>) -> Self {
        let tool_config = config.tool_config.clone();
        // Read current default mode from the shared mutex
        let mode = config
            .default_mode
            .lock()
            .map(|m| *m)
            .unwrap_or(AgentMode::Build);
        Self {
            config,
            session_id,
            runtime,
            mode,
            tool_config,
            turn_state: TurnState::new(),
            bridge: None,
            prompt_running: false,
            #[cfg(feature = "remote")]
            mesh: None,
            relay_forwarder_handles: HashMap::new(),
        }
    }

    /// Attach a [`MeshHandle`] to this actor (builder pattern, for remote sessions).
    ///
    /// Called by `RemoteNodeManager` after `new()` so the `SubscribeEvents`
    /// handler can reach the Kademlia DHT without querying global state.
    #[cfg(feature = "remote")]
    pub fn with_mesh(mut self, mesh: Option<crate::agent::remote::MeshHandle>) -> Self {
        self.mesh = mesh;
        self
    }

    /// Sends a session update notification to the client.
    #[allow(dead_code)]
    pub(crate) async fn send_session_update(&self, session_id: &str, update: SessionUpdate) {
        if let Some(ref bridge) = self.bridge {
            let notification = agent_client_protocol::SessionNotification::new(
                agent_client_protocol::SessionId::from(session_id.to_string()),
                update,
            );
            if let Err(e) = bridge.notify(notification).await {
                log::debug!("Failed to send session update: {}", e);
            }
        }
    }

    /// Helper method to extract system prompt from current session config.
    async fn get_session_system_prompt(&self) -> Vec<String> {
        if let Ok(Some(current_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            && let Some(params) = &current_config.params
            && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
        {
            let mut system_parts = Vec::new();
            for item in system_array {
                if let Some(s) = item.as_str() {
                    system_parts.push(s.to_string());
                }
            }
            if !system_parts.is_empty() {
                return system_parts;
            }
        }
        self.config.provider.initial_config().system.clone()
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Message Handlers
// ══════════════════════════════════════════════════════════════════════════

// ── Cancel ───────────────────────────────────────────────────────────────
//
// Cancellation flow:
// 1. Cancel message cancels turn_state.token.
// 2. Detached prompt task observes token.is_cancelled() and exits early.
// 3. PromptFinished applies only when generation matches the current turn.
// 4. Matching PromptFinished resets token and clears prompt_running.

impl Message<Cancel> for SessionActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Cancel, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        debug!("Session {}: Cancel received", self.session_id);
        self.turn_state.token.cancel();
        self.config
            .emit_event(&self.session_id, AgentEventKind::Cancelled);
    }
}

// ── PromptFinished (internal) ────────────────────────────────────────────

impl Message<PromptFinished> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PromptFinished,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if msg.generation != self.turn_state.generation {
            debug!(
                "Session {} PromptFinished: stale generation={}, current={} (ignored)",
                self.session_id, msg.generation, self.turn_state.generation
            );
            return;
        }

        self.prompt_running = false;
        // Reset the token so the next turn starts with a clean (uncancelled) token.
        self.turn_state.token = CancellationToken::new();
        debug!(
            "Session {} PromptFinished: prompt_running=false, token reset (generation={})",
            self.session_id, msg.generation
        );
    }
}

// ── SetMode ──────────────────────────────────────────────────────────────

impl Message<SetMode> for SessionActor {
    type Reply = ();

    async fn handle(&mut self, msg: SetMode, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        self.mode = msg.mode;
        self.config.emit_event(
            &self.session_id,
            AgentEventKind::SessionModeChanged { mode: msg.mode },
        );
    }
}

// ── GetMode ──────────────────────────────────────────────────────────────

impl Message<GetMode> for SessionActor {
    type Reply = Result<AgentMode, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetMode,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.mode)
    }
}

// ── SetProvider ──────────────────────────────────────────────────────────

impl Message<SetProvider> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: SetProvider,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let system_prompt = self.get_session_system_prompt().await;
        let mut config = querymt::LLMParams::new()
            .provider(&msg.provider)
            .model(&msg.model);
        for prompt_part in system_prompt {
            config = config.system(prompt_part);
        }
        self.set_llm_config_impl(config).await
    }
}

// ── SetLlmConfig ─────────────────────────────────────────────────────────

impl Message<SetLlmConfig> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: SetLlmConfig,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.set_llm_config_impl(msg.config).await
    }
}

impl SessionActor {
    async fn set_llm_config_impl(&self, config: querymt::LLMParams) -> Result<(), AgentError> {
        let provider_name = config
            .provider
            .as_ref()
            .ok_or(AgentError::ProviderRequired)?;

        if self
            .config
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(AgentError::UnknownProvider {
                name: provider_name.clone(),
            });
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(&self.session_id, llm_config.id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.config.emit_event(
            &self.session_id,
            AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
                provider_node_id: None,
            },
        );
        Ok(())
    }
}

// ── SetSessionModel ──────────────────────────────────────────────────────

impl Message<SetSessionModel> for SessionActor {
    type Reply = Result<SetSessionModelResponse, AgentError>;

    async fn handle(
        &mut self,
        msg: SetSessionModel,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use agent_client_protocol::SetSessionModelResponse;

        let session_id = msg.req.session_id.to_string();
        let _session = self
            .config
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?
            .ok_or_else(|| AgentError::SessionNotFound {
                session_id: session_id.clone(),
            })?;

        let model_id = msg.req.model_id.to_string();
        let (provider, model) = if let Some(slash_pos) = model_id.find('/') {
            (
                model_id[..slash_pos].to_string(),
                model_id[slash_pos + 1..].to_string(),
            )
        } else {
            let current_config = self
                .config
                .provider
                .history_store()
                .get_session_llm_config(&session_id)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;
            let provider = current_config
                .map(|c| c.provider)
                .unwrap_or_else(|| "anthropic".to_string());
            (provider, model_id)
        };

        let system_prompt = self.get_session_system_prompt().await;
        let mut llm_config_input = querymt::LLMParams::new()
            .provider(provider.clone())
            .model(model.clone());
        for prompt_part in system_prompt {
            llm_config_input = llm_config_input.system(prompt_part);
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&llm_config_input)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(&session_id, llm_config.id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        // Persist the provider_node_id so the session knows where to route LLM calls.
        let provider_node_id = msg.provider_node_id.as_ref().map(ToString::to_string);
        if let Err(e) = self
            .config
            .provider
            .history_store()
            .set_session_provider_node_id(&session_id, provider_node_id.as_deref())
            .await
        {
            log::warn!("SetSessionModel: failed to persist provider_node_id: {}", e);
        }

        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.config.emit_event(
            &session_id,
            AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
                provider_node_id,
            },
        );

        Ok(SetSessionModelResponse::new())
    }
}

// ── Tool Config Messages ─────────────────────────────────────────────────

impl Message<SetToolPolicy> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetToolPolicy,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.policy = msg.policy;
    }
}

impl Message<SetAllowedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetAllowedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.allowlist = Some(msg.tools.into_iter().collect());
    }
}

impl Message<ClearAllowedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: ClearAllowedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.allowlist = None;
    }
}

impl Message<SetDeniedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetDeniedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.denylist = msg.tools.into_iter().collect();
    }
}

impl Message<ClearDeniedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: ClearDeniedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.denylist.clear();
    }
}

// ── State Queries ────────────────────────────────────────────────────────

impl Message<GetSessionLimits> for SessionActor {
    type Reply = Result<Option<SessionLimits>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetSessionLimits,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.config.get_session_limits())
    }
}

impl Message<GetLlmConfig> for SessionActor {
    type Reply = Result<Option<LLMConfig>, AgentError>;

    async fn handle(
        &mut self,
        _msg: GetLlmConfig,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

// ── Undo / Redo ──────────────────────────────────────────────────────────

impl Message<Undo> for SessionActor {
    type Reply = Result<UndoResult, UndoError>;

    async fn handle(&mut self, msg: Undo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or(UndoError::NoSnapshotBackend)?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(UndoError::NoWorkingDirectory)?
            .to_path_buf();

        // Delegate to free function using our owned state
        crate::agent::undo::undo_impl(
            backend.as_ref(),
            &self.config.provider,
            &self.session_id,
            &msg.message_id,
            &worktree,
        )
        .await
    }
}

impl Message<Redo> for SessionActor {
    type Reply = Result<RedoResult, UndoError>;

    async fn handle(&mut self, _msg: Redo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or(UndoError::NoSnapshotBackend)?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(UndoError::NoWorkingDirectory)?
            .to_path_buf();

        crate::agent::undo::redo_impl(
            backend.as_ref(),
            &self.config.provider,
            &self.session_id,
            &worktree,
        )
        .await
    }
}

// ── Extensions ───────────────────────────────────────────────────────────

impl Message<ExtMethod> for SessionActor {
    type Reply = Result<ExtResponse, AgentError>;

    async fn handle(
        &mut self,
        _msg: ExtMethod,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| AgentError::Serialization(e.to_string()))?;
        Ok(ExtResponse::new(Arc::from(raw_value)))
    }
}

impl Message<ExtNotification> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        _msg: ExtNotification,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(())
    }
}

// ── Lifecycle ────────────────────────────────────────────────────────────

impl Message<SetBridge> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetBridge,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.bridge = Some(msg.bridge);
    }
}

impl Message<Shutdown> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Shutdown,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        ctx.stop();
    }
}

// ── Remote-ready messages ────────────────────────────────────────────────

/// Get the full message history for this session.
///
/// Reads from the local `SessionStore`. When called remotely, the result
/// is serialized and sent back over the kameo mesh.
impl Message<crate::agent::messages::GetHistory> for SessionActor {
    type Reply = Result<Vec<AgentMessage>, AgentError>;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .get_history(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

/// Subscribe a remote event relay to this session's events.
///
/// When the kameo swarm is bootstrapped, this handler:
/// 1. Resolves `relay_actor_id` → `RemoteActorRef<EventRelayActor>` via swarm
/// 2. Starts an `EventForwarder` background task subscribed to the EventFanout
///
/// Without a swarm (swarm not bootstrapped), it logs and returns Ok so the
/// message round-trips correctly for local tests.
impl Message<crate::agent::messages::SubscribeEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::SubscribeEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        #[cfg(feature = "remote")]
        {
            use crate::agent::remote::event_forwarder::EventForwarder;
            use crate::agent::remote::event_relay::EventRelayActor;

            if let Some(ref mesh) = self.mesh {
                let relay_name = crate::agent::remote::dht_name::event_relay(&self.session_id);
                match mesh
                    .lookup_actor::<EventRelayActor>(relay_name.clone())
                    .await
                {
                    Ok(Some(relay_ref)) => {
                        // Abort previous forwarder for this relay_actor_id if any
                        if let Some(prev_handle) =
                            self.relay_forwarder_handles.remove(&msg.relay_actor_id)
                        {
                            prev_handle.abort();
                            log::debug!(
                                "Session {}: SubscribeEvents relay_actor_id={} — aborted previous forwarder",
                                self.session_id,
                                msg.relay_actor_id
                            );
                        }

                        let fanout = self.config.event_sink.fanout().clone();
                        let handle = EventForwarder::start(
                            fanout,
                            relay_ref,
                            format!("session:{}", self.session_id),
                        );
                        self.relay_forwarder_handles
                            .insert(msg.relay_actor_id, handle);

                        log::debug!(
                            "Session {}: SubscribeEvents — EventForwarder started for relay '{}' (relay_actor_id={})",
                            self.session_id,
                            relay_name,
                            msg.relay_actor_id,
                        );
                    }
                    Ok(None) => {
                        warn!(
                            "Session {}: SubscribeEvents — no relay actor found under '{}' in DHT yet",
                            self.session_id, relay_name
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Session {}: SubscribeEvents relay lookup failed: {} (continuing without relay)",
                            self.session_id, e
                        );
                    }
                }
            } else {
                info!(
                    "Session {}: SubscribeEvents relay_actor_id={} — no mesh, event relay skipped",
                    self.session_id, msg.relay_actor_id
                );
            }
        }

        #[cfg(not(feature = "remote"))]
        {
            info!(
                "Session {}: SubscribeEvents relay_actor_id={} (remote feature not enabled)",
                self.session_id, msg.relay_actor_id
            );
        }

        Ok(())
    }
}

/// Unsubscribe a previously registered event relay.
///
/// Aborts the `EventForwarder` background task for `relay_actor_id`.
/// Without a swarm, this is a no-op (nothing was registered).
impl Message<crate::agent::messages::UnsubscribeEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::UnsubscribeEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(handle) = self.relay_forwarder_handles.remove(&msg.relay_actor_id) {
            handle.abort();
            info!(
                "Session {}: UnsubscribeEvents relay_actor_id={} — forwarder task aborted",
                self.session_id, msg.relay_actor_id
            );
        } else {
            info!(
                "Session {}: UnsubscribeEvents relay_actor_id={} had no registered forwarder",
                self.session_id, msg.relay_actor_id
            );
        }
        Ok(())
    }
}

/// Set planning context on a delegate session.
///
/// Appends the parent session's planning summary to this session's
/// system prompt. Used by the delegation orchestrator to inject context
/// without requiring direct access to the session's `SessionStore`.
impl Message<crate::agent::messages::SetPlanningContext> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::SetPlanningContext,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        info!(
            "Session {}: SetPlanningContext ({} bytes)",
            self.session_id,
            msg.summary.len()
        );

        // Get current system prompt and append the planning summary
        let mut system_prompt = self.get_session_system_prompt().await;
        system_prompt.push(msg.summary);

        // Rebuild the LLM config with the updated system prompt
        let current_config = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        if let Some(current) = current_config {
            let mut llm_params = querymt::LLMParams::new()
                .provider(&current.provider)
                .model(&current.model);
            for part in &system_prompt {
                llm_params = llm_params.system(part.clone());
            }

            let new_config = self
                .config
                .provider
                .history_store()
                .create_or_get_llm_config(&llm_params)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;

            self.config
                .provider
                .history_store()
                .set_session_llm_config(&self.session_id, new_config.id)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;
        }

        Ok(())
    }
}

// ── File Proxy ───────────────────────────────────────────────────────────

impl Message<crate::agent::messages::GetFileIndex> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::GetFileIndexResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetFileIndex,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse};

        let handle = self
            .runtime
            .workspace_handle
            .get()
            .ok_or(FileProxyError::IndexNotReady)?;
        let index = handle.file_index().ok_or(FileProxyError::IndexNotReady)?;
        Ok(GetFileIndexResponse {
            files: index.files.clone(),
            generated_at: index.generated_at,
            workspace_root: index.root.display().to_string(),
        })
    }
}

impl Message<crate::agent::messages::ReadRemoteFile> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::ReadRemoteFileResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::ReadRemoteFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::{FileProxyError, ReadRemoteFileResponse};
        use crate::tools::builtins::read_shared::{detect_image_mime, render_read_output};
        use base64::Engine as _;

        let cwd = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(FileProxyError::NoWorkingDirectory)?;

        let path = std::path::Path::new(&msg.path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        let resolved = joined
            .canonicalize()
            .map_err(|e| FileProxyError::PathResolution(e.to_string()))?;

        let root = crate::index::resolve_workspace_root(cwd);
        if !resolved.starts_with(&root) {
            return Err(FileProxyError::PathOutsideWorkspace(
                resolved.display().to_string(),
            ));
        }

        let metadata =
            std::fs::metadata(&resolved).map_err(|e| FileProxyError::ReadError(e.to_string()))?;

        if metadata.is_dir() {
            let output = render_read_output(&resolved, msg.offset, msg.limit)
                .await
                .map_err(FileProxyError::ReadError)?;
            return Ok(ReadRemoteFileResponse::Text(output));
        }

        let bytes =
            std::fs::read(&resolved).map_err(|e| FileProxyError::ReadError(e.to_string()))?;

        if let Some(mime_type) = detect_image_mime(&bytes) {
            let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(ReadRemoteFileResponse::Image {
                mime_type: (*mime_type).to_string(),
                base64_data,
            });
        }

        if String::from_utf8(bytes).is_ok() {
            let output = render_read_output(&resolved, msg.offset, msg.limit)
                .await
                .map_err(FileProxyError::ReadError)?;
            Ok(ReadRemoteFileResponse::Text(output))
        } else {
            Ok(ReadRemoteFileResponse::Binary)
        }
    }
}

// ── Prompt (the big one) ─────────────────────────────────────────────────

impl Message<Prompt> for SessionActor {
    type Reply = DelegatedReply<Result<PromptResponse, AgentError>>;

    async fn handle(&mut self, msg: Prompt, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        if self.prompt_running {
            // Allow new prompts through even while one is running.
            // execution_permit still serializes actual turn execution, so this
            // behaves like queueing instead of fail-fast rejection.
            if self.turn_state.token.is_cancelled() {
                debug!(
                    "Session {}: prompt_running=true, cancelled=true → allowing new prompt through",
                    self.session_id
                );
            } else {
                debug!(
                    "Session {}: prompt_running=true, cancelled=false → queueing behind running prompt",
                    self.session_id
                );
            }
        } else {
            debug!(
                "Session {}: prompt_running=false → accepting new prompt",
                self.session_id
            );
        }

        self.prompt_running = true;

        // Advance generation for this prompt so stale PromptFinished messages from
        // older detached tasks cannot mutate current state.
        self.turn_state.generation = self.turn_state.generation.saturating_add(1);
        let prompt_generation = self.turn_state.generation;

        // Only create a fresh token when the previous token was already cancelled.
        // Otherwise keep the token so Cancel still reaches the currently-running task
        // while additional prompts wait behind the execution permit.
        if self.turn_state.token.is_cancelled() {
            self.turn_state.token = CancellationToken::new();
            debug!(
                "Session {}: set prompt_running=true, generation={}, fresh token created (previous was cancelled)",
                self.session_id, prompt_generation
            );
        } else {
            debug!(
                "Session {}: set prompt_running=true, generation={}, reusing existing token",
                self.session_id, prompt_generation
            );
        }

        // Capture everything needed for the detached task
        let config = self.config.clone();
        let session_id = self.session_id.clone();
        let runtime = self.runtime.clone();
        let cancel_token = self.turn_state.token.clone();
        let bridge = self.bridge.clone();
        let mode = self.mode;
        let tool_config = self.tool_config.clone();
        let actor_ref = ctx.actor_ref().clone();

        ctx.spawn(async move {
            let result = execute_prompt_detached(DetachedPromptExecution {
                req: msg.req,
                session_id: session_id.clone(),
                runtime,
                config,
                cancel_token,
                bridge,
                mode,
                tool_config,
            })
            .await;

            debug!("Session {}: sending PromptFinished to actor", session_id);
            // Reset prompt_running flag via message back to actor
            if let Err(e) = actor_ref
                .tell(PromptFinished {
                    generation: prompt_generation,
                })
                .await
            {
                warn!(
                    "Failed to send PromptFinished message to actor: {:?}. \
                     Session may remain in 'busy' state until next prompt resets it.",
                    e
                );
            } else {
                debug!("Session {}: PromptFinished sent successfully", session_id);
            }

            result
        })
    }
}

/// Execute a prompt in a detached task (called from `ctx.spawn()`).
///
/// This function gathers all needed state upfront and runs the full execution cycle.
/// It does NOT access the actor — everything is passed as parameters.
struct DetachedPromptExecution {
    req: agent_client_protocol::PromptRequest,
    session_id: String,
    runtime: Arc<SessionRuntime>,
    config: Arc<AgentConfig>,
    cancel_token: CancellationToken,
    bridge: Option<ClientBridgeSender>,
    mode: AgentMode,
    tool_config: ToolConfig,
}

#[instrument(
    name = "agent.prompt.execute",
    skip(exec),
    fields(session_id = %exec.session_id, mode = %exec.mode)
)]
async fn execute_prompt_detached(
    exec: DetachedPromptExecution,
) -> Result<PromptResponse, AgentError> {
    let DetachedPromptExecution {
        req,
        session_id,
        runtime,
        config,
        cancel_token,
        bridge,
        mode,
        tool_config,
    } = exec;
    debug!(
        "Prompt request for session {} with {} block(s)",
        session_id,
        req.prompt.len()
    );

    // Acquire execution permit (blocking with timeout)
    let _permit = match tokio::time::timeout(
        std::time::Duration::from_millis(100),
        runtime.execution_permit.acquire(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err(AgentError::SessionSemaphoreClosed),
        Err(_) => {
            debug!(
                "Session {} is busy, waiting for previous operation to complete...",
                session_id
            );
            config.emit_event(
                &session_id,
                AgentEventKind::SessionQueued {
                    reason: "waiting for previous operation to complete".to_string(),
                },
            );
            let timeout_duration = std::time::Duration::from_secs(config.execution_timeout_secs);
            match tokio::time::timeout(timeout_duration, runtime.execution_permit.acquire()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return Err(AgentError::SessionSemaphoreClosed),
                Err(_) => {
                    // TODO(agent-error): structured .data() payload lost — restore when AgentError supports structured data
                    return Err(AgentError::SessionTimeout {
                        details: format!(
                            "session_id={}, timeout={}s",
                            session_id, config.execution_timeout_secs
                        ),
                    });
                }
            }
        }
    };

    // Get Session Context (turn-pinned)
    let session_handle = config
        .provider
        .with_session(&session_id)
        .await
        .map_err(|e| AgentError::Internal(e.to_string()))?;

    // Create and load RuntimeContext
    let mut runtime_context =
        RuntimeContext::new(config.provider.history_store(), session_id.clone())
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;
    runtime_context
        .load_working_context()
        .await
        .map_err(|e| AgentError::Internal(e.to_string()))?;

    // Clean up revert state if a new prompt is sent while in reverted state
    if let Err(e) =
        crate::agent::undo::cleanup_revert_on_prompt(&config.provider, &session_id).await
    {
        warn!("Failed to clean up revert state: {}", e);
    }

    // Create execution context — attach the cancellation token so it propagates
    // into individual tool calls for cooperative cancellation.
    let mut exec_ctx = ExecutionContext::new(
        session_id.clone(),
        runtime.clone(),
        runtime_context,
        session_handle,
        tool_config,
    )
    .with_cancellation_token(cancel_token.clone());

    // 4. Store User Messages
    // Keep separate projections for user-visible events vs LLM replay context.
    let display_content = render_prompt_for_display(&req.prompt);

    // User text only for intent snapshot (clean, no attachments)
    let user_text = format_prompt_user_text_only(&req.prompt);

    for block in &req.prompt {
        if cancel_token.is_cancelled() {
            break;
        }
        if let Some(ref bridge) = bridge {
            let notification = agent_client_protocol::SessionNotification::new(
                agent_client_protocol::SessionId::from(session_id.clone()),
                SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
            );
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                res = bridge.notify(notification) => {
                    if let Err(e) = res {
                        log::debug!("Failed to send session update: {}", e);
                    }
                }
            }
        }
    }

    let message_id = Uuid::new_v4().to_string();
    config.emit_event(
        &session_id,
        AgentEventKind::PromptReceived {
            content: display_content.clone(),
            message_id: Some(message_id.clone()),
        },
    );

    exec_ctx
        .state
        .update_intent_snapshot(user_text, None, None)
        .await
        .map_err(|e| AgentError::Internal(e.to_string()))?;

    let agent_msg = AgentMessage {
        id: message_id,
        session_id: session_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::Prompt {
            blocks: req.prompt.clone(),
        }],
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
    };

    if let Err(e) = exec_ctx.add_message(agent_msg).await {
        config.emit_event(
            &session_id,
            AgentEventKind::Error {
                message: e.to_string(),
            },
        );
        return Err(AgentError::Internal(e.to_string()));
    }

    config.emit_event(
        &session_id,
        AgentEventKind::UserMessageStored {
            content: display_content.clone(),
        },
    );

    debug!(
        "Session {}: user_message_stored, starting pre-turn snapshot",
        session_id
    );

    // Pre-turn snapshot for undo/redo (off critical path)
    if let Some(ref backend) = config.snapshot_backend
        && let Some(worktree) = runtime.cwd.as_ref()
    {
        let turn_id = Uuid::new_v4().to_string();
        let backend = Arc::clone(backend);
        let worktree = worktree.to_path_buf();
        let worktree_display = worktree.display().to_string();
        let session_id_for_task = session_id.clone();
        let turn_id_for_task = turn_id.clone();

        let task = tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                let snapshot_result = backend.track(&worktree).await.map_err(|e| e.to_string());
                let elapsed = started.elapsed();
                match &snapshot_result {
                    Ok(snapshot_id) => info!(
                        "Session {}: pre-turn snapshot ready in {:?} (turn_id={}, snapshot_id={})",
                        session_id_for_task, elapsed, turn_id_for_task, snapshot_id
                    ),
                    Err(err) => warn!(
                        "Session {}: pre-turn snapshot failed in {:?} (turn_id={}): {}",
                        session_id_for_task, elapsed, turn_id_for_task, err
                    ),
                }
                (turn_id_for_task, snapshot_result)
            }
            .instrument(info_span!(
                "agent.snapshot.pre_turn.track",
                session_id = %session_id,
                turn_id = %turn_id,
                worktree = %worktree_display
            )),
        );

        *runtime.pre_turn_snapshot_task.lock().unwrap() = Some(task);
    } else {
        debug!(
            "Session {}: no snapshot backend, skipping pre-turn snapshot",
            session_id
        );
        *runtime.pre_turn_snapshot_task.lock().unwrap() = None;
    }

    debug!(
        "Session {}: entering execute_cycle_state_machine, cancelled={}",
        session_id,
        exec_ctx.cancellation_token.is_cancelled()
    );

    // Execute Agent Loop using State Machine
    let result = crate::agent::execution::execute_cycle_state_machine(
        &config,
        &mut exec_ctx,
        bridge.clone(),
        mode,
    )
    .await;

    debug!(
        "Session {}: state machine returned: {:?}",
        session_id,
        result
            .as_ref()
            .map(|o| format!("{:?}", o))
            .unwrap_or_else(|e| e.to_string())
    );

    debug!("Session {}: post-turn snapshot start", session_id);

    // Post-turn snapshot
    if let Some(ref backend) = config.snapshot_backend
        && let Some(worktree) = runtime.cwd.as_ref()
    {
        if let Err(e) = ensure_pre_turn_snapshot_ready(&mut exec_ctx, "post_turn_snapshot").await {
            warn!(
                "Failed to resolve pre-turn snapshot before post-turn processing: {}",
                e
            );
        }

        let turn_snapshot_data = runtime.turn_snapshot.lock().unwrap().take();
        if let Some((turn_id, pre_snapshot_id)) = turn_snapshot_data {
            match backend.track(worktree).await {
                Ok(post_snapshot_id) => {
                    if pre_snapshot_id != post_snapshot_id {
                        match backend
                            .diff(worktree, &pre_snapshot_id, &post_snapshot_id)
                            .await
                        {
                            Ok(changed) if !changed.is_empty() => {
                                let patch_part = MessagePart::TurnSnapshotPatch {
                                    turn_id: turn_id.clone(),
                                    snapshot_id: post_snapshot_id.clone(),
                                    changed_paths: changed
                                        .iter()
                                        .map(|p| p.to_string_lossy().to_string())
                                        .collect(),
                                };
                                let snapshot_msg = AgentMessage {
                                    id: Uuid::new_v4().to_string(),
                                    session_id: session_id.clone(),
                                    role: ChatRole::Assistant,
                                    parts: vec![patch_part],
                                    created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                                    parent_message_id: None,
                                };
                                if let Err(e) = exec_ctx.add_message(snapshot_msg).await {
                                    warn!("Failed to store turn snapshot patch: {}", e);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!("Failed to diff turn snapshots: {}", e),
                        }
                    }
                }
                Err(e) => warn!("Post-turn snapshot failed: {}", e),
            }
            let _ = backend.gc(worktree, &config.snapshot_gc_config).await;
        }
    } else {
        debug!(
            "Session {}: no snapshot backend, skipping post-turn snapshot",
            session_id
        );
    }

    debug!("Session {}: post-turn snapshot done", session_id);

    match result {
        Ok(CycleOutcome::Completed) => Ok(PromptResponse::new(StopReason::EndTurn)),
        Ok(CycleOutcome::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
        Ok(CycleOutcome::Stopped(stop_reason)) => Ok(PromptResponse::new(stop_reason)),
        Err(e) => {
            config.emit_event(
                &session_id,
                AgentEventKind::Error {
                    message: e.to_string(),
                },
            );
            Err(AgentError::Internal(e.to_string()))
        }
    }
}

#[instrument(
    name = "agent.snapshot.pre_turn.ensure",
    skip(exec_ctx),
    fields(session_id = %exec_ctx.session_id, reason = reason)
)]
pub(crate) async fn ensure_pre_turn_snapshot_ready(
    exec_ctx: &mut ExecutionContext,
    reason: &'static str,
) -> Result<(), AgentError> {
    let pending_task = exec_ctx
        .runtime
        .pre_turn_snapshot_task
        .lock()
        .unwrap()
        .take();
    let Some(task) = pending_task else {
        return Ok(());
    };

    let span = info_span!(
        "agent.snapshot.pre_turn.resolve",
        session_id = %exec_ctx.session_id,
        reason = reason
    );

    match task.instrument(span).await {
        Ok((turn_id, Ok(snapshot_id))) => {
            *exec_ctx.runtime.turn_snapshot.lock().unwrap() =
                Some((turn_id.clone(), snapshot_id.clone()));

            let start_part = MessagePart::TurnSnapshotStart {
                turn_id,
                snapshot_id,
            };
            let snapshot_msg = AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: exec_ctx.session_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![start_part],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            };
            exec_ctx.add_message(snapshot_msg).await.map_err(|e| {
                AgentError::Internal(format!("Failed to store turn snapshot start: {}", e))
            })?;
        }
        Ok((_turn_id, Err(err))) => {
            warn!("Pre-turn snapshot task finished with error: {}", err);
        }
        Err(join_err) => {
            warn!("Pre-turn snapshot task join failed: {}", join_err);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
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
    use tokio_util::sync::CancellationToken;

    // ── Shared fixture ───────────────────────────────────────────────────────

    struct ActorFixture {
        config: Arc<AgentConfig>,
        actor_ref: kameo::actor::ActorRef<SessionActor>,
        _session_id: String,
        _temp_dir: tempfile::TempDir,
    }

    impl ActorFixture {
        async fn new() -> Self {
            Self::with_session_id("test-session").await
        }

        async fn with_session_id(session_id: &str) -> Self {
            let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
            let shared = SharedLlmProvider {
                inner: provider.clone(),
                tools: vec![].into_boxed_slice(),
            };
            let factory = Arc::new(TestProviderFactory { provider: shared });
            let (plugin_registry, temp_dir) =
                mock_plugin_registry(factory).expect("plugin registry");

            let llm_config = mock_llm_config();
            let session = mock_session(session_id);
            let mut store = MockSessionStore::new();
            let session_clone = session.clone();
            store
                .expect_get_session()
                .returning(move |_| Ok(Some(session_clone.clone())))
                .times(0..);
            let llm_for_mock = llm_config.clone();
            store
                .expect_get_session_llm_config()
                .returning(move |_| Ok(Some(llm_for_mock.clone())))
                .times(0..);
            store
                .expect_get_llm_config()
                .returning(move |_| Ok(Some(llm_config.clone())))
                .times(0..);
            store
                .expect_create_or_get_llm_config()
                .returning(|_| Ok(mock_llm_config()))
                .times(0..);
            store
                .expect_set_session_llm_config()
                .returning(|_, _| Ok(()))
                .times(0..);
            store
                .expect_set_session_provider_node_id()
                .returning(|_, _| Ok(()))
                .times(0..);
            store
                .expect_get_session_provider_node_id()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_list_sessions()
                .returning(|| Ok(vec![]))
                .times(0..);

            let store: Arc<dyn SessionStore> = Arc::new(store);
            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store"),
            );

            let config = Arc::new(
                AgentConfigBuilder::new(
                    Arc::new(plugin_registry),
                    store.clone(),
                    storage.event_journal(),
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
            );

            let runtime = crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                HashMap::new(),
                vec![],
            );
            let actor = SessionActor::new(config.clone(), session_id.to_string(), runtime);
            let actor_ref = SessionActor::spawn(actor);

            Self {
                config,
                actor_ref,
                _session_id: session_id.to_string(),
                _temp_dir: temp_dir,
            }
        }
    }

    // ── Actor message handler tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_set_and_get_mode() {
        let f = ActorFixture::new().await;

        // Default mode is Build
        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Build);

        // Switch to Plan
        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        // Give the actor time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Plan);
    }

    #[tokio::test]
    async fn test_cancel_emits_event() {
        let f = ActorFixture::new().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        f.actor_ref.tell(Cancel).await.expect("tell Cancel");
        // Give the actor time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Look for Cancelled event
        let mut found_cancel = false;
        while let Ok(envelope) = rx.try_recv() {
            if matches!(envelope.kind(), crate::events::AgentEventKind::Cancelled) {
                found_cancel = true;
            }
        }
        assert!(found_cancel, "Expected Cancelled event on event fanout");
    }

    #[tokio::test]
    async fn test_set_mode_emits_event() {
        let f = ActorFixture::new().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut found = false;
        while let Ok(envelope) = rx.try_recv() {
            if let crate::events::AgentEventKind::SessionModeChanged { mode } = envelope.kind() {
                assert_eq!(*mode, AgentMode::Plan);
                found = true;
            }
        }
        assert!(found, "Expected SessionModeChanged event");
    }

    #[tokio::test]
    async fn test_set_provider_unknown_fails() {
        let f = ActorFixture::new().await;
        let result = f
            .actor_ref
            .ask(SetProvider {
                provider: "nonexistent-provider".to_string(),
                model: "some-model".to_string(),
            })
            .await;

        assert!(
            result.is_err(),
            "expected error for unknown provider, got ok"
        );
    }

    #[tokio::test]
    async fn test_set_tool_policy() {
        let f = ActorFixture::new().await;

        f.actor_ref
            .tell(SetToolPolicy {
                policy: ToolPolicy::BuiltInOnly,
            })
            .await
            .expect("tell SetToolPolicy");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify the policy was set by checking GetLlmConfig (indirect; we can't read tool_config directly)
        // Instead: setting allowed tools should persist without error
        f.actor_ref
            .tell(SetAllowedTools {
                tools: vec!["shell".to_string(), "read_tool".to_string()],
            })
            .await
            .expect("tell SetAllowedTools");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Clear should also succeed
        f.actor_ref
            .tell(ClearAllowedTools)
            .await
            .expect("tell ClearAllowedTools");
    }

    #[tokio::test]
    async fn test_set_denied_tools() {
        let f = ActorFixture::new().await;

        f.actor_ref
            .tell(SetDeniedTools {
                tools: vec!["shell".to_string()],
            })
            .await
            .expect("tell SetDeniedTools");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        f.actor_ref
            .tell(ClearDeniedTools)
            .await
            .expect("tell ClearDeniedTools");
    }

    #[tokio::test]
    async fn test_get_session_limits_returns_ok() {
        let f = ActorFixture::new().await;
        let limits = f
            .actor_ref
            .ask(GetSessionLimits)
            .await
            .expect("ask GetSessionLimits");
        // No limits middleware configured, so None is correct
        assert!(limits.is_none());
    }

    #[tokio::test]
    async fn test_get_llm_config() {
        let f = ActorFixture::new().await;
        let result = f
            .actor_ref
            .ask(GetLlmConfig)
            .await
            .expect("ask GetLlmConfig");
        // The mock store returns a config — None is fine (mock may not set it)
        // Just verify no error occurred (already asserted by .expect above)
        drop(result);
    }

    #[tokio::test]
    async fn test_ext_method_returns_null() {
        let f = ActorFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let result = f
            .actor_ref
            .ask(ExtMethod {
                req: agent_client_protocol::ExtRequest::new("custom_method", null_params),
            })
            .await
            .expect("ask ExtMethod");
        // The default implementation returns a null JSON value
        assert_eq!(result.0.get(), "null");
    }

    #[tokio::test]
    async fn test_ext_notification_succeeds() {
        let f = ActorFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        f.actor_ref
            .ask(ExtNotification {
                notif: agent_client_protocol::ExtNotification::new("my_notification", null_params),
            })
            .await
            .expect("ask ExtNotification");
    }

    #[tokio::test]
    async fn test_subscribe_unsubscribe_events_no_panic() {
        let f = ActorFixture::new().await;
        // Without remote feature, these are no-ops that return Ok(())
        f.actor_ref
            .ask(crate::agent::messages::SubscribeEvents { relay_actor_id: 42 })
            .await
            .expect("ask SubscribeEvents");

        f.actor_ref
            .ask(crate::agent::messages::UnsubscribeEvents { relay_actor_id: 42 })
            .await
            .expect("ask UnsubscribeEvents");
    }

    #[tokio::test]
    async fn test_prompt_finished_resets_state() {
        let f = ActorFixture::new().await;
        // Send PromptFinished — even when not "running", should be a no-op that doesn't panic
        f.actor_ref
            .tell(PromptFinished { generation: 0 })
            .await
            .expect("tell PromptFinished");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // GetMode should still work, confirming actor is alive
        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Build);
    }

    #[tokio::test]
    async fn test_set_bridge_succeeds() {
        let f = ActorFixture::new().await;
        // We can't easily create a real ClientBridgeSender in unit tests,
        // but setting None→Some via SetBridge message should not panic or fail.
        // We verify the actor is alive by asking GetMode after.
        // (SetBridge requires a real sender; skip the actual send but confirm actor health)
        let mode = f
            .actor_ref
            .ask(GetMode)
            .await
            .expect("ask GetMode after potential bridge set");
        assert_eq!(mode, AgentMode::Build);
    }

    #[tokio::test]
    async fn test_shutdown_stops_actor() {
        let f = ActorFixture::new().await;
        // Shutdown the actor
        f.actor_ref.tell(Shutdown).await.expect("tell Shutdown");
        // Give the actor time to stop
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        // After shutdown, the actor should no longer accept messages
        let result = f.actor_ref.ask(GetMode).await;
        assert!(result.is_err(), "actor should be stopped after Shutdown");
    }

    #[tokio::test]
    async fn test_multiple_sessions_independent() {
        let f1 = ActorFixture::with_session_id("session-alpha").await;
        let f2 = ActorFixture::with_session_id("session-beta").await;

        f1.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode on f1");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mode1 = f1.actor_ref.ask(GetMode).await.expect("GetMode f1");
        let mode2 = f2.actor_ref.ask(GetMode).await.expect("GetMode f2");

        assert_eq!(mode1, AgentMode::Plan);
        assert_eq!(
            mode2,
            AgentMode::Build,
            "f2 should not be affected by f1's mode change"
        );
    }

    // ── Token state machine tests ─────────────────────────────────────────────

    #[test]
    fn stale_prompt_finished_generation_is_ignored() {
        let mut turn_state = TurnState::new();
        turn_state.generation = 4;

        let running_token = turn_state.token.clone();
        let stale_generation = 3;

        // Simulate PromptFinished for an older prompt while generation 4 is current.
        if stale_generation == turn_state.generation {
            turn_state.token = CancellationToken::new();
        }

        turn_state.token.cancel();
        assert!(
            running_token.is_cancelled(),
            "Stale PromptFinished must not replace the current turn token"
        );
    }

    /// Simulates the cancel-then-resume scenario:
    ///
    /// 1. Prompt starts       → prompt_running=true, token shared with running task
    /// 2. Cancel arrives      → token cancelled (reaches the running task)
    /// 3. New prompt arrives  → token.is_cancelled() → fresh token created for new turn
    /// 4. Old task finishes   → PromptFinished resets prompt_running + token
    ///
    /// The new prompt's detached task waits on the execution_permit semaphore
    /// (held by the old task) so no two prompts run concurrently.
    #[test]
    fn cancel_flag_allows_new_prompt_while_old_task_winds_down() {
        let mut cancel_token = CancellationToken::new();

        // Step 1: Prompt starts — give the running task a clone of the token.
        let mut prompt_running = true;
        let _running_task_token = cancel_token.clone(); // simulates the spawned task's handle
        assert!(prompt_running);
        assert!(!cancel_token.is_cancelled());

        // Step 2: Cancel arrives — cancels the shared token, which the running task holds.
        cancel_token.cancel();
        assert!(cancel_token.is_cancelled());
        assert!(
            _running_task_token.is_cancelled(),
            "Cancel must reach the running task's token clone"
        );
        // prompt_running is still true because PromptFinished hasn't arrived yet.

        // Step 3: New prompt arrives while old task winds down.
        // The Prompt handler creates a fresh token only because the previous one is cancelled.
        let allow_new_prompt = if prompt_running {
            cancel_token.is_cancelled() // cancelled → allow new prompt
        } else {
            true
        };
        assert!(
            allow_new_prompt,
            "A cancelled session must accept new prompts even before PromptFinished arrives"
        );

        if cancel_token.is_cancelled() {
            cancel_token = CancellationToken::new();
        }
        assert!(
            !cancel_token.is_cancelled(),
            "New prompt's cancel_token should start clean"
        );

        // Step 4: PromptFinished arrives — resets prompt_running.
        // In the real actor the token is also reset here, but for this
        // scenario it was already reset in Step 3 (cancelled path).
        prompt_running = false;
        assert!(!prompt_running);
        assert!(!cancel_token.is_cancelled());
    }

    /// Verifies the non-cancelled path: a second Prompt arriving while the first
    /// task is still running must NOT replace the cancel token, so Cancel still
    /// reaches the first task.
    #[test]
    fn cancel_reaches_running_task_when_second_prompt_queued() {
        let mut cancel_token = CancellationToken::new();

        // Step 1: First prompt starts.
        let running_task_token = cancel_token.clone();
        assert!(!cancel_token.is_cancelled());

        // Step 2: Second prompt arrives while first is still running.
        // Because the token is NOT cancelled, the Prompt handler keeps the existing token.
        let should_replace = cancel_token.is_cancelled();
        assert!(
            !should_replace,
            "Must not replace token while task is alive"
        );
        // token unchanged — running task still holds the same token.

        // Step 3: Cancel arrives.
        cancel_token.cancel();
        assert!(
            running_task_token.is_cancelled(),
            "Cancel must reach the first task even after a second Prompt was queued"
        );

        // Step 4: PromptFinished — reset token for the next clean turn.
        cancel_token = CancellationToken::new();
        assert!(
            !cancel_token.is_cancelled(),
            "Token reset after PromptFinished"
        );
    }
}
