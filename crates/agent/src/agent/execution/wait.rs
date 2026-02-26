//! Event waiting and delegation completion handling
//!
//! This module handles the WaitingForEvent state, where the agent pauses execution
//! to wait for external events (like delegation completions).

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::delegation::{format_delegation_completion_message, format_delegation_failure_message};
use crate::events::{AgentEventKind, StopType};
use crate::middleware::{ExecutionState, WaitCondition, WaitReason};
use crate::model::{AgentMessage, MessagePart};
use log::{debug, warn};
use querymt::chat::ChatRole;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

enum WaitEventOutcome {
    Completed {
        delegation_id: String,
        result: Option<String>,
    },
    Failed {
        delegation_id: String,
        error: String,
    },
    Cancelled {
        delegation_id: String,
    },
}

/// Handle the WaitingForEvent state transition.
///
/// This function blocks until matching events are received or execution is cancelled.
/// For delegation waits, policy controls whether we resume on first result (Any)
/// or wait for all requested delegations (All), with optional timeout cleanup.
pub(super) async fn transition_waiting_for_event(
    config: &AgentConfig,
    wait: &WaitCondition,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    event_rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "WaitingForEvent: session={}, reason={:?}",
        exec_ctx.session_id, wait.reason
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    if wait.reason != WaitReason::Delegation {
        return Ok(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        });
    }

    if config.delegation_wait_policy == crate::config::DelegationWaitPolicy::Any {
        loop {
            tokio::select! {
                _ = exec_ctx.cancellation_token.cancelled() => {
                    return Ok(ExecutionState::Cancelled);
                }
                event = event_rx.recv() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Ok(ExecutionState::Stopped {
                                message: "Event stream closed while waiting".into(),
                                stop_type: StopType::Other,
                                context: Some(context.clone()),
                            });
                        }
                    };

                    if event.session_id() != exec_ctx.session_id {
                        continue;
                    }

                    if let Some(outcome) = match_wait_event(wait, &event) {
                        let message = format_wait_outcome(&outcome);
                        let new_context = inject_wait_message(config, context, exec_ctx, message).await?;
                        return Ok(ExecutionState::BeforeLlmCall { context: new_context });
                    }
                }
            }
        }
    }

    let mut pending: HashSet<String> = wait.correlation_ids.iter().cloned().collect();
    if pending.is_empty() {
        return Ok(ExecutionState::BeforeLlmCall {
            context: context.clone(),
        });
    }

    let mut outcomes: Vec<String> = Vec::new();
    let timeout_secs = config.delegation_wait_timeout_secs;

    // Use a resettable sleep so that each delegation in the queue gets a fresh
    // timeout window.  When max_parallel_delegations < N, queued delegations
    // do not start executing until earlier ones finish; a one-shot timer would
    // consume their budget while they are still waiting on the semaphore.
    //
    // Every time a delegation completes or fails we reset the deadline to
    // "now + timeout_secs", so the clock only fires if *no progress* is made
    // within the configured window.  timeout_secs == 0 means no timeout.
    let sleep = if timeout_secs > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(timeout_secs))
    } else {
        // Zero-duration sleep that we immediately re-arm to far future so it
        // never fires.  We cannot use `std::future::pending` inside pin! with
        // the same type as a real sleep, so we use a very large deadline instead.
        tokio::time::sleep(std::time::Duration::from_secs(u64::MAX / 2))
    };
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            _ = exec_ctx.cancellation_token.cancelled() => {
                return Ok(ExecutionState::Cancelled);
            }
            _ = &mut sleep, if timeout_secs > 0 => {
                let timed_out_ids: Vec<String> = pending.iter().cloned().collect();
                cleanup_timed_out_delegations(config, exec_ctx, &timed_out_ids).await;
                outcomes.extend(timed_out_ids.iter().map(|id| {
                    format!("- {}: timed out after {}s of inactivity (cancel requested)", id, timeout_secs)
                }));

                let message = format_wait_all_summary(&outcomes);
                let new_context = inject_wait_message(config, context, exec_ctx, message).await?;
                return Ok(ExecutionState::BeforeLlmCall { context: new_context });
            }
            event = event_rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Ok(ExecutionState::Stopped {
                            message: "Event stream closed while waiting".into(),
                            stop_type: StopType::Other,
                            context: Some(context.clone()),
                        });
                    }
                };

                if event.session_id() != exec_ctx.session_id {
                    continue;
                }

                let Some(outcome) = match_wait_event(wait, &event) else {
                    continue;
                };

                let delegation_id = match &outcome {
                    WaitEventOutcome::Completed { delegation_id, .. }
                    | WaitEventOutcome::Failed { delegation_id, .. }
                    | WaitEventOutcome::Cancelled { delegation_id } => delegation_id,
                };

                if !pending.remove(delegation_id) {
                    continue;
                }

                outcomes.push(format_wait_all_item(&outcome));
                if pending.is_empty() {
                    let message = format_wait_all_summary(&outcomes);
                    let new_context = inject_wait_message(config, context, exec_ctx, message).await?;
                    return Ok(ExecutionState::BeforeLlmCall { context: new_context });
                }

                // Progress was made: reset the inactivity deadline so the next
                // queued delegation gets its own full timeout window.
                if timeout_secs > 0 {
                    sleep.as_mut().reset(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_secs(timeout_secs),
                    );
                }
            }
        }
    }
}

fn match_wait_event(
    wait: &WaitCondition,
    event: &crate::events::EventEnvelope,
) -> Option<WaitEventOutcome> {
    match wait.reason {
        WaitReason::Delegation => match event.kind() {
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                Some(WaitEventOutcome::Completed {
                    delegation_id: delegation_id.clone(),
                    result: result.clone(),
                })
            }
            AgentEventKind::DelegationFailed {
                delegation_id,
                error,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                Some(WaitEventOutcome::Failed {
                    delegation_id: delegation_id.clone(),
                    error: error.clone(),
                })
            }
            AgentEventKind::DelegationCancelled { delegation_id } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                Some(WaitEventOutcome::Cancelled {
                    delegation_id: delegation_id.clone(),
                })
            }
            _ => None,
        },
    }
}

fn format_wait_outcome(outcome: &WaitEventOutcome) -> String {
    match outcome {
        WaitEventOutcome::Completed {
            delegation_id,
            result,
        } => {
            let summary = result.as_deref().unwrap_or("No summary provided.");
            format_delegation_completion_message(delegation_id, summary)
        }
        WaitEventOutcome::Failed {
            delegation_id,
            error,
        } => format_delegation_failure_message(delegation_id, error),
        WaitEventOutcome::Cancelled { delegation_id } => {
            format!("Delegation cancelled.\n\nDelegation ID: {}", delegation_id)
        }
    }
}

fn format_wait_all_item(outcome: &WaitEventOutcome) -> String {
    match outcome {
        WaitEventOutcome::Completed {
            delegation_id,
            result,
        } => {
            let summary = result.as_deref().unwrap_or("No summary provided.");
            format!("- {}: completed ({})", delegation_id, summary)
        }
        WaitEventOutcome::Failed {
            delegation_id,
            error,
        } => format!("- {}: failed ({})", delegation_id, error),
        WaitEventOutcome::Cancelled { delegation_id } => {
            format!("- {}: cancelled", delegation_id)
        }
    }
}

fn format_wait_all_summary(items: &[String]) -> String {
    let mut msg = String::from("Delegation batch completed.\n\nResults:\n");
    for item in items {
        msg.push_str(item);
        msg.push('\n');
    }
    msg.push_str("\nPlease review outcomes and decide next actions.");
    msg
}

async fn cleanup_timed_out_delegations(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    delegation_ids: &[String],
) {
    for delegation_id in delegation_ids {
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::DelegationCancelRequested {
                delegation_id: delegation_id.clone(),
            },
        );

        if let Err(err) = exec_ctx
            .state
            .store
            .update_delegation_status(
                delegation_id,
                crate::session::domain::DelegationStatus::Cancelled,
            )
            .await
        {
            warn!(
                "Failed to mark timed out delegation '{}' as cancelled: {}",
                delegation_id, err
            );
        }

        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::DelegationCancelled {
                delegation_id: delegation_id.clone(),
            },
        );
    }
}

/// Inject a wait completion message into the conversation history.
///
/// This stores the message and returns an updated context with the message injected.
async fn inject_wait_message(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    content: String,
) -> Result<Arc<crate::middleware::ConversationContext>, anyhow::Error> {
    let agent_msg = AgentMessage {
        id: Uuid::new_v4().to_string(),
        session_id: exec_ctx.session_id.clone(),
        role: ChatRole::User,
        parts: vec![MessagePart::Text {
            content: content.clone(),
        }],
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
    };

    exec_ctx
        .add_message(agent_msg)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store wait message: {}", e))?;

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::UserMessageStored {
            content: content.clone(),
        },
    );

    Ok(Arc::new(context.inject_message(content)))
}
