//! Post-turn maintenance operations
//!
//! This module handles pruning and AI compaction of conversation history to manage
//! context window size.

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution::Stage;
use crate::agent::execution::TurnError;
use crate::agent::execution::turn::{Turn, TurnEnv};
use crate::agent::utils::u32_from_usize;
use crate::hooks::{PostCompactionRequest, PreCompactionRequest, SummarySource};
use crate::middleware::Stop;
use crate::session::compaction::SessionCompaction;
use crate::session::provider::CompactionModel;
use log::{debug, info, warn};
use querymt_agent_core::events::{AgentEventKind, StopType};
use querymt_agent_core::model::MessagePart;
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
    emit_compaction_ended_unsuccessfully(config, session_id, compaction_id, error, false).await;
}

/// [`emit_compaction_failure`] for a compaction the run's cancellation ended.
async fn emit_compaction_cancelled(
    config: &AgentConfig,
    session_id: &str,
    compaction_id: &str,
    error: &anyhow::Error,
) {
    emit_compaction_ended_unsuccessfully(config, session_id, compaction_id, error, true).await;
}

async fn emit_compaction_ended_unsuccessfully(
    config: &AgentConfig,
    session_id: &str,
    compaction_id: &str,
    error: &anyhow::Error,
    cancelled: bool,
) {
    emit_compaction_event(
        config,
        session_id,
        AgentEventKind::CompactionFailed {
            compaction_id: compaction_id.to_string(),
            reason: format!("{error:#}"),
            cancelled,
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
/// Uses the cycle the compaction was initiated from. The provider, model and
/// billing basis are those the request is sent to (`target`), which
/// `[agent.execution.compaction]` may have moved off the session's.
fn compaction_ledger(
    config: &AgentConfig,
    env: &TurnEnv,
    target: &CompactionModel,
) -> querymt_agent_services::accounting::DispatchLedger {
    let cycle = &env.cycle_accounting;
    let recorder = Arc::new(
        querymt_agent_services::accounting::AttemptRecorder::with_projection(
            config.event_sink.session_stream(&env.session_id),
            cycle.session_accounting().clone(),
        ),
    );
    querymt_agent_services::accounting::DispatchLedger {
        operation: Arc::new(
            querymt_agent_services::accounting::AttemptOperation::new(
                recorder,
                cycle,
                querymt_agent_services::accounting::AttemptPurpose::Compaction,
                target.provider_name.as_str(),
                target.model.as_str(),
            )
            .with_budget_pricing(config.budget_pricing())
            .with_budget_cap(config.budget_cap_usd()),
        ),
        basis: target.basis.clone(),
    }
}

/// Run pruning on tool results to reduce context size.
///
/// This marks low-value tool results as compacted based on the pruning configuration.
/// Pruning is a lightweight operation that can run after every turn.
pub(super) async fn run_pruning(config: &AgentConfig, env: &TurnEnv) -> Result<(), anyhow::Error> {
    let session_id = &env.session_id;
    let messages = env
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
        crate::session::pruning::content_cost_estimator_for_llm_config(env.llm_config());
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

/// Hook trigger for compaction started by the context-threshold middleware.
const TRIGGER_CONTEXT_THRESHOLD: &str = "context_threshold";
/// Hook trigger for compaction requested by the user (`/compact`).
pub(crate) const TRIGGER_MANUAL: &str = "manual";

/// How a compaction attempt ended without an error.
#[derive(Debug)]
pub(crate) enum CompactionOutcome {
    /// A summary boundary was stored and the run's history is the new
    /// effective history. Carries its estimated token count.
    Completed { context_tokens: usize },
    /// A pre-compaction hook blocked the compaction.
    Blocked { message: String },
    /// The cycle budget refused the paid compaction request.
    PriceLimit { message: String },
    /// The session has no history to compact.
    NothingToCompact,
}

/// Run AI-powered compaction on the conversation history.
///
/// This generates a summary of old messages and injects it into the conversation,
/// then returns a new execution state with the compacted context. This is a heavier
/// operation typically triggered when context thresholds are hit.
pub(super) async fn run_ai_compaction(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Stage, TurnError> {
    let outcome = compact_history(config, env, turn, TRIGGER_CONTEXT_THRESHOLD, None).await?;
    let new_context_tokens = match outcome {
        CompactionOutcome::Completed { context_tokens } => context_tokens,
        CompactionOutcome::Blocked { message } => {
            return Ok(Stage::Stopping(Stop::new(
                StopType::ContextThreshold,
                message,
            )));
        }
        CompactionOutcome::PriceLimit { message } => {
            return Ok(Stage::Stopping(Stop::new(StopType::PriceLimit, message)));
        }
        CompactionOutcome::NothingToCompact => {
            return Err(anyhow::anyhow!("No history available for compaction").into());
        }
    };
    debug!(
        "Post-compaction context tokens updated: {} -> {} ({} messages)",
        turn.stats.context_tokens,
        new_context_tokens,
        turn.history.as_slice().len()
    );
    // Compaction may have removed the stored reminder and the tool baseline;
    // the history now holds what survived, so the next request stores them
    // again if needed.
    turn.stats.context_tokens = new_context_tokens;
    Ok(Stage::BeforeRequest)
}

/// Summarize the effective history and store the summary boundary.
///
/// Shared by automatic (context threshold) and manual (`/compact`)
/// compaction. Emits the full compaction event lifecycle, runs the
/// pre/post-compaction hooks with `trigger`, and records the paid request
/// against the initiating cycle. `instructions` are optional user focus
/// instructions added to the summary prompt.
pub(crate) async fn compact_history(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
    trigger: &str,
    instructions: Option<&str>,
) -> Result<CompactionOutcome, anyhow::Error> {
    compact_history_with(config, env, turn, trigger, instructions, None).await
}

/// [`compact_history`] for a run whose session provider could not be built.
///
/// Native `/compact` is the one run that can make progress without its main
/// model: a `pre_compaction` hook may supply the summary, and the policy may
/// send the request to a different, working provider. So the hook runs
/// first, and `main_model_error` (the provider-initialization failure, with
/// `env` holding [`crate::session::provider::ResolvedModel::unavailable`])
/// ends the compaction only when the summary has to come from the session's
/// own provider. It does so before `CompactionStart`, with the provider
/// failure itself, as when the model is resolved up front.
pub(crate) async fn compact_history_with(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
    trigger: &str,
    instructions: Option<&str>,
    main_model_error: Option<TurnError>,
) -> Result<CompactionOutcome, anyhow::Error> {
    let session_id = &env.session_id;
    // The run's history is the stored one: nothing else writes it.
    let messages = turn.history.as_slice().to_vec();
    if messages.is_empty() {
        return Ok(CompactionOutcome::NothingToCompact);
    }

    env.turn_authority().require_provider_dispatch()?;
    let prompt_limit = env.execution_config().and_then(|cfg| cfg.max_prompt_bytes);
    let tool_changes = env.model.profile.capabilities.tool_updates;
    let token_estimate =
        crate::session::pruning::estimate_history_tokens(&messages, prompt_limit, tool_changes)
            .map_err(|e| anyhow::anyhow!("Invalid prompt content in history: {e}"))?;

    let turn_id = env.turn_id().to_string();
    // Where the summary is requested from, by name: what the compaction hooks
    // are told, and what is built (only when no hook supplies the summary)
    // for the request. A policy that cannot name one is refused before the
    // compaction starts.
    let target = env
        .session_handle
        .compaction_target(&config.execution_policy.compaction)
        .map_err(|error| anyhow::anyhow!("Compaction target unavailable: {error}"))?;
    let token_estimate_u32 = u32_from_usize(token_estimate, "token_estimate", Some(session_id));
    let message_count = u32_from_usize(messages.len(), "messages.len", Some(session_id));

    env.require_turn_authority().await?;
    let pre_hook = config
        .hooks
        .run_pre_compaction(PreCompactionRequest {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            cwd: env.cwd().map(|path| path.to_path_buf()),
            provider: target.provider.clone(),
            model: target.model.clone(),
            permission_mode: env.permission_mode().to_string(),
            trigger: trigger.to_string(),
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
        return Ok(CompactionOutcome::Blocked { message: reason });
    }

    // No hook summary and no model to ask: the session's own provider is the
    // one that failed to build, so say so now, before the compaction starts.
    if let Some(error) = main_model_error
        && pre_hook.custom_summary.is_none()
        && target.is_session_model()
    {
        return Err(anyhow::Error::from(error));
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

    let summary_source = if pre_hook.custom_summary.is_some() {
        SummarySource::Hook
    } else {
        SummarySource::Model
    };
    let mut result = if let Some(summary) = pre_hook.custom_summary {
        crate::session::compaction::CompactionResult {
            summary_token_count: summary.len() / 4,
            summary,
            original_token_count: token_estimate,
        }
    } else {
        // A provider that cannot be built ends the started compaction like
        // any other failure. Building another provider can wait (credential
        // refresh, a mesh lookup), so the run's cancellation stops it: no
        // request is sent, and the started compaction still gets its
        // terminal event.
        let built = tokio::select! {
            biased;
            () = env.cancellation_token.cancelled() => {
                let error = anyhow::anyhow!("Compaction cancelled while preparing its provider");
                emit_compaction_cancelled(config, session_id, &compaction_id, &error).await;
                return Err(error);
            }
            built = env.session_handle.build_compaction_model(&target) => built,
        };
        let built = match built {
            Ok(built) => built,
            Err(error) => {
                let error = anyhow::anyhow!("Compaction provider unavailable: {error}");
                emit_compaction_failure(config, session_id, &compaction_id, &error).await;
                return Err(error);
            }
        };
        // Compaction is a paid request: its attempts are recorded against the
        // initiating cycle and session, never inferred from message history.
        let ledger = compaction_ledger(config, env, &built);
        match config
            .compaction
            .process(
                &messages,
                prompt_limit,
                instructions,
                &crate::session::compaction::CompactionDispatch {
                    provider: built.provider,
                    config,
                    cancel: &env.cancellation_token,
                    ledger: &ledger,
                },
            )
            .await
        {
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
                    return Ok(CompactionOutcome::PriceLimit {
                        message: format!("Price limit reached: ${spent:.4} >= ${cap:.2}"),
                    });
                }
                let error = anyhow::anyhow!("Compaction failed: {}", error);
                if env.cancellation_token.is_cancelled() {
                    emit_compaction_cancelled(config, session_id, &compaction_id, &error).await;
                } else {
                    emit_compaction_failure(config, session_id, &compaction_id, &error).await;
                }
                return Err(error);
            }
        }
    };

    // A cancel that arrived as the summary finished must not store a boundary:
    // the user asked for the compaction to stop.
    if env.cancellation_token.is_cancelled() {
        let error = anyhow::anyhow!("Compaction cancelled");
        emit_compaction_cancelled(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    // A hook-provided summary must be usable on its own, not rescued by post-hook notes.
    if let Err(error) = SessionCompaction::validate_summary(&result.summary) {
        let error = anyhow::Error::from(error);
        emit_compaction_failure(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    // Appended after validation, like the verbatim prompts, so it can never
    // rescue an empty summary.
    if let Some(section) =
        crate::session::compaction::active_task_section(turn.state.active_task.as_ref())
    {
        use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
        result.summary_token_count = result
            .summary_token_count
            .saturating_add(SimpleTokenEstimator.estimate(&section));
        result.summary.push_str("\n\n");
        result.summary.push_str(&section);
    }

    if let Err(error) = env.require_turn_authority().await {
        let error = anyhow::Error::from(error);
        emit_compaction_failure(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }
    let post_hook = match config
        .hooks
        .run_post_compaction(PostCompactionRequest {
            session_id: session_id.clone(),
            turn_id,
            cwd: env.cwd().map(|path| path.to_path_buf()),
            provider: target.provider.clone(),
            model: target.model.clone(),
            permission_mode: env.permission_mode().to_string(),
            trigger: trigger.to_string(),
            summary_source,
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

    // The post-compaction hook can take a while: a cancel that arrived during
    // it must still stop the compaction before any boundary is written.
    if env.cancellation_token.is_cancelled() {
        let error = anyhow::anyhow!("Compaction cancelled");
        emit_compaction_cancelled(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    info!(
        "Compaction generated summary: {} tokens -> {} tokens",
        result.original_token_count, result.summary_token_count
    );

    let (request_msg, summary_msg) = SessionCompaction::create_compaction_messages(
        session_id,
        &result.summary,
        result.original_token_count,
    );

    if let Err(error) = turn
        .history
        .commit(&env.session_handle, vec![request_msg, summary_msg])
        .await
    {
        let error = anyhow::anyhow!("Failed to store compaction boundary: {}", error);
        emit_compaction_failure(config, session_id, &compaction_id, &error).await;
        return Err(error);
    }

    // Re-establish a bounded set of recently relevant repository cards under
    // the new compaction epoch. Additive and fail-open.
    let restored =
        crate::agent::repository_delivery::post_compaction_contexts(config, env, turn, &messages)
            .await;
    if !restored.is_empty() {
        let mut message = querymt_agent_core::model::AgentMessage::new(
            session_id.clone(),
            querymt::chat::ChatRole::User,
        );
        message.parts = restored
            .into_iter()
            .map(|context| MessagePart::HookContext {
                event_name: context.event_name,
                handler_id: context.handler_id,
                tool_use_id: context.tool_use_id,
                content: context.content,
            })
            .collect();
        if let Err(error) = turn
            .history
            .commit(&env.session_handle, vec![message])
            .await
        {
            warn!("failed to store post-compaction repository context: {error}");
        }
    }

    // The boundary closes what came before it, as a load of the store would.
    turn.history.keep_effective();
    let new_context_tokens = match crate::session::pruning::estimate_history_tokens(
        turn.history.as_slice(),
        prompt_limit,
        tool_changes,
    ) {
        Ok(tokens) => tokens,
        Err(error) => {
            let error = anyhow::anyhow!("Invalid prompt content after compaction: {error}");
            emit_compaction_failure(config, session_id, &compaction_id, &error).await;
            return Err(error);
        }
    };

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

    Ok(CompactionOutcome::Completed {
        context_tokens: new_context_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compacts the history the store holds: the run's history is loaded
    /// first, as a run does when it starts.
    async fn compact_stored(
        config: &AgentConfig,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> Result<Stage, TurnError> {
        turn.history = super::super::turn::History::load(&env.session_handle).await?;
        run_ai_compaction(config, env, turn).await
    }
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::{McpToolState, SessionRuntime};
    use crate::hooks::{
        HookCommandConfig, HookHandlerConfig, Hooks, HooksConfig, MatcherGroupConfig,
    };
    use crate::session::RuntimeContext;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::session::store::SessionExecutionConfig;
    use crate::test_utils::helpers::mock_plugin_registry;
    use crate::test_utils::mocks::{MockLlmProvider, SharedLlmProvider, TestProviderFactory};
    use querymt::LLMParams;
    use querymt::chat::{ChatOutput, ChatRole, FinishReason};
    use querymt_agent_core::events::{DurableEvent, EventEnvelope};
    use querymt_agent_core::model::AgentMessage;

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
    ) -> (AgentConfig, TurnEnv, Turn, tempfile::TempDir) {
        let mut mock = MockLlmProvider::new();
        if let Some(output) = output {
            // The configured single retry may call the provider at most twice.
            mock.expect_chat()
                .times(1..=2)
                .returning(move |_| Ok(output.clone()));
        } else {
            mock.expect_chat().times(0);
        }
        fixture_with_mock(hooks, mock).await
    }

    async fn fixture_with_mock(
        hooks: Hooks,
        mock: MockLlmProvider,
    ) -> (AgentConfig, TurnEnv, Turn, tempfile::TempDir) {
        let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
            mock,
            vec![],
        )));
        let (registry, tempdir) = mock_plugin_registry(factory).unwrap();
        fixture_with_registry(hooks, registry, tempdir).await
    }

    async fn fixture_with_registry(
        hooks: Hooks,
        registry: querymt::plugin::host::PluginRegistry,
        tempdir: tempfile::TempDir,
    ) -> (AgentConfig, TurnEnv, Turn, tempfile::TempDir) {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let mut config = AgentConfigBuilder::new(
            Arc::new(registry),
            storage,
            LLMParams::new().provider("mock").model("mock"),
        )
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
        let (env, mut turn) =
            crate::test_utils::turn_parts(&config, session_handle, runtime, runtime_context).await;
        turn.stats.context_tokens = 100_000;
        (config, env, turn, tempdir)
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
        let (config, env, mut turn, _tempdir) = fixture(hooks, output).await;
        let before = config
            .provider
            .history_store()
            .get_history(&env.session_id)
            .await
            .unwrap();
        let mut events = config.subscribe_events();
        let error = compact_stored(&config, &env, &mut turn)
            .await
            .expect_err("compaction must fail");
        // Do not yield: all lifecycle events must be published before compaction returns.
        let compaction_events = take_compaction_events(&mut events);
        assert!(error.to_string().contains(expected_error), "{error:#}");
        let after = config
            .provider
            .history_store()
            .get_history(&env.session_id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
        let effective = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&effective).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(turn.stats.context_tokens, 100_000);

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
        assert_compaction_journal_matches(&config, &env.session_id, &compaction_events).await;
        assert!(!crate::session::compaction::has_compaction(&after));
    }

    /// `session-compaction / Retry wait during compaction is reported`: a
    /// provider error during the compaction request is retried through the
    /// shared retry, so clients see the wait and resume events of a model
    /// request that waits, and nothing else is added to the lifecycle.
    #[tokio::test]
    async fn retry_wait_during_compaction_is_reported() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let mut mock = MockLlmProvider::new();
        mock.expect_chat().times(2).returning(move |_| {
            if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(querymt::error::LLMError::HttpStatus {
                    status_code: 503,
                    message: "unavailable".into(),
                    // The provider names no wait: the test does not sleep.
                    retry_after_secs: Some(0),
                })
            } else {
                Ok(ChatOutput::from_projections(
                    None,
                    Some("A usable summary".into()),
                    None,
                    None,
                    Some(FinishReason::Stop),
                ))
            }
        });
        let (config, env, mut turn, _tempdir) = fixture_with_mock(Hooks::disabled(), mock).await;
        turn.history = super::super::turn::History::load(&env.session_handle)
            .await
            .unwrap();
        let mut events = config.subscribe_events();

        let outcome = compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
            .await
            .expect("the retry succeeds");

        assert!(matches!(outcome, CompactionOutcome::Completed { .. }));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let mut retry_events = Vec::new();
        loop {
            match events.try_recv() {
                Ok(EventEnvelope::Durable(event)) => {
                    if matches!(
                        event.kind,
                        AgentEventKind::LlmRetryWait { .. }
                            | AgentEventKind::LlmRetryResume { .. }
                            | AgentEventKind::RateLimited { .. }
                            | AgentEventKind::RateLimitResume { .. }
                    ) {
                        retry_events.push(event.kind);
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(error) => panic!("failed to receive event: {error}"),
            }
        }
        let [
            AgentEventKind::LlmRetryWait {
                attempt: wait_attempt,
                max_attempts,
                wait_secs,
                ..
            },
            AgentEventKind::LlmRetryResume {
                attempt: resume_attempt,
            },
        ] = retry_events.as_slice()
        else {
            panic!("expected one wait and one resume event, got {retry_events:?}");
        };
        assert_eq!((*wait_attempt, *resume_attempt), (1, 2));
        assert_eq!(*max_attempts, 3, "rate_limit.max_retries is the budget");
        assert_eq!(*wait_secs, 0);
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
        let (mut config, env, mut turn, _tempdir) = fixture(Hooks::disabled(), Some(output)).await;
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
            .get_history(&env.session_id)
            .await
            .unwrap();
        turn.history = super::super::turn::History::load(&env.session_handle)
            .await
            .unwrap();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            super::super::run_turn(
                &config,
                &env,
                &mut turn,
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
            .get_history(&env.session_id)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
    }

    #[tokio::test]
    async fn compaction_drops_a_tool_baseline_the_history_no_longer_holds() {
        let (config, env, mut turn, _tempdir) =
            fixture(custom_summary_hooks("Summary", None), None).await;
        // The run holds a baseline that was stored before the compaction, and
        // a tool change stored after it.
        let mut baseline = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        baseline.parts.push(MessagePart::ToolBaseline {
            tools: Vec::new(),
            origin: "mock/mock".into(),
        });
        let mut change = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        change.parts.push(MessagePart::ToolChange {
            added: Vec::new(),
            removed: vec!["gone".into()],
        });
        turn.history
            .commit(&env.session_handle, vec![baseline, change])
            .await
            .unwrap();
        assert!(turn.history.tool_baseline().is_some());

        compact_stored(&config, &env, &mut turn).await.unwrap();

        // The effective history starts at the summary, so a load finds no
        // baseline: the run must not keep the old one, or the next request
        // would lead with a list no stored baseline records.
        assert!(turn.history.tool_baseline().is_none());
        assert!(
            !turn
                .history
                .as_slice()
                .iter()
                .flat_map(|message| &message.parts)
                .any(MessagePart::is_tool_record)
        );
        let loaded = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(turn.history.as_slice()).unwrap(),
            serde_json::to_value(&loaded).unwrap(),
            "the run holds what a load gives it"
        );
    }

    fn main_provider_failure() -> TurnError {
        TurnError::Provider {
            operation: "provider initialization",
            provider: "mock".into(),
            model: "mock".into(),
            source: querymt::error::LLMError::InvalidRequest("cannot build".into()),
        }
    }

    /// Native `/compact` with a session provider that cannot be built: the
    /// `pre_compaction` hook runs first, and a summary it supplies is stored
    /// without any provider (the mock expects no request).
    #[tokio::test]
    async fn hook_summary_is_stored_when_the_session_provider_cannot_be_built() {
        let (config, mut env, mut turn, _tempdir) =
            fixture(custom_summary_hooks("Summary from the hook", None), None).await;
        env.model = crate::session::provider::ResolvedModel::unavailable("cannot build");
        turn.history = super::super::turn::History::load(&env.session_handle)
            .await
            .unwrap();

        let outcome = compact_history_with(
            &config,
            &env,
            &mut turn,
            TRIGGER_MANUAL,
            None,
            Some(main_provider_failure()),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, CompactionOutcome::Completed { .. }));
        let effective = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&effective)
                .unwrap()
                .contains("Summary from the hook")
        );
    }

    /// Without a hook summary the request would go to the session's own
    /// provider, the one that failed: the provider failure is the error,
    /// before the compaction starts.
    #[tokio::test]
    async fn provider_failure_is_reported_before_the_compaction_starts() {
        let (config, mut env, mut turn, _tempdir) = fixture(Hooks::disabled(), None).await;
        env.model = crate::session::provider::ResolvedModel::unavailable("cannot build");
        turn.history = super::super::turn::History::load(&env.session_handle)
            .await
            .unwrap();
        let mut events = config.subscribe_events();

        let error = compact_history_with(
            &config,
            &env,
            &mut turn,
            TRIGGER_MANUAL,
            None,
            Some(main_provider_failure()),
        )
        .await
        .unwrap_err();

        let error = TurnError::from(error);
        assert!(
            matches!(
                error.llm_error(),
                Some(querymt::error::LLMError::InvalidRequest(_))
            ),
            "the provider failure survives: {error}"
        );
        assert!(take_compaction_events(&mut events).is_empty());
    }

    #[tokio::test]
    async fn valid_custom_summary_is_stored_and_emitted_as_success() {
        let (config, env, mut turn, _tempdir) = fixture(
            custom_summary_hooks("Valid continuation summary", Some("Keep the tests passing")),
            None,
        )
        .await;
        let mut events = config.subscribe_events();
        let result = compact_stored(&config, &env, &mut turn).await.unwrap();
        let compaction_events = take_compaction_events(&mut events);
        let expected = "Valid continuation summary\n\nKeep the tests passing";
        let effective = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        assert_eq!(effective.len(), 2);
        assert!(
            matches!(&effective[1].parts[0], MessagePart::Compaction { summary, .. } if summary == expected)
        );
        assert!(matches!(result, Stage::BeforeRequest));
        assert!(turn.stats.context_tokens > 0);
        assert_eq!(
            serde_json::to_value(turn.history.as_slice()).unwrap(),
            serde_json::to_value(&effective).unwrap(),
            "the run holds the effective history"
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
        assert_compaction_journal_matches(&config, &env.session_id, &compaction_events).await;
    }

    /// The estimate of the request the stored history of `env`'s session
    /// would be sent as, without its tail: what every history estimate must
    /// equal.
    async fn request_estimate(env: &TurnEnv) -> usize {
        let history = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        let projected = querymt_agent_core::projection::project_request(
            &history,
            &querymt_agent_core::projection::RequestTarget {
                output: None,
                max_prompt_bytes: None,
                tool_changes: true,
            },
        )
        .unwrap();
        crate::session::pruning::estimate_chat_tokens(&projected.messages)
            + projected
                .baseline_tools
                .filter(|tools| !tools.is_empty())
                .map_or(0, |tools| {
                    crate::session::pruning::tool_json_tokens(
                        serde_json::to_vec(tools).unwrap().len(),
                    )
                })
    }

    async fn compaction_start_figure(
        config: &AgentConfig,
        env: &TurnEnv,
        turn: &mut Turn,
    ) -> usize {
        let mut events = config.subscribe_events();
        compact_stored(config, env, turn).await.unwrap();
        let compaction_events = take_compaction_events(&mut events);
        let AgentEventKind::CompactionStart { token_estimate, .. } = &compaction_events[0].kind
        else {
            panic!("first compaction event must be a start: {compaction_events:?}");
        };
        *token_estimate as usize
    }

    #[tokio::test]
    async fn compaction_start_counts_stored_notices() {
        let (config, env, mut turn, _tempdir) =
            fixture(custom_summary_hooks("Summary", None), None).await;
        let notice_text = "The instructions from the earlier mode no longer apply. ".repeat(10);
        let mut notice = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        notice.parts.push(MessagePart::Notice {
            source: querymt_agent_core::model::NoticeSource::AgentMode,
            content: notice_text.clone(),
        });
        env.session_handle.add_message(notice).await.unwrap();
        let before_notice = request_estimate(&env).await;

        let figure = compaction_start_figure(&config, &env, &mut turn).await;

        assert_eq!(figure, before_notice);
        let original = "Keep the implemented shader and verified test results.";
        assert!(
            figure >= original.len() / 4 + notice_text.len() / 4,
            "the notice is counted: {figure}"
        );
    }

    /// run-history-consistency / Compaction figure equals the request estimate.
    #[tokio::test]
    async fn compaction_figure_equals_the_request_estimate() {
        let (config, mut env, mut turn) = {
            let (config, env, turn, _tempdir) =
                fixture(custom_summary_hooks("Summary", None), None).await;
            std::mem::forget(_tempdir);
            (config, env, turn)
        };
        let session_id = env.session_id.clone();
        let mut steering = AgentMessage::new(session_id.clone(), ChatRole::User);
        steering.parts.push(MessagePart::Steering {
            run_id: "run".into(),
            client_input_id: None,
            blocks: vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new("use another file name, please"),
            )],
        });
        let mut reply = AgentMessage::new(session_id.clone(), ChatRole::Assistant);
        reply.parts.push(MessagePart::Reasoning {
            item: querymt::chat::ChatReasoningItem {
                id: None,
                summary: Vec::new(),
                content: vec![querymt::chat::ChatReasoningPart::text("r".repeat(256))],
                encrypted_content: None,
                signature: None,
                status: None,
                extensions: Default::default(),
            },
            time_ms: None,
        });
        reply.parts.push(MessagePart::Text {
            content: "Done.".into(),
        });
        let mut baseline = AgentMessage::new(session_id.clone(), ChatRole::User);
        baseline.parts.push(MessagePart::ToolBaseline {
            tools: Vec::new(),
            origin: "mock/mock".into(),
        });
        let mut change = AgentMessage::new(session_id, ChatRole::User);
        change.parts.push(MessagePart::ToolChange {
            added: vec![querymt::chat::Tool {
                tool_type: "function".into(),
                function: querymt::chat::FunctionTool {
                    name: "big".into(),
                    description: "d".repeat(4_000),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }],
            removed: Vec::new(),
        });
        for message in [steering, reply, baseline, change] {
            env.session_handle.add_message(message).await.unwrap();
        }
        let expected = request_estimate(&env).await;
        // A provider that takes tool changes is sent the definitions.
        Arc::make_mut(&mut env.model.profile)
            .capabilities
            .tool_updates = true;

        let figure = compaction_start_figure(&config, &env, &mut turn).await;

        assert_eq!(figure, expected);
        assert!(
            figure > 1_000,
            "the 4,000-byte definition is counted: {figure}"
        );
    }

    /// A provider without `tool_updates` is never sent tool records, so the
    /// figure does not count them.
    #[tokio::test]
    async fn compaction_figure_leaves_out_tool_records_for_a_provider_without_tool_updates() {
        let (config, env, mut turn) = {
            let (config, env, turn, _tempdir) =
                fixture(custom_summary_hooks("Summary", None), None).await;
            std::mem::forget(_tempdir);
            (config, env, turn)
        };
        let mut text = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        text.parts.push(MessagePart::Text {
            content: "hello".into(),
        });
        let mut baseline = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        baseline.parts.push(MessagePart::ToolBaseline {
            tools: vec![querymt::chat::Tool {
                tool_type: "function".into(),
                function: querymt::chat::FunctionTool {
                    name: "big".into(),
                    description: "d".repeat(4_000),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }],
            origin: "mock/mock".into(),
        });
        for message in [text, baseline] {
            env.session_handle.add_message(message).await.unwrap();
        }
        assert!(!env.model.profile.capabilities.tool_updates);

        let figure = compaction_start_figure(&config, &env, &mut turn).await;

        assert!(
            figure < 100,
            "the baseline's tool list is not counted: {figure}"
        );
    }

    #[tokio::test]
    async fn invalid_stored_attachment_prevents_model_request_preparation() {
        use super::super::transitions::{ContextTrigger, transition_before_llm_call};
        use crate::acp::protocol::{ContentBlock, ImageContent};

        let (config, env, mut turn, _tempdir) = fixture(Hooks::default(), None).await;
        let mut message = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        message.parts.push(MessagePart::Prompt {
            blocks: vec![ContentBlock::Image(ImageContent::new("%%%", "image/png"))],
        });
        env.session_handle.add_message(message).await.unwrap();
        turn.history = super::super::turn::History::load(&env.session_handle)
            .await
            .unwrap();

        let result =
            transition_before_llm_call(&config, &env, &mut turn, ContextTrigger::UserPrompt, None)
                .await;
        assert!(
            matches!(result, Err(error) if error.to_string().contains("Invalid stored message content"))
        );
    }

    /// run-history-consistency / Tool-change definitions are counted.
    #[tokio::test]
    async fn tool_change_definitions_are_counted() {
        let (_config, env, _turn, _tempdir) =
            fixture(custom_summary_hooks("Summary", None), None).await;
        let session_id = env.session_id.clone();
        let mut baseline = AgentMessage::new(session_id.clone(), ChatRole::User);
        baseline.parts.push(MessagePart::ToolBaseline {
            tools: Vec::new(),
            origin: "mock/mock".into(),
        });
        env.session_handle.add_message(baseline).await.unwrap();
        let without = request_estimate(&env).await;
        let definition = querymt::chat::Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: "big".into(),
                description: "d".repeat(4_000),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        };
        let mut change = AgentMessage::new(session_id, ChatRole::User);
        change.parts.push(MessagePart::ToolChange {
            added: vec![definition.clone()],
            removed: Vec::new(),
        });
        env.session_handle.add_message(change).await.unwrap();

        let with = request_estimate(&env).await;
        let history = env
            .session_handle
            .get_effective_agent_history()
            .await
            .unwrap();
        let shared =
            crate::session::pruning::estimate_history_tokens(&history, None, true).unwrap();

        let definition_tokens = serde_json::to_string(&definition)
            .unwrap()
            .len()
            .div_ceil(4);
        assert!(with >= without + definition_tokens, "{without} -> {with}");
        assert_eq!(shared, with, "the shared estimate counts it the same way");
    }

    mod accounting_tests {
        use super::*;
        use querymt_agent_services::accounting::{
            AccountingReason, AttemptPurpose, CycleAccounting,
        };

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

        async fn totals(env: &TurnEnv) -> querymt_agent_services::accounting::ScopeTotals {
            env.cycle_accounting.session_totals()
        }

        #[tokio::test]
        async fn successful_compaction_charges_the_initiating_cycle_as_compaction_purpose() {
            let (config, env, mut turn, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            compact_stored(&config, &env, &mut turn).await.unwrap();

            let cycle = &env.cycle_accounting;
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
            assert_eq!(totals(&env).await.priced_attempts, 1);
        }

        #[tokio::test]
        async fn rejected_summary_and_retry_both_settle_before_failure() {
            // Both mock calls return an empty summary: validation rejects each.
            let (config, env, mut turn, _tempdir) =
                fixture(Hooks::disabled(), Some(response("  "))).await;
            let mut events = config.subscribe_events();
            let error = compact_stored(&config, &env, &mut turn)
                .await
                .expect_err("empty summary must fail");
            assert!(error.to_string().contains("empty summary"), "{error:#}");
            let _ = take_compaction_events(&mut events);

            let t = totals(&env).await;
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
            let (config, env, mut turn, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            let before = turn.stats.clone();
            assert_eq!(before.context_tokens, 100_000);

            let next = compact_stored(&config, &env, &mut turn).await.unwrap();
            assert!(matches!(next, Stage::BeforeRequest));
            let after = turn.stats.clone();

            // Effective context shrinks to the post-compaction estimate ...
            assert!(after.context_tokens < before.context_tokens);
            assert!(after.context_tokens > 0);
            // ... while spend (the ledger) grows.
            let t = totals(&env).await;
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
            let (config, env, mut turn, _tempdir) =
                fixture(Hooks::disabled(), Some(response("A usable summary"))).await;
            compact_stored(&config, &env, &mut turn).await.unwrap();
            let accounting = env.cycle_accounting.session_accounting();
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
            let (config, env, mut turn, _tempdir) =
                fixture(custom_summary_hooks("Hook summary", None), None).await;
            compact_stored(&config, &env, &mut turn).await.unwrap();
            assert_eq!(
                env.cycle_accounting.cycle_totals(),
                Default::default(),
                "no provider request, so nothing was charged"
            );
        }

        #[tokio::test]
        async fn maintenance_cycle_has_its_own_identity_and_session_totals() {
            let (config, mut env, mut turn, _tempdir) =
                fixture(Hooks::disabled(), Some(response("Summary"))).await;
            // Compaction outside a prompt runs under a dedicated maintenance cycle.
            let accounting = env
                .runtime
                .accounting(&env.session_id, &config.event_sink)
                .await
                .unwrap();
            let maintenance = CycleAccounting::begin_maintenance(accounting.clone());
            assert!(maintenance.is_maintenance());
            let id = maintenance.cycle_id().to_string();
            env.cycle_accounting = maintenance;

            compact_stored(&config, &env, &mut turn).await.unwrap();
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
            let (mut config, env, mut turn, _tempdir) = fixture(Hooks::disabled(), None).await;
            // A zero cap is already reached by a cycle with no spend.
            config
                .middleware_drivers
                .push(Arc::new(crate::middleware::LimitsMiddleware::new(
                    crate::middleware::LimitsConfig::default().max_price_usd(0.0),
                )));
            let mut events = config.subscribe_events();

            let next = compact_stored(&config, &env, &mut turn)
                .await
                .expect("a refused dispatch is a stop, not an error");
            let Stage::Stopping(Stop { stop_type, message }) = next else {
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
            let t = totals(&env).await;
            assert_eq!(t.priced_attempts, 0);
            assert_eq!(t.unresolved_attempts, 0);
        }
    }

    /// `[agent.execution.compaction]` `provider` / `model` choose where the
    /// compaction request is sent; what the request runs on, what its
    /// accounting attempts name and what it is priced for are one and the same.
    mod compaction_target_tests {
        use super::*;
        use querymt::LLMProvider;
        use querymt::error::LLMError;
        use querymt::plugin::LLMProviderFactory;
        use querymt_agent_core::events::AgentEventKind;

        /// A provider named `mock` that answers `summary from <model>` for the
        /// model it was built for, and records each model it is built for.
        struct ModelEchoFactory {
            built: Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl LLMProviderFactory for ModelEchoFactory {
            fn name(&self) -> &str {
                "mock"
            }

            fn config_schema(&self) -> String {
                r#"{"type":"object","properties":{"model":{"type":"string"}}}"#.to_string()
            }

            fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
                let model = serde_json::from_str::<serde_json::Value>(cfg)
                    .ok()
                    .and_then(|value| value["model"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                self.built.lock().unwrap().push(model.clone());
                let mut mock = MockLlmProvider::new();
                mock.expect_chat().returning(move |_| {
                    Ok(crate::test_utils::MockChatResponse::text_only(&format!(
                        "summary from {model}"
                    ))
                    .into())
                });
                Ok(Box::new(SharedLlmProvider::new(mock, vec![])))
            }

            fn list_models<'a>(
                &'a self,
                _cfg: &str,
            ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
                Box::pin(async { Ok(vec!["mock".to_string()]) })
            }
        }

        /// A session bound to model `mock`, whose compaction policy is `edit`.
        async fn fixture_with_policy(
            edit: impl FnOnce(&mut crate::config::CompactionConfig),
        ) -> (
            AgentConfig,
            TurnEnv,
            Turn,
            tempfile::TempDir,
            Arc<std::sync::Mutex<Vec<String>>>,
        ) {
            fixture_with_policy_and_hooks(Hooks::disabled(), edit).await
        }

        async fn fixture_with_policy_and_hooks(
            hooks: Hooks,
            edit: impl FnOnce(&mut crate::config::CompactionConfig),
        ) -> (
            AgentConfig,
            TurnEnv,
            Turn,
            tempfile::TempDir,
            Arc<std::sync::Mutex<Vec<String>>>,
        ) {
            let built = Arc::new(std::sync::Mutex::new(Vec::new()));
            let dir = tempfile::TempDir::new().unwrap();
            // Two providers, `mock` (the session's) and `other`, served by the
            // same echo factory.
            let wasm = dir.path().join("mock.wasm");
            std::fs::write(&wasm, "").unwrap();
            let wasm = format!("{:?}", wasm.display().to_string());
            let config_path = dir.path().join("providers.toml");
            std::fs::write(
                &config_path,
                format!(
                    "[[providers]]\nname = \"mock\"\npath = {wasm}\n\n\
                     [[providers]]\nname = \"other\"\npath = {wasm}\n"
                ),
            )
            .unwrap();
            let mut registry =
                querymt::plugin::host::PluginRegistry::from_path(&config_path).unwrap();
            registry.register_loader(Box::new(crate::test_utils::mocks::TestPluginLoader {
                factory: Arc::new(ModelEchoFactory {
                    built: built.clone(),
                }),
            }));
            let (mut config, env, mut turn, dir) =
                fixture_with_registry(hooks, registry, dir).await;
            // The session's own provider is built by the run before it compacts.
            env.session_handle.provider().await.unwrap();
            edit(&mut config.execution_policy.compaction);
            turn.history = super::super::super::turn::History::load(&env.session_handle)
                .await
                .unwrap();
            (config, env, turn, dir, built)
        }

        /// Model and provider of every attempt of the session's journal.
        async fn attempts(config: &AgentConfig, session_id: &str) -> Vec<(String, String)> {
            config
                .event_sink
                .journal()
                .load_session_stream(session_id, None, None)
                .await
                .unwrap()
                .into_iter()
                .filter_map(|event| match event.kind {
                    AgentEventKind::LlmAttemptStarted { start } => {
                        Some((start.provider, start.model))
                    }
                    _ => None,
                })
                .collect()
        }

        async fn stored_summary_mentions(env: &TurnEnv, text: &str) -> bool {
            let history = env.session_handle.get_agent_history().await.unwrap();
            serde_json::to_string(&history).unwrap().contains(text)
        }

        #[tokio::test]
        async fn without_a_policy_the_request_runs_on_the_session_model() {
            let (config, env, mut turn, _dir, built) = fixture_with_policy(|_| {}).await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            assert_eq!(*built.lock().unwrap(), ["mock"], "no second provider");
            assert!(stored_summary_mentions(&env, "summary from mock").await);
            assert_eq!(
                attempts(&config, &env.session_id).await,
                [("mock".to_string(), "mock".to_string())]
            );
        }

        #[tokio::test]
        async fn a_compaction_model_is_what_the_request_runs_on_and_is_recorded() {
            let (config, env, mut turn, _dir, built) =
                fixture_with_policy(|policy| policy.model = Some("cheap".into())).await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            assert_eq!(*built.lock().unwrap(), ["mock", "cheap"]);
            assert!(
                stored_summary_mentions(&env, "summary from cheap").await,
                "the cheaper model answered"
            );
            assert!(!stored_summary_mentions(&env, "summary from mock").await);
            assert_eq!(
                attempts(&config, &env.session_id).await,
                [("mock".to_string(), "cheap".to_string())],
                "the attempt names the model that ran"
            );
        }

        #[tokio::test]
        async fn a_compaction_provider_and_model_are_resolved_and_recorded_together() {
            let (config, env, mut turn, _dir, built) = fixture_with_policy(|policy| {
                policy.provider = Some("other".into());
                policy.model = Some("cheap".into());
            })
            .await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            assert_eq!(*built.lock().unwrap(), ["mock", "cheap"]);
            assert!(stored_summary_mentions(&env, "summary from cheap").await);
            assert_eq!(
                attempts(&config, &env.session_id).await,
                [("other".to_string(), "cheap".to_string())],
                "the attempt names the provider and model that ran"
            );
        }

        #[tokio::test]
        async fn the_session_model_is_unchanged_after_a_compaction_model_ran() {
            let (config, env, mut turn, _dir, _built) =
                fixture_with_policy(|policy| policy.model = Some("cheap".into())).await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            assert_eq!(env.model_name(), "mock");
            let again = env.session_handle.provider().await.unwrap();
            assert!(Arc::ptr_eq(&again, &env.model.provider));
        }

        /// A policy built in code (a file is refused when it loads) that
        /// names a provider without a model is refused before the compaction
        /// starts: no event, no hook, no provider, no request.
        #[tokio::test]
        async fn a_compaction_provider_needs_a_model() {
            let (dir_hooks, hooks) = recording_hooks(None);
            let (config, env, mut turn, _dir, built) =
                fixture_with_policy_and_hooks(hooks, |policy| {
                    policy.provider = Some("other".into())
                })
                .await;
            let mut events = config.subscribe_events();

            let error = compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .expect_err("the session's model belongs to another provider");

            let text = format!("{error:#}");
            assert!(text.contains("needs a compaction model"), "{text}");
            assert_eq!(*built.lock().unwrap(), ["mock"], "nothing was built");
            assert!(attempts(&config, &env.session_id).await.is_empty());
            assert!(
                take_compaction_events(&mut events).is_empty(),
                "a compaction that never started has no lifecycle"
            );
            assert!(!dir_hooks.path().join("pre.json").exists(), "no hook ran");
        }

        #[tokio::test]
        async fn a_compaction_provider_with_the_sessions_own_name_still_needs_a_model() {
            let (config, env, mut turn, _dir, built) =
                fixture_with_policy(|policy| policy.provider = Some("mock".into())).await;

            let error = compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .expect_err("the configuration file would be refused as well");

            assert!(format!("{error:#}").contains("needs a compaction model"));
            assert_eq!(*built.lock().unwrap(), ["mock"]);
        }

        /// Hooks that record their stdin under a temporary directory.
        /// `custom_summary` makes the pre-compaction hook supply the summary.
        fn recording_hooks(custom_summary: Option<&str>) -> (tempfile::TempDir, Hooks) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().display();
            let pre_output = match custom_summary {
                Some(summary) => serde_json::json!({"hook_specific_output": {
                    "hook_event_name": "pre_compaction", "compaction": {"summary": summary}
                }})
                .to_string(),
                None => "{}".to_string(),
            };
            let group = |file: &str, output: &str| {
                vec![MatcherGroupConfig {
                    matcher: None,
                    hooks: vec![HookHandlerConfig::Command(HookCommandConfig {
                        command: format!("cat > '{path}/{file}'; printf '%s' '{output}'"),
                        timeout_sec: Some(5),
                        ..HookCommandConfig::default()
                    })],
                }]
            };
            let hooks = Hooks::new(HooksConfig {
                enabled: true,
                pre_compaction: group("pre.json", &pre_output),
                post_compaction: group("post.json", "{}"),
                ..HooksConfig::default()
            })
            .unwrap();
            (dir, hooks)
        }

        /// What the two compaction hooks were told: `(provider, model)` of
        /// each, and the post hook's `summary_source`.
        fn hook_payloads(dir: &tempfile::TempDir) -> [serde_json::Value; 2] {
            ["pre.json", "post.json"].map(|file| {
                let text = std::fs::read_to_string(dir.path().join(file))
                    .unwrap_or_else(|error| panic!("{file} was not written: {error}"));
                serde_json::from_str(&text).unwrap()
            })
        }

        #[tokio::test]
        async fn the_hooks_name_the_session_model_without_a_policy() {
            let (hook_dir, hooks) = recording_hooks(None);
            let (config, env, mut turn, _dir, _built) =
                fixture_with_policy_and_hooks(hooks, |_| {}).await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            let [pre, post] = hook_payloads(&hook_dir);
            for payload in [&pre, &post] {
                assert_eq!(payload["provider"], "mock");
                assert_eq!(payload["model"], "mock");
            }
            assert_eq!(post["summary_source"], "model");
        }

        #[tokio::test]
        async fn the_hooks_name_the_compaction_model() {
            let (hook_dir, hooks) = recording_hooks(None);
            let (config, env, mut turn, _dir, _built) =
                fixture_with_policy_and_hooks(hooks, |policy| policy.model = Some("cheap".into()))
                    .await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            let [pre, post] = hook_payloads(&hook_dir);
            for payload in [&pre, &post] {
                assert_eq!(payload["provider"], "mock", "the session's provider");
                assert_eq!(payload["model"], "cheap");
            }
            assert_eq!(post["summary_source"], "model");
        }

        #[tokio::test]
        async fn the_hooks_name_the_compaction_provider_and_model() {
            let (hook_dir, hooks) = recording_hooks(None);
            let (config, env, mut turn, _dir, _built) =
                fixture_with_policy_and_hooks(hooks, |policy| {
                    policy.provider = Some("other".into());
                    policy.model = Some("cheap".into());
                })
                .await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            let [pre, post] = hook_payloads(&hook_dir);
            for payload in [&pre, &post] {
                assert_eq!(payload["provider"], "other");
                assert_eq!(payload["model"], "cheap");
            }
        }

        /// A pre-compaction hook that supplies the summary means no request:
        /// no provider is built for the compaction model, and the post hook
        /// still names the model that would have been asked.
        #[tokio::test]
        async fn a_hook_summary_builds_no_provider_and_is_marked_as_the_hooks() {
            let (hook_dir, hooks) = recording_hooks(Some("A summary the hook wrote"));
            let (config, env, mut turn, _dir, built) =
                fixture_with_policy_and_hooks(hooks, |policy| policy.model = Some("cheap".into()))
                    .await;

            compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .unwrap();

            assert_eq!(*built.lock().unwrap(), ["mock"], "no model was built");
            assert!(attempts(&config, &env.session_id).await.is_empty());
            let [pre, post] = hook_payloads(&hook_dir);
            assert_eq!(pre["model"], "cheap");
            assert_eq!(post["model"], "cheap");
            assert_eq!(post["summary_source"], "hook");
            assert!(stored_summary_mentions(&env, "A summary the hook wrote").await);
        }

        #[tokio::test]
        async fn an_unknown_compaction_provider_fails_the_compaction_without_a_request() {
            let (config, env, mut turn, _dir, built) = fixture_with_policy(|policy| {
                policy.provider = Some("not-a-provider".into());
                policy.model = Some("m".into());
            })
            .await;

            let error = compact_history(&config, &env, &mut turn, TRIGGER_MANUAL, None)
                .await
                .expect_err("no such provider");

            assert!(
                format!("{error:#}").contains("Compaction provider unavailable"),
                "{error:#}"
            );
            assert_eq!(*built.lock().unwrap(), ["mock"]);
            assert!(attempts(&config, &env.session_id).await.is_empty());
            let after = env.session_handle.get_agent_history().await.unwrap();
            assert!(!crate::session::compaction::has_compaction(&after));
        }
    }

    // ── Repository knowledge re-evaluation after compaction (5.10) ──────────

    struct Repo {
        dir: tempfile::TempDir,
        cache: tempfile::TempDir,
    }

    fn repo() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        for (path, body) in [
            (
                ".qmt/knowledge.toml",
                "version = 1\n[documents]\ninclude = [\"docs/**/*.md\", \"src/**/AGENTS.md\"]\n[code]\ninclude = [\"src/**/*.rs\"]\n",
            ),
            ("src/queue.rs", "pub fn push_task() {}\n"),
            (
                "src/AGENTS.md",
                "# Source\n\n| File | Purpose |\n|---|---|\n| `queue.rs` | Priority queue. |\n",
            ),
            (
                "docs/arch.md",
                "# Architecture\n## Scheduling\nThe scheduler dispatches queued tasks to workers using a priority queue.\n",
            ),
        ] {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, body).unwrap();
        }
        Repo {
            dir,
            cache: tempfile::tempdir().unwrap(),
        }
    }

    async fn knowledge_fixture(repo: &Repo) -> (AgentConfig, TurnEnv, Turn, tempfile::TempDir) {
        let (mut config, mut env, turn, tempdir) = fixture(
            custom_summary_hooks("Valid continuation summary", None),
            None,
        )
        .await;
        let settings = crate::config::RepositoryKnowledgeSettings {
            delivery: crate::config::RepositoryKnowledgeDeliverySettings {
                orientation: true,
                index_context: true,
                ..Default::default()
            },
            ..crate::config::RepositoryKnowledgeSettings::enabled()
        };
        config.repository_knowledge =
            crate::knowledge::repository::RepositoryKnowledgeRegistry::from_settings_in(
                &settings,
                repo.cache.path().join("config"),
            );
        env.runtime = SessionRuntime::new(
            Some(repo.dir.path().to_path_buf()),
            Default::default(),
            McpToolState::empty(),
        );
        // The task in progress and a successful index call before compaction.
        let mut task = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        task.parts.push(MessagePart::Prompt {
            blocks: vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(
                    "the scheduler dispatches queued tasks to workers",
                ),
            )],
        });
        env.session_handle.add_message(task).await.unwrap();
        let mut indexed = AgentMessage::new(env.session_id.clone(), ChatRole::User);
        indexed.parts.push(MessagePart::ToolResult {
            call_id: "c1".into(),
            content: vec![querymt::chat::ToolResultPart::text("outline")],
            is_error: false,
            tool_name: Some("index".into()),
            tool_arguments: Some(r#"{"path":"src/queue.rs"}"#.into()),
            compacted_at: None,
        });
        // The live stage enriched this call, which is what makes it eligible
        // for re-evaluation after compaction. The text is stale on purpose:
        // re-selection must come from current sources, never from this card.
        indexed.parts.push(MessagePart::HookContext {
            event_name: "post_tool_use".into(),
            handler_id: "qmt.repository_knowledge.index_context".into(),
            tool_use_id: Some("c1".into()),
            content:
                "[repository-knowledge version=old epoch=0 card=src/queue.rs#old]\nSTALE-INDEX"
                    .into(),
        });
        env.session_handle.add_message(indexed).await.unwrap();
        (config, env, turn, tempdir)
    }

    async fn stored_cards(env: &TurnEnv) -> Vec<String> {
        env.session_handle
            .get_effective_agent_history()
            .await
            .unwrap()
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                MessagePart::HookContext { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn compaction_redelivers_recent_cards_under_the_new_epoch_from_current_sources() {
        let repo = repo();
        let (config, env, mut turn, _td) = knowledge_fixture(&repo).await;
        compact_stored(&config, &env, &mut turn).await.unwrap();

        let cards = stored_cards(&env).await;
        // Bounded by the shared per-turn budget (two cards by default).
        assert!(!cards.is_empty() && cards.len() <= 2, "{cards:?}");
        assert!(cards.iter().all(|c| c.contains("epoch=1")), "{cards:?}");
        assert!(
            cards
                .iter()
                .any(|c| c.contains("src/AGENTS.md") || c.contains("docs/arch.md"))
        );
    }

    #[tokio::test]
    async fn changed_and_removed_documents_are_not_resurrected_from_old_payloads() {
        let repo = repo();
        let (config, env, mut turn, _td) = knowledge_fixture(&repo).await;
        // An old card for the note sits in pre-compaction history.
        let old = {
            let mut m = AgentMessage::new(env.session_id.clone(), ChatRole::User);
            m.parts.push(MessagePart::HookContext {
                event_name: "user_prompt_submit".into(),
                handler_id: "qmt.repository_knowledge.orientation".into(),
                tool_use_id: None,
                content:
                    "[repository-knowledge version=old epoch=0 card=docs/arch.md#x]\nSTALE-TEXT"
                        .into(),
            });
            m
        };
        env.session_handle.add_message(old).await.unwrap();
        std::fs::write(
            repo.dir.path().join("docs/arch.md"),
            "# Architecture\n## Scheduling\nThe scheduler now uses a fair queue for dispatching tasks to workers.\n",
        )
        .unwrap();
        std::fs::remove_file(repo.dir.path().join("src/AGENTS.md")).unwrap();

        compact_stored(&config, &env, &mut turn).await.unwrap();
        let cards = stored_cards(&env).await;
        assert!(cards.iter().all(|c| !c.contains("STALE-TEXT")), "{cards:?}");
        assert!(cards.iter().any(|c| c.contains("fair queue")), "{cards:?}");
        assert!(
            cards.iter().all(|c| !c.contains("Priority queue.")),
            "removed purpose row must not return: {cards:?}"
        );
    }

    #[tokio::test]
    async fn retained_cards_are_not_duplicated_by_a_second_evaluation() {
        let repo = repo();
        let (config, env, mut turn, _td) = knowledge_fixture(&repo).await;
        compact_stored(&config, &env, &mut turn).await.unwrap();
        let first = stored_cards(&env).await;
        assert!(!first.is_empty());

        let before = env.session_handle.get_agent_history().await.unwrap();
        let again = crate::agent::repository_delivery::post_compaction_contexts(
            &config, &env, &mut turn, &before,
        )
        .await;
        assert!(
            again.iter().all(|c| !first.contains(&c.content)),
            "a retained card version is not delivered twice: {again:?}"
        );
    }

    fn index_call_result(call_id: &str) -> AgentMessage {
        let mut m = AgentMessage::new("s".to_string(), ChatRole::User);
        m.parts.push(MessagePart::ToolResult {
            call_id: call_id.into(),
            content: vec![querymt::chat::ToolResultPart::text("outline")],
            is_error: false,
            tool_name: Some("index".into()),
            tool_arguments: Some(r#"{"path":"src/queue.rs"}"#.into()),
            compacted_at: None,
        });
        m
    }

    #[tokio::test]
    async fn only_index_calls_that_received_a_native_card_are_re_evaluated() {
        let repo = repo();
        let (config, env, mut turn, _td) = knowledge_fixture(&repo).await;

        // No persisted index-context card names this call (a foreign `index`
        // tool, or a result a hook suppressed): nothing is recovered for it.
        let unenriched = vec![index_call_result("foreign")];
        let none = crate::agent::repository_delivery::post_compaction_contexts(
            &config,
            &env,
            &mut turn,
            &unenriched,
        )
        .await;
        assert!(none.is_empty(), "{none:?}");

        // The same call, once the live stage enriched it, is re-evaluated.
        let mut enriched = index_call_result("c1");
        enriched.parts.push(MessagePart::HookContext {
            event_name: "post_tool_use".into(),
            handler_id: "qmt.repository_knowledge.index_context".into(),
            tool_use_id: Some("c1".into()),
            content: "[repository-knowledge version=old epoch=0 card=x]\nold".into(),
        });
        let some = crate::agent::repository_delivery::post_compaction_contexts(
            &config,
            &env,
            &mut turn,
            &[enriched],
        )
        .await;
        assert!(!some.is_empty(), "an enriched call is re-evaluated");
    }

    #[tokio::test]
    async fn restore_counts_only_native_cards_and_reads_history_once() {
        use crate::knowledge::repository::budget::Refusal;
        let repo = repo();
        let (config, env, mut turn, _td) = knowledge_fixture(&repo).await;
        let card = |handler: &str, id: &str| crate::hooks::HookContextContribution {
            event_name: "post_tool_use".into(),
            handler_id: handler.into(),
            tool_use_id: None,
            content: format!("[repository-knowledge version=v1 card={id}]\nbody"),
        };
        let stored = |handler: &str, id: &str| {
            let c = card(handler, id);
            let mut m = AgentMessage::new(env.session_id.clone(), ChatRole::User);
            m.parts.push(MessagePart::HookContext {
                event_name: c.event_name,
                handler_id: c.handler_id,
                tool_use_id: c.tool_use_id,
                content: c.content,
            });
            m
        };
        // A foreign hook imitating a marker, and a genuine native card.
        for m in [
            stored("user-hook", "imitated"),
            stored("qmt.repository_knowledge.orientation", "native"),
        ] {
            env.session_handle.add_message(m).await.unwrap();
        }

        crate::agent::repository_delivery::restore_state(&config, &env, &mut turn).await;
        // History added after the first restore is not re-read.
        env.session_handle
            .add_message(stored("qmt.repository_knowledge.orientation", "late"))
            .await
            .unwrap();
        crate::agent::repository_delivery::restore_state(&config, &env, &mut turn).await;

        let budget = turn.delivery_budget.as_mut().expect("restored");
        let native = "qmt.repository_knowledge.orientation";
        assert!(
            budget.admit(&card(native, "imitated")).is_ok(),
            "a foreign hook must not suppress a real card"
        );
        assert_eq!(
            budget.admit(&card(native, "native")),
            Err(Refusal::AlreadyDelivered)
        );
        assert!(
            budget.admit(&card(native, "late")).is_ok(),
            "restore reads history once per budget"
        );
    }

    #[tokio::test]
    async fn without_repository_knowledge_compaction_adds_no_context() {
        let (config, env, mut turn, _td) = fixture(
            custom_summary_hooks("Valid continuation summary", None),
            None,
        )
        .await;
        compact_stored(&config, &env, &mut turn).await.unwrap();
        assert!(stored_cards(&env).await.is_empty());
    }
}
