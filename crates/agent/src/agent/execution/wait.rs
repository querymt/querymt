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

pub(crate) struct DelegationRecovery {
    outcomes: futures_util::future::BoxFuture<'static, Vec<WaitEventOutcome>>,
    ids: HashSet<String>,
    dirty: bool,
}

impl DelegationRecovery {
    fn new(config: &AgentConfig, session_id: &str, ids: Vec<String>) -> Self {
        let journal = config.event_sink.journal().clone();
        let session_id = session_id.to_string();
        let covered_ids = ids.iter().cloned().collect();
        Self {
            ids: covered_ids,
            dirty: false,
            outcomes: Box::pin(async move {
                let mut delay = std::time::Duration::from_millis(100);
                loop {
                    match durable_wait_outcomes(journal.as_ref(), &session_id, &ids).await {
                        Ok(outcomes) => return outcomes,
                        Err(error) => warn!(
                            "Delegation terminal recovery unavailable for {session_id}: {error}"
                        ),
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                }
            }),
        }
    }
}

async fn recovery_outcomes(recovery: &mut Option<DelegationRecovery>) -> Vec<WaitEventOutcome> {
    match recovery {
        Some(recovery) => recovery.outcomes.as_mut().await,
        None => std::future::pending().await,
    }
}

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
async fn wait_for_pending_steering(exec_ctx: &ExecutionContext) {
    let Some(inbox) = &exec_ctx.steering else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if inbox.pending_count().await > 0 {
            return;
        }
        inbox.notified().await;
        if inbox.pending_count().await > 0 {
            return;
        }
    }
}

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
        let mut recovery = Some(DelegationRecovery::new(
            config,
            &exec_ctx.session_id,
            wait.correlation_ids.clone(),
        ));
        loop {
            tokio::select! {
                biased;
                _ = exec_ctx.cancellation_token.cancelled() => {
                    return Ok(ExecutionState::Cancelled);
                }
                _ = wait_for_pending_steering(exec_ctx) => {
                    exec_ctx
                        .suspended_delegations
                        .lock()
                        .await
                        .extend(wait.correlation_ids.iter().cloned());
                    let pending = wait.correlation_ids.join(", ");
                    let message = format!(
                        "New user steering was received. Delegations still running and awaiting later reconciliation: {pending}"
                    );
                    let new_context = inject_wait_message(config, context, exec_ctx, message).await?;
                    return Ok(ExecutionState::BeforeLlmCall { context: new_context });
                }
                recovered = recovery_outcomes(&mut recovery) => {
                    let dirty = recovery.take().is_some_and(|read| read.dirty);
                    if dirty {
                        recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, wait.correlation_ids.clone()));
                    }
                    if let Some(outcome) = recovered.into_iter().next() {
                        let new_context = inject_wait_message(config, context, exec_ctx, format_wait_outcome(&outcome)).await?;
                        return Ok(ExecutionState::BeforeLlmCall { context: new_context });
                    }
                }
                event = event_rx.recv() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Some(read) = &mut recovery {
                                read.dirty = true;
                            } else {
                                recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, wait.correlation_ids.clone()));
                            }
                            continue;
                        },
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
    let mut recovery = Some(DelegationRecovery::new(
        config,
        &exec_ctx.session_id,
        pending.iter().cloned().collect(),
    ));

    loop {
        tokio::select! {
            biased;
            _ = exec_ctx.cancellation_token.cancelled() => {
                return Ok(ExecutionState::Cancelled);
            }
            _ = wait_for_pending_steering(exec_ctx) => {
                exec_ctx
                    .suspended_delegations
                    .lock()
                    .await
                    .extend(pending.iter().cloned());
                let message = format!(
                    "New user steering was received. Delegations still running and awaiting later reconciliation: {}",
                    pending.iter().cloned().collect::<Vec<_>>().join(", ")
                );
                let new_context = inject_wait_message(config, context, exec_ctx, message).await?;
                return Ok(ExecutionState::BeforeLlmCall { context: new_context });
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
            recovered = recovery_outcomes(&mut recovery) => {
                let dirty = recovery.take().is_some_and(|read| read.dirty);
                let mut progressed = false;
                for outcome in recovered {
                    if pending.remove(outcome_id(&outcome)) {
                        outcomes.push(format_wait_all_item(&outcome));
                        progressed = true;
                    }
                }
                if pending.is_empty() {
                    let new_context = inject_wait_message(config, context, exec_ctx, format_wait_all_summary(&outcomes)).await?;
                    return Ok(ExecutionState::BeforeLlmCall { context: new_context });
                }
                if progressed && timeout_secs > 0 {
                    sleep.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs));
                }
                if dirty {
                    recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, pending.iter().cloned().collect()));
                }
            }
            event = event_rx.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(read) = &mut recovery {
                            read.dirty = true;
                        } else {
                            recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, pending.iter().cloned().collect()));
                        }
                        continue;
                    },
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

pub(super) async fn reconcile_suspended_delegations(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    event_rx: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
) -> Result<Arc<crate::middleware::ConversationContext>, anyhow::Error> {
    let mut context = context.clone();
    if exec_ctx.suspended_delegations.lock().await.is_empty() {
        *exec_ctx.suspended_recovery.lock().await = None;
        return Ok(context);
    }
    let mut lagged = false;
    loop {
        let event = match event_rx.try_recv() {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                lagged = true;
                continue;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        };
        if event.session_id() != exec_ctx.session_id {
            continue;
        }
        let suspended = exec_ctx.suspended_delegations.lock().await.clone();
        if suspended.is_empty() {
            break;
        }
        let wait = WaitCondition {
            reason: WaitReason::Delegation,
            correlation_ids: suspended.iter().cloned().collect(),
        };
        let Some(outcome) = match_wait_event(&wait, &event) else {
            continue;
        };
        let delegation_id = match &outcome {
            WaitEventOutcome::Completed { delegation_id, .. }
            | WaitEventOutcome::Failed { delegation_id, .. }
            | WaitEventOutcome::Cancelled { delegation_id } => delegation_id,
        };
        exec_ctx
            .suspended_delegations
            .lock()
            .await
            .remove(delegation_id);
        context =
            inject_wait_message(config, &context, exec_ctx, format_wait_outcome(&outcome)).await?;
    }
    recover_suspended(config, context, exec_ctx, lagged).await
}

async fn recover_suspended(
    config: &AgentConfig,
    mut context: Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    lagged: bool,
) -> Result<Arc<crate::middleware::ConversationContext>, anyhow::Error> {
    let ids: Vec<_> = exec_ctx
        .suspended_delegations
        .lock()
        .await
        .iter()
        .cloned()
        .collect();
    let mut recovery = exec_ctx.suspended_recovery.lock().await;
    if ids.is_empty() {
        *recovery = None;
        return Ok(context);
    }
    if let Some(read) = recovery.as_mut() {
        read.dirty |= lagged || ids.iter().any(|id| !read.ids.contains(id));
    } else {
        *recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, ids));
    }
    // Keep the captured read, but remember newer lag/IDs need one follow-up.
    // Both reads share a bounded BeforeModel budget, including across boundaries.
    let budget = tokio::time::sleep(std::time::Duration::from_millis(50));
    tokio::pin!(budget);
    loop {
        let outcomes = tokio::select! {
            biased;
            _ = exec_ctx.cancellation_token.cancelled() => return Ok(context),
            _ = &mut budget => return Ok(context),
            outcomes = recovery_outcomes(&mut recovery) => outcomes,
        };
        let dirty = recovery.take().is_some_and(|read| read.dirty);
        for outcome in outcomes {
            if exec_ctx
                .suspended_delegations
                .lock()
                .await
                .remove(outcome_id(&outcome))
            {
                context =
                    inject_wait_message(config, &context, exec_ctx, format_wait_outcome(&outcome))
                        .await?;
            }
        }
        if !dirty {
            return Ok(context);
        }
        let ids = exec_ctx
            .suspended_delegations
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(context);
        }
        *recovery = Some(DelegationRecovery::new(config, &exec_ctx.session_id, ids));
    }
}

fn outcome_id(outcome: &WaitEventOutcome) -> &str {
    match outcome {
        WaitEventOutcome::Completed { delegation_id, .. }
        | WaitEventOutcome::Failed { delegation_id, .. }
        | WaitEventOutcome::Cancelled { delegation_id } => delegation_id,
    }
}

async fn durable_wait_outcomes(
    journal: &dyn crate::session::projection::EventJournal,
    session_id: &str,
    ids: &[String],
) -> Result<Vec<WaitEventOutcome>, anyhow::Error> {
    let wait = WaitCondition {
        reason: WaitReason::Delegation,
        correlation_ids: ids.to_vec(),
    };
    let events = journal.load_delegation_terminals(session_id, ids).await?;
    let mut seen = HashSet::new();
    Ok(events
        .into_iter()
        .filter_map(|event| {
            let outcome = match_wait_event(&wait, &crate::events::EventEnvelope::Durable(event))?;
            seen.insert(outcome_id(&outcome).to_string())
                .then_some(outcome)
        })
        .collect())
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

pub(crate) async fn cleanup_timed_out_delegations(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    delegation_ids: &[String],
) {
    for delegation_id in delegation_ids {
        let transitioned = match exec_ctx
            .state
            .store
            .transition_delegation_status(
                delegation_id,
                crate::session::domain::DelegationStatus::Running,
                crate::session::domain::DelegationStatus::Cancelled,
            )
            .await
        {
            Ok(true) => true,
            Ok(false) => exec_ctx
                .state
                .store
                .transition_delegation_status(
                    delegation_id,
                    crate::session::domain::DelegationStatus::Requested,
                    crate::session::domain::DelegationStatus::Cancelled,
                )
                .await
                .unwrap_or_else(|err| {
                    warn!(
                        "Failed to mark requested delegation '{}' as cancelled: {}",
                        delegation_id, err
                    );
                    false
                }),
            Err(err) => {
                warn!(
                    "Failed to mark timed out delegation '{}' as cancelled: {}",
                    delegation_id, err
                );
                false
            }
        };
        if !transitioned {
            continue;
        }

        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::DelegationCancelRequested {
                delegation_id: delegation_id.clone(),
            },
        );
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
        source_provider: None,
        source_model: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{backend::StorageBackend, sqlite_storage::SqliteStorage};

    async fn fixture() -> (
        AgentConfig,
        ExecutionContext,
        Arc<crate::middleware::ConversationContext>,
        tempfile::TempDir,
    ) {
        let (plugins, dir) = crate::test_utils::empty_plugin_registry().unwrap();
        let storage = Arc::new(
            SqliteStorage::connect(dir.path().join("wait.db"))
                .await
                .unwrap(),
        );
        let store = storage.session_store();
        let session = store.create_session(None, None, None, None).await.unwrap();
        let config = crate::agent::agent_config_builder::AgentConfigBuilder::new(
            Arc::new(plugins),
            storage,
            querymt::LLMParams::new()
                .provider("mock")
                .model("mock-model"),
        )
        .build();
        let handle =
            crate::session::provider::SessionHandle::new(config.provider.clone(), session.clone())
                .await
                .unwrap();
        let state = crate::session::runtime::RuntimeContext::new(store, session.public_id.clone())
            .await
            .unwrap();
        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            Default::default(),
            crate::agent::core::McpToolState::empty(),
        );
        let exec = ExecutionContext::new(
            session.public_id.clone(),
            runtime,
            state,
            handle,
            config.tool_config.clone(),
        );
        let context = Arc::new(crate::middleware::ConversationContext::new(
            Arc::from(session.public_id.as_str()),
            Arc::from([]),
            Arc::new(crate::middleware::AgentStats::default()),
            Arc::from("mock"),
            Arc::from("mock-model"),
        ));
        (config, exec, context, dir)
    }

    #[tokio::test]
    async fn delegation_wait_recovers_terminal_after_broadcast_lag_or_late_entry() {
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            for late_entry in [false, true] {
                let (mut config, exec, context, _dir) = fixture().await;
                config.delegation_wait_policy = policy.clone();
                let mut rx = config.subscribe_events();
                config
                    .emit_event_persisted(
                        &exec.session_id,
                        AgentEventKind::DelegationCompleted {
                            delegation_id: "finished-child".into(),
                            result: Some("result survives lost wakeup".into()),
                        },
                    )
                    .await
                    .unwrap();
                if late_entry {
                    rx = config.subscribe_events();
                } else {
                    for _ in 0..2048 {
                        config.event_sink.emit_ephemeral(
                            &exec.session_id,
                            AgentEventKind::AssistantContentDelta {
                                message_id: "noise".into(),
                                content: "x".into(),
                            },
                        );
                    }
                    config
                        .event_sink
                        .session_stream(&exec.session_id)
                        .flush()
                        .await;
                    assert!(matches!(
                        rx.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
                    ));
                }
                let wait = WaitCondition {
                    reason: WaitReason::Delegation,
                    correlation_ids: vec!["finished-child".into()],
                };
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx),
                )
                .await;
                assert!(
                    matches!(result, Ok(Ok(ExecutionState::BeforeLlmCall { .. }))),
                    "terminal outcome lost for {policy:?}, late_entry={late_entry}"
                );
            }
        }
    }

    #[tokio::test]
    async fn delegation_wait_recovers_lag_after_wait_entry() {
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            let (mut config, exec, context, _dir) = fixture().await;
            config.delegation_wait_policy = policy;
            let mut rx = config.subscribe_events();
            let mut lag_witness = config.subscribe_events();
            let wait = WaitCondition {
                reason: WaitReason::Delegation,
                correlation_ids: vec!["mid-wait-child".into()],
            };
            let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
            tokio::pin!(waiting);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiting)
                    .await
                    .is_err()
            );
            config
                .emit_event_persisted(
                    &exec.session_id,
                    AgentEventKind::DelegationCompleted {
                        delegation_id: "mid-wait-child".into(),
                        result: Some("durable mid-wait completion".into()),
                    },
                )
                .await
                .unwrap();
            for _ in 0..2200 {
                config.event_sink.emit_ephemeral(
                    &exec.session_id,
                    AgentEventKind::AssistantContentDelta {
                        message_id: "noise".into(),
                        content: "x".into(),
                    },
                );
            }
            config
                .event_sink
                .session_stream(&exec.session_id)
                .flush()
                .await;
            assert!(matches!(
                lag_witness.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
            ));
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting).await,
                Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
            ));
        }
    }

    struct TerminalReadGate {
        inner: Arc<dyn crate::session::projection::EventJournal>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        reads: std::sync::atomic::AtomicUsize,
        failures: std::sync::atomic::AtomicUsize,
        block_after: usize,
        blocking: std::sync::atomic::AtomicBool,
        snapshot_before_wait: std::sync::atomic::AtomicBool,
    }

    impl TerminalReadGate {
        fn new(
            inner: Arc<dyn crate::session::projection::EventJournal>,
            block_after: usize,
        ) -> Self {
            Self {
                inner,
                entered: Default::default(),
                release: Default::default(),
                reads: Default::default(),
                failures: Default::default(),
                block_after,
                blocking: std::sync::atomic::AtomicBool::new(true),
                snapshot_before_wait: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::session::projection::EventJournal for TerminalReadGate {
        async fn load_delegation_terminals(
            &self,
            session: &str,
            ids: &[String],
        ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
            use std::sync::atomic::Ordering;
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            let snapshot = if self.snapshot_before_wait.load(Ordering::SeqCst) {
                Some(self.inner.load_delegation_terminals(session, ids).await?)
            } else {
                None
            };
            self.entered.notify_one();
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(crate::session::error::SessionError::DatabaseError(
                    "transient terminal read".into(),
                ));
            }
            if read >= self.block_after && self.blocking.load(Ordering::SeqCst) {
                self.release.notified().await;
            }
            match snapshot {
                Some(snapshot) => Ok(snapshot),
                None => self.inner.load_delegation_terminals(session, ids).await,
            }
        }
        fn max_session_seq_blocking(&self, session: &str) -> i64 {
            self.inner.max_session_seq_blocking(session)
        }
        async fn append_durable(
            &self,
            event: &crate::session::projection::NewDurableEvent,
        ) -> crate::session::error::SessionResult<crate::events::DurableEvent> {
            self.inner.append_durable(event).await
        }
        async fn load_session_stream(
            &self,
            session: &str,
            after: Option<i64>,
            limit: Option<usize>,
        ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
            self.inner.load_session_stream(session, after, limit).await
        }
        async fn load_global_stream(
            &self,
            after: Option<i64>,
            limit: Option<usize>,
        ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
            self.inner.load_global_stream(after, limit).await
        }
        async fn delete_session_events_from(
            &self,
            session: &str,
            from: i64,
        ) -> crate::session::error::SessionResult<usize> {
            self.inner.delete_session_events_from(session, from).await
        }
        async fn append_durable_from_source(
            &self,
            event: &crate::session::projection::NewDurableEvent,
        ) -> crate::session::error::SessionResult<Option<crate::events::DurableEvent>> {
            self.inner.append_durable_from_source(event).await
        }
        async fn latest_source_seq(
            &self,
            session: &str,
            node: &str,
        ) -> crate::session::error::SessionResult<Option<i64>> {
            self.inner.latest_source_seq(session, node).await
        }
        async fn remote_sync_cursor(
            &self,
            session: &str,
            node: &str,
        ) -> crate::session::error::SessionResult<Option<i64>> {
            self.inner.remote_sync_cursor(session, node).await
        }
        async fn advance_remote_sync_cursor(
            &self,
            session: &str,
            node: &str,
            seq: i64,
            complete: bool,
        ) -> crate::session::error::SessionResult<()> {
            self.inner
                .advance_remote_sync_cursor(session, node, seq, complete)
                .await
        }
        async fn load_remote_session_stream(
            &self,
            session: &str,
            node: &str,
        ) -> crate::session::error::SessionResult<Vec<crate::events::DurableEvent>> {
            self.inner.load_remote_session_stream(session, node).await
        }
        async fn max_stream_seq(&self, session: &str) -> crate::session::error::SessionResult<i64> {
            self.inner.max_stream_seq(session).await
        }
    }

    #[tokio::test]
    async fn delayed_durable_recovery_preserves_cancellation_and_inactivity_timeout() {
        for cancel in [true, false] {
            let (mut config, exec, context, _dir) = fixture().await;
            config.delegation_wait_policy = crate::config::DelegationWaitPolicy::All;
            config.delegation_wait_timeout_secs = 1;
            let gate = Arc::new(TerminalReadGate::new(
                config.event_sink.journal().clone(),
                0,
            ));
            config.event_sink = Arc::new(crate::event_sink::EventSink::new(
                gate.clone(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let mut rx = config.subscribe_events();
            let wait = WaitCondition {
                reason: WaitReason::Delegation,
                correlation_ids: vec!["stalled-child".into()],
            };
            let token = exec.cancellation_token.clone();
            let trigger_gate = gate.clone();
            let trigger = tokio::spawn(async move {
                trigger_gate.entered.notified().await;
                if cancel {
                    token.cancel();
                }
            });
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(1500),
                transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx),
            )
            .await;
            gate.release.notify_waiters();
            trigger.abort();
            if cancel {
                assert!(matches!(result, Ok(Ok(ExecutionState::Cancelled))));
            } else {
                assert!(
                    matches!(result, Ok(Ok(ExecutionState::BeforeLlmCall { .. }))),
                    "terminal read must not defeat existing inactivity timeout"
                );
            }
        }
    }

    fn gated_sink(config: &mut AgentConfig, block_after: usize) -> Arc<TerminalReadGate> {
        let gate = Arc::new(TerminalReadGate::new(
            config.event_sink.journal().clone(),
            block_after,
        ));
        config.event_sink = Arc::new(crate::event_sink::EventSink::new(
            gate.clone(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        gate
    }

    #[tokio::test]
    async fn live_terminal_is_consumed_during_retained_recovery_entry_and_lag() {
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            for lag in [false, true] {
                let (mut config, exec, context, _dir) = fixture().await;
                config.delegation_wait_policy = policy.clone();
                let gate = gated_sink(&mut config, usize::from(lag));
                let mut rx = config.subscribe_events();
                let wait = WaitCondition {
                    reason: WaitReason::Delegation,
                    correlation_ids: vec!["live-child".into()],
                };
                let waiting =
                    transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
                tokio::pin!(waiting);
                tokio::select! {
                    _ = gate.entered.notified() => {},
                    result = &mut waiting => panic!("wait ended before recovery: {result:?}"),
                }
                if lag {
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                            .await
                            .is_err()
                    );
                    for _ in 0..2200 {
                        config.event_sink.emit_ephemeral(
                            &exec.session_id,
                            AgentEventKind::AssistantContentDelta {
                                message_id: "noise".into(),
                                content: "x".into(),
                            },
                        );
                    }
                    tokio::select! {
                        _ = gate.entered.notified() => {},
                        result = &mut waiting => panic!("wait ended during lag recovery: {result:?}"),
                    }
                }
                config.event_sink.emit_ephemeral(
                    &exec.session_id,
                    AgentEventKind::AssistantContentDelta {
                        message_id: "unrelated".into(),
                        content: "x".into(),
                    },
                );
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                        .await
                        .is_err()
                );
                assert_eq!(
                    gate.reads.load(std::sync::atomic::Ordering::SeqCst),
                    1 + usize::from(lag),
                    "unrelated events must not restart recovery"
                );
                config.event_sink.emit_ephemeral(
                    &exec.session_id,
                    AgentEventKind::DelegationCompleted {
                        delegation_id: "live-child".into(),
                        result: Some("live result".into()),
                    },
                );
                assert!(
                    matches!(
                        tokio::time::timeout(std::time::Duration::from_millis(200), &mut waiting)
                            .await,
                        Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
                    ),
                    "live completion blocked by recovery: {policy:?} lag={lag}"
                );
            }
        }
    }

    #[tokio::test]
    async fn live_all_progress_resets_inactivity_while_recovery_is_pending() {
        let (mut config, exec, context, _dir) = fixture().await;
        config.delegation_wait_policy = crate::config::DelegationWaitPolicy::All;
        config.delegation_wait_timeout_secs = 1;
        let gate = gated_sink(&mut config, 0);
        let mut rx = config.subscribe_events();
        let wait = WaitCondition {
            reason: WaitReason::Delegation,
            correlation_ids: vec!["a".into(), "b".into()],
        };
        let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
        tokio::pin!(waiting);
        tokio::select! { _ = gate.entered.notified() => {}, _ = &mut waiting => panic!("early wait exit") }
        for id in ["a", "b"] {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(600), &mut waiting)
                    .await
                    .is_err(),
                "progress must renew the original one-second deadline"
            );
            config.event_sink.emit_ephemeral(
                &exec.session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: id.into(),
                    result: Some(format!("result-{id}")),
                },
            );
        }
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut waiting).await,
            Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
        ));
        let history = config
            .provider
            .history_store()
            .get_history(&exec.session_id)
            .await
            .unwrap();
        let text = format!("{:?}", history.last().unwrap().parts);
        assert!(
            !text.contains("timed out"),
            "live progress was ignored: {text}"
        );
    }

    #[tokio::test]
    async fn transient_recovery_error_is_nonfatal_for_any_all_and_suspended() {
        use std::sync::atomic::Ordering;
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            for lag in [false, true] {
                let (mut config, exec, context, _dir) = fixture().await;
                config.delegation_wait_policy = policy.clone();
                let gate = gated_sink(&mut config, usize::MAX);
                let mut rx = config.subscribe_events();
                let wait = WaitCondition {
                    reason: WaitReason::Delegation,
                    correlation_ids: vec!["retry-child".into()],
                };
                if !lag {
                    gate.failures.store(1, Ordering::SeqCst);
                }
                let waiting =
                    transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
                tokio::pin!(waiting);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(25), &mut waiting)
                        .await
                        .is_err(),
                    "recovery error must not fail a viable wait"
                );
                if lag {
                    gate.failures.store(1, Ordering::SeqCst);
                    for _ in 0..2200 {
                        config.event_sink.emit_ephemeral(
                            &exec.session_id,
                            AgentEventKind::AssistantContentDelta {
                                message_id: "noise".into(),
                                content: "x".into(),
                            },
                        );
                    }
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(25), &mut waiting)
                            .await
                            .is_err()
                    );
                }
                config
                    .emit_event_persisted(
                        &exec.session_id,
                        AgentEventKind::DelegationCompleted {
                            delegation_id: "retry-child".into(),
                            result: Some("recovered result".into()),
                        },
                    )
                    .await
                    .unwrap();
                assert!(matches!(
                    tokio::time::timeout(std::time::Duration::from_millis(500), &mut waiting).await,
                    Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
                ));
            }
        }
        let (mut config, exec, context, _dir) = fixture().await;
        let gate = gated_sink(&mut config, usize::MAX);
        config
            .emit_event_persisted(
                &exec.session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: "saved".into(),
                    result: Some("saved".into()),
                },
            )
            .await
            .unwrap();
        exec.suspended_delegations
            .lock()
            .await
            .insert("saved".into());
        let mut rx = config.subscribe_events();
        gate.failures.store(1, Ordering::SeqCst);
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .expect("transient read must not fail BeforeModel");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .unwrap();
        assert!(
            exec.suspended_delegations.lock().await.is_empty(),
            "pending IDs must survive and retry durable recovery"
        );
    }

    #[tokio::test]
    async fn suspended_live_progress_does_not_wait_for_stalled_recovery() {
        let (mut config, exec, context, _dir) = fixture().await;
        let gate = gated_sink(&mut config, 0);
        exec.suspended_delegations
            .lock()
            .await
            .insert("live".into());
        let mut rx = config.subscribe_events();
        config.event_sink.emit_ephemeral(
            &exec.session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id: "live".into(),
                result: Some("live".into()),
            },
        );
        assert!(
            matches!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(150),
                    reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
                )
                .await,
                Ok(Ok(_))
            ),
            "BeforeModel blocked by a supplementary recovery read"
        );
        assert!(exec.suspended_delegations.lock().await.is_empty());
        drop(gate);
    }

    #[tokio::test]
    async fn lag_during_captured_recovery_requests_a_followup_read() {
        use std::sync::atomic::Ordering;
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            let (mut config, exec, context, _dir) = fixture().await;
            config.delegation_wait_policy = policy;
            let gate = gated_sink(&mut config, 0);
            gate.snapshot_before_wait.store(true, Ordering::SeqCst);
            let mut rx = config.subscribe_events();
            let mut witness = config.subscribe_events();
            let wait = WaitCondition {
                reason: WaitReason::Delegation,
                correlation_ids: vec!["lost-after-snapshot".into()],
            };
            let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
            tokio::pin!(waiting);
            tokio::select! { _ = gate.entered.notified() => {}, _ = &mut waiting => panic!("early wait exit") }
            // The actual query has already returned an empty snapshot; only its delivery is held.
            config
                .emit_event_persisted(
                    &exec.session_id,
                    AgentEventKind::DelegationCompleted {
                        delegation_id: "lost-after-snapshot".into(),
                        result: Some("saved after first query".into()),
                    },
                )
                .await
                .unwrap();
            for _ in 0..2200 {
                config.event_sink.emit_ephemeral(
                    &exec.session_id,
                    AgentEventKind::AssistantContentDelta {
                        message_id: "noise".into(),
                        content: "x".into(),
                    },
                );
            }
            assert!(matches!(
                witness.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
            ));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiting)
                    .await
                    .is_err()
            );
            assert_eq!(gate.reads.load(Ordering::SeqCst), 1);
            gate.blocking.store(false, Ordering::SeqCst);
            gate.release.notify_one();
            assert!(
                matches!(
                    tokio::time::timeout(std::time::Duration::from_millis(250), &mut waiting).await,
                    Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
                ),
                "lag during a retained old read must not lose the durable wakeup"
            );
            assert_eq!(gate.reads.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn suspended_recovery_retains_new_ids_and_lag_during_a_captured_read() {
        use std::sync::atomic::Ordering;
        let (mut config, exec, context, _dir) = fixture().await;
        let gate = gated_sink(&mut config, 0);
        gate.snapshot_before_wait.store(true, Ordering::SeqCst);
        exec.suspended_delegations
            .lock()
            .await
            .insert("older".into());
        let mut rx = config.subscribe_events();
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            reconcile_suspended_delegations(&config, &context, &exec, &mut rx),
        )
        .await
        .unwrap()
        .unwrap();
        exec.suspended_delegations
            .lock()
            .await
            .insert("newer".into());
        for id in ["older", "newer"] {
            config
                .emit_event_persisted(
                    &exec.session_id,
                    AgentEventKind::DelegationCompleted {
                        delegation_id: id.into(),
                        result: Some("late durable result".into()),
                    },
                )
                .await
                .unwrap();
        }
        for _ in 0..2200 {
            config.event_sink.emit_ephemeral(
                &exec.session_id,
                AgentEventKind::AssistantContentDelta {
                    message_id: "noise".into(),
                    content: "x".into(),
                },
            );
        }
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .unwrap();
        assert_eq!(
            gate.reads.load(Ordering::SeqCst),
            1,
            "boundaries must retain the in-flight read"
        );
        gate.blocking.store(false, Ordering::SeqCst);
        gate.release.notify_one();
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .unwrap();
        assert!(
            exec.suspended_delegations.lock().await.is_empty(),
            "lag and new IDs must survive the old query result"
        );
        assert_eq!(gate.reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn duplicate_live_and_recovered_outcomes_do_not_renew_all_inactivity() {
        use std::sync::atomic::Ordering;
        let (mut config, exec, context, _dir) = fixture().await;
        config.delegation_wait_policy = crate::config::DelegationWaitPolicy::All;
        config.delegation_wait_timeout_secs = 1;
        let gate = gated_sink(&mut config, 0);
        config
            .emit_event_persisted(
                &exec.session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: "a".into(),
                    result: Some("a".into()),
                },
            )
            .await
            .unwrap();
        gate.snapshot_before_wait.store(true, Ordering::SeqCst);
        let mut rx = config.subscribe_events();
        let wait = WaitCondition {
            reason: WaitReason::Delegation,
            correlation_ids: vec!["a".into(), "b".into()],
        };
        let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
        tokio::pin!(waiting);
        tokio::select! { _ = gate.entered.notified() => {}, _ = &mut waiting => panic!("early wait exit") }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(400), &mut waiting)
                .await
                .is_err()
        );
        config.event_sink.emit_ephemeral(
            &exec.session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id: "a".into(),
                result: Some("a".into()),
            },
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(600), &mut waiting)
                .await
                .is_err()
        );
        gate.release.notify_one();
        config.event_sink.emit_ephemeral(
            &exec.session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id: "a".into(),
                result: Some("a".into()),
            },
        );
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_millis(650), waiting).await,
                Ok(Ok(ExecutionState::BeforeLlmCall { .. }))
            ),
            "duplicates must not postpone b's original inactivity deadline"
        );
        let history = config
            .provider
            .history_store()
            .get_history(&exec.session_id)
            .await
            .unwrap();
        let text = format!("{:?}", history.last().unwrap().parts);
        assert_eq!(
            text.matches("- a:").count(),
            1,
            "recovery/live duplicates must be summarized once"
        );
        assert!(text.contains("- b: timed out"));
    }

    #[tokio::test]
    async fn persistent_recovery_errors_keep_cancel_timeout_and_pending_ids() {
        use std::sync::atomic::Ordering;
        for cancel in [true, false] {
            let (mut config, exec, context, _dir) = fixture().await;
            config.delegation_wait_policy = crate::config::DelegationWaitPolicy::All;
            config.delegation_wait_timeout_secs = 1;
            let gate = gated_sink(&mut config, usize::MAX);
            gate.failures.store(usize::MAX, Ordering::SeqCst);
            let mut rx = config.subscribe_events();
            let wait = WaitCondition {
                reason: WaitReason::Delegation,
                correlation_ids: vec!["pending".into()],
            };
            let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
            tokio::pin!(waiting);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(250), &mut waiting)
                    .await
                    .is_err()
            );
            assert_eq!(
                gate.reads.load(Ordering::SeqCst),
                2,
                "read errors must back off, not spin"
            );
            if cancel {
                exec.cancellation_token.cancel();
            }
            let result = tokio::time::timeout(std::time::Duration::from_millis(1000), &mut waiting)
                .await
                .unwrap()
                .unwrap();
            if cancel {
                assert!(matches!(result, ExecutionState::Cancelled));
            } else {
                assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
                assert!(
                    format!(
                        "{:?}",
                        config
                            .provider
                            .history_store()
                            .get_history(&exec.session_id)
                            .await
                            .unwrap()
                    )
                    .contains("timed out")
                );
            }
            assert!(gate.reads.load(Ordering::SeqCst) <= 5);
        }
        let (mut config, exec, context, _dir) = fixture().await;
        let gate = gated_sink(&mut config, usize::MAX);
        gate.failures.store(usize::MAX, Ordering::SeqCst);
        exec.suspended_delegations
            .lock()
            .await
            .insert("pending".into());
        let mut rx = config.subscribe_events();
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            reconcile_suspended_delegations(&config, &context, &exec, &mut rx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(exec.suspended_delegations.lock().await.contains("pending"));
        assert!(exec.suspended_recovery.lock().await.is_some());
        exec.cancellation_token.cancel();
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .unwrap();
        assert_eq!(gate.reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn steering_during_recovery_suspends_only_pending_ids_for_reconciliation() {
        use std::sync::atomic::Ordering;
        for policy in [
            crate::config::DelegationWaitPolicy::Any,
            crate::config::DelegationWaitPolicy::All,
        ] {
            let (mut config, exec, context, _dir) = fixture().await;
            config.delegation_wait_policy = policy.clone();
            let gate = gated_sink(&mut config, usize::MAX);
            gate.failures.store(usize::MAX, Ordering::SeqCst);
            let inbox = Arc::new(crate::agent::turn_control::SteeringInbox::new("run".into()));
            let exec = exec.with_steering(inbox.clone());
            let mut rx = config.subscribe_events();
            let wait = WaitCondition {
                reason: WaitReason::Delegation,
                correlation_ids: vec!["a".into(), "b".into()],
            };
            {
                let waiting =
                    transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
                tokio::pin!(waiting);
                tokio::select! { _ = gate.entered.notified() => {}, _ = &mut waiting => panic!("early wait exit") }
                if policy == crate::config::DelegationWaitPolicy::All {
                    config.event_sink.emit_ephemeral(
                        &exec.session_id,
                        AgentEventKind::DelegationCompleted {
                            delegation_id: "a".into(),
                            result: Some("a".into()),
                        },
                    );
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiting)
                            .await
                            .is_err()
                    );
                }
                inbox
                    .push(
                        "steering".into(),
                        None,
                        vec![crate::acp::protocol::ContentBlock::Text(
                            crate::acp::protocol::TextContent::new("continue"),
                        )],
                    )
                    .await
                    .unwrap();
                let result =
                    tokio::time::timeout(std::time::Duration::from_millis(150), &mut waiting)
                        .await
                        .unwrap()
                        .unwrap();
                assert!(matches!(result, ExecutionState::BeforeLlmCall { .. }));
            }
            let suspended = exec.suspended_delegations.lock().await.clone();
            assert!(suspended.contains("b"));
            assert_eq!(
                suspended.contains("a"),
                policy == crate::config::DelegationWaitPolicy::Any
            );
            assert_eq!(
                inbox.pending_count().await,
                1,
                "waiting must not consume accepted steering"
            );
            gate.failures.store(0, Ordering::SeqCst);
            for id in suspended {
                config
                    .emit_event_persisted(
                        &exec.session_id,
                        AgentEventKind::DelegationCompleted {
                            delegation_id: id,
                            result: Some("late".into()),
                        },
                    )
                    .await
                    .unwrap();
            }
            reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
                .await
                .unwrap();
            assert!(exec.suspended_delegations.lock().await.is_empty());
        }
    }

    #[tokio::test]
    async fn live_completion_still_propagates_real_wait_message_write_failure() {
        let (mut config, exec, context, _dir) = fixture().await;
        let gate = gated_sink(&mut config, 0);
        let mut rx = config.subscribe_events();
        let wait = WaitCondition {
            reason: WaitReason::Delegation,
            correlation_ids: vec!["live".into()],
        };
        let waiting = transition_waiting_for_event(&config, &wait, &context, &exec, &mut rx);
        tokio::pin!(waiting);
        tokio::select! { _ = gate.entered.notified() => {}, _ = &mut waiting => panic!("early wait exit") }
        config
            .provider
            .history_store()
            .delete_session(&exec.session_id)
            .await
            .unwrap();
        config.event_sink.emit_ephemeral(
            &exec.session_id,
            AgentEventKind::DelegationCompleted {
                delegation_id: "live".into(),
                result: Some("live".into()),
            },
        );
        let result = tokio::time::timeout(std::time::Duration::from_millis(200), waiting)
            .await
            .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to store wait message")
        );
    }

    #[tokio::test]
    async fn suspended_delegation_recovery_does_not_depend_on_live_receiver() {
        let (config, exec, context, _dir) = fixture().await;
        exec.suspended_delegations
            .lock()
            .await
            .insert("finished-child".into());
        config
            .emit_event_persisted(
                &exec.session_id,
                AgentEventKind::DelegationCompleted {
                    delegation_id: "finished-child".into(),
                    result: Some("saved result".into()),
                },
            )
            .await
            .unwrap();
        let mut rx = config.subscribe_events();
        reconcile_suspended_delegations(&config, &context, &exec, &mut rx)
            .await
            .unwrap();
        assert!(
            exec.suspended_delegations.lock().await.is_empty(),
            "durable result must resolve suspended steering wait"
        );
    }
}
