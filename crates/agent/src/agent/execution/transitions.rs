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
use crate::events::{AgentEventKind, ExecutionMetrics, StopType};
use crate::middleware::{
    ExecutionState, LlmResponse, ToolCall as MiddlewareToolCall, ToolFunction, ToolResult,
    calculate_context_tokens,
};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
use anyhow::Context as _;
use futures_util::StreamExt;
use futures_util::future::join_all;
use log::{debug, info, trace, warn};
use querymt::ToolCall;
use querymt::chat::{CacheHint, ChatMessage, ChatRole, FinishReason, StreamChunk};
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
    fields(session_id = %exec_ctx.session_id, steps = context.stats.steps)
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

    let provider = exec_ctx
        .session_handle
        .provider()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

    let tools = config.collect_tools(
        provider,
        Some(exec_ctx.runtime.as_ref()),
        Some(&exec_ctx.tool_config),
    );

    let tools_json =
        serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
    let new_hash = crate::hash::RapidHash::new(&tools_json);

    let mut current = exec_ctx.runtime.mcp_tool_state.tools_hash.lock().unwrap();
    let changed = current.is_none_or(|h| h != new_hash);
    if changed {
        *current = Some(new_hash);
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

    Ok(ExecutionState::CallLlm {
        context: context.clone(),
        tools: Arc::from(tools.into_boxed_slice()),
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

/// Transition from CallLlm to AfterLlm.
///
/// This invokes the LLM (with or without tools), handles streaming for codex provider,
/// tracks usage/costs, and emits LlmRequestStart/End events.
#[instrument(
    name = "agent.transition.call_llm",
    skip(config, context, tools, exec_ctx),
    fields(
        session_id = %exec_ctx.session_id,
        provider = %context.provider,
        model = %context.model,
        message_count = context.messages.len(),
        tool_count = tools.len()
    )
)]
pub(super) async fn transition_call_llm(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    tools: &Arc<[querymt::chat::Tool]>,
    exec_ctx: &ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    debug!(
        "CallLlm: session={}, messages={}",
        session_id,
        context.messages.len()
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    config.emit_event(
        session_id,
        AgentEventKind::LlmRequestStart {
            message_count: context.messages.len(),
        },
    );

    let session_handle = &exec_ctx.session_handle;
    let messages_with_cache = apply_cache_breakpoints(&context.messages);

    // Pre-allocated message_id for streaming path so that delta events and the
    // final AssistantMessageStored share the same ID.
    let mut streaming_message_id: Option<String> = None;

    // Determine response via streaming or non-streaming path.
    // Each arm produces the same tuple so the rest of the function is uniform.
    let (response_content, response_thinking, tool_calls, usage, finish_reason) = if tools
        .is_empty()
    {
        // No tools — always use the non-streaming simple submit path.
        let cancel = exec_ctx.cancellation_token.clone();
        let resp = super::llm_retry::call_llm_with_retry(
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
        .await?;

        (
            resp.text().unwrap_or_default(),
            resp.thinking(),
            resp.tool_calls().unwrap_or_default(),
            resp.usage(),
            resp.finish_reason(),
        )
    } else {
        let provider = session_handle
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

        if provider.supports_streaming() {
            // === STREAMING PATH (all capable providers) ===
            let message_id = Uuid::new_v4().to_string();
            streaming_message_id = Some(message_id.clone());

            let mut stream = super::llm_retry::create_stream_with_retry(
                config,
                session_id,
                &exec_ctx.cancellation_token,
                || {
                    let provider = &provider;
                    let messages_with_cache = &messages_with_cache;
                    let tools_slice = tools.as_ref();
                    async move {
                        provider
                            .chat_stream_with_tools(messages_with_cache, Some(tools_slice))
                            .await
                    }
                },
            )
            .await?;

            let mut text = String::new();
            let mut thinking = String::new();
            let mut stream_tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_call_ids = std::collections::HashSet::new();
            let mut usage: Option<querymt::Usage> = None;

            // Batching buffers — we flush at most every 50ms or 256 chars to
            // avoid per-token React state updates on fast local models.
            let mut text_buffer = String::new();
            let mut thinking_buffer = String::new();
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

            while let Some(item) = stream.next().await {
                if exec_ctx.cancellation_token.is_cancelled() {
                    return Ok(ExecutionState::Cancelled);
                }

                match item.map_err(|e| anyhow::anyhow!("LLM streaming error: {}", e))? {
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
                    StreamChunk::Done { .. } => {
                        trace!(
                            "stream chunk: session={} message_id={} type=done",
                            session_id, message_id
                        );
                        break;
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
            }

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

            let finish_reason = if stream_tool_calls.is_empty() {
                Some(FinishReason::Stop)
            } else {
                Some(FinishReason::ToolCalls)
            };

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
                stream_tool_calls,
                usage,
                finish_reason,
            )
        } else {
            // === NON-STREAMING FALLBACK ===
            let cancel = exec_ctx.cancellation_token.clone();
            let resp = super::llm_retry::call_llm_with_retry(
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
            .await?;

            (
                resp.text().unwrap_or_default(),
                resp.thinking(),
                resp.tool_calls().unwrap_or_default(),
                resp.usage(),
                resp.finish_reason(),
            )
        }
    };

    let (request_cost, cumulative_cost) = if let Some(usage_info) = &usage {
        let pricing = session_handle.get_pricing();
        let request_cost = pricing.as_ref().and_then(|p| {
            p.calculate_cost(
                usage_info.input_tokens as u64,
                usage_info.output_tokens as u64,
            )
        });
        let cumulative_cost = pricing.as_ref().and_then(|p| {
            p.calculate_cost(
                context.stats.total_input_tokens + usage_info.input_tokens as u64,
                context.stats.total_output_tokens + usage_info.output_tokens as u64,
            )
        });
        (request_cost, cumulative_cost)
    } else {
        (None, None)
    };

    info!(
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
            tool_calls: tool_calls.len(),
            finish_reason,
            cost_usd: request_cost,
            cumulative_cost_usd: cumulative_cost,
            context_tokens,
            metrics: ExecutionMetrics {
                steps: context.stats.steps + 1,
                turns: context.stats.turns,
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
        .with_thinking(response_thinking);
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
    skip(config, response, context, exec_ctx, bridge),
    fields(session_id = %exec_ctx.session_id, has_tool_calls = response.has_tool_calls())
)]
pub(super) async fn transition_after_llm(
    config: &AgentConfig,
    response: &Arc<LlmResponse>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
    bridge: Option<&ClientBridgeSender>,
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
            time_ms: None,
        });
    }

    if !response.content.is_empty() {
        super::bridge::send_session_update(
            bridge,
            &exec_ctx.session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(response.content.clone()),
            ))),
            Some(&exec_ctx.cancellation_token),
        )
        .await;
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
    if let Some(token_usage) = &response.usage {
        updated_stats.total_input_tokens += token_usage.input_tokens as u64;
        updated_stats.total_output_tokens += token_usage.output_tokens as u64;
        updated_stats.reasoning_tokens += token_usage.reasoning_tokens as u64;
        updated_stats.cache_read_tokens += token_usage.cache_read as u64;
        updated_stats.cache_write_tokens += token_usage.cache_write as u64;
        updated_stats.context_tokens = calculate_context_tokens(Some(token_usage)) as usize;
        updated_stats.steps += 1;

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
        .with_session_mode(context.session_mode),
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
                Ok(ExecutionState::Complete)
            }
        }

        Some(FinishReason::Stop) => {
            if exec_ctx.state.active_task.is_some() {
                if let Err(e) = exec_ctx.state.update_task_status(TaskStatus::Done).await {
                    debug!("Failed to auto-complete task on stop: {}", e);
                } else if let Some(task) = exec_ctx.state.active_task.clone() {
                    config.emit_event(
                        &exec_ctx.session_id,
                        AgentEventKind::TaskStatusChanged { task },
                    );
                }
            }
            Ok(ExecutionState::Complete)
        }

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
                Ok(ExecutionState::Complete)
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
/// This executes remaining tool calls in parallel, collects results, and either:
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
        completed_results = results.len()
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
                        "Error: Cancelled by user".to_string(),
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
    for result in tool_results {
        all_results.push(result?);
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
    use querymt::chat::{ChatMessage, ChatRole, MessageType};

    fn make_message(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            message_type: MessageType::Text,
            content: content.to_string(),
            thinking: None,
            cache: None,
        }
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
        assert_eq!(result[0].content, "important content");
        assert_eq!(result[1].content, "response text");
        assert_eq!(result[2].content, "follow-up");
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
