//! Post-turn maintenance operations
//!
//! This module handles pruning and AI compaction of conversation history to manage
//! context window size.

use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::events::AgentEventKind;
use crate::middleware::ExecutionState;
use crate::model::MessagePart;
use crate::session::compaction::SessionCompaction;
use log::{debug, info};
use std::sync::Arc;

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

    let estimator = crate::session::pruning::SimpleTokenEstimator;
    let analysis =
        crate::session::pruning::compute_prune_candidates(&messages, &prune_config, &estimator);

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
    exec_ctx: &ExecutionContext,
    current_state: &ExecutionState,
) -> Result<ExecutionState, anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    let messages = exec_ctx
        .session_handle
        .get_agent_history()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get agent history: {}", e))?;

    let token_estimate = messages
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .map(|p| match p {
                    MessagePart::Text { content } => content.len() / 4,
                    MessagePart::ToolResult { content, .. } => content.len() / 4,
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum();

    config.emit_event(
        session_id,
        AgentEventKind::CompactionStart { token_estimate },
    );

    let llm_provider = exec_ctx
        .session_handle
        .provider()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get LLM provider: {}", e))?;

    let llm_config = exec_ctx
        .llm_config()
        .ok_or_else(|| anyhow::anyhow!("No LLM config for session"))?;

    let model = config
        .execution_policy
        .compaction
        .model
        .as_ref()
        .unwrap_or(&llm_config.model);

    let retry_config = crate::session::compaction::RetryConfig {
        max_retries: config.execution_policy.compaction.retry.max_retries,
        initial_backoff_ms: config.execution_policy.compaction.retry.initial_backoff_ms,
        backoff_multiplier: config.execution_policy.compaction.retry.backoff_multiplier,
    };

    let result = config
        .compaction
        .process(
            &messages,
            llm_provider,
            model,
            &retry_config,
            exec_ctx
                .execution_config()
                .and_then(|cfg| cfg.max_prompt_bytes),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Compaction failed: {}", e))?;

    info!(
        "Compaction generated summary: {} tokens -> {} tokens",
        result.original_token_count, result.summary_token_count
    );

    let compaction_msg = SessionCompaction::create_compaction_message(
        session_id,
        &result.summary,
        result.original_token_count,
    );

    exec_ctx
        .add_message(compaction_msg)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store compaction message: {}", e))?;

    config.emit_event(
        session_id,
        AgentEventKind::CompactionEnd {
            summary: result.summary.clone(),
            summary_len: result.summary.len(),
        },
    );

    let new_messages = exec_ctx
        .session_handle
        .get_agent_history()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get new history: {}", e))?;
    let filtered_messages = crate::session::compaction::filter_to_effective_history(new_messages);

    // Convert AgentMessages to ChatMessages for the ConversationContext
    let prompt_limit = exec_ctx
        .execution_config()
        .and_then(|cfg| cfg.max_prompt_bytes);
    let chat_messages: Vec<querymt::chat::ChatMessage> = filtered_messages
        .iter()
        .map(|m| m.to_chat_message_with_max_prompt_bytes(prompt_limit))
        .collect();

    let new_context_tokens = config
        .compaction
        .estimate_messages_tokens(&filtered_messages, prompt_limit);

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
            .with_session_mode(ctx.session_mode),
        )
    } else {
        return Err(anyhow::anyhow!("No context available for compaction"));
    };

    Ok(ExecutionState::BeforeLlmCall {
        context: new_context,
    })
}
