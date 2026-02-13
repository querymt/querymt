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
use log::debug;
use querymt::chat::ChatRole;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// Handle the WaitingForEvent state transition.
///
/// This function blocks until a matching event is received or the execution is cancelled.
/// It monitors the event stream for events matching the wait condition, and when found,
/// injects the result as a user message and transitions back to BeforeLlmCall.
pub(super) async fn transition_waiting_for_event(
    config: &AgentConfig,
    wait: &WaitCondition,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    cancel_rx: &mut watch::Receiver<bool>,
    event_rx: &mut tokio::sync::broadcast::Receiver<crate::events::AgentEvent>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "WaitingForEvent: session={}, reason={:?}",
        exec_ctx.session_id, wait.reason
    );

    if *cancel_rx.borrow() {
        return Ok(ExecutionState::Cancelled);
    }

    loop {
        tokio::select! {
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    return Ok(ExecutionState::Cancelled);
                }
            }
            event = event_rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Ok(ExecutionState::Stopped {
                            message: "Event stream closed while waiting".into(),
                            stop_type: StopType::Other,
                            context: Some(context.clone()),
                        });
                    }
                };

                if event.session_id != exec_ctx.session_id {
                    continue;
                }

                if let Some(message) = match_wait_event(wait, &event) {
                    let new_context = inject_wait_message(
                        config, context, exec_ctx, message,
                    )
                    .await?;
                    return Ok(ExecutionState::BeforeLlmCall {
                        context: new_context,
                    });
                }
            }
        }
    }
}

/// Match an event against a wait condition and extract the result message.
///
/// Returns `Some(message)` if the event matches the wait condition, `None` otherwise.
fn match_wait_event(wait: &WaitCondition, event: &crate::events::AgentEvent) -> Option<String> {
    match wait.reason {
        WaitReason::Delegation => match &event.kind {
            AgentEventKind::DelegationCompleted {
                delegation_id,
                result,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                let summary = result.as_deref().unwrap_or("No summary provided.");
                Some(format_delegation_completion_message(delegation_id, summary))
            }
            AgentEventKind::DelegationFailed {
                delegation_id,
                error,
            } => {
                if !wait.correlation_ids.is_empty() && !wait.correlation_ids.contains(delegation_id)
                {
                    return None;
                }
                Some(format_delegation_failure_message(delegation_id, error))
            }
            _ => None,
        },
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
