//! Post-turn maintenance operations
//!
//! This module handles pruning and AI compaction of conversation history to manage
//! context window size.

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::utils::{genai, u32_from_usize};
use crate::events::{AgentEventKind, StopType};
use crate::hooks::{PostCompactionRequest, PreCompactionRequest};
use crate::middleware::ExecutionState;
use crate::model::MessagePart;
use crate::session::compaction::{SessionCompaction, codex};
use log::{debug, info, warn};
use std::sync::Arc;

/// Emits the terminal event for a started compaction before propagating an
/// error, so every reported compaction start is matched by exactly one
/// terminal outcome.
async fn emit_compaction_failure(
    config: &AgentConfig,
    session_id: &str,
    compaction_id: &str,
    error: &anyhow::Error,
) {
    emit_compaction_event(
        config,
        session_id,
        AgentEventKind::CompactionFailed {
            compaction_id: compaction_id.to_string(),
            reason: format!("{error:#}"),
            cancelled: false,
        },
    )
    .await;
}

// Await persistence and publication to preserve compaction lifecycle ordering.
async fn emit_compaction_event(config: &AgentConfig, session_id: &str, kind: AgentEventKind) {
    if let Err(error) = config.emit_event_persisted(session_id, kind).await {
        warn!("failed to emit compaction event for session {session_id}: {error}");
    }
}

/// The accounting ledger for one compaction operation.
///
/// Uses the cycle the compaction was initiated from. The billing basis is the
/// session provider's (that provider handles the request), priced for the
/// compaction model actually used.
async fn compaction_ledger(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    model: &str,
) -> Result<crate::accounting::DispatchLedger, anyhow::Error> {
    let cycle = exec_ctx
        .cycle_accounting(&config.event_sink)
        .await
        .map_err(|error| anyhow::anyhow!("accounting unavailable: {error}"))?;
    let recorder = Arc::new(crate::accounting::AttemptRecorder::with_projection(
        config.event_sink.session_stream(&exec_ctx.session_id),
        cycle.session_accounting().clone(),
    ));
    let provider = exec_ctx
        .llm_config()
        .map(|c| c.provider.clone())
        .unwrap_or_default();
    Ok(crate::accounting::DispatchLedger {
        operation: Arc::new(
            crate::accounting::AttemptOperation::new(
                recorder,
                cycle,
                crate::accounting::AttemptPurpose::Compaction,
                provider,
                model,
            )
            .with_budget_pricing(config.budget_pricing())
            .with_budget_cap(config.budget_cap_usd()),
        ),
        basis: exec_ctx.session_handle.billing_basis_for_model(model),
    })
}

/// Run pruning on tool results to reduce context size.
///
/// This marks low-value tool results as compacted based on the pruning configuration.
/// Pruning is a lightweight operation that can run after every turn.
pub(super) async fn run_pruning(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
) -> Result<(), anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    let messages = exec_ctx
        .session_handle
        .get_agent_history()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get agent history: {}", e))?;

    let prune_config = crate::session::pruning::PruneConfig {
        protect_tokens: config.execution_policy.pruning.protect_tokens,
        minimum_tokens: config.execution_policy.pruning.minimum_tokens,
        protected_tools: config.execution_policy.pruning.protected_tools.clone(),
    };

    let estimator =
        crate::session::pruning::content_cost_estimator_for_llm_config(exec_ctx.llm_config());
    let analysis = crate::session::pruning::compute_prune_candidates(
        &messages,
        &prune_config,
        estimator.as_ref(),
    );

    if analysis.should_prune && !analysis.candidates.is_empty() {
        let call_ids = crate::session::pruning::extract_call_ids(&analysis.candidates);
        info!(
            "Pruning {} tool results ({} tokens) for session {}",
            call_ids.len(),
            analysis.prunable_tokens,
            session_id
        );

        let updated = config
            .provider
            .history_store()
            .mark_tool_results_compacted(session_id, &call_ids)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to mark tool results compacted: {}", e))?;

        debug!("Marked {} tool results as compacted", updated);
    } else {
        debug!(
            "Pruning skipped: should_prune={}, candidates={}, prunable_tokens={}",
            analysis.should_prune,
            analysis.candidates.len(),
            analysis.prunable_tokens
        );
    }

    Ok(())
}

/// Run AI-powered compaction on the conversation history.
///
/// This generates a summary of old messages and injects it into the conversation,
/// then returns a new execution state with the compacted context. This is a heavier
/// operation typically triggered when context thresholds are hit.
pub(super) async fn run_ai_compaction(
    config: &AgentConfig,
    exec_ctx: &mut ExecutionContext,
    current_state: &ExecutionState,
) -> Result<ExecutionState, anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    let messages = exec_ctx
        .session_handle
        .get_effective_agent_history()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get agent history: {}", e))?;

    let prompt_limit = exec_ctx
        .execution_config()
        .and_then(|cfg| cfg.max_prompt_bytes);
    let token_estimate: usize = messages
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .map(|p| match p {
                    MessagePart::Text { content } => content.len() / 4,
                    MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                        crate::agent::utils::render_prompt_for_llm(blocks, prompt_limit).len() / 4
                    }
                    MessagePart::ToolResult { content, .. } => {
                        content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .map(|t| t.len())
                            .sum::<usize>()
                            / 4
                    }
                    MessagePart::Output { output } => output.estimate_text().len() / 4,
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum();

    let turn_id = exec_ctx.turn_id().unwrap_or_default().to_string();
    let model_name = exec_ctx
        .llm_config()
        .map(|cfg| cfg.model.clone())
        .unwrap_or_default();
    let token_estimate_u32 = u32_from_usize(token_estimate, "token_estimate", Some(session_id));
    let message_count = u32_from_usize(messages.len(), "messages.len", Some(session_id));

    let pre_hook = config
        .hooks
        .run_pre_compaction(PreCompactionRequest {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: model_name.clone(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            trigger: "context_threshold".to_string(),
            token_estimate: token_estimate_u32,
            message_count,
            messages: messages
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?,
        })
        .await?;
    for notice in pre_hook.notices {
        config.emit_event(
            session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    if pre_hook.should_block {
        let mut reason = pre_hook
            .block_reason
            .unwrap_or_else(|| "compaction blocked by hook".to_string());
        if !pre_hook.additional_contexts.is_empty() {
            reason = format!(
                "{}\n\n{}",
                reason,
                pre_hook.additional_contexts.join("\n\n")
            );
        }
        return Ok(ExecutionState::Stopped {
            message: reason.into(),
            stop_type: StopType::ContextThreshold,
            context: current_state.context().cloned(),
        });
    }

    // The compaction identity is fixed before the start event so every
    // related update shares one session-unique ID.
    let compaction_id = uuid::Uuid::now_v7().to_string();

    emit_compaction_event(
        config,
        session_id,
        AgentEventKind::CompactionStart {
            token_estimate: token_estimate_u32,
            compaction_id: Some(compaction_id.clone()),
        },
    )
    .await;

    let llm_config = exec_ctx
        .llm_config()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;

    let target = exec_ctx
        .session_handle
        .output_target()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;
    let native_codex = codex::supports_compaction(&target);
    let strategy = if pre_hook.custom_summary.is_some() {
        "hook_summary"
    } else if native_codex {
        codex::COMPACTION_STRATEGY
    } else {
        "llm_summary"
    };
    let model = if native_codex {
        &llm_config.model
    } else {
        config
            .execution_policy
            .compaction
            .model
            .as_ref()
            .unwrap_or(&llm_config.model)
    };

    let mut result = if let Some(summary) = pre_hook.custom_summary {
        crate::session::compaction::CompactionResult {
            summary_token_count: summary.len() / 4,
            summary,
            original_token_count: token_estimate,
            native_output: None,
        }
    } else {
        let llm_provider = match exec_ctx
            .session_handle
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get LLM provider: {}", e))
        {
            Ok(provider) => provider,
            Err(error) => {
                emit_compaction_failure(config, session_id, &compaction_id, &error).await;
                return Err(error);
            }
        };
        // Compaction is a paid request: its attempts are recorded against the
        // initiating cycle and session, never inferred from message history.
        let ledger = match compaction_ledger(config, exec_ctx, model).await {
            Ok(ledger) => ledger,
            Err(error) => {
                emit_compaction_failure(config, session_id, &compaction_id, &error).await;
                return Err(error);
            }
        };
        let retry_config = crate::session::compaction::RetryConfig {
            max_retries: config.execution_policy.compaction.retry.max_retries,
            initial_backoff_ms: config.execution_policy.compaction.retry.initial_backoff_ms,
            backoff_multiplier: config.execution_policy.compaction.retry.backoff_multiplier,
        };
        let result = if native_codex {
            let tools = config.collect_tools(
                llm_provider.clone(),
                Some(exec_ctx.runtime.as_ref()),
                Some(&exec_ctx.tool_config),
            );
            config
                .compaction
                .process_codex(
                    &messages,
                    llm_provider,
                    (&target, &tools),
                    &retry_config,
                    prompt_limit,
                    &ledger,
                )
                .await
        } else {
            config
                .compaction
                .process(
                    &messages,
                    llm_provider,
                    model,
                    &retry_config,
                    prompt_limit,
                    &ledger,
                )
                .await
        };
        match result {
            Ok(result) => result,
            Err(error) => {
                // The cycle budget gate refused a paid compaction dispatch:
                // nothing was sent. End the cycle as a price-limit stop, as
                // the main path does, not as a compaction/accounting error.
                // The started compaction still gets its terminal event.
                if let Some((cap, spent)) = ledger.operation.budget_refusal() {
                    let reason = anyhow::anyhow!(
                        "compaction not dispatched: price limit reached: ${spent:.4} >= ${cap:.2}"
                    );
                    emit_compaction_failure(config, session_id, &compaction_id, &reason).await;
                    return Ok(ExecutionState::Stopped {
                        message: format!("Price limit reached: ${spent:.4} >= ${cap:.2}").into(),
                        stop_type: StopType::PriceLimit,
                        context: current_state.context().cloned(),
                    });
                }
                let error = anyhow::anyhow!("Compaction failed: {}", error);
                emit_compaction_failure(config, session_id, &compaction_id, &error).await;
                return Err(error);
            }
        }
    };

    // A hook-provided summary must be usable on its own, not rescued by post-hook notes.
    if let Err(error) = SessionCompaction::validate_summary(&result.summary) {
        let error = anyhow::Error::from(error);
        emit_compaction_failure(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    let post_hook = match config
        .hooks
        .run_post_compaction(PostCompactionRequest {
            session_id: session_id.clone(),
            turn_id,
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: model_name,
            permission_mode: exec_ctx.permission_mode().to_string(),
            trigger: "context_threshold".to_string(),
            summary: result.summary.clone(),
            original_token_count: u32_from_usize(
                result.original_token_count,
                "result.original_token_count",
                Some(session_id),
            ),
            summary_token_count: u32_from_usize(
                result.summary_token_count,
                "result.summary_token_count",
                Some(session_id),
            ),
            message_count,
        })
        .await
    {
        Ok(post_hook) => post_hook,
        Err(error) => {
            emit_compaction_failure(config, session_id, &compaction_id, &error).await;
            return Err(error);
        }
    };
    for notice in post_hook.notices {
        config.emit_event(
            session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    if !post_hook.additional_contexts.is_empty() {
        if let Some(output) = &mut result.native_output {
            output.items.extend(
                querymt::chat::ChatOutput::from_projections(
                    None,
                    Some(post_hook.additional_contexts.join("\n\n")),
                    None,
                    None,
                    None,
                )
                .items,
            );
        }
        result.summary = format!(
            "{}\n\n{}",
            result.summary,
            post_hook.additional_contexts.join("\n\n")
        );
    }

    // Final validation must precede both writes: a blank boundary would hide the original history.
    if let Err(error) = SessionCompaction::validate_summary(&result.summary) {
        let error = anyhow::Error::from(error);
        emit_compaction_failure(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    tracing::info!(
        strategy,
        native_checkpoint = result.native_output.is_some(),
        original_tokens = result.original_token_count,
        result_tokens = result.summary_token_count,
        "Compaction generated result"
    );

    for message in result.take_messages(session_id) {
        if let Err(error) = exec_ctx.add_message(message).await {
            let error = anyhow::anyhow!("Failed to store compaction checkpoint: {}", error);
            emit_compaction_failure(config, session_id, &compaction_id, &error).await;
            return Err(error);
        }
    }

    let filtered_messages = match exec_ctx.session_handle.get_effective_agent_history().await {
        Ok(messages) => messages,
        Err(error) => {
            let error = anyhow::anyhow!("Failed to get new history: {}", error);
            emit_compaction_failure(config, session_id, &compaction_id, &error).await;
            return Err(error);
        }
    };

    let new_context_tokens = config
        .compaction
        .estimate_messages_tokens(&filtered_messages, prompt_limit)
        .max(result.summary_token_count);

    // One validated text summary chunk after the start and before the terminal update.
    emit_compaction_event(
        config,
        session_id,
        AgentEventKind::CompactionSummaryChunk {
            compaction_id: compaction_id.clone(),
            content: result.summary.clone(),
        },
    )
    .await;

    emit_compaction_event(
        config,
        session_id,
        AgentEventKind::CompactionEnd {
            summary: result.summary.clone(),
            summary_len: u32_from_usize(
                result.summary.len(),
                "result.summary.len",
                Some(session_id),
            ),
            compaction_id: Some(compaction_id.clone()),
            // The authoritative post-compaction token count rides with the
            // terminal event so clients can project the reduced usage.
            context_tokens: Some(u64::try_from(new_context_tokens).unwrap_or(u64::MAX)),
        },
    )
    .await;

    // Convert AgentMessages to ChatMessages for the ConversationContext
    let chat_messages: Vec<querymt::chat::ChatMessage> = filtered_messages
        .iter()
        .map(|message| message.to_chat_message_with_output_target(Some(&target), prompt_limit))
        .collect::<Result<_, _>>()
        .map_err(|error| anyhow::anyhow!("Invalid prompt content after compaction: {error}"))?;

    debug!(
        "Post-compaction context tokens updated: {} -> {} (filtered {} messages)",
        current_state
            .context()
            .map(|c| c.stats.context_tokens)
            .unwrap_or(0),
        new_context_tokens,
        filtered_messages.len()
    );

    let new_context = if let Some(ctx) = current_state.context() {
        let mut new_stats = crate::middleware::AgentStats::clone(&ctx.stats);
        new_stats.context_tokens = new_context_tokens;

        Arc::new(
            crate::middleware::ConversationContext::new(
                ctx.session_id.clone(),
                Arc::from(chat_messages.into_boxed_slice()),
                Arc::new(new_stats),
                ctx.provider.clone(),
                ctx.model.clone(),
            )
            .with_session_mode(ctx.session_mode)
            .with_profile(ctx.profile.clone()),
        )
    } else {
        return Err(anyhow::anyhow!("No context available for compaction"));
    };

    exec_ctx.compaction_summaries =
        genai::compaction_summaries(&filtered_messages, &new_context.messages);
    Ok(ExecutionState::BeforeLlmCall {
        context: new_context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::{McpToolState, SessionRuntime, ToolConfig};
    use crate::events::{DurableEvent, EventEnvelope};
    use crate::hooks::{
        HookCommandConfig, HookHandlerConfig, Hooks, HooksConfig, MatcherGroupConfig,
    };
    use crate::middleware::{AgentStats, ConversationContext};
    use crate::model::AgentMessage;
    use crate::session::RuntimeContext;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::SessionExecutionConfig;
    use crate::test_utils::helpers::mock_plugin_registry;
    use crate::test_utils::mocks::{MockLlmProvider, SharedLlmProvider, TestProviderFactory};
    use querymt::LLMParams;
    use querymt::chat::{ChatMessage, ChatOutput, ChatRole, FinishReason};

    fn hook_group(output: serde_json::Value) -> Vec<MatcherGroupConfig> {
        vec![MatcherGroupConfig {
            matcher: None,
            hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                command: format!("printf '%s' '{}'", output),
                timeout_sec: Some(5),
                ..HookCommandConfig::default()
            })],
        }]
    }

    fn custom_summary_hooks(summary: &str, post_context: Option<&str>) -> Hooks {
        Hooks::new(HooksConfig {
            enabled: true,
            pre_compaction: hook_group(serde_json::json!({"hook_specific_output": {
                "hook_event_name": "pre_compaction", "compaction": {"summary": summary}
            }})),
            post_compaction: post_context.map_or_else(Vec::new, |context| hook_group(serde_json::json!({
                "hook_specific_output": {"hook_event_name": "post_compaction", "additional_context": context}
            }))),
            ..HooksConfig::default()
        }).unwrap()
    }

    async fn fixture(
        hooks: Hooks,
        output: Option<ChatOutput>,
    ) -> (
        AgentConfig,
        ExecutionContext,
        ExecutionState,
        tempfile::TempDir,
    ) {
        fixture_with_provider(hooks, output, "mock").await
    }

    async fn fixture_with_provider(
        hooks: Hooks,
        output: Option<ChatOutput>,
        provider_name: &str,
    ) -> (
        AgentConfig,
        ExecutionContext,
        ExecutionState,
        tempfile::TempDir,
    ) {
        let mut mock = MockLlmProvider::new();
        if let Some(output) = output {
            // The configured single retry may call the provider at most twice.
            if provider_name == "codex" {
                mock.expect_chat_with_tools()
                    .times(1..=2)
                    .returning(move |_, tools| {
                        assert!(tools.is_some_and(|tools| !tools.is_empty()));
                        Ok(output.clone())
                    });
            } else {
                mock.expect_chat()
                    .times(1..=2)
                    .returning(move |_| Ok(output.clone()));
            }
        } else {
            mock.expect_chat().times(0);
        }
        let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
            mock,
            vec![],
        )));
        let (mut registry, tempdir) = mock_plugin_registry(factory).unwrap();
        registry.config.providers[0].name = provider_name.into();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let params = LLMParams::new().provider(provider_name).model("mock");
        let mut config = AgentConfigBuilder::new(Arc::new(registry), storage, params)
            .with_hooks(hooks)
            .build();
        config.execution_policy.compaction.retry.max_retries = 1;
        config.execution_policy.compaction.retry.initial_backoff_ms = 0;
        let session_handle = config
            .provider
            .create_session(None, None, &SessionExecutionConfig::default())
            .await
            .unwrap();
        let session_id = session_handle.session().public_id.clone();
        let mut original = AgentMessage::new(session_id.clone(), ChatRole::User);
        original.parts.push(MessagePart::Text {
            content: "Keep the implemented shader and verified test results.".into(),
        });
        session_handle.add_message(original).await.unwrap();
        let runtime_context =
            RuntimeContext::new(config.provider.history_store(), session_id.clone())
                .await
                .unwrap();
        let runtime = SessionRuntime::new(None, Default::default(), McpToolState::empty());
        let exec_ctx = ExecutionContext::new(
            session_id.clone(),
            runtime,
            runtime_context,
            session_handle,
            ToolConfig::default(),
        )
        .with_turn_id("run-compaction");
        let state = ExecutionState::Stopped {
            message: "context threshold".into(),
            stop_type: StopType::ContextThreshold,
            context: Some(Arc::new(ConversationContext::new(
                session_id.into(),
                Arc::from([ChatMessage::user().text("Continue the task").build()]),
                Arc::new(AgentStats {
                    context_tokens: 100_000,
                    ..AgentStats::default()
                }),
                "mock".into(),
                "mock".into(),
            ))),
        };
        (config, exec_ctx, state, tempdir)
    }

    fn is_compaction_event(kind: &AgentEventKind) -> bool {
        matches!(
            kind,
            AgentEventKind::CompactionStart { .. }
                | AgentEventKind::CompactionSummaryChunk { .. }
                | AgentEventKind::CompactionEnd { .. }
                | AgentEventKind::CompactionFailed { .. }
        )
    }

    fn take_compaction_events(
        receiver: &mut tokio::sync::broadcast::Receiver<EventEnvelope>,
    ) -> Vec<DurableEvent> {
        let mut events = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(EventEnvelope::Durable(event)) if is_compaction_event(&event.kind) => {
                    events.push(event);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return events,
                Err(error) => panic!("failed to receive compaction event: {error}"),
            }
        }
    }

    async fn assert_compaction_journal_matches(
        config: &AgentConfig,
        session_id: &str,
        live_events: &[DurableEvent],
    ) {
        let journal_events: Vec<_> = config
            .event_sink
            .journal()
            .load_session_stream(session_id, None, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| is_compaction_event(&event.kind))
            .collect();
        assert_eq!(
            serde_json::to_value(&journal_events).unwrap(),
            serde_json::to_value(live_events).unwrap()
        );
        for pair in live_events.windows(2) {
            assert!(pair[0].stream_seq < pair[1].stream_seq);
        }
    }

    async fn assert_failed_compaction_preserves_history(
        hooks: Hooks,
        output: Option<ChatOutput>,
        expected_error: &str,
    ) {
        let (config, mut exec_ctx, state, _tempdir) = fixture(hooks, output).await;
        let before = config
            .provider
            .history_store()
            .get_history(&exec_ctx.session_id)
            .await
            .unwrap();
        let mut events = config.subscribe_events();
        let error = run_ai_compaction(&config, &mut exec_ctx, &state)
            .await
            .expect_err("compaction must fail");
        // Do not yield: all lifecycle events must be published before compaction returns.
        let compaction_events = take_compaction_events(&mut events);
        assert!(error.to_string().contains(expected_error), "{error:#}");
        let after = config
            .provider
            .history_store()
            .get_history(&exec_ctx.session_id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        let effective = exec_ctx
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&effective).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(state.context().unwrap().stats.context_tokens, 100_000);

        let [start, failed] = compaction_events.as_slice() else {
            panic!("expected start and failure before return: {compaction_events:?}");
        };
        let AgentEventKind::CompactionStart {
            compaction_id: Some(start_id),
            ..
        } = &start.kind
        else {
            panic!("first compaction event must be a start: {start:?}");
        };
        let AgentEventKind::CompactionFailed {
            compaction_id,
            reason,
            cancelled,
        } = &failed.kind
        else {
            panic!("failed compaction must emit a failure, not success: {failed:?}");
        };
        assert_eq!(compaction_id, start_id);
        assert!(reason.contains(expected_error));
        assert!(!cancelled);
        assert_compaction_journal_matches(&config, &exec_ctx.session_id, &compaction_events).await;
        assert!(!crate::session::compaction::has_compaction(&after));
    }

    #[tokio::test]
    async fn codex_native_compaction_resumes_with_reasoning_and_a_single_durable_checkpoint() {
        use querymt::chat::{ChatOpaqueItem, ChatOutputItem, ChatOutputProvenance};
        let mut output = ChatOutput::from_projections(
            Some("visible reasoning summary".into()),
            None,
            None,
            None,
            Some(FinishReason::Stop),
        );
        output.provenance = Some(ChatOutputProvenance {
            provider: "codex".into(),
            model: "mock".into(),
            protocol: "responses".into(),
            endpoint: "https://chatgpt.com/backend-api/codex/responses".into(),
        });
        if let ChatOutputItem::Reasoning(reasoning) = &mut output.items[0] {
            reasoning.encrypted_content = Some("encrypted-reasoning".into());
        }
        output.items.push(ChatOutputItem::Opaque(ChatOpaqueItem {
            original_type: "compaction".into(),
            payload: serde_json::json!({"type":"compaction", "encrypted_content":"encrypted-checkpoint"}),
        }));
        let hooks = Hooks::new(HooksConfig {
            enabled: true,
            post_compaction: hook_group(serde_json::json!({"hook_specific_output": {
                "hook_event_name":"post_compaction", "additional_context":"keep the hook note"
            }})),
            ..Default::default()
        })
        .unwrap();
        let (config, mut exec_ctx, state, _dir) =
            fixture_with_provider(hooks, Some(output), "codex").await;
        // Real SQLite configs have empty protocol/endpoint fields. Native compaction
        // and normal history replay must both resolve the Codex target correctly.
        assert!(exec_ctx.llm_config().unwrap().protocol.is_empty());
        let mut events = config.subscribe_events();
        let resumed = run_ai_compaction(&config, &mut exec_ctx, &state)
            .await
            .unwrap();
        assert!(matches!(resumed, ExecutionState::BeforeLlmCall { .. }));
        let history = config
            .provider
            .history_store()
            .get_history(&exec_ctx.session_id)
            .await
            .unwrap();
        assert_eq!(
            history.len(),
            2,
            "one original user and one atomic checkpoint"
        );
        assert!(!history.iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::CompactionRequest { .. }))
        }));
        let encoded =
            serde_json::to_string(&exec_ctx.session_handle.history().await.unwrap()).unwrap();
        assert!(encoded.contains("visible reasoning summary"));
        assert!(encoded.contains("encrypted-reasoning"));
        assert!(encoded.contains("encrypted-checkpoint"));
        assert!(encoded.contains("keep the hook note"));
        let encoded = serde_json::to_string(&resumed.context().unwrap().messages).unwrap();
        assert!(encoded.contains("encrypted-checkpoint"));
        let events = take_compaction_events(&mut events);
        assert!(matches!(
            &events.last().unwrap().kind,
            AgentEventKind::CompactionEnd { .. }
        ));
        assert_compaction_journal_matches(&config, &exec_ctx.session_id, &events).await;
    }

    #[tokio::test]
    async fn blank_provider_summary_emits_failure_without_writing_a_boundary() {
        for summary in ["", " \n\t "] {
            let output = ChatOutput::from_projections(
                None,
                Some(summary.into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            assert_failed_compaction_preserves_history(
                Hooks::disabled(),
                Some(output),
                "empty summary",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn blank_custom_summary_fails_without_provider_fallback_or_post_hook_rescue() {
        for summary in ["", " \n\t "] {
            assert_failed_compaction_preserves_history(
                custom_summary_hooks(
                    summary,
                    Some("Do not use these notes as a replacement summary"),
                ),
                None,
                "empty summary",
            )
            .await;
        }
    }

    #[tokio::test]
    async fn incomplete_provider_summary_emits_failure_and_preserves_history() {
        let output = ChatOutput::from_projections(
            None,
            Some("partial summary".into()),
            None,
            None,
            Some(FinishReason::Length),
        );
        assert_failed_compaction_preserves_history(
            Hooks::disabled(),
            Some(output),
            "did not complete successfully",
        )
        .await;
    }

    #[tokio::test]
    async fn automatic_compaction_failure_stops_the_run_without_reentering_inference() {
        let output = ChatOutput::from_projections(
            None,
            Some(" \n ".into()),
            None,
            None,
            Some(FinishReason::Stop),
        );
        let (mut config, mut exec_ctx, _state, _tempdir) =
            fixture(Hooks::disabled(), Some(output)).await;
        config.execution_policy.compaction.auto = true;
        config.middleware_drivers = vec![Arc::new(crate::middleware::ContextMiddleware::new(
            // Force compaction before the first model request, even at initial zero usage.
            crate::middleware::ContextConfig {
                compact_at_percent: 0,
                ..crate::middleware::ContextConfig::with_manual_limit(100)
            },
        ))];
        let before = config
            .provider
            .history_store()
            .get_history(&exec_ctx.session_id)
            .await
            .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::super::execute_cycle_state_machine(
                &config,
                &mut exec_ctx,
                crate::agent::core::AgentMode::Build,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            outcome,
            super::super::CycleOutcome::Stopped { .. }
        ));
        let after = config
            .provider
            .history_store()
            .get_history(&exec_ctx.session_id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }

    #[tokio::test]
    async fn valid_custom_summary_is_stored_and_emitted_as_success() {
        let (config, mut exec_ctx, state, _tempdir) = fixture(
            custom_summary_hooks("Valid continuation summary", Some("Keep the tests passing")),
            None,
        )
        .await;
        let mut events = config.subscribe_events();
        let result = run_ai_compaction(&config, &mut exec_ctx, &state)
            .await
            .unwrap();
        let compaction_events = take_compaction_events(&mut events);
        let expected = "Valid continuation summary\n\nKeep the tests passing";
        let effective = exec_ctx
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert_eq!(effective.len(), 2);
        assert!(
            matches!(&effective[1].parts[0], MessagePart::Compaction { summary, .. } if summary == expected)
        );
        assert!(result.context().unwrap().stats.context_tokens > 0);
        assert_eq!(exec_ctx.compaction_summaries.len(), 1);
        assert_eq!(
            exec_ctx.compaction_summaries[0].payload(),
            result.context().unwrap().messages[1].payload()
        );
        let [start, chunk, end] = compaction_events.as_slice() else {
            panic!("expected start, summary, and end before return: {compaction_events:?}");
        };
        let AgentEventKind::CompactionStart {
            compaction_id: Some(start_id),
            ..
        } = &start.kind
        else {
            panic!("first compaction event must be a start: {start:?}");
        };
        let AgentEventKind::CompactionSummaryChunk {
            compaction_id,
            content,
        } = &chunk.kind
        else {
            panic!("second compaction event must be the summary: {chunk:?}");
        };
        assert_eq!(compaction_id, start_id);
        assert_eq!(content, expected);
        let AgentEventKind::CompactionEnd {
            compaction_id,
            summary,
            context_tokens,
            ..
        } = &end.kind
        else {
            panic!("successful compaction must end after its summary: {end:?}");
        };
        assert_eq!(compaction_id.as_ref(), Some(start_id));
        assert_eq!(summary, expected);
        assert!(context_tokens.unwrap() > 0);
        assert_compaction_journal_matches(&config, &exec_ctx.session_id, &compaction_events).await;
    }

    mod accounting_tests {
        use super::*;
        use crate::accounting::{AccountingReason, AttemptPurpose, CycleAccounting};

        fn usage() -> querymt::Usage {
            querymt::Usage {
                input_tokens: 100,
                output_tokens: 10,
                reasoning_tokens: 5,
                cache_read: 50,
                cache_write: 20,
                cache_write_tiers: None,
            }
        }

        fn response(text: &str) -> ChatOutput {
            let mut out = ChatOutput::from_projections(
                None,
                Some(text.into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            out.usage = Some(usage());
            out
        }

        async fn totals(exec: &ExecutionContext) -> crate::accounting::ScopeTotals {
            exec.cycle_accounting
                .get()
                .expect("compaction opens the cycle's accounting")
                .session_totals()
        }

        #[tokio::test]
        async fn successful_compaction_charges_the_initiating_cycle_as_compaction_purpose() {
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .unwrap();

            let cycle = exec_ctx.cycle_accounting.get().unwrap();
            let by_cycle = cycle.cycle_totals();
            assert_eq!(by_cycle.priced_attempts, 1);
            assert_eq!(by_cycle.unresolved_attempts, 0);
            let accounting = cycle.session_accounting();
            let compaction = accounting.read(|r| r.purpose(AttemptPurpose::Compaction));
            assert_eq!(compaction.unresolved_attempts, 0);
            assert_eq!(
                accounting.read(|r| r.purpose(AttemptPurpose::MainResponse)),
                Default::default(),
                "auxiliary spend is not main-response spend"
            );
            // Usage was reported, so the attempt is a settled metered attempt
            // even though the mock catalog has no price for it.
            assert_eq!(totals(&exec_ctx).await.priced_attempts, 1);
        }

        #[tokio::test]
        async fn rejected_summary_and_retry_both_settle_before_failure() {
            // Both mock calls return an empty summary: validation rejects each.
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), Some(response("  "))).await;
            let mut events = config.subscribe_events();
            let error = run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .expect_err("empty summary must fail");
            assert!(error.to_string().contains("empty summary"), "{error:#}");
            let _ = take_compaction_events(&mut events);

            let t = totals(&exec_ctx).await;
            assert_eq!(t.unresolved_attempts, 0, "every attempt is settled");
            assert_eq!(
                t.priced_attempts, 2,
                "the refused summary and its retry both cost"
            );
            assert!(
                t.reasons()
                    .iter()
                    .all(|r| matches!(r, AccountingReason::Valuation { .. })),
                "uncertainty is only about missing prices: {:?}",
                t.reasons()
            );
        }

        #[tokio::test]
        async fn compaction_reduces_effective_context_while_increasing_spend() {
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            let before = state.context().unwrap().stats.clone();
            assert_eq!(before.context_tokens, 100_000);

            let next = run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .unwrap();
            let after = next.context().unwrap().stats.clone();

            // Effective context shrinks to the post-compaction estimate ...
            assert!(after.context_tokens < before.context_tokens);
            assert!(after.context_tokens > 0);
            // ... while spend (the ledger) grows.
            let t = totals(&exec_ctx).await;
            assert_eq!(t.priced_attempts, 1);
            assert!(t.tokens.input > 0);

            // The summary's own usage never replaces main usage or steps.
            assert_eq!(after.steps, before.steps);
            assert_eq!(after.total_input_tokens, before.total_input_tokens);
            assert_eq!(after.total_output_tokens, before.total_output_tokens);
            assert_eq!(after.reasoning_tokens, before.reasoning_tokens);
            assert_eq!(after.cache_read_tokens, before.cache_read_tokens);
            assert_eq!(after.cache_write_tokens, before.cache_write_tokens);
            assert_ne!(
                after.context_tokens as u64,
                u64::from(usage().input_tokens + usage().output_tokens),
                "context is not the summary request's usage"
            );
        }

        #[tokio::test]
        async fn auxiliary_tokens_are_a_separate_purpose_total() {
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .unwrap();
            let accounting = exec_ctx
                .cycle_accounting
                .get()
                .unwrap()
                .session_accounting();
            let compaction = accounting.read(|r| r.purpose(AttemptPurpose::Compaction));
            let main = accounting.read(|r| r.purpose(AttemptPurpose::MainResponse));
            assert_eq!(compaction.tokens.input, 100);
            assert_eq!(compaction.tokens.reasoning, 5);
            assert_eq!(compaction.tokens.cache_read, 50);
            assert_eq!(compaction.tokens.cache_write, 20);
            assert_eq!(main.tokens, Default::default());
            assert_eq!(accounting.read(|r| r.session().tokens), compaction.tokens);
        }

        #[tokio::test]
        async fn hook_provided_summary_dispatches_nothing_and_charges_nothing() {
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(custom_summary_hooks("Hook summary", None), None).await;
            run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .unwrap();
            assert!(
                exec_ctx.cycle_accounting.get().is_none(),
                "no provider request, so no accounting was opened"
            );
        }

        #[tokio::test]
        async fn maintenance_cycle_has_its_own_identity_and_session_totals() {
            let (config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), Some(response("Summary"))).await;
            // Compaction outside a prompt runs under a dedicated maintenance cycle.
            let accounting = exec_ctx
                .runtime
                .accounting(&exec_ctx.session_id, &config.event_sink)
                .await
                .unwrap();
            let maintenance = CycleAccounting::begin_maintenance(accounting.clone());
            assert!(maintenance.is_maintenance());
            let id = maintenance.cycle_id().to_string();
            exec_ctx = exec_ctx.with_cycle_accounting(maintenance);

            run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .unwrap();
            assert_eq!(accounting.read(|r| r.cycle(&id).priced_attempts), 1);

            // A later prompt cycle starts at zero while session spend remains.
            let prompt_cycle = CycleAccounting::begin(accounting);
            assert!(!prompt_cycle.is_maintenance());
            assert_eq!(prompt_cycle.cycle_totals(), Default::default());
            assert_eq!(prompt_cycle.session_totals().priced_attempts, 1);
        }

        #[tokio::test]
        async fn budget_refusal_ends_compaction_as_a_price_limit_stop() {
            // No provider call is expected: the fixture's mock fails on any.
            let (mut config, mut exec_ctx, state, _tempdir) =
                fixture(Hooks::disabled(), None).await;
            // A zero cap is already reached by a cycle with no spend.
            config
                .middleware_drivers
                .push(Arc::new(crate::middleware::LimitsMiddleware::new(
                    crate::middleware::LimitsConfig::default().max_price_usd(0.0),
                )));
            let mut events = config.subscribe_events();

            let next = run_ai_compaction(&config, &mut exec_ctx, &state)
                .await
                .expect("a refused dispatch is a stop, not an error");
            let ExecutionState::Stopped {
                stop_type, message, ..
            } = next
            else {
                panic!("expected a stop, got {next:?}");
            };
            assert_eq!(stop_type, StopType::PriceLimit);
            assert!(message.contains("Price limit reached"), "{message}");

            // The started compaction still gets exactly one terminal event.
            let compaction_events = take_compaction_events(&mut events);
            assert!(
                matches!(
                    compaction_events.last().map(|e| &e.kind),
                    Some(AgentEventKind::CompactionFailed { .. })
                ),
                "{compaction_events:?}"
            );
            // Nothing was dispatched, so nothing was recorded or charged.
            let t = totals(&exec_ctx).await;
            assert_eq!(t.priced_attempts, 0);
            assert_eq!(t.unresolved_attempts, 0);
        }
    }
}
