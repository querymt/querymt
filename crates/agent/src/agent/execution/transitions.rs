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
use log::{debug, info};
use querymt::ToolCall;
use querymt::chat::{CacheHint, ChatMessage, ChatRole, FinishReason, StreamChunk};
use querymt::plugin::extism_impl::ExtismChatResponse;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// Transition from BeforeLlmCall to CallLlm.
///
/// This collects available tools, computes their hash, and emits a ToolsAvailable event
/// if the tool set has changed.
pub(super) async fn transition_before_llm_call(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &ExecutionContext,
    cancel_rx: &watch::Receiver<bool>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "BeforeLlmCall: session={}, steps={}",
        exec_ctx.session_id, context.stats.steps
    );

    if *cancel_rx.borrow() {
        return Ok(ExecutionState::Cancelled);
    }

    let provider = exec_ctx
        .session_handle
        .provider()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

    let tools = config.collect_tools(provider, Some(exec_ctx.runtime.as_ref()));

    let tools_json =
        serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
    let new_hash = crate::hash::RapidHash::new(&tools_json);

    let mut current = exec_ctx.runtime.current_tools_hash.lock().unwrap();
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
pub(super) async fn transition_call_llm(
    config: &AgentConfig,
    context: &Arc<crate::middleware::ConversationContext>,
    tools: &Arc<[querymt::chat::Tool]>,
    cancel_rx: &watch::Receiver<bool>,
    exec_ctx: &ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    let session_id = &exec_ctx.session_id;
    debug!(
        "CallLlm: session={}, messages={}",
        session_id,
        context.messages.len()
    );

    if *cancel_rx.borrow() {
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

    let response = if tools.is_empty() {
        let cancel_rx_clone = cancel_rx.clone();
        super::llm_retry::call_llm_with_retry(config, session_id, cancel_rx, || {
            let messages_with_cache = &messages_with_cache;
            let mut cancel_rx_clone = cancel_rx_clone.clone();
            async move {
                tokio::select! {
                    result = session_handle.submit_request(messages_with_cache) => {
                        result
                    }
                    _ = cancel_rx_clone.changed() => {
                        Err(querymt::error::LLMError::Cancelled)
                    }
                }
            }
        })
        .await?
    } else {
        let provider = session_handle
            .provider()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build provider: {}", e))?;

        if context.provider.as_ref() == "codex" {
            let mut stream = provider
                .chat_stream_with_tools(&messages_with_cache, Some(tools))
                .await
                .map_err(|e| anyhow::anyhow!("LLM streaming request with tools failed: {}", e))?;

            let mut text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_call_ids = std::collections::HashSet::new();
            let mut usage: Option<querymt::Usage> = None;

            while let Some(item) = stream.next().await {
                if *cancel_rx.borrow() {
                    return Ok(ExecutionState::Cancelled);
                }

                match item.map_err(|e| {
                    anyhow::anyhow!("LLM streaming request with tools failed: {}", e)
                })? {
                    StreamChunk::Text(delta) => text.push_str(&delta),
                    StreamChunk::Thinking(_) => {}
                    StreamChunk::ToolUseComplete { tool_call, .. } => {
                        if tool_call_ids.insert(tool_call.id.clone()) {
                            tool_calls.push(tool_call);
                        }
                    }
                    StreamChunk::Usage(u) => usage = Some(u),
                    StreamChunk::Done { .. } => break,
                    _ => {}
                }
            }

            let finish_reason = if tool_calls.is_empty() {
                Some(FinishReason::Stop)
            } else {
                Some(FinishReason::ToolCalls)
            };

            Box::new(ExtismChatResponse {
                text: if text.is_empty() { None } else { Some(text) },
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                thinking: None,
                usage,
                finish_reason,
            })
        } else {
            let cancel_rx_clone = cancel_rx.clone();
            super::llm_retry::call_llm_with_retry(config, session_id, cancel_rx, || {
                let provider = &provider;
                let messages_with_cache = &messages_with_cache;
                let tools = tools.as_ref();
                let mut cancel_rx_clone = cancel_rx_clone.clone();
                async move {
                    tokio::select! {
                        result = provider.chat_with_tools(messages_with_cache, Some(tools)) => {
                            result
                        }
                        _ = cancel_rx_clone.changed() => {
                            Err(querymt::error::LLMError::Cancelled)
                        }
                    }
                }
            })
            .await?
        }
    };

    let usage = response.usage();
    let response_content = response.text().unwrap_or_default();
    let tool_calls = response.tool_calls().unwrap_or_default();
    let finish_reason = response.finish_reason();

    let (request_cost, cumulative_cost) = if let Some(usage_info) = response.usage() {
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

    let context_tokens = calculate_context_tokens(response.usage().as_ref());

    config.emit_event(
        session_id,
        AgentEventKind::LlmRequestEnd {
            usage: response.usage(),
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

    Ok(ExecutionState::AfterLlm {
        response: Arc::new(LlmResponse::new(
            response_content,
            llm_tool_calls,
            usage,
            finish_reason,
        )),
        context: context.clone(),
    })
}

/// Transition from AfterLlm to ProcessingToolCalls or Complete.
///
/// This stores the assistant's response, updates statistics, sends client updates,
/// and determines next state based on finish reason and tool calls.
pub(super) async fn transition_after_llm(
    config: &AgentConfig,
    response: &Arc<LlmResponse>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
    cancel_rx: &watch::Receiver<bool>,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "AfterLlm: session={}, has_tool_calls={}",
        exec_ctx.session_id,
        response.has_tool_calls()
    );

    if *cancel_rx.borrow() {
        return Ok(ExecutionState::Cancelled);
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
    if !response.content.is_empty() {
        super::bridge::send_session_update(
            bridge,
            &exec_ctx.session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(response.content.clone()),
            ))),
        );
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

    let assistant_msg = AgentMessage {
        id: Uuid::new_v4().to_string(),
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
pub(super) async fn transition_processing_tool_calls(
    config: &AgentConfig,
    remaining_calls: &Arc<[MiddlewareToolCall]>,
    results: &Arc<[ToolResult]>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
    cancel_rx: &watch::Receiver<bool>,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "ProcessingToolCalls: session={}, remaining={}, completed={}",
        exec_ctx.session_id,
        remaining_calls.len(),
        results.len()
    );

    if *cancel_rx.borrow() {
        return Ok(ExecutionState::Cancelled);
    }

    if remaining_calls.is_empty() {
        return super::tool_calls::store_all_tool_results(config, results, context, exec_ctx).await;
    }

    debug!(
        "Executing {} tool calls in parallel for session {}",
        remaining_calls.len(),
        exec_ctx.session_id
    );

    let futures: Vec<_> = remaining_calls
        .iter()
        .map(|call| super::tool_calls::execute_tool_call(config, call, exec_ctx, bridge))
        .collect();

    let mut cancel_rx_clone = cancel_rx.clone();
    let tool_results = tokio::select! {
        results = join_all(futures) => results,
        _ = cancel_rx_clone.changed() => {
            return Ok(ExecutionState::Cancelled);
        }
    };

    let mut all_results = (**results).to_vec();
    for result in tool_results {
        all_results.push(result?);
    }

    debug!(
        "Completed {} tool calls for session {}",
        all_results.len() - results.len(),
        exec_ctx.session_id
    );

    Ok(ExecutionState::ProcessingToolCalls {
        remaining_calls: Arc::from(Vec::<MiddlewareToolCall>::new().into_boxed_slice()),
        results: Arc::from(all_results.into_boxed_slice()),
        context: context.clone(),
    })
}
