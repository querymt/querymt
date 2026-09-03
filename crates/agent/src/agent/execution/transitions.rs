//! State transition functions for the execution state machine
//!
//! This module contains the logic for transitioning between execution states:
//! - BeforeLlmCall → CallLlm (tool collection)
//! - CallLlm → AfterLlm (LLM invocation)
//! - AfterLlm → ProcessingToolCalls or Complete (response handling)
//! - ProcessingToolCalls → BeforeLlmCall or WaitingForEvent (parallel tool execution)

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::session_actor::ensure_pre_turn_snapshot_ready;
use crate::agent::utils::u32_from_usize;
use crate::events::{AgentEventKind, ExecutionMetrics, StopType};
use crate::middleware::{
    ExecutionState, LlmResponse, PreparedModelRequest, ToolCall as MiddlewareToolCall,
    ToolFunction, ToolResult, calculate_context_tokens,
};
use crate::model::{AgentMessage, MessagePart};
use anyhow::Context as _;
use futures_util::StreamExt;
use futures_util::future::join_all;
use log::{debug, trace, warn};
use querymt::ToolCall;
use querymt::chat::{CacheHint, ChatMessage, ChatRole, FinishReason, StreamChunk};
use querymt::error::LLMError;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{Instrument, info_span, instrument};
use uuid::Uuid;

/// Transition from BeforeLlmCall to CallLlm.
///
/// This collects available tools, computes their hash, and emits a ToolsAvailable event
/// if the tool set has changed.
#[instrument(
    name = "agent.transition.before_llm_call",
    skip(config, context, exec_ctx),
    fields(
        session_id = %exec_ctx.session_id,
        provider = %context.provider,
        model = %context.model,
        steps = context.stats.steps
    )
)]
pub(super) async fn transition_before_llm_call(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "BeforeLlmCall: session={}, steps={}",
        exec_ctx.session_id, context.stats.steps
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    let provider = match super::llm_retry::call_with_retry(
        config,
        &exec_ctx.session_id,
        &exec_ctx.cancellation_token,
        || {
            let session_handle = exec_ctx.session_handle.clone();
            let cancel = exec_ctx.cancellation_token.clone();
            async move {
                tokio::select! {
                    result = session_handle.provider() => result.map_err(LLMError::from),
                    _ = cancel.cancelled() => Err(LLMError::Cancelled),
                }
            }
        },
    )
    .await
    {
        Ok(provider) => provider,
        Err(LLMError::Cancelled) => return Ok(ExecutionState::Cancelled),
        Err(error) => {
            return Err(contextualize_llm_error(
                error,
                "provider initialization",
                context,
            ));
        }
    };

    let tools = config.collect_tools(
        provider,
        Some(exec_ctx.runtime.as_ref()),
        Some(&exec_ctx.tool_config),
    );

    let tools_json =
        serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
    let new_hash = crate::hash::RapidHash::new(&tools_json);

    let current_hash = exec_ctx.runtime.mcp_tool_state.load().tools_hash;
    let changed = current_hash.is_none_or(|h| h != new_hash);
    if changed {
        exec_ctx.runtime.mcp_tool_state.rcu(|snap| {
            let mut s = snap.clone();
            s.tools_hash = Some(new_hash);
            s
        });
    }

    if changed {
        config.emit_event(
            &exec_ctx.session_id,
            crate::events::AgentEventKind::ToolsAvailable {
                tools: tools.clone(),
                tools_hash: new_hash,
            },
        );
    }

    let mut messages = context.request_messages();
    let trigger = if messages.last().is_some_and(|message| {
        message
            .content
            .iter()
            .any(querymt::chat::Content::is_tool_result)
    }) {
        "after_tool_batch"
    } else {
        "user_prompt"
    };
    let context_window = crate::model_info::get_model_info(&context.provider, &context.model)
        .and_then(|info| info.limits.context)
        .unwrap_or(u32::MAX as u64)
        .min(u32::MAX as u64) as u32;
    let hook_result = config
        .hooks
        .run_context(crate::hooks::ContextHookRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: exec_ctx.turn_id().unwrap_or_default().to_string(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: context.model.to_string(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            trigger: trigger.to_string(),
            context_window,
            messages,
        })
        .await?;
    for notice in hook_result.notices {
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    messages = hook_result.messages.unwrap_or_default();
    if hook_result.estimated_tokens > context_window as usize {
        return Ok(ExecutionState::Stopped {
            message: format!(
                "Prepared request is approximately {} tokens, exceeding the {} token context window",
                hook_result.estimated_tokens, context_window
            )
            .into(),
            stop_type: StopType::ContextThreshold,
            context: Some(context.clone()),
        });
    }
    let messages = apply_cache_breakpoints(&messages);
    Ok(ExecutionState::CallLlm {
        context: context.clone(),
        request: Arc::new(PreparedModelRequest {
            messages: Arc::from(messages.into_boxed_slice()),
            tools: Arc::from(tools.into_boxed_slice()),
            estimated_tokens: hook_result.estimated_tokens,
        }),
    })
}

/// Apply cache breakpoints to the last 2 messages in the conversation.
///
/// This enables prompt caching for the most recent context.
pub(super) fn apply_cache_breakpoints(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let len = messages.len();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            let mut m = msg.clone();
            if len >= 2 && i >= len - 2 {
                m.cache = Some(CacheHint::Ephemeral { ttl_seconds: None });
            }
            m
        })
        .collect()
}

fn contextualize_llm_error(
    error: LLMError,
    operation: &'static str,
    context: &crate::middleware::ConversationContext,
) -> anyhow::Error {
    let source_message = error.to_string();
    anyhow::Error::new(error).context(format!(
        "LLM {operation} error (provider={}, model={}): {source_message}",
        context.provider, context.model
    ))
}

/// Map a failed LLM setup/call into either `Cancelled` or a contextualized error.
///
/// Cancel must never bubble as `Err`; it is an execution state, not a provider failure.
fn map_failed_llm_call(
    error: LLMError,
    streaming: bool,
    context: &crate::middleware::ConversationContext,
) -> Result<ExecutionState, anyhow::Error> {
    match error {
        LLMError::Cancelled => Ok(ExecutionState::Cancelled),
        error => Err(contextualize_llm_error(
            error,
            if streaming { "streaming" } else { "chat" },
            context,
        )),
    }
}

fn validate_stream_terminal(
    finish_reason: FinishReason,
    tool_calls: &[ToolCall],
) -> Result<FinishReason, LLMError> {
    if finish_reason == FinishReason::ToolCalls && tool_calls.is_empty() {
        return Err(LLMError::from(
            querymt::error::ProviderFailure::new(
                querymt::error::ProviderErrorKind::UnknownTransient,
                "provider ended with tool_calls but emitted no completed tool calls",
            )
            .with_code(Some("empty_tool_calls_terminal".into())),
        ));
    }

    Ok(finish_reason)
}

/// Transition from CallLlm to AfterLlm.
///
/// This invokes the LLM (with or without tools), handles streaming for codex provider,
/// tracks usage/costs, and emits LlmRequestStart/End events.
#[instrument(
    name = "agent.transition.call_llm",
    skip(config, context, request, exec_ctx),
    fields(
        session_id = %exec_ctx.session_id,
        provider = %context.provider,
        model = %context.model,
        message_count = context.messages.len(),
        tool_count = request.tools.len()
    )
)]
pub(super) async fn transition_call_llm(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    request: &Arc<PreparedModelRequest>,
    exec_ctx: &ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    let request_messages = request.messages.as_ref();
    let tools = &request.tools;
    debug!(
        "CallLlm: session={}, messages={}",
        session_id,
        request_messages.len()
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    config.emit_event(
        session_id,
        AgentEventKind::LlmRequestStart {
            message_count: u32_from_usize(
                request_messages.len(),
                "request_messages.len",
                Some(session_id),
            ),
        },
    );

    let session_handle = &exec_ctx.session_handle;
    let messages_with_cache = request_messages;

    // Pre-allocated message_id for streaming path so that delta events and the
    // final AssistantMessageStored share the same ID.
    let mut streaming_message_id: Option<String> = None;

    // Determine response via streaming or non-streaming path.
    // Each arm produces the same tuple so the rest of the function is uniform.
    let (
        response_content,
        response_thinking,
        response_thinking_signature,
        tool_calls,
        usage,
        finish_reason,
    ) = if tools.is_empty() {
        // No tools — always use the non-streaming simple submit path.
        let cancel = exec_ctx.cancellation_token.clone();
        let resp = match super::llm_retry::call_with_retry(
            config,
            session_id,
            &exec_ctx.cancellation_token,
            || {
                let messages_with_cache = &messages_with_cache;
                let cancel = cancel.clone();
                async move {
                    tokio::select! {
                        result = session_handle.submit_request(messages_with_cache) => {
                            result
                        }
                        _ = cancel.cancelled() => {
                            Err(querymt::error::LLMError::Cancelled)
                        }
                    }
                }
            },
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => return map_failed_llm_call(e, false, context),
        };

        (
            resp.text().unwrap_or_default(),
            resp.thinking(),
            None,
            resp.tool_calls().unwrap_or_default(),
            resp.usage(),
            resp.finish_reason(),
        )
    } else {
        let provider = match super::llm_retry::call_with_retry(
            config,
            session_id,
            &exec_ctx.cancellation_token,
            || {
                let session_handle = session_handle.clone();
                let cancel = exec_ctx.cancellation_token.clone();
                async move {
                    tokio::select! {
                        result = session_handle.provider() => result.map_err(LLMError::from),
                        _ = cancel.cancelled() => Err(LLMError::Cancelled),
                    }
                }
            },
        )
        .await
        {
            Ok(provider) => provider,
            Err(LLMError::Cancelled) => return Ok(ExecutionState::Cancelled),
            Err(error) => {
                return Err(contextualize_llm_error(
                    error,
                    "provider initialization",
                    context,
                ));
            }
        };

        if provider.supports_streaming() {
            // === STREAMING PATH (all capable providers) ===
            let message_id = Uuid::new_v4().to_string();
            streaming_message_id = Some(message_id.clone());

            let max_stream_retries = config.execution_policy.rate_limit.max_stream_retries;

            // Accumulators live outside the retry loop so the post-stream
            // processing below can read them regardless of how many attempts
            // were needed. On each retry they are reset to empty.
            let mut text = String::new();
            let mut thinking = String::new();
            // Initial values are always overwritten inside the retry loop below
            // before first read, so suppress the unused-assignment lint.
            #[allow(unused_assignments)]
            let mut thinking_signature: Option<String> = None;
            let mut stream_tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_call_ids = std::collections::HashSet::new();
            #[allow(unused_assignments)]
            let mut usage: Option<querymt::Usage> = None;
            #[allow(unused_assignments)]
            let mut stream_finish_reason: Option<FinishReason> = None;

            // Batching buffers — we flush at most every 50ms or 256 chars to
            // avoid per-token React state updates on fast local models.
            let mut text_buffer = String::new();
            let mut thinking_buffer = String::new();
            #[allow(unused_assignments)]
            let mut last_flush = Instant::now();
            const BATCH_INTERVAL: Duration = Duration::from_millis(50);
            const BATCH_CHARS: usize = 256;

            macro_rules! flush_buffers {
                ($reset_timer:expr) => {
                    if !text_buffer.is_empty() {
                        let text_delta: String = text_buffer.drain(..).collect();
                        trace!(
                            "stream flush: session={} message_id={} text_delta_len={}",
                            session_id,
                            message_id,
                            text_delta.len()
                        );
                        config.emit_event(
                            session_id,
                            AgentEventKind::AssistantContentDelta {
                                content: text_delta,
                                message_id: message_id.clone(),
                            },
                        );
                    }
                    if !thinking_buffer.is_empty() {
                        let thinking_delta: String = thinking_buffer.drain(..).collect();
                        debug!(
                            "stream flush: session={} message_id={} thinking_delta_len={}",
                            session_id,
                            message_id,
                            thinking_delta.len()
                        );
                        config.emit_event(
                            session_id,
                            AgentEventKind::AssistantThinkingDelta {
                                content: thinking_delta,
                                message_id: message_id.clone(),
                            },
                        );
                    }
                    if $reset_timer {
                        #[allow(unused_assignments)]
                        {
                            last_flush = Instant::now();
                        }
                    }
                };
            }

            // ── Outer retry loop ─────────────────────────────────────────────
            // Preserve main's any-stage recreation behavior while enforcing one
            // shared physical-request budget across setup and parser failures.
            // TODO(stream-retry-safety): once clients can atomically replace or
            // roll back attempt-scoped deltas, eliminate possible duplicate output.
            let mut retry_budget = super::llm_retry::StreamRetryBudget::new(
                max_stream_retries,
                config.execution_policy.rate_limit.max_attempts(),
            );
            'stream: loop {
                let mut semantic_output_seen = false;

                // Reset accumulators on retry so we start fresh.
                text.clear();
                thinking.clear();
                thinking_signature = None;
                stream_tool_calls.clear();
                tool_call_ids.clear();
                usage = None;
                text_buffer.clear();
                thinking_buffer.clear();
                last_flush = Instant::now();

                let attempt = retry_budget
                    .reserve_attempt()
                    .expect("a retry is accepted only when another attempt is available");
                debug!(
                    "Session {}: starting physical stream request attempt {}/{}",
                    session_id,
                    attempt,
                    config.execution_policy.rate_limit.max_attempts(),
                );
                if exec_ctx.cancellation_token.is_cancelled() {
                    return Ok(ExecutionState::Cancelled);
                }

                let mut stream = match provider
                    .chat_stream_with_tools(messages_with_cache, Some(tools.as_ref()))
                    .await
                {
                    Ok(stream) => stream,
                    Err(error) => match super::llm_retry::handle_stream_failure(
                        config,
                        session_id,
                        error,
                        &mut retry_budget,
                        false,
                        Some(message_id.clone()),
                        &exec_ctx.cancellation_token,
                    )
                    .await
                    {
                        super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                        super::llm_retry::StreamFailureAction::Cancelled => {
                            return Ok(ExecutionState::Cancelled);
                        }
                        super::llm_retry::StreamFailureAction::Terminal(error) => {
                            return map_failed_llm_call(error, true, context);
                        }
                    },
                };

                // ── Inner consume loop ───────────────────────────────────────
                loop {
                    let item = super::llm_retry::next_stream_chunk(
                        &mut stream,
                        &exec_ctx.cancellation_token,
                    )
                    .await;

                    let chunk = match item {
                        Ok(chunk) => chunk,
                        Err(LLMError::RemoteStreamDisconnected { message }) => {
                            flush_buffers!(true);
                            config.emit_event(
                                session_id,
                                AgentEventKind::RemoteStreamDisconnected {
                                    message,
                                    message_id: Some(message_id.clone()),
                                },
                            );
                            continue;
                        }
                        Err(LLMError::RemoteStreamReconnected { message }) => {
                            config.emit_event(
                                session_id,
                                AgentEventKind::RemoteStreamReconnected {
                                    message,
                                    message_id: Some(message_id.clone()),
                                },
                            );
                            continue;
                        }
                        Err(error) => match super::llm_retry::handle_stream_failure(
                            config,
                            session_id,
                            error,
                            &mut retry_budget,
                            semantic_output_seen,
                            Some(message_id.clone()),
                            &exec_ctx.cancellation_token,
                        )
                        .await
                        {
                            super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                            super::llm_retry::StreamFailureAction::Cancelled => {
                                return Ok(ExecutionState::Cancelled);
                            }
                            super::llm_retry::StreamFailureAction::Terminal(error) => {
                                return Err(contextualize_llm_error(error, "streaming", context));
                            }
                        },
                    };

                    semantic_output_seen |= super::llm_retry::stream_chunk_commits_output(&chunk);

                    match chunk {
                        StreamChunk::Text(delta) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=text len={}",
                                session_id,
                                message_id,
                                delta.len()
                            );
                            text.push_str(&delta);
                            text_buffer.push_str(&delta);
                        }
                        StreamChunk::Thinking(delta) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=thinking len={}",
                                session_id,
                                message_id,
                                delta.len()
                            );
                            thinking.push_str(&delta);
                            thinking_buffer.push_str(&delta);
                        }
                        StreamChunk::ThinkingSignature(signature) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=thinking_signature len={}",
                                session_id,
                                message_id,
                                signature.len()
                            );
                            thinking_signature = Some(signature);
                        }
                        StreamChunk::ToolUseComplete { tool_call, .. } => {
                            // Flush before tool use so UI sees final text before tool starts
                            trace!(
                                "stream chunk: session={} message_id={} type=tool_use_complete id={}",
                                session_id, message_id, tool_call.id
                            );
                            flush_buffers!(true);
                            if tool_call_ids.insert(tool_call.id.clone()) {
                                stream_tool_calls.push(tool_call);
                            }
                        }
                        StreamChunk::Usage(u) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=usage input={} output={} reasoning={}",
                                session_id,
                                message_id,
                                u.input_tokens,
                                u.output_tokens,
                                u.reasoning_tokens
                            );
                            // Anthropic (and potentially other providers) split usage across
                            // multiple streaming events: `input_tokens` arrives in
                            // `message_start`, while cumulative `output_tokens` arrives in
                            // `message_delta`.  Taking the field-wise maximum merges both
                            // events correctly regardless of order.
                            usage = Some(match usage {
                                Some(prev) => prev.merge_max(u),
                                None => u,
                            });
                        }
                        StreamChunk::Done { finish_reason } => {
                            trace!(
                                "stream chunk: session={} message_id={} type=done finish_reason={:?}",
                                session_id, message_id, finish_reason
                            );
                            // Some providers emit Usage AFTER Done in the same SSE
                            // batch. Drain remaining items to capture any trailing
                            // Usage events before exiting the loop.
                            loop {
                                let remaining = tokio::select! {
                                    remaining = stream.next() => remaining,
                                    _ = exec_ctx.cancellation_token.cancelled() => {
                                        return Ok(ExecutionState::Cancelled);
                                    }
                                };
                                let Some(remaining) = remaining else {
                                    break;
                                };
                                match remaining {
                                    Ok(StreamChunk::Usage(u)) => {
                                        trace!(
                                            "stream chunk: session={} message_id={} type=usage (post-done drain) input={} output={}",
                                            session_id, message_id, u.input_tokens, u.output_tokens
                                        );
                                        usage = Some(match usage {
                                            Some(prev) => prev.merge_max(u),
                                            None => u,
                                        });
                                    }
                                    Err(_) | Ok(_) => break,
                                }
                            }

                            match validate_stream_terminal(finish_reason, &stream_tool_calls) {
                                Ok(finish_reason) => {
                                    stream_finish_reason = Some(finish_reason);
                                    break 'stream;
                                }
                                Err(error) => match super::llm_retry::handle_stream_failure(
                                    config,
                                    session_id,
                                    error,
                                    &mut retry_budget,
                                    semantic_output_seen,
                                    Some(message_id.clone()),
                                    &exec_ctx.cancellation_token,
                                )
                                .await
                                {
                                    super::llm_retry::StreamFailureAction::Retry => {
                                        continue 'stream;
                                    }
                                    super::llm_retry::StreamFailureAction::Cancelled => {
                                        return Ok(ExecutionState::Cancelled);
                                    }
                                    super::llm_retry::StreamFailureAction::Terminal(error) => {
                                        return Err(contextualize_llm_error(
                                            error,
                                            "streaming",
                                            context,
                                        ));
                                    }
                                },
                            }
                        }
                        _ => {}
                    }

                    // Time- or size-based flush
                    if last_flush.elapsed() >= BATCH_INTERVAL
                        || text_buffer.len() >= BATCH_CHARS
                        || thinking_buffer.len() >= BATCH_CHARS
                    {
                        flush_buffers!(true);
                    }
                } // end inner consume loop
            } // end outer retry loop ('stream)

            // Final flush of any remaining buffered content (no timer reset needed)
            flush_buffers!(false);
            debug!(
                "stream finished: session={} message_id={} final_text_len={} final_thinking_len={} tool_calls={}",
                session_id,
                message_id,
                text.len(),
                thinking.len(),
                stream_tool_calls.len()
            );

            // The streaming loop exits via `Done => break`, which bypasses the
            // per-chunk cancellation check at the top of the loop. Re-check here
            // so a cancel signal that arrived concurrently with the Done chunk is
            // not missed — without this the state machine would advance to AfterLlm.
            if exec_ctx.cancellation_token.is_cancelled() {
                return Ok(ExecutionState::Cancelled);
            }

            let finish_reason = Some(
                stream_finish_reason
                    .expect("successful stream loop exits only after an explicit Done chunk"),
            );

            // Stash message_id in response so transition_after_llm reuses it
            // (see LlmResponse::with_message_id)
            // We return the id via a side-channel: we wrap it below.
            // Use an Option wrapper: the streaming_message_id is set later.
            (
                text,
                if thinking.is_empty() {
                    None
                } else {
                    Some(thinking)
                },
                thinking_signature,
                stream_tool_calls,
                usage,
                finish_reason,
            )
        } else {
            // === NON-STREAMING FALLBACK ===
            let cancel = exec_ctx.cancellation_token.clone();
            let resp = match super::llm_retry::call_with_retry(
                config,
                session_id,
                &exec_ctx.cancellation_token,
                || {
                    let provider = &provider;
                    let messages_with_cache = &messages_with_cache;
                    let tools = tools.as_ref();
                    let cancel = cancel.clone();
                    async move {
                        tokio::select! {
                            result = provider.chat_with_tools(messages_with_cache, Some(tools)) => {
                                result
                            }
                            _ = cancel.cancelled() => {
                                Err(querymt::error::LLMError::Cancelled)
                            }
                        }
                    }
                },
            )
            .await
            {
                Ok(resp) => resp,
                Err(e) => return map_failed_llm_call(e, false, context),
            };

            (
                resp.text().unwrap_or_default(),
                resp.thinking(),
                None,
                resp.tool_calls().unwrap_or_default(),
                resp.usage(),
                resp.finish_reason(),
            )
        }
    };

    let (request_cost, cumulative_cost) = if let Some(usage_info) = &usage {
        let pricing = session_handle.get_pricing();
        // Reasoning tokens are billed at the output rate (no separate pricing).
        let billable_output = usage_info.output_tokens as u64 + usage_info.reasoning_tokens as u64;
        let request_cost = pricing
            .as_ref()
            .and_then(|p| p.calculate_cost(usage_info.input_tokens as u64, billable_output));
        let cumulative_cost = pricing.as_ref().and_then(|p| {
            p.calculate_cost(
                context.stats.total_input_tokens + usage_info.input_tokens as u64,
                context.stats.total_output_tokens
                    + usage_info.reasoning_tokens as u64
                    + billable_output,
            )
        });
        (request_cost, cumulative_cost)
    } else {
        (None, None)
    };

    debug!(
        "Session {} received provider response ({} chars, {} tool call(s), finish: {:?}, cost: ${:.4?})",
        session_id,
        response_content.len(),
        tool_calls.len(),
        finish_reason,
        request_cost,
    );

    let context_tokens = calculate_context_tokens(usage.as_ref());

    config.emit_event(
        session_id,
        AgentEventKind::LlmRequestEnd {
            usage: usage.clone(),
            tool_calls: u32_from_usize(tool_calls.len(), "tool_calls.len", Some(session_id)),
            finish_reason,
            cost_usd: request_cost,
            cumulative_cost_usd: cumulative_cost,
            context_tokens,
            metrics: ExecutionMetrics {
                steps: u32_from_usize(
                    context.stats.steps.saturating_add(1),
                    "context.stats.steps + 1",
                    Some(session_id),
                ),
                turns: u32_from_usize(context.stats.turns, "context.stats.turns", Some(session_id)),
            },
        },
    );

    let llm_tool_calls: Vec<MiddlewareToolCall> = tool_calls
        .into_iter()
        .map(|tc| MiddlewareToolCall {
            id: tc.id.clone(),
            function: ToolFunction {
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            },
        })
        .collect();

    let mut llm_response = LlmResponse::new(response_content, llm_tool_calls, usage, finish_reason)
        .with_thinking(response_thinking)
        .with_thinking_signature(response_thinking_signature);
    if let Some(mid) = streaming_message_id {
        llm_response = llm_response.with_message_id(mid);
    }

    Ok(ExecutionState::AfterLlm {
        response: Arc::new(llm_response),
        context: context.clone(),
    })
}

/// Transition from AfterLlm to ProcessingToolCalls or Complete.
///
/// This stores the assistant's response, updates statistics, sends client updates,
/// and determines next state based on finish reason and tool calls.
#[instrument(
    name = "agent.transition.after_llm",
    skip(config, response, context, exec_ctx),
    fields(session_id = %exec_ctx.session_id, has_tool_calls = response.has_tool_calls())
)]
pub(super) async fn transition_after_llm(
    config: &AgentConfig,
    response: &Arc<LlmResponse>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "AfterLlm: session={}, has_tool_calls={}",
        exec_ctx.session_id,
        response.has_tool_calls()
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    if let Err(e) = ensure_pre_turn_snapshot_ready(exec_ctx, "before_first_response").await {
        warn!(
            "Failed to resolve pre-turn snapshot before first response: {}",
            e
        );
    }

    let progress_description = if response.has_tool_calls() {
        format!(
            "Received response with {} tool call(s)",
            response.tool_calls.len()
        )
    } else {
        "Received response from LLM".to_string()
    };

    let progress_entry = exec_ctx
        .state
        .record_progress(
            crate::session::domain::ProgressKind::Note,
            progress_description,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to record progress: {}", e))?;

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ProgressRecorded { progress_entry },
    );

    let mut parts = Vec::new();

    // Persist thinking/reasoning content before the text part
    if let Some(thinking) = &response.thinking
        && !thinking.is_empty()
    {
        parts.push(MessagePart::Reasoning {
            content: thinking.clone(),
            signature: response.thinking_signature.clone(),
            time_ms: None,
        });
    }

    if !response.content.is_empty() {
        parts.push(MessagePart::Text {
            content: response.content.clone(),
        });
    }

    for call in &response.tool_calls {
        parts.push(MessagePart::ToolUse(querymt::ToolCall {
            id: call.id.clone(),
            call_type: "function".to_string(),
            function: querymt::FunctionCall {
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            },
        }));
    }

    // Re-use the pre-allocated message_id from the streaming path when available,
    // so the UI can replace the live stream accumulator with the final message.
    let msg_id = response
        .message_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let assistant_msg = AgentMessage {
        id: msg_id,
        session_id: exec_ctx.session_id.clone(),
        role: ChatRole::Assistant,
        parts,
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: Some(context.provider.to_string()),
        source_model: Some(context.model.to_string()),
    };

    exec_ctx
        .add_message(assistant_msg.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store assistant message: {}", e))?;

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::AssistantMessageStored {
            content: response.content.clone(),
            thinking: response.thinking.clone(),
            message_id: Some(assistant_msg.id.clone()),
        },
    );

    let mut messages = (*context.messages).to_vec();
    messages.push(assistant_msg.to_chat_message());

    let mut updated_stats = (*context.stats).clone();
    updated_stats.steps += 1;
    if let Some(token_usage) = &response.usage {
        updated_stats.total_input_tokens += token_usage.input_tokens as u64;
        updated_stats.total_output_tokens += token_usage.output_tokens as u64;
        updated_stats.reasoning_tokens += token_usage.reasoning_tokens as u64;
        updated_stats.cache_read_tokens += token_usage.cache_read as u64;
        updated_stats.cache_write_tokens += token_usage.cache_write as u64;
        updated_stats.context_tokens = calculate_context_tokens(Some(token_usage)) as usize;

        if let Some(pricing) = exec_ctx.session_handle.get_pricing() {
            updated_stats.update_costs(&pricing);
        }
    }

    let new_context = Arc::new(
        crate::middleware::ConversationContext::new(
            context.session_id.clone(),
            Arc::from(messages.into_boxed_slice()),
            Arc::new(updated_stats),
            context.provider.clone(),
            context.model.clone(),
        )
        .with_session_mode(context.session_mode)
        .with_fragments(context.fragments.clone()),
    );

    match response.finish_reason {
        Some(FinishReason::ToolCalls) => {
            if !response.tool_calls.is_empty() {
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls: Arc::from(response.tool_calls.clone().into_boxed_slice()),
                    results: Arc::from(Vec::new().into_boxed_slice()),
                    context: new_context,
                })
            } else {
                Ok(ExecutionState::Complete {
                    context: new_context,
                })
            }
        }

        Some(FinishReason::Stop) => Ok(ExecutionState::Complete {
            context: new_context,
        }),

        Some(FinishReason::Length) => Ok(ExecutionState::Stopped {
            message: "Model hit token limit".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(new_context),
        }),

        Some(FinishReason::ContentFilter) => Ok(ExecutionState::Stopped {
            message: "Response blocked by content filter".into(),
            stop_type: StopType::ContentFilter,
            context: Some(new_context),
        }),

        Some(FinishReason::Error)
        | Some(FinishReason::Unknown)
        | Some(FinishReason::Other)
        | None => {
            if response.tool_calls.is_empty() {
                Ok(ExecutionState::Complete {
                    context: new_context,
                })
            } else {
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls: Arc::from(response.tool_calls.clone().into_boxed_slice()),
                    results: Arc::from(Vec::new().into_boxed_slice()),
                    context: new_context,
                })
            }
        }
    }
}

/// Transition from ProcessingToolCalls to BeforeLlmCall or WaitingForEvent.
///
/// This executes parallel-safe runs concurrently while preserving stateful and
/// clarification boundaries, then either:
/// - Returns to BeforeLlmCall with results (normal flow)
/// - Enters WaitingForEvent if a delegation was initiated
///
/// ## Cancellation
///
/// When the session is cancelled mid-execution, this function still completes the
/// full store step before returning `Cancelled`. This is required because the
/// assistant message with `ToolUse` blocks has already been written to history;
/// LLM APIs (e.g. Anthropic) require a matching `tool_result` for every
/// `tool_use` in the conversation. Without this repair the session becomes
/// permanently broken and cannot send further prompts.
///
/// Each tool future is individually raced against the cancel signal. A tool that
/// is interrupted receives a synthetic `"Cancelled by user"` error result so the
/// history invariant is always maintained.
#[instrument(
    name = "agent.transition.processing_tool_calls",
    skip(config, remaining_calls, results, context, exec_ctx, bridge),
    fields(
        session_id = %exec_ctx.session_id,
        remaining_calls = remaining_calls.len(),
        completed_results = results.len(),
        execution_mode = tracing::field::Empty,
        state_reload_ms = tracing::field::Empty,
    )
)]
pub(super) async fn transition_processing_tool_calls(
    config: &AgentConfig,
    remaining_calls: &Arc<[MiddlewareToolCall]>,
    results: &Arc<[ToolResult]>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "ProcessingToolCalls: session={}, remaining={}, completed={}",
        exec_ctx.session_id,
        remaining_calls.len(),
        results.len()
    );

    // If already cancelled before we even start, we still need to store synthetic
    // results for every pending call to keep history consistent.
    let already_cancelled = exec_ctx.cancellation_token.is_cancelled();

    if remaining_calls.is_empty() {
        let session_id = exec_ctx.session_id.clone();
        let next_state =
            super::tool_calls::store_all_tool_results(config, results, context, exec_ctx)
                .instrument(info_span!(
                    "agent.tools.store_results",
                    session_id = %session_id,
                    result_count = results.len()
                ))
                .await?;

        if already_cancelled {
            return Ok(ExecutionState::Cancelled);
        }
        return Ok(next_state);
    }

    let execution_class = |call: &MiddlewareToolCall| {
        config
            .tool_registry
            .find(&call.function.name)
            .map(|tool| tool.execution_class())
            .unwrap_or(crate::tools::ToolExecutionClass::ParallelSafe)
    };
    let has_execution_boundary = remaining_calls
        .iter()
        .any(|call| execution_class(call) != crate::tools::ToolExecutionClass::ParallelSafe);
    if has_execution_boundary {
        tracing::Span::current().record("execution_mode", "mixed_boundary");
        debug!(
            "Executing mixed tool batch with stateful boundaries for session {}",
            exec_ctx.session_id
        );
        let mut all_results = (**results).to_vec();
        let mut unprocessed_start = if already_cancelled { Some(0) } else { None };
        let mut cancelled = already_cancelled;
        let mut stateful_executed = false;
        let mut call_index = 0;

        while call_index < remaining_calls.len() && !cancelled {
            let class = execution_class(&remaining_calls[call_index]);
            if class == crate::tools::ToolExecutionClass::ParallelSafe {
                let group_end = remaining_calls[call_index..]
                    .iter()
                    .position(|call| {
                        execution_class(call) != crate::tools::ToolExecutionClass::ParallelSafe
                    })
                    .map_or(remaining_calls.len(), |offset| call_index + offset);
                let exec_ctx_ref: &ExecutionContext = exec_ctx;
                let futures = remaining_calls[call_index..group_end].iter().map(|call| {
                    let call = call.clone();
                    let cancel = exec_ctx_ref.cancellation_token.clone();
                    async move {
                        tokio::select! {
                            result = super::tool_calls::execute_tool_call(
                                config, &call, exec_ctx_ref, bridge,
                            ) => result,
                            _ = cancel.cancelled() => Ok(ToolResult::new(
                                call.id.clone(),
                                vec![querymt::chat::Content::text("Error: Cancelled by user")],
                                true,
                                Some(call.function.name.clone()),
                                Some(call.function.arguments.clone()),
                            )),
                        }
                    }
                });
                for (result, call) in join_all(futures)
                    .await
                    .into_iter()
                    .zip(remaining_calls[call_index..group_end].iter())
                {
                    match result {
                        Ok(tool_result) => all_results.push(tool_result),
                        Err(error) => all_results.push(ToolResult::new(
                            call.id.clone(),
                            vec![querymt::chat::Content::text(format!(
                                "Error: internal tool execution failed: {error}"
                            ))],
                            true,
                            Some(call.function.name.clone()),
                            Some(call.function.arguments.clone()),
                        )),
                    }
                }
                call_index = group_end;
                if exec_ctx.cancellation_token.is_cancelled() {
                    cancelled = true;
                    unprocessed_start = Some(call_index);
                }
                continue;
            }

            let call = &remaining_calls[call_index];
            stateful_executed |= class == crate::tools::ToolExecutionClass::SerialStateful;
            let result = super::tool_calls::execute_tool_call(config, call, exec_ctx, bridge).await;
            let mut clarification_applied = false;
            match result {
                Ok(tool_result) => {
                    if class == crate::tools::ToolExecutionClass::ClarificationBoundary
                        && !tool_result.is_error
                    {
                        let clarification = tool_result
                            .content
                            .iter()
                            .filter_map(|block| block.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !clarification.trim().is_empty() {
                            let summary = exec_ctx
                                .state
                                .current_intent
                                .as_ref()
                                .map(|intent| intent.summary.clone())
                                .unwrap_or_else(|| clarification.clone());
                            match exec_ctx
                                .state
                                .update_intent_projection(
                                    summary,
                                    None,
                                    Some(clarification.clone()),
                                    "clarification_answer".to_string(),
                                    Some(call.id.clone()),
                                )
                                .await
                            {
                                Ok(_) => {
                                    clarification_applied = true;
                                    if let Some(objective) = exec_ctx.run_objective.as_mut() {
                                        objective.amend(
                                            crate::agent::objective::ObjectiveDirective {
                                                text: clarification.clone(),
                                                source: crate::agent::objective::ObjectiveSource::ClarificationAnswer,
                                                source_ref: Some(call.id.clone()),
                                                accepted_at_ms: None,
                                                application_boundary: Some(
                                                    "clarification_boundary".to_string(),
                                                ),
                                            },
                                        );
                                    }
                                }
                                Err(error) => warn!(
                                    "Failed to persist clarification intent projection for tool call {}: {}",
                                    call.id, error
                                ),
                            }
                        }
                    }
                    all_results.push(tool_result)
                }
                Err(error) => all_results.push(ToolResult::new(
                    call.id.clone(),
                    vec![querymt::chat::Content::text(format!(
                        "Error: internal tool execution failed: {error}"
                    ))],
                    true,
                    Some(call.function.name.clone()),
                    Some(call.function.arguments.clone()),
                )),
            }
            call_index += 1;
            if exec_ctx.cancellation_token.is_cancelled() {
                cancelled = true;
                unprocessed_start = Some(call_index);
            } else if clarification_applied {
                unprocessed_start = Some(call_index);
                break;
            }
        }

        if let Some(start) = unprocessed_start {
            for call in remaining_calls.iter().skip(start) {
                let message = if cancelled {
                    "Error: Cancelled by user"
                } else {
                    "Skipped because a user clarification changed the run objective; the model must reconsider this action."
                };
                all_results.push(ToolResult::new(
                    call.id.clone(),
                    vec![querymt::chat::Content::text(message.to_string())],
                    true,
                    Some(call.function.name.clone()),
                    Some(call.function.arguments.clone()),
                ));
            }
        }
        if stateful_executed {
            let reload_started = std::time::Instant::now();
            match exec_ctx.state.load_working_context().await {
                Ok(()) => {
                    if let Some(objective) = exec_ctx.run_objective.as_mut() {
                        objective.set_task(
                            exec_ctx.state.active_task.as_ref(),
                            crate::agent::objective::ObjectiveSource::TaskUpdated,
                        );
                    }
                }
                Err(error) => warn!(
                    "Failed to refresh working context after stateful tool execution: {}",
                    error
                ),
            }
            tracing::Span::current().record(
                "state_reload_ms",
                reload_started.elapsed().as_millis() as u64,
            );
        }
        let next_state = super::tool_calls::store_all_tool_results(
            config,
            &Arc::from(all_results.into_boxed_slice()),
            context,
            exec_ctx,
        )
        .await?;
        if cancelled {
            return Ok(ExecutionState::Cancelled);
        }
        return Ok(next_state);
    }

    tracing::Span::current().record("execution_mode", "parallel");
    debug!(
        "Executing {} tool calls in parallel for session {}",
        remaining_calls.len(),
        exec_ctx.session_id
    );

    // Wrap each individual tool future with a per-call cancel race.
    //
    // When the cancel signal fires, each future resolves immediately with a
    // synthetic error result rather than waiting for the underlying work to
    // finish. This lets `join_all` complete quickly on cancellation without
    // leaving orphaned `tool_use` blocks in history.
    //
    // We reborrow `exec_ctx` as a plain `&ExecutionContext` (immutable) so it
    // can be shared across all futures — `execute_tool_call` only needs `&`.
    let exec_ctx_ref: &ExecutionContext = exec_ctx;
    let mut futures = Vec::with_capacity(remaining_calls.len());
    for call in remaining_calls.iter() {
        let per_call_cancel = exec_ctx_ref.cancellation_token.clone();
        let call = call.clone();
        futures.push(async move {
            tokio::select! {
                result = super::tool_calls::execute_tool_call(
                    config, &call, exec_ctx_ref, bridge,
                ) => result,
                _ = per_call_cancel.cancelled() => {
                    // Produce a synthetic cancelled result so history stays valid.
                    Ok(ToolResult::new(
                        call.id.clone(),
                        vec![querymt::chat::Content::text("Error: Cancelled by user")],
                        true,
                        Some(call.function.name.clone()),
                        Some(call.function.arguments.clone()),
                    ))
                }
            }
        });
    }

    let tool_results = join_all(futures).await;

    let was_cancelled = already_cancelled || exec_ctx.cancellation_token.is_cancelled();

    let mut all_results = (**results).to_vec();
    for (result, call) in tool_results.into_iter().zip(remaining_calls.iter()) {
        match result {
            Ok(tool_result) => all_results.push(tool_result),
            Err(e) => {
                warn!(
                    "Tool call {} ({}) failed with infrastructure error: {}. \
                     Synthesizing error result to maintain tool_use/tool_result invariant.",
                    call.id, call.function.name, e
                );
                all_results.push(ToolResult::new(
                    call.id.clone(),
                    vec![querymt::chat::Content::text(format!(
                        "Error: internal tool execution failed: {}",
                        e
                    ))],
                    true,
                    Some(call.function.name.clone()),
                    Some(call.function.arguments.clone()),
                ));
            }
        }
    }

    debug!(
        "Completed {} tool calls for session {} (cancelled={})",
        all_results.len() - results.len(),
        exec_ctx.session_id,
        was_cancelled,
    );

    // Always store results — even on cancellation — to maintain the
    // tool_use → tool_result history invariant required by LLM APIs.
    let session_id = exec_ctx.session_id.clone();
    let next_state = super::tool_calls::store_all_tool_results(
        config,
        &Arc::from(all_results.into_boxed_slice()),
        context,
        exec_ctx,
    )
    .instrument(info_span!(
        "agent.tools.store_results",
        session_id = %session_id,
        cancelled = was_cancelled,
    ))
    .await?;

    if was_cancelled {
        return Ok(ExecutionState::Cancelled);
    }

    Ok(next_state)
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::ConversationContext;
    use querymt::chat::{ChatMessage, ChatRole, Content};

    fn make_message(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![Content::text(content)],
            cache: None,
        }
    }

    // ── map_failed_llm_call ───────────────────────────────────────────────────

    fn llm_context() -> ConversationContext {
        ConversationContext::new(
            "session".into(),
            Arc::from([]),
            Arc::new(Default::default()),
            "openrouter".into(),
            "anthropic/claude".into(),
        )
    }

    #[test]
    fn setup_cancel_maps_to_cancelled_not_error() {
        let context = llm_context();
        let non_stream = map_failed_llm_call(LLMError::Cancelled, false, &context)
            .expect("cancel must be Ok(Cancelled)");
        assert!(matches!(non_stream, ExecutionState::Cancelled));

        let stream = map_failed_llm_call(LLMError::Cancelled, true, &context)
            .expect("stream setup cancel must be Ok(Cancelled)");
        assert!(matches!(stream, ExecutionState::Cancelled));
    }

    #[test]
    fn tool_calls_finish_without_completed_calls_is_rejected() {
        let error = validate_stream_terminal(FinishReason::ToolCalls, &[])
            .expect_err("tool_calls without completed calls must not become success");
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == querymt::error::ProviderErrorKind::UnknownTransient
        ));
        assert!(error.is_retryable());
    }

    #[test]
    fn llm_failures_add_operation_and_model_context_without_erasing_source() {
        let context = llm_context();
        let chat_err = map_failed_llm_call(
            LLMError::InvalidRequest("missing api_key".into()),
            false,
            &context,
        )
        .expect_err("non-stream failure is Err");
        let chat_context = chat_err.to_string();
        assert!(chat_context.contains("LLM chat error"));
        assert!(chat_context.contains("provider=openrouter"));
        assert!(chat_context.contains("model=anthropic/claude"));
        assert!(chat_context.contains("Invalid Request: missing api_key"));
        assert!(matches!(
            chat_err.downcast_ref::<LLMError>(),
            Some(LLMError::InvalidRequest(message)) if message == "missing api_key"
        ));

        let stream_err = map_failed_llm_call(LLMError::GenericError("boom".into()), true, &context)
            .expect_err("stream failure is Err");
        assert!(stream_err.to_string().contains("LLM streaming error"));
        assert!(matches!(
            stream_err.downcast_ref::<LLMError>(),
            Some(LLMError::GenericError(message)) if message == "boom"
        ));
    }

    // ── apply_cache_breakpoints ───────────────────────────────────────────────

    #[test]
    fn test_cache_breakpoints_empty_slice() {
        let result = apply_cache_breakpoints(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_cache_breakpoints_single_message_gets_cache_hint() {
        let msgs = vec![make_message(ChatRole::User, "hello")];
        let result = apply_cache_breakpoints(&msgs);
        assert_eq!(result.len(), 1);
        // With len < 2, the guard `len >= 2` is false — no cache hint applied
        assert!(
            result[0].cache.is_none(),
            "single message should NOT get cache hint (len < 2)"
        );
    }

    #[test]
    fn test_cache_breakpoints_two_messages_both_cached() {
        let msgs = vec![
            make_message(ChatRole::User, "msg-0"),
            make_message(ChatRole::Assistant, "msg-1"),
        ];
        let result = apply_cache_breakpoints(&msgs);
        assert_eq!(result.len(), 2);
        // Both are within last 2, so both get cache hints
        assert!(result[0].cache.is_some(), "msg-0 should be cached");
        assert!(result[1].cache.is_some(), "msg-1 should be cached");
    }

    #[test]
    fn test_cache_breakpoints_three_messages_last_two_cached() {
        let msgs = vec![
            make_message(ChatRole::User, "msg-0"),
            make_message(ChatRole::Assistant, "msg-1"),
            make_message(ChatRole::User, "msg-2"),
        ];
        let result = apply_cache_breakpoints(&msgs);
        assert_eq!(result.len(), 3);
        assert!(
            result[0].cache.is_none(),
            "first message should NOT be cached"
        );
        assert!(result[1].cache.is_some(), "second-to-last should be cached");
        assert!(result[2].cache.is_some(), "last should be cached");
    }

    #[test]
    fn test_cache_breakpoints_five_messages_only_last_two_cached() {
        let msgs: Vec<ChatMessage> = (0..5)
            .map(|i| make_message(ChatRole::User, &format!("msg-{i}")))
            .collect();
        let result = apply_cache_breakpoints(&msgs);
        assert_eq!(result.len(), 5);
        for (i, msg) in result.iter().enumerate().take(3) {
            assert!(msg.cache.is_none(), "msg-{i} should NOT have cache hint");
        }
        assert!(result[3].cache.is_some(), "msg-3 should be cached");
        assert!(result[4].cache.is_some(), "msg-4 should be cached");
    }

    #[test]
    fn test_cache_breakpoints_preserves_content() {
        let msgs = vec![
            make_message(ChatRole::User, "important content"),
            make_message(ChatRole::Assistant, "response text"),
            make_message(ChatRole::User, "follow-up"),
        ];
        let result = apply_cache_breakpoints(&msgs);
        assert_eq!(result[0].text(), "important content");
        assert_eq!(result[1].text(), "response text");
        assert_eq!(result[2].text(), "follow-up");
    }

    #[test]
    fn test_cache_breakpoints_cache_hint_is_ephemeral() {
        // With 2 messages the last one should get an Ephemeral hint
        let msgs = vec![
            make_message(ChatRole::User, "test"),
            make_message(ChatRole::Assistant, "reply"),
        ];
        let result = apply_cache_breakpoints(&msgs);
        match &result[1].cache {
            Some(CacheHint::Ephemeral { ttl_seconds }) => {
                assert!(ttl_seconds.is_none(), "ttl should be None");
            }
            other => panic!("expected Ephemeral cache hint, got: {:?}", other),
        }
    }
}
