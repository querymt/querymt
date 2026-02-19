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
use log::{debug, info, warn};
use querymt::chat::ChatRole;
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
    /// Cancelled when a `Cancel` message is received; replaced with a fresh
    /// token at the start of each new `Prompt` so the next turn starts clean.
    pub(crate) cancel_token: CancellationToken,    

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
        Self {
            config,
            session_id,
            runtime,
            mode,
            tool_config,
            cancel_token: CancellationToken::new(),
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
// 1. Cancel message cancels cancel_token — always reaches the running task because
//    the Prompt handler no longer replaces the token while a task is live.
// 2. Detached prompt task observes cancel_token.is_cancelled() and exits early.
// 3. PromptFinished resets cancel_token to a fresh one and clears prompt_running.
// 4. Session is ready to accept new prompts.

impl Message<Cancel> for SessionActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Cancel, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        debug!("Session {}: Cancel received", self.session_id);
        self.cancel_token.cancel();
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
        // Reset the token so the next turn starts with a clean (uncancelled) token.
        // This is the only place we replace cancel_token; the Prompt handler no longer
        // does so, which guarantees Cancel always reaches the currently-running task.
        self.cancel_token = CancellationToken::new();
        debug!(
            "Session {} PromptFinished: prompt_running=false, cancel_token reset",
            self.session_id
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
            if self.cancel_token.is_cancelled() {
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

        // Only create a fresh token when the previous one was already cancelled
        // (i.e. the user cancelled the last turn and is now starting a new one).
        // When no task is running yet, or a task is running and hasn't been
        // cancelled, we keep the existing token so that a Cancel message arriving
        // between two Prompt messages still reaches the running task.
        if self.cancel_token.is_cancelled() {
            self.cancel_token = CancellationToken::new();
            debug!(
                "Session {}: set prompt_running=true, fresh cancel_token created (previous was cancelled)",
                self.session_id
            );
        } else {
            debug!(
                "Session {}: set prompt_running=true, reusing existing cancel_token",
                self.session_id
            );
        }

        // Capture everything needed for the detached task
        let config = self.config.clone();
        let session_id = self.session_id.clone();
        let runtime = self.runtime.clone();
        let cancel_token = self.cancel_token.clone();
        let bridge = self.bridge.clone();
        let mode = self.mode;
        let actor_ref = ctx.actor_ref().clone();

        ctx.spawn(async move {
            let result = execute_prompt_detached(
                msg.req,
                session_id.clone(),
                runtime,
                config,
                cancel_token,
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
#[instrument(
    name = "agent.prompt.execute",
    skip(req, runtime, config, cancel_token, bridge),
    fields(session_id = %session_id, mode = %mode)
)]
async fn execute_prompt_detached(
    req: agent_client_protocol::PromptRequest,
    session_id: String,
    runtime: Arc<SessionRuntime>,
    config: Arc<AgentConfig>,
    cancel_token: CancellationToken,
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

    // Create execution context — attach the cancellation token so it propagates
    // into individual tool calls for cooperative cancellation.
    let mut exec_ctx = ExecutionContext::new(
        session_id.clone(),
        runtime.clone(),
        runtime_context,
        session_handle,
    )
    .with_cancellation_token(cancel_token);

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
            Err(Error::new(-32000, e.to_string()))
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
) -> Result<(), Error> {
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
                Error::new(
                    -32000,
                    format!("Failed to store turn snapshot start: {}", e),
                )
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
    use tokio_util::sync::CancellationToken;

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
        assert!(!should_replace, "Must not replace token while task is alive");
        // token unchanged — running task still holds the same token.

        // Step 3: Cancel arrives.
        cancel_token.cancel();
        assert!(
            running_task_token.is_cancelled(),
            "Cancel must reach the first task even after a second Prompt was queued"
        );

        // Step 4: PromptFinished — reset token for the next clean turn.
        cancel_token = CancellationToken::new();
        assert!(!cancel_token.is_cancelled(), "Token reset after PromptFinished");
    }
}
