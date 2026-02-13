//! Core execution logic for the agent
//!
//! This module contains the main state machine loop (`execute_cycle_state_machine`)
//! used by the kameo `SessionActor` via `ctx.spawn()`. The implementation is split
//! across focused submodules:
//!
//! - `state_machine` (this file) — Main execution loop and middleware integration
//! - `transitions` — State transition functions (BeforeLlmCall, CallLlm, AfterLlm, ProcessingToolCalls)
//! - `tool_calls` — Tool execution, permission checking, and result storage
//! - `wait` — Event waiting and delegation completion handling
//! - `llm_retry` — LLM retry logic with rate limit handling
//! - `maintenance` — Pruning and AI compaction
//! - `bridge` — Client communication helpers

mod bridge;
mod llm_retry;
mod maintenance;
mod tool_calls;
mod transitions;
mod wait;

use crate::agent::execution_context::ExecutionContext;
use crate::events::{AgentEventKind, ExecutionMetrics, StopType};
use crate::middleware::ExecutionState;
use agent_client_protocol::StopReason;
use log::{debug, info, trace, warn};
use querymt::chat::ChatRole;
use std::sync::Arc;
use tokio::sync::watch;

/// Outcome of a single execution cycle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleOutcome {
    Completed,
    Cancelled,
    Stopped(StopReason),
}

// ══════════════════════════════════════════════════════════════════════════
//  State machine implementation
// ══════════════════════════════════════════════════════════════════════════

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;

/// Execute the cycle state machine using an `AgentConfig`.
///
/// This is the entry point for the kameo `SessionActor`'s `ctx.spawn()` task.
/// It reads configuration from `AgentConfig` rather than requiring a full agent instance.
///
/// `session_mode` is the per-session `AgentMode` captured at turn start.
/// It is injected into every `ConversationContext` so middleware can read it.
pub(crate) async fn execute_cycle_state_machine(
    config: &AgentConfig,
    exec_ctx: &mut ExecutionContext,
    mut cancel_rx: watch::Receiver<bool>,
    bridge: Option<ClientBridgeSender>,
    session_mode: crate::agent::core::AgentMode,
) -> Result<CycleOutcome, anyhow::Error> {
    debug!(
        "Starting state machine execution (free fn) for session: {}",
        exec_ctx.session_id
    );

    let driver = config.create_driver();

    info!(
        "Session {}: state machine loading history, cancel_rx={}",
        exec_ctx.session_id,
        *cancel_rx.borrow()
    );

    let messages: Arc<[querymt::chat::ChatMessage]> =
        Arc::from(exec_ctx.session_handle.history().await.into_boxed_slice());

    info!(
        "Session {}: history loaded, {} messages",
        exec_ctx.session_id,
        messages.len()
    );
    let turns = messages
        .iter()
        .filter(|msg| matches!(msg.role, ChatRole::User))
        .filter(|msg| matches!(msg.message_type, querymt::chat::MessageType::Text))
        .count();
    let stats = crate::middleware::AgentStats {
        turns,
        ..Default::default()
    };
    let stats = Arc::new(stats);

    let llm_config = exec_ctx
        .llm_config()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;

    info!(
        "Session {}: llm_config provider={} model={}",
        exec_ctx.session_id, llm_config.provider, llm_config.model
    );

    let initial_context = Arc::new(
        crate::middleware::ConversationContext::new(
            exec_ctx.session_id.as_str().into(),
            messages,
            stats,
            llm_config.provider.as_str().into(),
            llm_config.model.as_str().into(),
        )
        .with_session_mode(session_mode),
    );

    let mut state = ExecutionState::BeforeLlmCall {
        context: initial_context,
    };
    let mut event_rx = config.event_bus.subscribe();

    state = driver
        .run_turn_start(state, Some(&exec_ctx.runtime))
        .await
        .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

    info!(
        "Session {}: run_turn_start done, state={}, cancel_rx={}",
        exec_ctx.session_id,
        state.name(),
        *cancel_rx.borrow()
    );

    loop {
        if *cancel_rx.borrow() {
            info!(
                "Session {}: CANCELLED at loop top (cancel_rx=true)",
                exec_ctx.session_id
            );
            return Ok(CycleOutcome::Cancelled);
        }

        let state_name = state.name();
        trace!(
            "State machine iteration: {} for session {}",
            state_name, exec_ctx.session_id
        );

        state = match state {
            ExecutionState::BeforeLlmCall { .. } => {
                let state = driver
                    .run_step_start(state, Some(&exec_ctx.runtime))
                    .await
                    .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

                match state {
                    ExecutionState::BeforeLlmCall {
                        context: ref conv_context,
                    } => {
                        transitions::transition_before_llm_call(
                            config,
                            conv_context,
                            exec_ctx,
                            &cancel_rx,
                        )
                        .await?
                    }
                    other => other,
                }
            }

            ExecutionState::CallLlm {
                ref context,
                ref tools,
            } => {
                transitions::transition_call_llm(config, context, tools, &cancel_rx, exec_ctx)
                    .await?
            }

            ExecutionState::AfterLlm { .. } => {
                let state = driver
                    .run_after_llm(state, Some(&exec_ctx.runtime))
                    .await
                    .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;
                match state {
                    ExecutionState::AfterLlm {
                        ref response,
                        ref context,
                    } => {
                        transitions::transition_after_llm(
                            config,
                            response,
                            context,
                            exec_ctx,
                            &cancel_rx,
                            bridge.as_ref(),
                        )
                        .await?
                    }
                    other => other,
                }
            }

            ExecutionState::ProcessingToolCalls { .. } => {
                let state = driver
                    .run_processing_tool_calls(state, Some(&exec_ctx.runtime))
                    .await
                    .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;
                match state {
                    ExecutionState::ProcessingToolCalls {
                        ref remaining_calls,
                        ref results,
                        ref context,
                    } => {
                        transitions::transition_processing_tool_calls(
                            config,
                            remaining_calls,
                            results,
                            context,
                            exec_ctx,
                            &cancel_rx,
                            bridge.as_ref(),
                        )
                        .await?
                    }
                    other => other,
                }
            }

            ExecutionState::WaitingForEvent {
                ref context,
                ref wait,
            } => {
                wait::transition_waiting_for_event(
                    config,
                    wait,
                    context,
                    exec_ctx,
                    &mut cancel_rx,
                    &mut event_rx,
                )
                .await?
            }

            ExecutionState::Complete => {
                let turn_end_state = driver
                    .run_turn_end(ExecutionState::Complete, Some(&exec_ctx.runtime))
                    .await
                    .map_err(|e| anyhow::anyhow!("Middleware error: {}", e))?;

                match turn_end_state {
                    ExecutionState::BeforeLlmCall { .. } => turn_end_state,
                    ExecutionState::Complete => {
                        debug!("State machine reached Complete state");

                        if config.pruning_config.enabled
                            && let Err(e) = maintenance::run_pruning(config, exec_ctx).await
                        {
                            warn!("Pruning failed: {}", e);
                        }

                        return Ok(CycleOutcome::Completed);
                    }
                    ExecutionState::Stopped {
                        ref message,
                        stop_type,
                        ..
                    } => {
                        info!("Turn-end middleware stopped: {} ({:?})", message, stop_type);
                        turn_end_state
                    }
                    other => other,
                }
            }

            ExecutionState::Stopped {
                ref message,
                stop_type,
                ..
            } => {
                info!("State machine stopped: {} ({:?})", message, stop_type);

                if stop_type == StopType::ContextThreshold && config.compaction_config.auto {
                    info!("Context threshold reached, triggering AI compaction");

                    match maintenance::run_ai_compaction(config, exec_ctx, &state).await {
                        Ok(new_state) => {
                            state = new_state;
                            continue;
                        }
                        Err(e) => {
                            warn!("AI compaction failed: {}", e);
                        }
                    }
                }

                let metrics = state
                    .context()
                    .map(|ctx| ExecutionMetrics {
                        steps: ctx.stats.steps,
                        turns: ctx.stats.turns,
                    })
                    .unwrap_or_default();

                config.emit_event(
                    &exec_ctx.session_id,
                    AgentEventKind::MiddlewareStopped {
                        stop_type,
                        reason: message.to_string(),
                        metrics,
                    },
                );

                return Ok(CycleOutcome::Stopped(StopReason::from(stop_type)));
            }

            ExecutionState::Cancelled => {
                debug!("State machine cancelled");
                return Ok(CycleOutcome::Cancelled);
            }
        };
    }
}

// ── Transitions moved to transitions.rs ──────────────────────────────────

// ── Tool execution moved to tool_calls.rs ────────────────────────────────

// ── Transition: WaitingForEvent moved to wait.rs ─────────────────────────

// ── LLM retry logic moved to llm_retry.rs ────────────────────────────────

// ── Pruning and AI compaction moved to maintenance.rs ────────────────────

// ── Session update helper moved to bridge.rs ─────────────────────────────
