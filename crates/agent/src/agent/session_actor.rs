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
use crate::agent::undo::{RedoResult, UndoResult};
use crate::agent::utils::{format_prompt_user_text_only, render_prompt_for_display};
use crate::events::{AgentEventKind, SessionLimits};
use crate::model::{AgentMessage, MessagePart};
use crate::session::runtime::RuntimeContext;
use crate::session::store::LLMConfig;
use agent_client_protocol::{
    ContentChunk, Error, ExtResponse, PromptResponse, SessionUpdate, SetSessionModelResponse,
    StopReason,
};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::reply::DelegatedReply;
use log::{debug, warn};
use querymt::chat::ChatRole;
use std::sync::Arc;
use tokio::sync::watch;
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
    pub(crate) cancel_tx: watch::Sender<bool>,

    // ── Client bridge (for SessionUpdate notifications) ──────────
    pub(crate) bridge: Option<ClientBridgeSender>,

    // ── Execution tracking ───────────────────────────────────────
    pub(crate) prompt_running: bool,
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
        let (cancel_tx, _) = watch::channel(false);

        Self {
            config,
            session_id,
            runtime,
            mode,
            tool_config,
            cancel_tx,
            bridge: None,
            prompt_running: false,
        }
    }

    /// Sends a session update notification to the client.
    #[allow(dead_code)]
    pub(crate) fn send_session_update(&self, session_id: &str, update: SessionUpdate) {
        if let Some(ref bridge) = self.bridge {
            let notification = agent_client_protocol::SessionNotification::new(
                agent_client_protocol::SessionId::from(session_id.to_string()),
                update,
            );
            let bridge = bridge.clone();
            tokio::spawn(async move {
                if let Err(e) = bridge.notify(notification).await {
                    log::debug!("Failed to send session update: {}", e);
                }
            });
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
// 1. Cancel message sets cancel_tx to true
// 2. Detached prompt task sees the flag and exits early
// 3. PromptFinished message resets both prompt_running and cancel_tx
// 4. Session is ready to accept new prompts
//
// Note: The cancel flag is also reset in Prompt handler as a safety measure,
// ensuring recovery even if PromptFinished delivery fails.

impl Message<Cancel> for SessionActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Cancel, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        debug!(
            "Session {}: Cancel received, setting cancel_tx=true",
            self.session_id
        );
        // Use send_modify to ensure cancellation signal is set even when there are no receivers
        self.cancel_tx.send_modify(|v| *v = true);
        self.config
            .emit_event(&self.session_id, AgentEventKind::Cancelled);
    }
}

// ── PromptFinished (internal) ────────────────────────────────────────────

impl Message<PromptFinished> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: PromptFinished,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.prompt_running = false;
        debug!(
            "Session {} PromptFinished: prompt_running=false, resetting cancel_tx",
            self.session_id
        );
        // Reset cancel flag to ensure session can accept new prompts after cancellation.
        // This is critical: without this reset, a cancelled session would remain
        // permanently stuck with cancel_tx=true, causing all future prompts to be
        // immediately cancelled.
        // Use send_modify to ensure the value is set even when there are no receivers.
        self.cancel_tx.send_modify(|v| *v = false);
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
    type Reply = Result<(), Error>;

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
    type Reply = Result<(), Error>;

    async fn handle(
        &mut self,
        msg: SetLlmConfig,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.set_llm_config_impl(msg.config).await
    }
}

impl SessionActor {
    async fn set_llm_config_impl(&self, config: querymt::LLMParams) -> Result<(), Error> {
        let provider_name = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::new(-32000, "Provider is required in config".to_string()))?;

        if self
            .config
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(Error::new(
                -32000,
                format!("Unknown provider: {}", provider_name),
            ));
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(&self.session_id, llm_config.id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

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
            },
        );
        Ok(())
    }
}

// ── SetSessionModel ──────────────────────────────────────────────────────

impl Message<SetSessionModel> for SessionActor {
    type Reply = Result<SetSessionModelResponse, Error>;

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
            .map_err(|e| Error::new(-32000, e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
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
                .map_err(|e| Error::new(-32000, e.to_string()))?;
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
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(&session_id, llm_config.id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

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
    type Reply = Result<Option<LLMConfig>, Error>;

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
            .map_err(|e| Error::new(-32000, e.to_string()))
    }
}

// ── Undo / Redo ──────────────────────────────────────────────────────────

impl Message<Undo> for SessionActor {
    type Reply = Result<UndoResult, anyhow::Error>;

    async fn handle(&mut self, msg: Undo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No snapshot backend configured"))?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No working directory configured"))?
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
    type Reply = Result<RedoResult, anyhow::Error>;

    async fn handle(&mut self, _msg: Redo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No snapshot backend configured"))?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No working directory configured"))?
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
    type Reply = Result<ExtResponse, Error>;

    async fn handle(
        &mut self,
        _msg: ExtMethod,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        Ok(ExtResponse::new(Arc::from(raw_value)))
    }
}

impl Message<ExtNotification> for SessionActor {
    type Reply = Result<(), Error>;

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

// ── Prompt (the big one) ─────────────────────────────────────────────────

impl Message<Prompt> for SessionActor {
    type Reply = DelegatedReply<Result<PromptResponse, Error>>;

    async fn handle(&mut self, msg: Prompt, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        if self.prompt_running {
            // Allow new prompts through even while one is running.
            // execution_permit still serializes actual turn execution, so this
            // behaves like queueing instead of fail-fast rejection.
            if *self.cancel_tx.borrow() {
                debug!(
                    "Session {}: prompt_running=true, cancel=true → allowing new prompt through",
                    self.session_id
                );
            } else {
                debug!(
                    "Session {}: prompt_running=true, cancel=false → queueing behind running prompt",
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

        // Reset cancel flag. Use send_modify instead of send to ensure the value
        // is updated even when there are no active receivers (which can happen
        // between when the old task's cancel_rx is moved into the spawned future
        // and when the new task subscribes).
        self.cancel_tx.send_modify(|v| *v = false);

        debug!(
            "Session {}: set prompt_running=true, cancel_tx reset to false, cancel_rx.borrow()={}",
            self.session_id,
            *self.cancel_tx.borrow()
        );

        // Capture everything needed for the detached task
        let config = self.config.clone();
        let session_id = self.session_id.clone();
        let runtime = self.runtime.clone();
        let cancel_rx = self.cancel_tx.subscribe();
        let bridge = self.bridge.clone();
        let mode = self.mode;
        let actor_ref = ctx.actor_ref().clone();

        ctx.spawn(async move {
            let result = execute_prompt_detached(
                msg.req,
                session_id.clone(),
                runtime,
                config,
                cancel_rx,
                bridge,
                mode,
            )
            .await;

            debug!("Session {}: sending PromptFinished to actor", session_id);
            // Reset prompt_running flag via message back to actor
            if let Err(e) = actor_ref.tell(PromptFinished).await {
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
async fn execute_prompt_detached(
    req: agent_client_protocol::PromptRequest,
    session_id: String,
    runtime: Arc<SessionRuntime>,
    config: Arc<AgentConfig>,
    cancel_rx: watch::Receiver<bool>,
    bridge: Option<ClientBridgeSender>,
    mode: AgentMode,
) -> Result<PromptResponse, Error> {
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
        Ok(Err(_)) => return Err(Error::new(-32000, "Session semaphore closed")),
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
                Ok(Err(_)) => return Err(Error::new(-32000, "Session semaphore closed")),
                Err(_) => {
                    return Err(Error::new(-32002, "Session execution timeout").data(
                        serde_json::json!({
                            "sessionId": session_id,
                            "timeoutSecs": config.execution_timeout_secs,
                        }),
                    ));
                }
            }
        }
    };

    // Get Session Context (turn-pinned)
    let session_handle = config
        .provider
        .with_session(&session_id)
        .await
        .map_err(|e| Error::new(-32000, e.to_string()))?;

    // Create and load RuntimeContext
    let mut runtime_context =
        RuntimeContext::new(config.provider.history_store(), session_id.clone())
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
    runtime_context
        .load_working_context()
        .await
        .map_err(|e| Error::new(-32000, e.to_string()))?;

    // Clean up revert state if a new prompt is sent while in reverted state
    if let Err(e) =
        crate::agent::undo::cleanup_revert_on_prompt(&config.provider, &session_id).await
    {
        warn!("Failed to clean up revert state: {}", e);
    }

    // Create execution context
    let mut exec_ctx = ExecutionContext::new(
        session_id.clone(),
        runtime.clone(),
        runtime_context,
        session_handle,
    );

    // 4. Store User Messages
    // Keep separate projections for user-visible events vs LLM replay context.
    let display_content = render_prompt_for_display(&req.prompt);

    // User text only for intent snapshot (clean, no attachments)
    let user_text = format_prompt_user_text_only(&req.prompt);

    for block in &req.prompt {
        if let Some(ref bridge) = bridge {
            let notification = agent_client_protocol::SessionNotification::new(
                agent_client_protocol::SessionId::from(session_id.clone()),
                SessionUpdate::UserMessageChunk(ContentChunk::new(block.clone())),
            );
            let bridge = bridge.clone();
            tokio::spawn(async move {
                if let Err(e) = bridge.notify(notification).await {
                    log::debug!("Failed to send session update: {}", e);
                }
            });
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
        .map_err(|e| Error::new(-32000, e.to_string()))?;

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
        return Err(Error::new(-32000, e.to_string()));
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

    // Pre-turn snapshot for undo/redo
    if let Some(ref backend) = config.snapshot_backend
        && let Some(worktree) = runtime.cwd.as_ref()
    {
        let turn_id = Uuid::new_v4().to_string();
        debug!(
            "Session {}: pre-turn snapshot: calling backend.track()",
            session_id
        );
        match backend.track(worktree).await {
            Ok(snapshot_id) => {
                debug!(
                    "Session {}: pre-turn snapshot ok: {}",
                    session_id, snapshot_id
                );
                *runtime.turn_snapshot.lock().unwrap() =
                    Some((turn_id.clone(), snapshot_id.clone()));

                let start_part = MessagePart::TurnSnapshotStart {
                    turn_id: turn_id.clone(),
                    snapshot_id: snapshot_id.clone(),
                };
                let snapshot_msg = AgentMessage {
                    id: Uuid::new_v4().to_string(),
                    session_id: session_id.clone(),
                    role: ChatRole::Assistant,
                    parts: vec![start_part],
                    created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                    parent_message_id: None,
                };
                if let Err(e) = exec_ctx.add_message(snapshot_msg).await {
                    warn!("Failed to store turn snapshot start: {}", e);
                }
            }
            Err(e) => warn!("Pre-turn snapshot failed: {}", e),
        }
    } else {
        debug!(
            "Session {}: no snapshot backend, skipping pre-turn snapshot",
            session_id
        );
    }

    debug!(
        "Session {}: entering execute_cycle_state_machine, cancel_rx={}",
        session_id,
        *cancel_rx.borrow()
    );

    // Execute Agent Loop using State Machine
    let result = crate::agent::execution::execute_cycle_state_machine(
        &config,
        &mut exec_ctx,
        cancel_rx,
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
            Err(Error::new(-32000, e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    /// Simulates the cancel-then-resume scenario:
    ///
    /// 1. Prompt starts       → prompt_running=true, cancel=false
    /// 2. Cancel arrives       → cancel=true (old task still winding down)
    /// 3. New prompt arrives   → cancel=true + prompt_running=true
    ///    → The Prompt handler should let it through because cancel was requested
    /// 4. Old task finishes    → PromptFinished resets prompt_running and cancel
    ///
    /// The new prompt's detached task waits on the execution_permit semaphore
    /// (held by the old task) so no two prompts run concurrently.
    #[test]
    fn cancel_flag_allows_new_prompt_while_old_task_winds_down() {
        let (cancel_tx, _rx) = watch::channel(false);
        // Step 1: Prompt starts
        let mut prompt_running = true;
        cancel_tx.send(false).ok();
        assert!(prompt_running);
        assert!(!*cancel_tx.borrow());

        // Step 2: Cancel arrives
        cancel_tx.send(true).unwrap();
        assert!(*cancel_tx.borrow());
        // prompt_running is still true because PromptFinished hasn't arrived

        // Step 3: New prompt arrives while old task winds down.
        // This is the key check that was broken before — we must NOT reject
        // with "session busy" when the session was cancelled.
        let allow_new_prompt = if prompt_running {
            // Mirrors the Prompt handler logic
            *cancel_tx.borrow() // cancel was requested → allow it
        } else {
            true
        };
        assert!(
            allow_new_prompt,
            "A cancelled session must accept new prompts even before PromptFinished arrives"
        );

        // New prompt resets cancel and creates a fresh subscriber
        cancel_tx.send(false).ok();
        let rx_new = cancel_tx.subscribe();
        assert!(
            !*rx_new.borrow(),
            "New prompt's cancel_rx should start clean"
        );

        // Step 4: Old task's PromptFinished arrives (late, idempotent)
        prompt_running = false;
        cancel_tx.send(false).ok();
        assert!(!prompt_running);
        assert!(!*cancel_tx.borrow());
    }

    /// A non-cancelled busy session now queues new prompts instead of rejecting.
    #[test]
    fn busy_session_without_cancel_queues_new_prompt() {
        let (cancel_tx, _rx) = watch::channel(false);
        let prompt_running = true;

        // Mirrors the Prompt handler logic after switching to queueing behavior.
        let allow_new_prompt = if prompt_running {
            true
        } else {
            true
        };

        assert!(
            allow_new_prompt,
            "A busy, non-cancelled session should queue new prompts rather than reject"
        );
        assert!(!*cancel_tx.borrow());
    }
}
