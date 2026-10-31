//! State transition functions for the execution state machine
//!
//! This module contains the logic for transitioning between execution states:
//! - BeforeLlmCall → CallLlm (tool collection)
//! - CallLlm → AfterLlm (LLM invocation)
//! - AfterLlm → ProcessingToolCalls or Complete (response handling)
//! - ProcessingToolCalls → BeforeLlmCall or WaitingForEvent (parallel tool execution)

use super::tool_calls::{Disposition, PreparedCall};
use super::wave_plan::{CallInfo, plan_waves, serialized_call_count};
use crate::agent::agent_config::AgentConfig;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::session_actor::ensure_pre_turn_snapshot_ready;
use crate::agent::utils::{genai, u32_from_usize};
use crate::events::{
    AgentEventKind, ExecutionMetrics, ReasoningPartStored, StopType, reasoning_content_part_id,
    reasoning_summary_part_id,
};
use crate::middleware::{
    CompositeDriver, ExecutionState, LlmResponse, PreparedBatch, PreparedCallVerdict,
    PreparedModelRequest, PreparedToolCall, ToolCall as MiddlewareToolCall, ToolFunction,
    ToolResult, apply_usage_to_stats, calculate_context_tokens,
};
use crate::model::{AgentMessage, MessagePart};
use anyhow::Context as _;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::future::join_all;
use log::{debug, trace, warn};
use querymt::ToolCall;
use querymt::chat::{
    CacheHint, ChatCall, ChatMessage, ChatMessagePartDelta, ChatOutput, ChatOutputItem,
    ChatOutputStatus, ChatRole, ChatStreamAccumulator, ChatStreamAccumulatorError,
    ChatStreamFinish, FinishReason, ReasoningPartKind, StreamChunk, StructuredStreamEvent,
};
use querymt::error::LLMError;
use querymt::error::{ProviderErrorKind, ProviderFailure};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
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
        "provider_init",
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

    // A failed eager initialization may have left the turn with an empty profile.
    // Provider retries must refresh it before request options are filtered.
    let context = if context.profile.capabilities != provider.capabilities() {
        Arc::new(
            (**context)
                .clone()
                .with_profile(exec_ctx.session_handle.profile().await),
        )
    } else {
        context.clone()
    };

    let collect_span = info_span!(
        "agent.tools.collect",
        tool_count = tracing::field::Empty,
        tools_bytes = tracing::field::Empty
    );
    let (tools, tools_json, new_hash) = {
        // Synchronous block: the guard never crosses an await.
        let _guard = collect_span.enter();
        let tools = config.collect_tools(
            provider,
            Some(exec_ctx.runtime.as_ref()),
            Some(&exec_ctx.tool_config),
        );
        let tools_json =
            serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
        let new_hash = crate::hash::RapidHash::new(&tools_json);
        collect_span.record("tool_count", tools.len() as u64);
        collect_span.record("tools_bytes", tools_json.len() as u64);
        (tools, tools_json, new_hash)
    };

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
    let trigger = if messages
        .last()
        .is_some_and(|message| message.has_tool_result())
    {
        "after_tool_batch"
    } else {
        "user_prompt"
    };
    let context_window = crate::model_info::get_model_info(&context.provider, &context.model)
        .and_then(|info| info.limits.context)
        .unwrap_or(u32::MAX as u64)
        .min(u32::MAX as u64) as u32;
    let hook_span = info_span!(
        "agent.hooks.context",
        trigger,
        message_count = messages.len() as u64
    );
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
        .instrument(hook_span)
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
    // Providers reject a tool call without its result. Reaching this point
    // with one is a bug upstream; answer it so the run survives, and make it loud.
    let unanswered = crate::model::answer_unanswered_tool_calls(&mut messages);
    if !unanswered.is_empty() {
        log::error!(
            "Session {}: request had unanswered tool call(s) {:?}; answered them as cancelled",
            exec_ctx.session_id,
            unanswered
        );
        debug_assert!(
            unanswered.is_empty(),
            "request had unanswered tool calls: {unanswered:?}"
        );
    }
    // Tool definitions are model input; reuse the serialization already used for their hash.
    let tool_tokens = if tools.is_empty() {
        0
    } else {
        tools_json.len().div_ceil(4)
    };
    validate_tool_context_budget(tool_tokens, context_window)?;
    let estimated_tokens = hook_result.estimated_tokens.saturating_add(tool_tokens);
    if estimated_tokens >= context_window as usize {
        return Ok(ExecutionState::Stopped {
            message: format!(
                "Prepared request is approximately {} tokens, exceeding the {} token context window",
                estimated_tokens, context_window
            )
            .into(),
            stop_type: StopType::ContextThreshold,
            context: Some(context.clone()),
        });
    }
    // Clones the whole history, so its cost grows with the conversation.
    let messages = info_span!(
        "agent.request.cache_breakpoints",
        message_count = messages.len() as u64
    )
    .in_scope(|| apply_cache_breakpoints(&messages));
    // Re-evaluate room after history, steering, tools, and context hooks are prepared.
    let mut options = context.request_overrides;
    if let Some(cap) = options.max_tokens {
        let room = context_window.saturating_sub(estimated_tokens as u32);
        options.max_tokens = Some(cap.min(room));
    }
    Ok(ExecutionState::CallLlm {
        context: context.clone(),
        request: Arc::new(PreparedModelRequest {
            messages: Arc::from(messages.into_boxed_slice()),
            tools: Arc::from(tools.into_boxed_slice()),
            estimated_tokens,
            options,
        }),
    })
}

/// Reject fixed tool overhead that history compaction cannot reduce.
fn validate_tool_context_budget(tool_tokens: usize, context_window: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        tool_tokens < context_window as usize,
        "Tool definitions require approximately {tool_tokens} tokens, leaving no room in the \
         {context_window} token context window. Reduce the available tools or use a larger-context model."
    );
    Ok(())
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
            if len >= 2 && i >= len - 2 && m.cache.is_none() {
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
    genai::llm_error(&error);
    let source_message = error.to_string();
    anyhow::Error::new(error).context(format!(
        "LLM {operation} error (provider={}, model={}): {source_message}",
        context.provider, context.model
    ))
}

/// Map a failed LLM setup/call into either `Cancelled` or a contextualized error.
///
/// Cancel must never bubble as `Err`; it is an execution state, not a provider failure.
fn accounting_failure(error: crate::accounting::AccountingError) -> anyhow::Error {
    anyhow::anyhow!("accounting failure: {error}")
}

/// The logical accounting operation for one main model request. Its retries
/// are separate attempts. Failing to open accounting is an execution error:
/// no paid request may be sent without a ledger.
async fn main_attempt_operation(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
) -> Result<crate::accounting::AttemptOperation, anyhow::Error> {
    let cycle = exec_ctx
        .cycle_accounting(&config.event_sink)
        .await
        .map_err(|error| anyhow::anyhow!("accounting unavailable: {error}"))?;
    let recorder = Arc::new(crate::accounting::AttemptRecorder::with_projection(
        config.event_sink.session_stream(&exec_ctx.session_id),
        cycle.session_accounting().clone(),
    ));
    let (provider, model) = exec_ctx
        .llm_config()
        .map(|c| (c.provider.clone(), c.model.clone()))
        .unwrap_or_default();
    Ok(crate::accounting::AttemptOperation::new(
        recorder,
        cycle,
        crate::accounting::AttemptPurpose::MainResponse,
        provider,
        model,
    )
    .with_budget_pricing(config.budget_pricing())
    .with_budget_cap(config.budget_cap_usd()))
}

/// A paid dispatch the cycle budget gate refused ends the cycle as a
/// price-limit stop, not an execution failure. Nothing was sent.
fn budget_stop(
    operation: &crate::accounting::AttemptOperation,
    context: &Arc<crate::middleware::ConversationContext>,
) -> Option<ExecutionState> {
    operation
        .budget_refusal()
        .map(|(cap, spent)| ExecutionState::Stopped {
            message: format!("Price limit reached: ${spent:.4} >= ${cap:.2}").into(),
            stop_type: StopType::PriceLimit,
            context: Some(context.clone()),
        })
}

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

/// Result text for a tool call that was not executed because the reply was incomplete.
///
/// A length cut-off tells the model how to avoid it next time; any other
/// incomplete cause keeps the generic wording.
fn skipped_call_text(finish_reason: Option<FinishReason>, detail: &str) -> String {
    match finish_reason {
        Some(FinishReason::Length) => {
            "Error: tool call skipped because the response was truncated \
by the output token limit; split the write into smaller calls."
                .to_string()
        }
        _ => format!("Error: tool call skipped because the response was incomplete. {detail}"),
    }
}

/// Persist synthesized tool results, then stop the run on the output token limit.
///
/// A length cut-off must end in `Stopped` (so stop recovery can act on it), but
/// the skipped results still have to be stored to keep the
/// tool_use/tool_result history invariant.
async fn store_results_then_stop_on_length(
    config: &AgentConfig,
    results: Arc<[ToolResult]>,
    context: Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    let next =
        super::tool_calls::store_all_tool_results(config, &results, &context, exec_ctx).await?;
    Ok(match next {
        ExecutionState::BeforeLlmCall { context } => ExecutionState::Stopped {
            message: "Model hit token limit".into(),
            stop_type: StopType::ModelTokenLimit,
            context: Some(context),
        },
        other => other,
    })
}

/// Wrap an accumulator violation as a transient provider stream failure.
fn stream_accumulation_error(error: ChatStreamAccumulatorError) -> LLMError {
    LLMError::from(
        ProviderFailure::new(ProviderErrorKind::UnknownTransient, error.to_string())
            .with_code(Some("stream_accumulation".into())),
    )
}

/// Visible stream text, or one reasoning part. Encrypted continuation stays out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UiReasoningKind {
    Summary,
    Content,
}

/// Visible stream text, or one reasoning part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiStreamDelta<'a> {
    Text(&'a str),
    Reasoning {
        output_index: usize,
        part_index: usize,
        kind: UiReasoningKind,
        delta: &'a str,
    },
}

/// Map a structured item delta onto the UI batching buffers.
///
/// Summary titles and plaintext reasoning keep their part index. Function
/// argument deltas and encrypted continuation do not surface as assistant text.
fn structured_delta_for_ui(event: &StructuredStreamEvent) -> Option<UiStreamDelta<'_>> {
    match event {
        StructuredStreamEvent::MessagePartDelta {
            delta: ChatMessagePartDelta::Text { delta },
            ..
        }
        | StructuredStreamEvent::MessagePartDelta {
            delta: ChatMessagePartDelta::Refusal { delta },
            ..
        } => Some(UiStreamDelta::Text(delta)),
        StructuredStreamEvent::ReasoningPartDelta {
            output_index,
            part,
            part_index,
            delta,
        } => {
            let kind = match part {
                ReasoningPartKind::Summary => UiReasoningKind::Summary,
                ReasoningPartKind::Content => UiReasoningKind::Content,
            };
            Some(UiStreamDelta::Reasoning {
                output_index: *output_index,
                part_index: *part_index,
                kind,
                delta,
            })
        }
        _ => None,
    }
}

fn remember_reasoning_item_id(
    ids: &mut HashMap<usize, String>,
    output_index: usize,
    item: &ChatOutputItem,
) {
    let ChatOutputItem::Reasoning(reasoning) = item else {
        return;
    };
    let Some(id) = reasoning.id.as_deref().filter(|id| !id.is_empty()) else {
        return;
    };
    ids.insert(output_index, id.to_string());
}

fn reasoning_ui_part_id(
    ids: &HashMap<usize, String>,
    output_index: usize,
    part_index: usize,
    kind: UiReasoningKind,
) -> String {
    let item_id = ids
        .get(&output_index)
        .map(String::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("output:{output_index}"));
    match kind {
        UiReasoningKind::Summary => reasoning_summary_part_id(&item_id, part_index),
        UiReasoningKind::Content => reasoning_content_part_id(&item_id, part_index),
    }
}

/// Keep the first id assigned to a reasoning part.
///
/// Item ids can arrive after the first delta. Changing the id then would split
/// one part into two UI entries.
fn pinned_reasoning_part_id(
    pinned: &mut HashMap<(usize, usize, UiReasoningKind), String>,
    ids: &HashMap<usize, String>,
    output_index: usize,
    part_index: usize,
    kind: UiReasoningKind,
) -> String {
    pinned
        .entry((output_index, part_index, kind))
        .or_insert_with(|| reasoning_ui_part_id(ids, output_index, part_index, kind))
        .clone()
}

/// Visible summary titles and plaintext reasoning. Encrypted continuation stays out.
fn reasoning_parts_from_output(output: &ChatOutput) -> Vec<ReasoningPartStored> {
    let mut parts = Vec::new();
    for (output_index, item) in output.items.iter().enumerate() {
        let ChatOutputItem::Reasoning(reasoning) = item else {
            continue;
        };
        let item_id = reasoning
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("output:{output_index}"));
        for (part_index, part) in reasoning.summary.iter().enumerate() {
            if part.text.is_empty() {
                continue;
            }
            parts.push(ReasoningPartStored {
                id: reasoning_summary_part_id(&item_id, part_index),
                text: part.text.clone(),
            });
        }
        for (part_index, part) in reasoning.content.iter().enumerate() {
            if part.text.is_empty() {
                continue;
            }
            parts.push(ReasoningPartStored {
                id: reasoning_content_part_id(&item_id, part_index),
                text: part.text.clone(),
            });
        }
    }
    parts
}

/// Batched thinking text for one summary part. A new part id flushes the previous part.
struct ThinkingPartBuffer {
    part_id: Option<String>,
    text: String,
}

impl ThinkingPartBuffer {
    fn new() -> Self {
        Self {
            part_id: None,
            text: String::new(),
        }
    }

    fn clear(&mut self) {
        self.part_id = None;
        self.text.clear();
    }

    fn len(&self) -> usize {
        self.text.len()
    }

    /// Append `delta`. Returns the previous part when the part id changes.
    fn push(&mut self, part_id: Option<String>, delta: &str) -> Option<(Option<String>, String)> {
        if delta.is_empty() {
            return None;
        }
        let flushed = if self.part_id != part_id && !self.text.is_empty() {
            Some(self.take())
        } else {
            None
        };
        self.part_id = part_id;
        self.text.push_str(delta);
        flushed
    }

    fn take(&mut self) -> (Option<String>, String) {
        (self.part_id.clone(), std::mem::take(&mut self.text))
    }
}

/// Merge usage events that arrived after the terminal chunk into the output.
fn merge_drained_usage(mut output: ChatOutput, drained: Option<querymt::Usage>) -> ChatOutput {
    if let Some(extra) = drained {
        output.usage = Some(match output.usage.take() {
            Some(previous) => previous.merge_max(extra),
            None => extra,
        });
    }
    output
}

/// Drain remaining stream items after the terminal chunk, collecting trailing
/// usage events. Some providers emit usage after Done in the same SSE batch.
///
/// Cancellation is reported by the caller checking the token after the drain.
async fn drain_trailing_usage(
    stream: &mut Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
    cancel_token: &CancellationToken,
) -> Option<querymt::Usage> {
    let mut usage: Option<querymt::Usage> = None;
    loop {
        let remaining = tokio::select! {
            remaining = stream.next() => remaining,
            _ = cancel_token.cancelled() => return usage,
        };
        let Some(remaining) = remaining else {
            break;
        };
        match remaining {
            Ok(StreamChunk::Usage(u)) => {
                usage = Some(match usage {
                    Some(previous) => previous.merge_max(u),
                    None => u,
                });
            }
            Err(_) | Ok(_) => break,
        }
    }
    usage
}

/// Keep the first call per call identity; duplicates execute at most once.
fn dedupe_tool_calls_by_call_id(calls: Vec<ToolCall>) -> Vec<ToolCall> {
    let mut seen = std::collections::HashSet::new();
    calls
        .into_iter()
        .filter(|call| seen.insert(call.id.clone()))
        .collect()
}

/// Executable calls of a canonical output, deduplicated by call identity.
///
/// Provider invalid-input markers live on the canonical function-call items and
/// would be lost in the projection to `querymt::ToolCall`, so they are read from
/// the items here and carried explicitly on the middleware call.
fn middleware_calls_from_output(output: &ChatOutput) -> Vec<MiddlewareToolCall> {
    let marked_invalid: std::collections::HashSet<&str> = output
        .function_calls()
        .filter(|call| call.is_marked_invalid())
        .map(|call| call.call_id.as_str())
        .collect();
    dedupe_tool_calls_by_call_id(output.tool_calls().unwrap_or_default())
        .into_iter()
        .map(|tc| MiddlewareToolCall {
            invalid_input: marked_invalid.contains(tc.id.as_str()),
            id: tc.id,
            function: ToolFunction {
                name: tc.function.name,
                arguments: tc.function.arguments,
            },
        })
        .collect()
}

/// Split function calls into executable calls and synthesized error results.
///
/// Local execution is gated on response validation: a call executes only when
/// its raw arguments parse as a JSON object. Invalid arguments never execute and
/// never degrade to an empty object; they receive an error result so the
/// tool_use/tool_result history invariant is preserved. Duplicate call
/// identities are skipped after the first occurrence.
fn gate_function_calls_for_execution(
    calls: &[MiddlewareToolCall],
) -> (Vec<MiddlewareToolCall>, Vec<ToolResult>) {
    let mut seen = std::collections::HashSet::new();
    let mut executable = Vec::new();
    let mut gated_results = Vec::new();
    for call in calls {
        if !seen.insert(call.id.clone()) {
            warn!(
                "Skipping duplicate tool call identity {} ({})",
                call.id, call.function.name
            );
            continue;
        }
        if super::tool_calls::has_invalid_input(call) {
            // Log the call identity only, never the raw arguments.
            warn!(
                "Gating tool call {} ({}): invalid input (provider-marked, malformed or not a JSON object)",
                call.id, call.function.name
            );
            gated_results.push(super::tool_calls::invalid_input_result(call));
        } else {
            executable.push(call.clone());
        }
    }
    (executable, gated_results)
}

/// Transition from CallLlm to AfterLlm.
///
/// This invokes the LLM (with or without tools), handles streaming for codex provider,
/// tracks usage/costs, and emits LlmRequestStart/End events.
#[instrument(
    name = "agent.transition.call_llm",
    skip(config, context, request, exec_ctx),
    fields(
        otel.name = %format!("chat {}", context.model),
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = %genai::provider_name(&context.provider),
        gen_ai.request.model = %context.model,
        gen_ai.conversation.id = %exec_ctx.session_id,
        session.id = %exec_ctx.session_id,
        gen_ai.request.stream = tracing::field::Empty,
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
    // A single expected generation; early cancellation/drop retains an abnormal finish
    // without marking the span as an error. Successful canonical output replaces it.
    genai::finish_reason("error");
    let chat_span = tracing::Span::current();
    genai::agent_id(&chat_span, config.provider.agent_id.as_deref());
    let session_id = &exec_ctx.session_id;
    let request_messages = request.messages.as_ref();
    genai::conversation_compacted(&chat_span, &exec_ctx.compaction_summaries, request_messages);
    let tools = &request.tools;
    debug!(
        "CallLlm: session={}, messages={}",
        session_id,
        request_messages.len()
    );

    if exec_ctx.cancellation_token.is_cancelled() {
        return Ok(ExecutionState::Cancelled);
    }

    // Overrides last for exactly this request. Options the provider does not
    // honour are dropped (and logged); `applied` records what was really sent.
    let options = context
        .profile
        .capabilities
        .options
        .retain_supported(request.options);
    if options != request.options {
        debug!(
            "provider does not honour some per-request generation options; skipping them (requested={:?}, applied={:?})",
            request.options, options
        );
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
    // Recorded on every exit path (success, error, cancel) when dropped.
    let chat_timer = crate::agent::utils::llm_metrics::ChatTimer::start(
        genai::provider_name(&context.provider),
        &context.model,
        &exec_ctx.cancellation_token,
    );
    let operation = main_attempt_operation(config, exec_ctx)
        .instrument(info_span!("agent.accounting.open"))
        .await?;

    // Pre-allocated message_id for streaming path so that delta events and the
    // final AssistantMessageStored share the same ID.
    let mut streaming_message_id: Option<String> = None;

    // Determine response via streaming or non-streaming path.
    // Each arm produces canonical ordered output so the rest of the function is
    // uniform and item-aware data survives without flattening.
    let output: ChatOutput = if tools.is_empty() {
        // No tools — always use the non-streaming simple submit path.
        let cancel = exec_ctx.cancellation_token.clone();
        match super::llm_retry::call_with_retry(
            config,
            session_id,
            "chat",
            &exec_ctx.cancellation_token,
            || {
                let messages_with_cache = &messages_with_cache;
                let cancel = cancel.clone();
                let operation = &operation;
                async move {
                    // Provider resolution is local: a failure here is not a
                    // paid attempt. Billing is bound once it is resolved.
                    let provider = tokio::select! {
                        result = session_handle.provider() => result.map_err(LLMError::from)?,
                        _ = cancel.cancelled() => return Err(LLMError::Cancelled),
                    };
                    // Per-request options the provider does not honour are dropped.
                    let options =
                        crate::session::provider::supported_overrides(provider.as_ref(), options);
                    let call = ChatCall::new(messages_with_cache).options(options);
                    crate::accounting::run_attempt(
                        operation,
                        session_handle.billing_basis(),
                        &cancel,
                        || provider.chat(&call),
                    )
                    .await
                }
            },
        )
        .await
        {
            Ok(resp) => {
                session_handle.record_generation_settings(&chat_span, options);
                resp
            }
            Err(e) => {
                session_handle.record_generation_settings(&chat_span, options);
                if let Some(stop) = budget_stop(&operation, context) {
                    return Ok(stop);
                }
                return map_failed_llm_call(e, false, context);
            }
        }
    } else {
        let provider = match super::llm_retry::call_with_retry(
            config,
            session_id,
            "provider_init",
            &exec_ctx.cancellation_token,
            || {
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

        session_handle.record_generation_settings(&chat_span, options);
        if provider.capabilities().streaming {
            tracing::Span::current().record("gen_ai.request.stream", true);
            // === STREAMING PATH (all capable providers) ===
            let message_id = Uuid::new_v4().to_string();
            streaming_message_id = Some(message_id.clone());

            let max_stream_retries = config.execution_policy.rate_limit.max_stream_retries;

            // Usage events that arrive after the terminal chunk bypass the
            // accumulator (which rejects post-terminal events) and are merged
            // into the attempt output afterwards.
            #[allow(unused_assignments)]
            let mut drained_usage: Option<querymt::Usage> = None;

            // Batching buffers — we flush at most every 50ms or 256 chars to
            // avoid per-token React state updates on fast local models.
            // Both legacy display chunks and structured item deltas feed them.
            let mut text_buffer = String::new();
            let mut thinking_buffer = ThinkingPartBuffer::new();
            let mut reasoning_item_ids: HashMap<usize, String> = HashMap::new();
            // First id for a reasoning part sticks. A later item id must not split it.
            let mut pinned_reasoning_part_ids: HashMap<(usize, usize, UiReasoningKind), String> =
                HashMap::new();
            #[allow(unused_assignments)]
            let mut last_flush = Instant::now();
            const BATCH_INTERVAL: Duration = Duration::from_millis(50);
            const BATCH_CHARS: usize = 256;

            macro_rules! emit_thinking_part {
                ($part_id:expr, $text:expr) => {{
                    let thinking_delta: String = $text;
                    if !thinking_delta.is_empty() {
                        debug!(
                            "stream flush: session={} message_id={} thinking_delta_len={} part_id={:?}",
                            session_id,
                            message_id,
                            thinking_delta.len(),
                            $part_id
                        );
                        config.emit_event(
                            session_id,
                            AgentEventKind::AssistantThinkingDelta {
                                content: thinking_delta,
                                message_id: message_id.clone(),
                                part_id: $part_id,
                            },
                        );
                    }
                }};
            }

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
                    {
                        let (part_id, thinking_delta) = thinking_buffer.take();
                        emit_thinking_part!(part_id, thinking_delta);
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
            let mut timing = genai::StreamTiming::default();
            let streamed_output: ChatOutput = 'stream: loop {
                let mut semantic_output_seen = false;

                // Attempt-local accumulator: selects structured mode when
                // structured metadata appears before semantic output; legacy
                // chunks synthesize a limited projection otherwise. This is
                // recreated per attempt (and consumed by finalization) so retries
                // never mix partial items.
                let mut accumulator = ChatStreamAccumulator::new();
                #[allow(unused_assignments)]
                {
                    drained_usage = None;
                }
                text_buffer.clear();
                thinking_buffer.clear();
                reasoning_item_ids.clear();
                pinned_reasoning_part_ids.clear();
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

                // Durable start before the request is sent; a failure here
                // means nothing was dispatched.
                let mut attempt = match operation
                    .begin(
                        crate::accounting::DispatchMode::Streaming,
                        session_handle.billing_basis(),
                    )
                    .await
                {
                    Ok(attempt) => attempt,
                    Err(error) => {
                        if let Some(stop) = budget_stop(&operation, context) {
                            return Ok(stop);
                        }
                        return Err(accounting_failure(error));
                    }
                };
                let mut response_started = false;

                timing.start();
                let mut stream = match provider
                    .chat_stream(
                        &ChatCall::new(messages_with_cache)
                            .tools(tools.as_ref())
                            .options(options),
                    )
                    .await
                {
                    Ok(stream) => {
                        timing.headers_received(&chat_span);
                        stream
                    }
                    Err(error) => match super::llm_retry::handle_stream_failure(
                        config,
                        session_id,
                        {
                            attempt
                                .settle_error(&error, false, None)
                                .await
                                .map_err(accounting_failure)?;
                            error
                        },
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
                    timing.wait_begin();
                    let item = super::llm_retry::next_stream_chunk(
                        &mut stream,
                        &exec_ctx.cancellation_token,
                    )
                    .await;
                    timing.wait_end();

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
                            {
                                attempt
                                    .settle_error(
                                        &error,
                                        response_started,
                                        accumulator.output().usage,
                                    )
                                    .await
                                    .map_err(accounting_failure)?;
                                error
                            },
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

                    timing.chunk_received(&chat_span, genai::chunk_commits_output(&chunk));
                    response_started = true;
                    semantic_output_seen |= super::llm_retry::stream_chunk_commits_output(&chunk);

                    // Canonical accumulation. Accumulator violations are provider
                    // stream-contract failures and follow the retry policy.
                    if let Err(error) = accumulator.push(&chunk) {
                        debug!(
                            "Stream accumulator rejected chunk: session={} message_id={} error={}",
                            session_id, message_id, error
                        );
                        match super::llm_retry::handle_stream_failure(
                            config,
                            session_id,
                            {
                                let error = stream_accumulation_error(error);
                                attempt
                                    .settle_error(&error, true, accumulator.output().usage)
                                    .await
                                    .map_err(accounting_failure)?;
                                error
                            },
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
                        }
                    }

                    // Display projections for the UI. In structured mode these
                    // legacy-shaped chunks are compatibility projections; the
                    // accumulator ignores them for canonical history.
                    match chunk {
                        StreamChunk::Structured(event) => {
                            if let StructuredStreamEvent::ItemStarted { output_index, item }
                            | StructuredStreamEvent::ItemCompleted { output_index, item } =
                                &event
                            {
                                remember_reasoning_item_id(
                                    &mut reasoning_item_ids,
                                    *output_index,
                                    item,
                                );
                            }
                            match structured_delta_for_ui(&event) {
                                Some(UiStreamDelta::Text(delta)) => text_buffer.push_str(delta),
                                Some(UiStreamDelta::Reasoning {
                                    output_index,
                                    part_index,
                                    kind,
                                    delta,
                                }) => {
                                    let part_id = pinned_reasoning_part_id(
                                        &mut pinned_reasoning_part_ids,
                                        &reasoning_item_ids,
                                        output_index,
                                        part_index,
                                        kind,
                                    );
                                    if let Some((previous_id, previous_text)) =
                                        thinking_buffer.push(Some(part_id), delta)
                                    {
                                        emit_thinking_part!(previous_id, previous_text);
                                    }
                                }
                                None => {}
                            }
                            if matches!(event, StructuredStreamEvent::ResponseTerminal { .. }) {
                                drained_usage =
                                    drain_trailing_usage(&mut stream, &exec_ctx.cancellation_token)
                                        .await;
                                if exec_ctx.cancellation_token.is_cancelled() {
                                    let partial = crate::accounting::merge_usage(
                                        accumulator.output().usage,
                                        drained_usage.take(),
                                    );
                                    attempt
                                        .settle_error(&LLMError::Cancelled, true, partial)
                                        .await
                                        .map_err(accounting_failure)?;
                                    return Ok(ExecutionState::Cancelled);
                                }
                                break;
                            }
                        }
                        StreamChunk::Text(delta) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=text len={}",
                                session_id,
                                message_id,
                                delta.len()
                            );
                            text_buffer.push_str(&delta);
                        }
                        StreamChunk::Thinking(delta) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=thinking len={}",
                                session_id,
                                message_id,
                                delta.len()
                            );
                            // Structured summary parts own the UI projection. A legacy
                            // thinking chunk would glue or duplicate those parts.
                            if !accumulator.is_structured() {
                                let part_id = format!("{message_id}:thinking");
                                if let Some((previous_id, previous_text)) =
                                    thinking_buffer.push(Some(part_id), &delta)
                                {
                                    emit_thinking_part!(previous_id, previous_text);
                                }
                            }
                        }
                        StreamChunk::ThinkingSignature(signature) => {
                            trace!(
                                "stream chunk: session={} message_id={} type=thinking_signature len={}",
                                session_id,
                                message_id,
                                signature.len()
                            );
                            let _ = signature;
                        }
                        StreamChunk::RedactedThinking(data) => {
                            // Opaque provider state: no UI output, never logged.
                            trace!(
                                "stream chunk: session={} message_id={} type=redacted_thinking len={}",
                                session_id,
                                message_id,
                                data.len()
                            );
                        }
                        StreamChunk::ToolUseComplete { tool_call, .. } => {
                            // Flush before tool use so UI sees final text before tool starts
                            trace!(
                                "stream chunk: session={} message_id={} type=tool_use_complete id={}",
                                session_id, message_id, tool_call.id
                            );
                            flush_buffers!(true);
                        }
                        StreamChunk::Usage(u) => {
                            // Keep streamed usage so an abandoned attempt settles with it.
                            attempt.observe_usage(Some(u.clone()));
                            trace!(
                                "stream chunk: session={} message_id={} type=usage input={} output={} reasoning={}",
                                session_id,
                                message_id,
                                u.input_tokens,
                                u.output_tokens,
                                u.reasoning_tokens
                            );
                        }
                        StreamChunk::Done { finish_reason } => {
                            trace!(
                                "stream chunk: session={} message_id={} type=done finish_reason={:?}",
                                session_id, message_id, finish_reason
                            );
                            if accumulator.is_structured() {
                                // Legacy framing accompanying a structured stream is a
                                // display projection; the semantic terminal is the
                                // structured ResponseTerminal event.
                            } else {
                                // Some providers emit Usage AFTER Done in the same SSE
                                // batch. Drain remaining items to capture any trailing
                                // Usage events before exiting the loop. These bypass
                                // the accumulator (post-terminal events are invalid)
                                // and merge into the output after validation.
                                drained_usage =
                                    drain_trailing_usage(&mut stream, &exec_ctx.cancellation_token)
                                        .await;
                                if exec_ctx.cancellation_token.is_cancelled() {
                                    let partial = crate::accounting::merge_usage(
                                        accumulator.output().usage,
                                        drained_usage.take(),
                                    );
                                    attempt
                                        .settle_error(&LLMError::Cancelled, true, partial)
                                        .await
                                        .map_err(accounting_failure)?;
                                    return Ok(ExecutionState::Cancelled);
                                }

                                let completed_calls =
                                    accumulator.output().tool_calls().unwrap_or_default();
                                if let Err(error) =
                                    validate_stream_terminal(finish_reason, &completed_calls)
                                {
                                    match super::llm_retry::handle_stream_failure(
                                        config,
                                        session_id,
                                        {
                                            let rejected = merge_drained_usage(
                                                accumulator.output(),
                                                drained_usage.take(),
                                            );
                                            attempt
                                                .settle_rejected_output(&rejected, false)
                                                .await
                                                .map_err(accounting_failure)?;
                                            error
                                        },
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
                                    }
                                }
                                break;
                            }
                        }
                        StreamChunk::ToolUseStart { .. }
                        | StreamChunk::ToolUseInputDelta { .. } => {}
                    }

                    // Time- or size-based flush
                    if last_flush.elapsed() >= BATCH_INTERVAL
                        || text_buffer.len() >= BATCH_CHARS
                        || thinking_buffer.len() >= BATCH_CHARS
                    {
                        flush_buffers!(true);
                    }
                } // end inner consume loop

                // ── Terminal validation ─────────────────────────────────────
                // A semantic terminal was observed. Validate completion before
                // the response is considered successful; unfinished items or
                // calls are attempt failures and never dispatch tools.
                match accumulator.finish() {
                    ChatStreamFinish::Completed(output) => {
                        let output = merge_drained_usage(output, drained_usage.take());
                        attempt
                            .settle_output(&output)
                            .await
                            .map_err(accounting_failure)?;
                        break 'stream output;
                    }
                    ChatStreamFinish::Incomplete { output, .. } => {
                        warn!(
                            "Provider reported incomplete response: session={} message_id={}",
                            session_id, message_id
                        );
                        // Preserve partial output and the terminal status; the
                        // incomplete cause is handled downstream without retries.
                        let output = merge_drained_usage(output, drained_usage.take());
                        attempt
                            .settle_output(&output)
                            .await
                            .map_err(accounting_failure)?;
                        break 'stream output;
                    }
                    ChatStreamFinish::Failed { output, error } => {
                        debug!(
                            "Stream failed terminal validation: session={} message_id={}",
                            session_id, message_id
                        );
                        match super::llm_retry::handle_stream_failure(
                            config,
                            session_id,
                            {
                                let error = stream_accumulation_error(error);
                                let rejected = merge_drained_usage(output, drained_usage.take());
                                attempt
                                    .settle_rejected_output(&rejected, true)
                                    .await
                                    .map_err(accounting_failure)?;
                                error
                            },
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
                        }
                    }
                }
            }; // end outer retry loop ('stream)

            timing.finish(&chat_span);
            crate::agent::utils::llm_metrics::stream_latency(
                genai::provider_name(&context.provider),
                &context.model,
                timing.first_byte_secs(),
                timing.first_token_secs(),
            );

            // Final flush of any remaining buffered content (no timer reset needed)
            flush_buffers!(false);
            let structured_stream = streamed_output.provenance.is_some()
                || streamed_output
                    .items
                    .iter()
                    .any(|item| matches!(item, querymt::chat::ChatOutputItem::Opaque(_)));
            debug!(
                "stream finished: session={} message_id={} final_text_len={:?} final_thinking_len={:?} tool_calls={} structured={}",
                session_id,
                message_id,
                streamed_output.text().map(|text| text.len()),
                streamed_output.thinking().map(|text| text.len()),
                streamed_output
                    .tool_calls()
                    .map(|calls| calls.len())
                    .unwrap_or(0),
                structured_stream,
            );

            // The streaming loop exits via a terminal chunk, which bypasses the
            // per-chunk cancellation check at the top of the loop. Re-check here
            // so a cancel signal that arrived concurrently with the terminal chunk
            // is not missed — without this the state machine would advance to AfterLlm.
            if exec_ctx.cancellation_token.is_cancelled() {
                return Ok(ExecutionState::Cancelled);
            }

            streamed_output
        } else {
            // === NON-STREAMING FALLBACK ===
            let cancel = exec_ctx.cancellation_token.clone();
            match super::llm_retry::call_with_retry(
                config,
                session_id,
                "chat",
                &exec_ctx.cancellation_token,
                || {
                    let provider = &provider;
                    let messages_with_cache = &messages_with_cache;
                    let tools = tools.as_ref();
                    let cancel = cancel.clone();
                    let operation = &operation;
                    async move {
                        let call = ChatCall::new(messages_with_cache)
                            .tools(tools)
                            .options(options);
                        crate::accounting::run_attempt(
                            operation,
                            session_handle.billing_basis(),
                            &cancel,
                            || provider.chat(&call),
                        )
                        .await
                    }
                },
            )
            .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    if let Some(stop) = budget_stop(&operation, context) {
                        return Ok(stop);
                    }
                    return map_failed_llm_call(e, false, context);
                }
            }
        }
    };

    // Canonical output is authoritative; the flattened fields below are
    // compatibility projections of it.
    // Stop the clock at the end of the model round trip, not after the bookkeeping below.
    chat_timer.succeeded();
    genai::output(&output);
    let response_content = output.text().unwrap_or_default();
    let response_thinking = output.thinking();
    let response_thinking_signature = output.signature();
    let llm_tool_calls = middleware_calls_from_output(&output);
    let tool_calls = &llm_tool_calls;
    let usage = output.usage.clone();
    let finish_reason = output.finish_reason;

    // Presentation only: both figures are views of recorded settlements. The
    // request cost covers every attempt of this logical operation (retries
    // included) and the cumulative figure is the session's recorded total.
    // Each is `None` unless it is a complete supported estimate.
    let request_cost = operation.totals().complete_total_usd();
    let cumulative_cost = exec_ctx
        .cycle_accounting(&config.event_sink)
        .await
        .map_err(|error| anyhow::anyhow!("accounting unavailable: {error}"))?
        .session_totals()
        .complete_total_usd();

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
            accounting: Some(operation.request_accounting()),
        },
    );

    let mut llm_response = LlmResponse::new(response_content, llm_tool_calls, usage, finish_reason)
        .with_applied(options)
        .with_thinking(response_thinking)
        .with_thinking_signature(response_thinking_signature)
        .with_output(Some(output));
    if let Some(mid) = streaming_message_id {
        llm_response = llm_response.with_message_id(mid);
    }

    // The overrides were consumed by this request; later states must not see
    // them. What the request was sent with travels on as `last_applied`.
    let context = if context.request_overrides.is_empty() && context.last_applied == options {
        context.clone()
    } else {
        Arc::new(
            context
                .without_request_overrides()
                .with_last_applied(options),
        )
    };

    Ok(ExecutionState::AfterLlm {
        response: Arc::new(llm_response),
        context,
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

    let structured_output = response.output.clone();

    let mut parts = Vec::new();

    if let Some(output) = &structured_output {
        // Item-aware turn: the canonical output part is the single source of
        // truth. Text/reasoning/tool-use parts are projections and are not
        // serialized as additional history.
        parts.push(MessagePart::Output {
            output: output.clone(),
        });
    } else {
        // Legacy projection persistence (unchanged behavior).
        // Persist thinking/reasoning content before the text part
        if let Some(thinking) = &response.thinking
            && !thinking.is_empty()
        {
            parts.push(MessagePart::Reasoning {
                item: querymt::chat::ChatReasoningItem {
                    id: None,
                    summary: Vec::new(),
                    content: vec![querymt::chat::ChatReasoningPart::text(thinking.clone())],
                    encrypted_content: None,
                    signature: response.thinking_signature.clone(),
                    status: None,
                    extensions: Default::default(),
                },
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

    // A thinking-only reply cut off by the output limit is not history: it is
    // neither persisted nor announced, but its usage below is still counted.
    let store_reply = !response.is_thinking_only_length_cutoff();
    if store_reply {
        exec_ctx
            .add_message(assistant_msg.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store assistant message: {}", e))?;

        let reasoning_parts = response
            .output
            .as_ref()
            .map(reasoning_parts_from_output)
            .unwrap_or_default();
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::AssistantMessageStored {
                content: response.content.clone(),
                thinking: response.thinking.clone(),
                reasoning_parts,
                message_id: Some(assistant_msg.id.clone()),
            },
        );
    } else {
        debug!(
            "Not storing thinking-only reply cut off by the output limit: session={}",
            exec_ctx.session_id
        );
    }

    let mut messages = (*context.messages).to_vec();
    if store_reply {
        // Same-turn continuation is still talking to the provider that just
        // produced this output. Re-converting through `to_chat_message()` with no
        // target treats the destination as unknown and portable-projects encrypted
        // reasoning away before the tool result is appended.
        messages.push(if let Some(output) = structured_output.clone() {
            ChatMessage::from_assistant_output(output)
        } else {
            assistant_msg
                .to_chat_message()
                .map_err(|error| anyhow::anyhow!("Invalid stored assistant content: {error}"))?
        });
    }

    let mut updated_stats = (*context.stats).clone();
    updated_stats.steps += 1;
    apply_usage_to_stats(&mut updated_stats, response.usage.as_ref());
    // Monetary fields are a view of this cycle's recorded components, never a
    // revaluation of token counters at current rates. Applied even when this
    // response reported no usage, so recorded charges stay visible.
    if let Some(cycle) = exec_ctx.cycle_accounting.get() {
        updated_stats.apply_recorded_costs(&cycle.cycle_totals());
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
        .with_profile(context.profile.clone())
        .with_last_applied(response.applied)
        .with_fragments(context.fragments.clone()),
    );

    let output_status = structured_output.as_ref().and_then(|output| output.status);

    if output_status == Some(ChatOutputStatus::Failed) {
        return Err(anyhow::anyhow!(
            "Provider reported a failed response (provider={}, model={})",
            context.provider,
            context.model
        ));
    }

    // Gate local execution on response-level validation. Only calls with valid
    // JSON-object arguments dispatch to the executor; invalid or duplicate
    // identities produce synthesized results that keep the
    // tool_use/tool_result history invariant without executing.
    let (mut executable_calls, mut gated_results) =
        gate_function_calls_for_execution(&response.tool_calls);

    // A length finish reason is a cut-off whatever status the provider
    // reported: Anthropic flushes interrupted tool blocks as completed (but
    // invalid) calls, so such a reply arrives as `Completed` + `Length`.
    let is_length_cutoff = response.is_length_cutoff();
    if output_status == Some(ChatOutputStatus::Incomplete) || is_length_cutoff {
        // An incomplete or cut-off terminal never authorizes execution, even
        // when some items completed. Every call gets exactly one error result:
        // invalid calls keep their `INVALID_JSON` result, the rest are skipped.
        let detail = response
            .output
            .as_ref()
            .map(|_| "response is incomplete".to_string())
            .unwrap_or_default();
        let skipped_text = skipped_call_text(response.finish_reason, &detail);
        if is_length_cutoff {
            // Invalid calls also get the split-the-write guidance.
            for result in &mut gated_results {
                result.content.push(querymt::chat::ToolResultPart::Text {
                    text: skipped_text.clone(),
                });
            }
        }
        let to_skip: Vec<MiddlewareToolCall> = std::mem::take(&mut executable_calls);
        gated_results.extend(to_skip.into_iter().map(|call| {
            ToolResult::new(
                call.id,
                vec![querymt::chat::ToolResultPart::Text {
                    text: skipped_text.clone(),
                }],
                true,
                Some(call.function.name),
                Some(call.function.arguments),
            )
        }));

        if gated_results.is_empty() {
            let (message, stop_type) = match response.finish_reason {
                Some(FinishReason::Length) => (
                    "Model hit token limit".to_string(),
                    StopType::ModelTokenLimit,
                ),
                Some(FinishReason::ContentFilter) => (
                    "Response blocked by content filter".into(),
                    StopType::ContentFilter,
                ),
                _ => (format!("Response incomplete. {detail}"), StopType::Other),
            };
            return Ok(ExecutionState::Stopped {
                message: message.into(),
                stop_type,
                context: Some(new_context),
            });
        }

        let results: Arc<[ToolResult]> = Arc::from(gated_results.into_boxed_slice());

        if response.finish_reason == Some(FinishReason::Length) {
            return store_results_then_stop_on_length(config, results, new_context, exec_ctx).await;
        }

        // Store the synthesized results (no execution) by routing through the
        // tool-result storage step with no remaining calls.
        return Ok(ExecutionState::ProcessingToolCalls {
            remaining_calls: Arc::from(Vec::new().into_boxed_slice()),
            results,
            context: new_context,
        });
    }

    let has_dispatchable_calls = !executable_calls.is_empty() || !gated_results.is_empty();

    match response.finish_reason {
        Some(FinishReason::ToolCalls) => {
            if has_dispatchable_calls {
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls: Arc::from(executable_calls.into_boxed_slice()),
                    results: Arc::from(gated_results.into_boxed_slice()),
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

        Some(FinishReason::Length) => {
            // Cut-offs with tool calls were answered and stopped above.
            debug_assert!(!has_dispatchable_calls);
            Ok(ExecutionState::Stopped {
                message: "Model hit token limit".into(),
                stop_type: StopType::ModelTokenLimit,
                context: Some(new_context),
            })
        }

        Some(FinishReason::ContentFilter) => Ok(ExecutionState::Stopped {
            message: "Response blocked by content filter".into(),
            stop_type: StopType::ContentFilter,
            context: Some(new_context),
        }),

        Some(FinishReason::Error)
        | Some(FinishReason::Unknown)
        | Some(FinishReason::Other)
        | None => {
            if has_dispatchable_calls {
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls: Arc::from(executable_calls.into_boxed_slice()),
                    results: Arc::from(gated_results.into_boxed_slice()),
                    context: new_context,
                })
            } else {
                Ok(ExecutionState::Complete {
                    context: new_context,
                })
            }
        }
    }
}

/// Transition from ProcessingToolCalls to BeforeLlmCall or WaitingForEvent.
///
/// The batch is planned into ordered waves (see `wave_plan::plan_waves`).
/// Calls in one wave have no file conflicts and run concurrently; calls that
/// conflict on a file run in later waves in the model's original order, and
/// stateful / clarification calls act as solo barrier waves. Results are
/// stored in the original call order. Then either:
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
    skip(config, driver, remaining_calls, results, context, exec_ctx),
    fields(
        session_id = %exec_ctx.session_id,
        remaining_calls = remaining_calls.len(),
        completed_results = results.len(),
        wave_count = tracing::field::Empty,
        serialized_calls = tracing::field::Empty,
        state_reload_ms = tracing::field::Empty,
    )
)]
pub(super) async fn transition_processing_tool_calls(
    config: &AgentConfig,
    driver: &CompositeDriver,
    remaining_calls: &Arc<[MiddlewareToolCall]>,
    results: &Arc<[ToolResult]>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
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

    // Calls are prepared (normalised, hooked, validated, access-declared) one
    // execution-barrier-delimited segment at a time, in model order. A segment is
    // a run of `ParallelSafe` calls or a single barrier call; nothing beyond a
    // stateful or clarification barrier is prepared until the barrier has
    // completed, so later hooks observe its effects.
    let classes: Vec<crate::tools::ToolExecutionClass> = remaining_calls
        .iter()
        .map(|call| {
            config
                .tool_registry
                .find(&call.function.name)
                .map(|tool| tool.execution_class())
                .unwrap_or(crate::tools::ToolExecutionClass::ParallelSafe)
        })
        .collect();
    let segments = split_segments(&classes);
    debug!(
        "Executing {} tool calls in {} segment(s) for session {}",
        remaining_calls.len(),
        segments.len(),
        exec_ctx.session_id
    );

    // One slot per call so results can be stored in the model's original order
    // regardless of the order in which waves completed.
    let mut slots: Vec<Option<ToolResult>> = (0..remaining_calls.len()).map(|_| None).collect();
    let mut cancelled = already_cancelled;
    let mut clarification_applied = false;
    let mut wave_count = 0u64;
    let mut serialized_calls = 0u64;
    let mut state_reload_ms = 0u64;

    for segment in &segments {
        if cancelled || clarification_applied {
            break;
        }
        if exec_ctx.cancellation_token.is_cancelled() {
            cancelled = true;
            break;
        }

        let indices: Vec<usize> = match segment {
            Segment::Parallel(indices) => indices.clone(),
            Segment::Barrier(index) => vec![*index],
        };
        let segment_plan =
            prepare_segment(config, driver, remaining_calls, &indices, exec_ctx).await?;
        for (index, result) in segment_plan.immediate {
            slots[index] = Some(result);
        }
        let mut prepared: Vec<Option<(PreparedCall, crate::tools::FileAccess)>> =
            (0..remaining_calls.len()).map(|_| None).collect();
        let mut call_infos: Vec<CallInfo> = Vec::with_capacity(segment_plan.ready.len());
        let mut ready_indices: Vec<usize> = Vec::with_capacity(segment_plan.ready.len());
        for (index, call, access) in segment_plan.ready {
            call_infos.push(CallInfo {
                class: classes[index],
                access: access.clone(),
            });
            ready_indices.push(index);
            prepared[index] = Some((call, access));
        }

        match segment {
            Segment::Parallel(_) => {
                // Plan the prepared calls into waves; calls inside a wave never
                // conflict on a file and run concurrently.
                let waves = plan_waves(&call_infos);
                wave_count += waves.len() as u64;
                serialized_calls += serialized_call_count(&call_infos, &waves) as u64;
                for wave in &waves {
                    if exec_ctx.cancellation_token.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                    // Reborrow immutably so the context can be shared across the
                    // wave; execution only needs `&ExecutionContext`.
                    let exec_ctx_ref: &ExecutionContext = exec_ctx;
                    let futures = wave.iter().map(|&position| {
                        let index = ready_indices[position];
                        let call = &remaining_calls[index];
                        let (prepared_call, _) =
                            prepared[index].take().expect("prepared call executes once");
                        async move {
                            (
                                index,
                                run_prepared_tool_call(config, call, prepared_call, exec_ctx_ref)
                                    .await,
                            )
                        }
                    });
                    for (index, result) in join_all(futures).await {
                        slots[index] = Some(result);
                    }
                }
            }
            Segment::Barrier(index) => {
                let index = *index;
                if let Some((prepared_call, _)) = prepared[index].take() {
                    wave_count += 1;
                    let class = classes[index];
                    let call = &remaining_calls[index];
                    let result =
                        run_prepared_tool_call(config, call, prepared_call, exec_ctx).await;
                    if class == crate::tools::ToolExecutionClass::ClarificationBoundary
                        && !result.is_error
                    {
                        clarification_applied =
                            apply_clarification_answer(exec_ctx, call, &result).await;
                    }
                    slots[index] = Some(result);
                    if class == crate::tools::ToolExecutionClass::SerialStateful {
                        // Following calls are prepared against the updated state.
                        state_reload_ms += reload_working_context(exec_ctx).await;
                    }
                }
            }
        }

        if exec_ctx.cancellation_token.is_cancelled() {
            cancelled = true;
        }
    }

    let span = tracing::Span::current();
    span.record("wave_count", wave_count);
    span.record("serialized_calls", serialized_calls);
    span.record("state_reload_ms", state_reload_ms);

    let was_cancelled = cancelled || exec_ctx.cancellation_token.is_cancelled();

    // Exactly one result per call: calls that never started (cancellation, or
    // a clarification that changed the run objective) get a synthetic result.
    let mut all_results = (**results).to_vec();
    all_results.reserve(remaining_calls.len());
    for (call, slot) in remaining_calls.iter().zip(slots) {
        all_results.push(slot.unwrap_or_else(|| {
            let message = if was_cancelled {
                "Error: Cancelled by user"
            } else {
                "Skipped because a user clarification changed the run objective; the model must reconsider this action."
            };
            synthetic_error_result(call, message)
        }));
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

/// A run of calls prepared together: either consecutive `ParallelSafe` calls or
/// one execution-barrier call (stateful or clarification).
#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Parallel(Vec<usize>),
    Barrier(usize),
}

/// Split a batch (by execution class, in model order) into barrier-delimited
/// segments. Every call index appears in exactly one segment, in order.
fn split_segments(classes: &[crate::tools::ToolExecutionClass]) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for (index, class) in classes.iter().enumerate() {
        if *class == crate::tools::ToolExecutionClass::ParallelSafe {
            match segments.last_mut() {
                Some(Segment::Parallel(indices)) => indices.push(index),
                _ => segments.push(Segment::Parallel(vec![index])),
            }
        } else {
            segments.push(Segment::Barrier(index));
        }
    }
    segments
}

/// Outcome of preparing one segment.
struct SegmentPlan {
    /// Calls whose final result is already known (rejected or failed during
    /// preparation, or rejected by middleware review): `(call index, result)`.
    immediate: Vec<(usize, ToolResult)>,
    /// Executable calls with the file access declared for their effective
    /// arguments, in model order: `(call index, prepared call, access)`.
    ready: Vec<(usize, PreparedCall, crate::tools::FileAccess)>,
}

/// Prepare the calls of one segment in model order, declare their file access
/// from the effective arguments and let middleware review the prepared batch.
///
/// Cancellation is observed before each call is prepared; calls not reached keep
/// no result here and receive the ordered cancellation result later. Their
/// pre-tool hooks do not run.
async fn prepare_segment(
    config: &AgentConfig,
    driver: &CompositeDriver,
    calls: &[MiddlewareToolCall],
    indices: &[usize],
    exec_ctx: &ExecutionContext,
) -> Result<SegmentPlan, anyhow::Error> {
    let mut immediate: Vec<(usize, ToolResult)> = Vec::new();
    let mut prepared: Vec<(usize, PreparedCall)> = Vec::new();

    for &index in indices {
        if exec_ctx.cancellation_token.is_cancelled() {
            break;
        }
        let call = &calls[index];
        let outcome = tokio::select! {
            biased;
            _ = exec_ctx.cancellation_token.cancelled() => break,
            outcome = super::tool_calls::prepare_tool_call(config, call, exec_ctx) => outcome,
        };
        match outcome {
            Ok(prepared_call) => match prepared_call.disposition {
                Disposition::Rejected(ref result) => {
                    super::tool_calls::record_rejected_outcome(
                        call,
                        !prepared_call.repairs.is_empty(),
                    );
                    immediate.push((index, (**result).clone()));
                }
                Disposition::Ready => prepared.push((index, prepared_call)),
            },
            Err(error) => {
                warn!(
                    "Tool call {} ({}) failed during preparation: {}. \
                     Synthesizing error result to maintain tool_use/tool_result invariant.",
                    call.id, call.function.name, error
                );
                immediate.push((
                    index,
                    synthetic_error_result(
                        call,
                        &format!("Error: internal tool execution failed: {error}"),
                    ),
                ));
            }
        }
    }

    // File access is declared once, for the final effective arguments.
    let cwd = exec_ctx.cwd().map(|path| path.to_path_buf());
    let accesses = config
        .tool_registry
        .file_access_batch(
            prepared
                .iter()
                .map(|(index, call)| (calls[*index].function.name.clone(), call.args.clone()))
                .collect(),
            cwd.clone(),
        )
        .await;

    let verdicts = {
        let views: Vec<PreparedToolCall<'_>> = prepared
            .iter()
            .zip(&accesses)
            .map(|((index, call), access)| PreparedToolCall {
                call: &calls[*index],
                args: &call.args,
                access,
            })
            .collect();
        driver
            .run_prepared_tool_calls(&PreparedBatch {
                session_id: &exec_ctx.session_id,
                cwd: cwd.as_deref(),
                calls: &views,
            })
            .await
            .map_err(|error| anyhow::anyhow!("Middleware error: {error}"))?
    };

    let mut ready = Vec::with_capacity(prepared.len());
    for (((index, call), access), verdict) in prepared.into_iter().zip(accesses).zip(verdicts) {
        match verdict {
            PreparedCallVerdict::Proceed => ready.push((index, call, access)),
            PreparedCallVerdict::Reject(result) => {
                super::tool_calls::record_rejected_outcome(&calls[index], !call.repairs.is_empty());
                immediate.push((index, *result));
            }
        }
    }
    Ok(SegmentPlan { immediate, ready })
}

/// Reload the working context after a stateful tool ran so later calls are
/// prepared against the updated state. Returns the elapsed milliseconds.
async fn reload_working_context(exec_ctx: &mut ExecutionContext) -> u64 {
    let started = Instant::now();
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
    started.elapsed().as_millis() as u64
}

/// Build a synthetic error result for a call that did not produce one.
fn synthetic_error_result(call: &MiddlewareToolCall, text: &str) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        vec![querymt::chat::ToolResultPart::Text {
            text: text.to_string(),
        }],
        true,
        Some(call.function.name.clone()),
        Some(call.function.arguments.clone()),
    )
}

/// Execute one prepared call, racing it against the cancellation token.
///
/// A cancelled call resolves immediately with a synthetic `"Cancelled by user"`
/// result, and an infrastructure error becomes an error result, so the
/// `tool_use` → `tool_result` history invariant always holds. The prepared
/// call is consumed: arguments are never re-parsed and hooks never re-run.
async fn run_prepared_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    prepared: PreparedCall,
    exec_ctx: &ExecutionContext,
) -> ToolResult {
    let cancel = exec_ctx.cancellation_token.clone();
    tokio::select! {
        result = super::tool_calls::execute_prepared_tool_call(config, call, prepared, exec_ctx) => {
            match result {
                Ok(tool_result) => tool_result,
                Err(error) => {
                    warn!(
                        "Tool call {} ({}) failed with infrastructure error: {}. \
                         Synthesizing error result to maintain tool_use/tool_result invariant.",
                        call.id, call.function.name, error
                    );
                    synthetic_error_result(
                        call,
                        &format!("Error: internal tool execution failed: {error}"),
                    )
                }
            }
        }
        _ = cancel.cancelled() => synthetic_error_result(call, "Error: Cancelled by user"),
    }
}

/// Fold a successful clarification answer into the run objective.
///
/// Returns `true` when the answer was persisted and applied, which means the
/// rest of the batch must be skipped so the model can reconsider.
async fn apply_clarification_answer(
    exec_ctx: &mut ExecutionContext,
    call: &MiddlewareToolCall,
    tool_result: &ToolResult,
) -> bool {
    let clarification = tool_result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    if clarification.trim().is_empty() {
        return false;
    }

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
            if let Some(objective) = exec_ctx.run_objective.as_mut() {
                objective.amend(crate::agent::objective::ObjectiveDirective {
                    text: clarification,
                    source: crate::agent::objective::ObjectiveSource::ClarificationAnswer,
                    source_ref: Some(call.id.clone()),
                    accepted_at_ms: None,
                    application_boundary: Some("clarification_boundary".to_string()),
                });
            }
            true
        }
        Err(error) => {
            warn!(
                "Failed to persist clarification intent projection for tool call {}: {}",
                call.id, error
            );
            false
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    mod genai_trace_tests {
        use super::*;
        use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use opentelemetry::{
            Array, Value,
            trace::{SpanKind, Status},
        };
        use querymt::chat::ChatOutputProvenance;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn request() -> Arc<PreparedModelRequest> {
            Arc::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("SECRET_PROMPT").build()].into(),
                tools: Arc::from([]),
                estimated_tokens: 1,
                options: Default::default(),
            })
        }

        #[tokio::test]
        async fn genai_compacted_requires_retained_typed_effective_summary() {
            use crate::agent::agent_config_builder::AgentConfigBuilder;
            use crate::hooks::{Hooks, HooksConfig};
            use crate::session::backend::StorageBackend;
            use crate::session::compaction::SessionCompaction;
            use crate::test_utils::helpers::genai_trace::capture_info;

            for mode in [
                "retained",
                "cached",
                "removed",
                "modified",
                "role_changed",
                "hook_removed",
                "hook_modified",
                "request_only",
                "orphan_request",
                "blank",
                "untyped",
                "superseded",
                "reload_removed",
                "empty",
            ] {
                let mut mock = MockLlmProvider::new();
                mock.expect_chat().times(1).returning(|_| {
                    Ok(ChatOutput::from_projections(
                        None,
                        Some("SECRET_RESPONSE".into()),
                        None,
                        None,
                        Some(FinishReason::Stop),
                    ))
                });
                let fixture =
                    TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
                let mut exec = fixture.execution_context().await;
                let (boundary, mut summary) = SessionCompaction::create_compaction_messages(
                    &exec.session_id,
                    if mode == "blank" {
                        " \n "
                    } else {
                        "SECRET_PROMPT"
                    },
                    100,
                );
                let boundary_id = boundary.id.clone();
                if mode == "untyped" {
                    summary.parts = vec![MessagePart::Text {
                        content: "SECRET_PROMPT".into(),
                    }];
                } else if mode == "superseded" {
                    summary.parts.push(MessagePart::Output {
                        output: ChatOutput::from_projections(
                            None,
                            Some("SECRET_ARGUMENT".into()),
                            None,
                            None,
                            Some(FinishReason::Stop),
                        ),
                    });
                }
                if mode != "empty" {
                    exec.add_message(boundary).await.unwrap();
                    if mode != "request_only" {
                        exec.add_message(summary).await.unwrap();
                    }
                }
                if mode == "orphan_request" {
                    let (orphan, _) = SessionCompaction::create_compaction_messages(
                        &exec.session_id,
                        "unused",
                        100,
                    );
                    exec.add_message(orphan).await.unwrap();
                }
                let (mut messages, summaries) =
                    exec.session_handle.history_with_compaction().await.unwrap();
                exec.compaction_summaries = summaries;
                if mode == "reload_removed" {
                    assert_eq!(exec.compaction_summaries.len(), 1);
                    fixture
                        .config
                        .provider
                        .history_store()
                        .delete_messages_after(&exec.session_id, &boundary_id)
                        .await
                        .unwrap();
                    let (_, refreshed) =
                        exec.session_handle.history_with_compaction().await.unwrap();
                    exec.compaction_summaries = refreshed;
                }
                let context = Arc::new(crate::middleware::ConversationContext::new(
                    exec.session_id.as_str().into(),
                    Arc::from(messages.clone()),
                    Arc::new(crate::middleware::AgentStats::default()),
                    "mock".into(),
                    "mock-model".into(),
                ));
                let mut builder = AgentConfigBuilder::from_provider(
                    fixture.storage.clone(),
                    fixture.config.provider.clone(),
                    fixture.storage.event_journal(),
                );
                if mode.starts_with("hook_") {
                    let replacement = if mode == "hook_removed" {
                        vec![ChatMessage::user().text("SECRET_ARGUMENT").build()]
                    } else {
                        vec![ChatMessage::assistant().text("SECRET_ARGUMENT").build()]
                    };
                    let output = serde_json::json!({"hook_specific_output": {
                        "hook_event_name": "context", "messages": replacement,
                    }})
                    .to_string();
                    builder = builder.with_hooks(Hooks::new(serde_json::from_value::<HooksConfig>(serde_json::json!({
                        "enabled": true, "context": [{"hooks": [{"type": "command",
                            "command": format!("printf '%s' '{output}'"), "timeout_sec": 5}]}],
                    })).unwrap()).unwrap());
                }
                let config = builder.build();
                match mode {
                    "cached" => messages = apply_cache_breakpoints(&messages),
                    "removed" => {
                        messages.pop();
                    }
                    "modified" => {
                        *messages.last_mut().unwrap() =
                            ChatMessage::assistant().text("SECRET_ARGUMENT").build();
                    }
                    "role_changed" => {
                        messages.last_mut().unwrap().role = ChatRole::User;
                    }
                    _ => {}
                }
                let (result, spans) = capture_info(async {
                    let request = if mode.starts_with("hook_") {
                        let ExecutionState::CallLlm { request, .. } =
                            transition_before_llm_call(&config, &context, &exec)
                                .await
                                .unwrap()
                        else {
                            panic!("expected prepared request");
                        };
                        request
                    } else {
                        Arc::new(PreparedModelRequest {
                            messages: Arc::from(messages),
                            tools: Arc::from([]),
                            estimated_tokens: 1,
                            options: Default::default(),
                        })
                    };
                    transition_call_llm(&config, &context, &request, &exec).await
                })
                .await;
                assert!(
                    matches!(result.unwrap(), ExecutionState::AfterLlm { .. }),
                    "mode={mode}"
                );
                let chats: Vec<_> = spans
                    .iter()
                    .filter(|span| {
                        attr(span, "gen_ai.operation.name") == Some(&Value::from("chat"))
                    })
                    .collect();
                assert_eq!(chats.len(), 1, "mode={mode}");
                assert_eq!(
                    attr(chats[0], "gen_ai.conversation.compacted"),
                    matches!(mode, "retained" | "cached").then_some(&Value::Bool(true)),
                    "mode={mode}"
                );
                assert_private(&spans);
            }
        }

        pub(super) type TimedChunks = Result<Vec<(u64, Result<StreamChunk, LLMError>)>, LLMError>;

        #[derive(Clone)]
        struct TimedProvider {
            attempts: Arc<std::sync::Mutex<std::collections::VecDeque<TimedChunks>>>,
            clock: Arc<std::sync::Mutex<(Option<Instant>, Option<f64>)>>,
            streaming: bool,
            /// Per-request options each physical request was sent with.
            options_seen: Arc<std::sync::Mutex<Vec<querymt::chat::GenerationOptions>>>,
            /// Options the provider claims to honour.
            support: Arc<std::sync::Mutex<querymt::chat::GenerationOptionSupport>>,
        }

        #[async_trait::async_trait]
        impl querymt::chat::ChatProvider for TimedProvider {
            fn capabilities(&self) -> querymt::chat::ChatCapabilities {
                querymt::chat::ChatCapabilities::NONE
                    .with_streaming(self.streaming)
                    .with_options(*self.support.lock().unwrap())
            }
            async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
                self.options_seen.lock().unwrap().push(call.options);
                Ok(ChatOutput::from_projections(
                    None,
                    Some("SECRET_RESPONSE".into()),
                    None,
                    None,
                    Some(FinishReason::Stop),
                ))
            }
            async fn chat_stream(
                &self,
                call: &ChatCall<'_>,
            ) -> Result<querymt::chat::ChatStream, LLMError> {
                self.options_seen.lock().unwrap().push(call.options);
                self.clock
                    .lock()
                    .unwrap()
                    .0
                    .get_or_insert_with(Instant::now);
                tokio::time::sleep(Duration::from_millis(20)).await;
                let chunks = self
                    .attempts
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("physical attempt")?;
                let pending_tail = !chunks
                    .iter()
                    .any(|(_, chunk)| chunk.as_ref().is_ok_and(querymt::chat::chunk_is_terminal));
                let clock = self.clock.clone();
                Ok(Box::pin(
                    futures_util::stream::iter(chunks)
                        .then(move |(delay, chunk)| {
                            let clock = clock.clone();
                            async move {
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                                if chunk.is_ok() {
                                    let mut clock = clock.lock().unwrap();
                                    if clock.1.is_none() {
                                        clock.1 = Some(clock.0.unwrap().elapsed().as_secs_f64());
                                    }
                                }
                                chunk
                            }
                        })
                        .chain(futures_util::stream::pending().take(usize::from(pending_tail))),
                ))
            }
        }
        #[async_trait::async_trait]
        impl querymt::completion::CompletionProvider for TimedProvider {
            async fn complete(
                &self,
                _: &querymt::completion::CompletionRequest,
            ) -> Result<querymt::completion::CompletionResponse, LLMError> {
                Err(LLMError::NotImplemented("test".into()))
            }
        }
        #[async_trait::async_trait]
        impl querymt::embedding::EmbeddingProvider for TimedProvider {
            async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
                Err(LLMError::NotImplemented("test".into()))
            }
        }
        impl querymt::LLMProvider for TimedProvider {}

        pub(super) struct TimedFactory {
            provider: TimedProvider,
            binding: std::sync::Mutex<serde_json::Value>,
            configs: std::sync::Mutex<Vec<serde_json::Value>>,
            resolutions: AtomicUsize,
            identity: Option<String>,
            name: &'static str,
        }
        impl querymt::plugin::LLMProviderFactory for TimedFactory {
            fn name(&self) -> &str {
                self.name
            }
            fn config_schema(&self) -> String {
                // top_p is deliberately unsupported to exercise final schema pruning.
                serde_json::json!({"type":"object", "properties": {
                    "model":{"type":"string"}, "temperature":{"type":"number"},
                    "max_tokens":{"type":"integer"}, "reasoning_effort":{"type":"string"}
                }, "additionalProperties":false})
                .to_string()
            }
            fn from_config(&self, cfg: &str) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
                self.configs
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(cfg).unwrap());
                Ok(Box::new(self.provider.clone()))
            }
            fn list_models<'a>(
                &'a self,
                _: &str,
            ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
                Box::pin(async { Ok(vec![]) })
            }
        }
        struct TimedResolver(Arc<TimedFactory>);
        #[async_trait::async_trait]
        impl querymt::plugin::host::ProviderResolver for TimedResolver {
            async fn resolve(
                &self,
                logical_name: &str,
            ) -> Result<querymt::plugin::host::ProviderBinding, LLMError> {
                let factory = &self.0;
                factory.resolutions.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(querymt::plugin::host::ProviderBinding {
                    logical_name: logical_name.into(),
                    factory: factory.clone(),
                    static_config: factory.binding.lock().unwrap().clone(),
                    implementation_id: factory.identity.clone(),
                })
            }
        }
        pub(super) async fn timed_fixture(
            streaming: bool,
            attempts: Vec<TimedChunks>,
            name: &'static str,
            identity: Option<&str>,
        ) -> (TestAgent, Arc<TimedFactory>) {
            use crate::session::backend::StorageBackend;
            let mut fixture = TestAgent::new().await;
            let factory = Arc::new(TimedFactory {
                provider: TimedProvider {
                    streaming,
                    attempts: Arc::new(std::sync::Mutex::new(attempts.into())),
                    clock: Default::default(),
                    options_seen: Default::default(),
                    support: Default::default(),
                },
                binding: std::sync::Mutex::new(
                    serde_json::json!({"temperature":0.25, "max_tokens":123, "top_p":0.75}),
                ),
                configs: Default::default(),
                resolutions: AtomicUsize::new(0),
                name,
                identity: identity.map(str::to_owned),
            });
            let provider = crate::session::provider::SessionProvider::new(
                fixture.config.provider.plugin_registry(),
                fixture.storage.session_store(),
                querymt::LLMParams::new()
                    .provider("alias")
                    .model(if name == "xai" {
                        "grok-4.3"
                    } else {
                        "mock-model"
                    })
                    .temperature(1.5),
            )
            .with_provider_resolver(Arc::new(TimedResolver(factory.clone())))
            .with_agent_id(Some("parent".into()));
            let mut config = crate::agent::agent_config_builder::AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                Arc::new(provider),
                fixture.storage.event_journal(),
            )
            .build();
            config.execution_policy.rate_limit.jitter_ratio = 0.0;
            config.execution_policy.rate_limit.default_wait_secs = 0;
            fixture.config = Arc::new(config);
            fixture.handle = Arc::new(crate::agent::LocalAgentHandle::from_config(
                fixture.config.clone(),
            ));
            (fixture, factory)
        }

        /// Run one streamed reply through `transition_call_llm` and
        /// `transition_after_llm`, returning the final state and every event
        /// that was emitted meanwhile.
        async fn run_streamed_reply(
            chunks: Vec<StreamChunk>,
        ) -> (
            ExecutionState,
            Vec<crate::events::AgentEventKind>,
            Arc<TimedFactory>,
        ) {
            let attempts = vec![Ok(chunks.into_iter().map(|chunk| (0, Ok(chunk))).collect())];
            let (fixture, factory) = timed_fixture(true, attempts, "openai", None).await;
            let mut events = fixture.config.subscribe_events();
            let mut exec = fixture.execution_context().await;
            let context = crate::test_utils::test_context(&exec.session_id, 0);

            // Tools must be present: a tool-less request takes the non-streaming path.
            let mut prepared = (*request()).clone();
            prepared.tools = vec![serde_json::from_value(serde_json::json!({
                "type": "function",
                "function": {"name": "write_file", "description": "", "parameters": {"type": "object"}}
            }))
            .unwrap()]
            .into();
            let state = transition_call_llm(&fixture.config, &context, &Arc::new(prepared), &exec)
                .await
                .expect("call llm");
            let ExecutionState::AfterLlm { response, context } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .expect("after llm");

            let mut seen = Vec::new();
            while let Ok(Ok(envelope)) =
                tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            {
                seen.push(envelope.kind().clone());
            }
            (state, seen, factory)
        }

        #[tokio::test]
        async fn length_cutoff_inside_tool_call_skips_the_call_and_stops_without_retry() {
            // One call completes, a second is cut off mid-arguments. The
            // incomplete reply must not execute the completed call either.
            let (state, events, factory) = run_streamed_reply(vec![
                StreamChunk::ToolUseStart {
                    index: 0,
                    id: "call_0".into(),
                    name: "write_file".into(),
                },
                StreamChunk::ToolUseComplete {
                    index: 0,
                    tool_call: ToolCall {
                        id: "call_0".into(),
                        call_type: "function".into(),
                        function: querymt::FunctionCall {
                            name: "write_file".into(),
                            arguments: "{\"path\":\"a.rs\"}".into(),
                        },
                    },
                    extensions: Default::default(),
                },
                StreamChunk::ToolUseStart {
                    index: 1,
                    id: "call_1".into(),
                    name: "write_file".into(),
                },
                StreamChunk::ToolUseInputDelta {
                    index: 1,
                    partial_json: "{\"path\":\"b.rs\",\"content\":\"fn main() {".into(),
                },
                StreamChunk::Done {
                    finish_reason: FinishReason::Length,
                },
            ])
            .await;

            assert!(
                !events
                    .iter()
                    .any(|kind| matches!(kind, crate::events::AgentEventKind::LlmRetryWait { .. })),
                "a length cut-off must not be retried as a transient stream failure"
            );
            assert!(
                factory.provider.attempts.lock().unwrap().is_empty(),
                "exactly one physical attempt is expected"
            );

            let ExecutionState::Stopped {
                stop_type, context, ..
            } = state
            else {
                panic!("expected Stopped, got {}", state.name());
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);

            let context = context.expect("stopped state keeps its context");
            let result_text = context
                .messages
                .iter()
                .filter(|message| message.has_tool_result())
                .map(|message| format!("{message:?}"))
                .collect::<String>();
            assert!(
                result_text.contains("split the write into smaller calls"),
                "skipped result must tell the model to split the write: {result_text}"
            );
        }

        fn flushed_call(index: usize, id: &str, arguments: &str, invalid: bool) -> StreamChunk {
            let mut extensions = querymt::chat::Extensions::new();
            if invalid {
                extensions.insert(
                    querymt::chat::INVALID_INPUT_EXTENSION.to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            StreamChunk::ToolUseComplete {
                index,
                tool_call: ToolCall {
                    id: id.into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "write_file".into(),
                        arguments: arguments.into(),
                    },
                },
                extensions,
            }
        }

        /// The stopped context's tool results, keyed by call id.
        fn stopped_results(
            state: ExecutionState,
        ) -> (Vec<querymt::chat::ChatMessage>, HashMap<String, String>) {
            let ExecutionState::Stopped {
                stop_type, context, ..
            } = state
            else {
                panic!("expected Stopped, got {}", state.name());
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
            let messages = context.expect("context kept").messages.to_vec();
            let results = messages
                .iter()
                .flat_map(|message| message.input().unwrap_or_default().to_vec())
                .filter_map(|part| part.as_tool_result().cloned())
                .map(|result| {
                    assert!(result.is_error, "cut-off results are errors");
                    let text = result
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            querymt::chat::ToolResultPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (result.call_id, text)
                })
                .collect();
            (messages, results)
        }

        /// Session 01a16d18: Anthropic flushes the interrupted block as a
        /// completed call marked invalid, so the reply is `Completed` +
        /// `Length`. The call must still be answered, with `INVALID_JSON`.
        #[tokio::test]
        async fn completed_length_reply_with_flushed_invalid_call_answers_it() {
            let (state, _, _) = run_streamed_reply(vec![
                StreamChunk::Text("Writing reconcile.rs now.".into()),
                StreamChunk::ToolUseStart {
                    index: 1,
                    id: "toolu_cut".into(),
                    name: "write_file".into(),
                },
                flushed_call(1, "toolu_cut", r#"{"path": "reconcile.rs""#, true),
                StreamChunk::Done {
                    finish_reason: FinishReason::Length,
                },
            ])
            .await;

            let (messages, results) = stopped_results(state);
            assert_eq!(results.len(), 1, "exactly one result: {results:?}");
            let text = &results["toolu_cut"];
            assert!(text.contains("INVALID_JSON"), "{text}");
            assert!(text.contains("split the write"), "{text}");
            // The result directly follows the assistant call.
            let call_at = messages
                .iter()
                .position(|m| m.has_tool_use())
                .expect("assistant call kept");
            assert!(messages[call_at + 1].has_tool_result());
        }

        #[tokio::test]
        async fn valid_call_before_a_length_cutoff_is_skipped_not_run() {
            let (state, _, factory) = run_streamed_reply(vec![
                StreamChunk::ToolUseStart {
                    index: 0,
                    id: "call_ok".into(),
                    name: "write_file".into(),
                },
                flushed_call(0, "call_ok", r#"{"path":"a.rs","content":"x"}"#, false),
                StreamChunk::ToolUseStart {
                    index: 1,
                    id: "call_cut".into(),
                    name: "write_file".into(),
                },
                flushed_call(1, "call_cut", r#"{"path":"b.rs""#, true),
                StreamChunk::Done {
                    finish_reason: FinishReason::Length,
                },
            ])
            .await;

            assert!(factory.provider.attempts.lock().unwrap().is_empty());
            let (_, results) = stopped_results(state);
            assert_eq!(results.len(), 2, "{results:?}");
            assert!(!results["call_ok"].contains("INVALID_JSON"));
            assert!(results["call_ok"].contains("split the write"));
            assert!(results["call_cut"].contains("INVALID_JSON"));
            assert!(results["call_cut"].contains("split the write"));
        }

        // ── Anthropic structured streams ────────────────────────────────────
        // Fixtures are the exact output of the Anthropic stream parser for a
        // checked-in SSE transcript; `qmt-anthropic` asserts they stay current.

        fn anthropic_fixture(name: &str) -> Vec<StreamChunk> {
            let path = format!(
                "{}/../providers/anthropic/tests/fixtures/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture"))
                .expect("fixture chunks")
        }

        fn has_retry_wait(events: &[crate::events::AgentEventKind]) -> bool {
            events
                .iter()
                .any(|kind| matches!(kind, crate::events::AgentEventKind::LlmRetryWait { .. }))
        }

        /// Session 01a18903: an opaque server-tool block alongside local calls
        /// used to fail every turn with unfinished tool call indexes and burn
        /// retries. Both local calls dispatch once; the opaque block never does.
        #[tokio::test]
        async fn anthropic_structured_stream_dispatches_local_calls_once_and_stores_history() {
            let (state, events, factory) =
                run_streamed_reply(anthropic_fixture("structured_tool_turn")).await;

            assert!(!has_retry_wait(&events), "no llm_retry_wait: {events:?}");
            assert!(factory.provider.attempts.lock().unwrap().is_empty());
            let ExecutionState::ProcessingToolCalls {
                remaining_calls,
                context,
                ..
            } = state
            else {
                panic!("expected ProcessingToolCalls, got {}", state.name());
            };
            let ids: Vec<&str> = remaining_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            assert_eq!(
                ids,
                ["toolu_a", "toolu_b"],
                "each local call dispatched once"
            );

            // Stored history keeps every block in order: signed reasoning, text,
            // the opaque server block, then both calls.
            let assistant = context
                .messages
                .iter()
                .find(|message| message.has_tool_use())
                .expect("assistant message in history");
            let items = &assistant.output().expect("assistant output kept").items;
            assert_eq!(items.len(), 5, "{items:?}");
            let ChatOutputItem::Reasoning(reasoning) = &items[0] else {
                panic!("expected reasoning first: {items:?}");
            };
            assert_eq!(reasoning.signature.as_deref(), Some("SIG"));
            assert_eq!(reasoning.visible_text(), "let me look");
            assert!(matches!(&items[1], ChatOutputItem::Message(_)));
            assert!(
                matches!(&items[2], ChatOutputItem::Opaque(o) if o.original_type == "server_tool_use")
            );
            let calls: Vec<(&str, &str)> = items[3..]
                .iter()
                .filter_map(|item| match item {
                    ChatOutputItem::FunctionCall(c) => {
                        Some((c.call_id.as_str(), c.arguments.as_str()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                calls,
                [
                    ("toolu_a", r#"{"path":"a.rs"}"#),
                    ("toolu_b", r#"{"path":"b.rs"}"#)
                ]
            );
        }

        #[tokio::test]
        async fn anthropic_structured_stream_projects_text_and_thinking_to_the_ui() {
            let (_, events, _) =
                run_streamed_reply(anthropic_fixture("structured_tool_turn")).await;

            let text: String = events
                .iter()
                .filter_map(|kind| match kind {
                    crate::events::AgentEventKind::AssistantContentDelta { content, .. } => {
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Checking both files.");

            let thinking: Vec<(&str, Option<&str>)> = events
                .iter()
                .filter_map(|kind| match kind {
                    crate::events::AgentEventKind::AssistantThinkingDelta {
                        content,
                        part_id,
                        ..
                    } => Some((content.as_str(), part_id.as_deref())),
                    _ => None,
                })
                .collect();
            assert!(!thinking.is_empty(), "thinking deltas must reach the UI");
            let joined: String = thinking.iter().map(|(content, _)| *content).collect();
            assert_eq!(joined, "let me look");
            assert!(
                thinking.iter().all(|(_, part)| *part == thinking[0].1),
                "one part id per reasoning item: {thinking:?}"
            );
        }

        #[tokio::test]
        async fn anthropic_max_tokens_with_open_tool_block_reaches_output_limit_recovery() {
            let (state, events, factory) =
                run_streamed_reply(anthropic_fixture("structured_max_tokens")).await;

            assert!(!has_retry_wait(&events), "a length cut-off must not retry");
            assert!(factory.provider.attempts.lock().unwrap().is_empty());
            // `stopped_results` also asserts the stop type is the model token limit.
            let (_, results) = stopped_results(state);
            assert_eq!(results.len(), 1, "exactly one result: {results:?}");
            let text = &results["toolu_cut"];
            assert!(text.contains("INVALID_JSON"), "{text}");
            assert!(text.contains("split the write"), "{text}");
        }

        #[test]
        fn length_cutoff_is_classified_by_finish_reason_only() {
            let reply = |finish| LlmResponse::new(String::new(), Vec::new(), None, Some(finish));
            assert!(reply(FinishReason::Length).is_length_cutoff());
            assert!(!reply(FinishReason::ToolCalls).is_length_cutoff());
        }

        fn write_file_tool() -> querymt::chat::Tool {
            serde_json::from_value(serde_json::json!({
                "type": "function",
                "function": {"name": "write_file", "description": "", "parameters": {"type": "object"}}
            }))
            .unwrap()
        }

        /// A model profile for a streaming provider honouring `support`.
        fn profile_with(
            support: querymt::chat::GenerationOptionSupport,
            caps: Option<crate::model_heuristics::OutputCaps>,
        ) -> Arc<crate::middleware::ModelProfile> {
            Arc::new(crate::middleware::ModelProfile {
                caps,
                capabilities: querymt::chat::ChatCapabilities::NONE
                    .with_streaming(true)
                    .with_options(support),
                ..Default::default()
            })
        }

        fn profiled_context(
            session_id: &str,
            profile: Arc<crate::middleware::ModelProfile>,
        ) -> Arc<ConversationContext> {
            Arc::new(
                (*crate::test_utils::test_context(session_id, 0))
                    .clone()
                    .with_profile(profile),
            )
        }

        fn stop_chunks() -> Vec<(u64, Result<StreamChunk, LLMError>)> {
            vec![(
                0,
                Ok(StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                }),
            )]
        }

        #[tokio::test]
        async fn before_llm_call_copies_context_overrides_into_the_request() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let exec = fixture.execution_context().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            let context = Arc::new(
                (*crate::test_utils::test_context(&exec.session_id, 0))
                    .clone()
                    .with_request_overrides(overrides),
            );

            let state = transition_before_llm_call(&fixture.config, &context, &exec)
                .await
                .expect("before llm call");
            let ExecutionState::CallLlm { request, context } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            assert_eq!(request.options, overrides);
            assert_eq!(
                context.profile.capabilities,
                exec.session_handle.provider().await.unwrap().capabilities(),
                "successful initialization must replace a stale or empty profile"
            );
        }

        #[tokio::test]
        async fn recovery_cap_is_clamped_after_preparing_the_request() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let exec = fixture.execution_context().await;
            let mut context = (*crate::test_utils::test_context(&exec.session_id, 0)).clone();
            context.messages = Arc::from([ChatMessage::user().text("continue the report").build()]);
            context.request_overrides = querymt::chat::GenerationOptions::NONE
                .with_max_tokens(u32::MAX)
                .with_reasoning_effort(querymt::chat::ReasoningEffort::Low);
            let window = crate::model_info::get_model_info(&context.provider, &context.model)
                .and_then(|info| info.limits.context)
                .unwrap_or(u64::from(u32::MAX))
                .min(u64::from(u32::MAX)) as u32;
            let state = transition_before_llm_call(&fixture.config, &Arc::new(context), &exec)
                .await
                .unwrap();
            let ExecutionState::CallLlm { request, .. } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            assert!(request.estimated_tokens > 0);
            assert_eq!(
                request.options.max_tokens,
                Some(window - request.estimated_tokens as u32)
            );
            assert_eq!(
                request.options.reasoning_effort,
                Some(querymt::chat::ReasoningEffort::Low)
            );
        }

        #[tokio::test]
        async fn override_applies_to_exactly_one_request() {
            let (fixture, factory) = timed_fixture(
                true,
                vec![Ok(stop_chunks()), Ok(stop_chunks())],
                "openai",
                None,
            )
            .await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            let exec = fixture.execution_context().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            let context = Arc::new(
                (*profiled_context(
                    &exec.session_id,
                    profile_with(querymt::chat::GenerationOptionSupport::ALL, None),
                ))
                .clone()
                .with_request_overrides(overrides),
            );
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            prepared.options = overrides;

            let state = transition_call_llm(
                &fixture.config,
                &context,
                &Arc::new(prepared.clone()),
                &exec,
            )
            .await
            .expect("first request");
            let ExecutionState::AfterLlm {
                response,
                context: next,
            } = state
            else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            assert_eq!(response.applied, overrides);
            assert_eq!(next.last_applied, overrides);
            assert!(
                next.request_overrides.is_empty(),
                "the consumed override must not carry into the next state"
            );

            // The follow-up request is prepared from the cleared context.
            prepared.options = next.request_overrides;
            let state = transition_call_llm(&fixture.config, &next, &Arc::new(prepared), &exec)
                .await
                .expect("second request");
            let ExecutionState::AfterLlm { response, .. } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            assert_eq!(response.applied, querymt::chat::GenerationOptions::NONE);

            assert_eq!(
                *factory.provider.options_seen.lock().unwrap(),
                vec![overrides, querymt::chat::GenerationOptions::NONE]
            );
        }

        #[tokio::test]
        async fn override_is_skipped_for_providers_that_do_not_support_it() {
            let (fixture, factory) =
                timed_fixture(true, vec![Ok(stop_chunks())], "openai", None).await;
            // `support` defaults to none.
            let exec = fixture.execution_context().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            let context = crate::test_utils::test_context(&exec.session_id, 0);
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            prepared.options = overrides;

            let state = transition_call_llm(&fixture.config, &context, &Arc::new(prepared), &exec)
                .await
                .expect("request");
            let ExecutionState::AfterLlm { response, .. } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };

            assert_eq!(
                *factory.provider.options_seen.lock().unwrap(),
                vec![querymt::chat::GenerationOptions::NONE],
                "unsupported overrides must not reach the provider"
            );
            assert_eq!(
                response.applied,
                querymt::chat::GenerationOptions::NONE,
                "the response reports what was really sent"
            );
        }

        #[tokio::test]
        async fn session_profile_carries_caps_capabilities_and_effort() {
            let (fixture, factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            factory.binding.lock().unwrap()["reasoning_effort"] = serde_json::json!("high");
            let exec = fixture.execution_context().await;

            let profile = exec.session_handle.profile().await;

            // The binding's explicit max_tokens is its own ceiling.
            assert_eq!(
                profile.caps,
                Some(crate::model_heuristics::OutputCaps {
                    initial: 123,
                    ceiling: 123,
                })
            );
            assert!(profile.capabilities.streaming);
            assert_eq!(
                profile.capabilities.options,
                querymt::chat::GenerationOptionSupport::ALL
            );
            assert_eq!(
                profile.reasoning_effort,
                Some(querymt::chat::ReasoningEffort::High)
            );
        }

        #[tokio::test]
        async fn model_switch_replaces_the_profile() {
            let (fixture, factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            let before = fixture.execution_context().await;
            let old = before.session_handle.profile().await;
            assert!(old.capabilities.options.max_tokens);

            // A switch rebinds the session to another model: the next turn builds a
            // new handle, and with it a new profile (new caps, new capabilities).
            *factory.provider.support.lock().unwrap() =
                querymt::chat::GenerationOptionSupport::NONE;
            factory.binding.lock().unwrap()["max_tokens"] = serde_json::json!(999);
            fixture.config.provider.clear_provider_cache().await;
            let after = fixture.execution_context().await;
            let new = after.session_handle.profile().await;

            assert!(!Arc::ptr_eq(&old, &new));
            assert_eq!(
                new.capabilities.options,
                querymt::chat::GenerationOptionSupport::NONE
            );
            assert_eq!(new.caps.map(|caps| caps.initial), Some(999));
            // The old turn keeps its own, unchanged profile.
            assert_eq!(old.caps.map(|caps| caps.initial), Some(123));
        }

        #[tokio::test]
        async fn chat_span_records_the_escalated_max_tokens() {
            let (fixture, factory) = timed_fixture(
                true,
                vec![Ok(stop_chunks()), Ok(stop_chunks())],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let support = querymt::chat::GenerationOptionSupport::ALL;
            *factory.provider.support.lock().unwrap() = support;
            let exec = fixture.execution_context().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            let profile = profile_with(support, None);
            let plain = profiled_context(&exec.session_id, profile.clone());
            let escalated = Arc::new((*plain).clone().with_request_overrides(overrides));
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            let plain_request = Arc::new(prepared.clone());
            prepared.options = overrides;
            let escalated_request = Arc::new(prepared);

            let (_, spans) = capture(async {
                transition_call_llm(&fixture.config, &escalated, &escalated_request, &exec)
                    .await
                    .expect("escalated request");
                transition_call_llm(&fixture.config, &plain, &plain_request, &exec)
                    .await
                    .expect("plain request");
            })
            .await;

            let max_tokens: Vec<_> = spans
                .iter()
                .filter(|span| attr(span, "gen_ai.operation.name") == Some(&Value::from("chat")))
                .map(|span| attr(span, "gen_ai.request.max_tokens").cloned())
                .collect();
            assert_eq!(max_tokens.len(), 2);
            assert!(
                max_tokens.contains(&Some(Value::I64(64_000))),
                "{max_tokens:?}"
            );
            // The configured value (binding override) returns once the override is spent.
            assert!(
                max_tokens.contains(&Some(Value::I64(123))),
                "{max_tokens:?}"
            );
        }

        fn text_reply(text: &str, finish_reason: FinishReason) -> TimedChunks {
            Ok(vec![
                (0, Ok(StreamChunk::Text(text.into()))),
                (0, Ok(StreamChunk::Done { finish_reason })),
            ])
        }

        #[tokio::test]
        async fn escalated_request_uses_the_ceiling_and_history_keeps_only_the_second_reply() {
            use crate::middleware::MiddlewareDriver;

            let (fixture, factory) = timed_fixture(
                true,
                vec![
                    text_reply("first, cut off", FinishReason::Length),
                    text_reply("second, complete", FinishReason::Stop),
                ],
                "openai",
                None,
            )
            .await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            let mut exec = fixture.execution_context().await;
            let middleware = crate::middleware::OutputLimitRecoveryMiddleware::default();
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            let context = profiled_context(
                &exec.session_id,
                profile_with(
                    querymt::chat::GenerationOptionSupport::ALL,
                    Some(crate::model_heuristics::OutputCaps {
                        initial: 32_000,
                        ceiling: 128_000,
                    }),
                ),
            );

            // First request: cut off at the initial cap.
            let state = transition_call_llm(
                &fixture.config,
                &context,
                &Arc::new(prepared.clone()),
                &exec,
            )
            .await
            .expect("first request");
            assert_eq!(state.name(), "AfterLlm");
            let state = middleware
                .on_after_llm(state, Some(&exec.runtime))
                .await
                .expect("middleware");
            let ExecutionState::BeforeLlmCall { context } = state else {
                panic!("expected escalation to BeforeLlmCall, got {}", state.name());
            };
            assert_eq!(context.request_overrides.max_tokens, Some(128_000));

            // Retry: same request at the ceiling.
            let state = transition_before_llm_call(&fixture.config, &context, &exec)
                .await
                .expect("before llm call");
            let ExecutionState::CallLlm { context, request } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            // The fixture config has no tools; restore the first request's so the
            // retry takes the same streaming path.
            let mut retry = (*request).clone();
            retry.tools = prepared.tools.clone();
            let state = transition_call_llm(&fixture.config, &context, &Arc::new(retry), &exec)
                .await
                .expect("second request");
            let state = middleware
                .on_after_llm(state, Some(&exec.runtime))
                .await
                .expect("middleware");
            let ExecutionState::AfterLlm { response, context } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .expect("after llm");

            assert_eq!(
                *factory.provider.options_seen.lock().unwrap(),
                vec![
                    querymt::chat::GenerationOptions::NONE,
                    querymt::chat::GenerationOptions::NONE.with_max_tokens(128_000),
                ]
            );
            let history = state
                .context()
                .expect("final state keeps its context")
                .messages
                .iter()
                .map(|message| format!("{message:?}"))
                .collect::<String>();
            assert!(history.contains("second, complete"), "{history}");
            assert!(
                !history.contains("first, cut off"),
                "the discarded reply must not reach history: {history}"
            );
        }

        fn thinking_reply(finish_reason: FinishReason) -> TimedChunks {
            Ok(vec![
                (0, Ok(StreamChunk::Thinking("pondering the file".into()))),
                (0, Ok(StreamChunk::Done { finish_reason })),
            ])
        }

        /// Replays the sequence that motivated output-limit recovery: a
        /// thinking-only cut-off, an escalation, another cut-off at the
        /// ceiling, a continuation, and finally a normal reply.
        #[tokio::test]
        async fn thinking_only_cutoff_escalates_then_continues_then_succeeds() {
            use crate::middleware::MiddlewareDriver;
            use querymt::chat::{GenerationOptions, ReasoningEffort};

            let (fixture, factory) = timed_fixture(
                true,
                vec![
                    thinking_reply(FinishReason::Length),
                    thinking_reply(FinishReason::Length),
                    text_reply("done", FinishReason::Stop),
                ],
                "openai",
                None,
            )
            .await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            let mut exec = fixture.execution_context().await;
            let middleware = crate::middleware::OutputLimitRecoveryMiddleware::default();
            let tools: Arc<[querymt::chat::Tool]> = vec![write_file_tool()].into();
            let instruction_marker = "cut off by the output-token limit";

            // Request 1: initial cap, thinking only, cut off -> escalate.
            let context = profiled_context(
                &exec.session_id,
                profile_with(
                    querymt::chat::GenerationOptionSupport::ALL,
                    Some(crate::model_heuristics::OutputCaps {
                        initial: 32_000,
                        ceiling: 128_000,
                    }),
                ),
            );
            let mut prepared = (*request()).clone();
            prepared.tools = tools.clone();
            let state = transition_call_llm(&fixture.config, &context, &Arc::new(prepared), &exec)
                .await
                .expect("request 1");
            let state = middleware
                .on_after_llm(state, Some(&exec.runtime))
                .await
                .expect("middleware 1");
            let ExecutionState::BeforeLlmCall { context } = state else {
                panic!("expected escalation, got {}", state.name());
            };
            assert_eq!(context.request_overrides.max_tokens, Some(128_000));

            // Request 2: ceiling, thinking only again -> not escalated twice, stops.
            let state = transition_before_llm_call(&fixture.config, &context, &exec)
                .await
                .expect("before 2");
            let ExecutionState::CallLlm { context, request } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            let mut retry = (*request).clone();
            retry.tools = tools.clone();
            let state = transition_call_llm(&fixture.config, &context, &Arc::new(retry), &exec)
                .await
                .expect("request 2");
            let state = middleware
                .on_after_llm(state, Some(&exec.runtime))
                .await
                .expect("middleware 2");
            let ExecutionState::AfterLlm { response, context } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .expect("after 2");
            assert!(
                matches!(
                    state,
                    ExecutionState::Stopped {
                        stop_type: StopType::ModelTokenLimit,
                        ..
                    }
                ),
                "got {}",
                state.name()
            );

            // Stop -> continuation with instruction, ceiling and lowered effort.
            let state = middleware
                .on_stopped(state, Some(&exec.runtime))
                .await
                .expect("on_stopped");
            let ExecutionState::BeforeLlmCall { context } = state else {
                panic!("expected continuation, got {}", state.name());
            };
            assert_eq!(
                context.request_overrides,
                GenerationOptions::NONE
                    .with_max_tokens(128_000)
                    .with_reasoning_effort(ReasoningEffort::Medium)
            );

            // Request 3: carries the instruction and succeeds.
            let state = transition_before_llm_call(&fixture.config, &context, &exec)
                .await
                .expect("before 3");
            let ExecutionState::CallLlm { context, request } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| format!("{message:?}").contains(instruction_marker)),
                "continuation request must include the instruction"
            );
            let mut last = (*request).clone();
            last.tools = tools;
            let state = transition_call_llm(&fixture.config, &context, &Arc::new(last), &exec)
                .await
                .expect("request 3");
            let state = middleware
                .on_after_llm(state, Some(&exec.runtime))
                .await
                .expect("middleware 3");
            let ExecutionState::AfterLlm { response, context } = state else {
                panic!("expected AfterLlm, got {}", state.name());
            };
            assert!(
                context
                    .fragments
                    .iter()
                    .all(|f| f.key != "output_limit_recovery"),
                "instruction must be gone after the normal reply"
            );
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .expect("after 3");

            assert_eq!(state.name(), "Complete");
            assert_eq!(
                *factory.provider.options_seen.lock().unwrap(),
                vec![
                    GenerationOptions::NONE,
                    GenerationOptions::NONE.with_max_tokens(128_000),
                    GenerationOptions::NONE
                        .with_max_tokens(128_000)
                        .with_reasoning_effort(ReasoningEffort::Medium),
                ]
            );
            let history = state
                .context()
                .expect("final context")
                .messages
                .iter()
                .map(|message| format!("{message:?}"))
                .collect::<String>();
            assert!(history.contains("done"), "{history}");
            assert!(
                !history.contains("pondering the file"),
                "thinking-only replies must not be stored: {history}"
            );
        }

        /// One model step as the execution loop runs it: step-start middleware,
        /// request preparation, the call, and after-llm middleware. Asserts the
        /// prepared request answers every tool call.
        async fn model_step(
            config: &Arc<AgentConfig>,
            exec: &ExecutionContext,
            middleware: &crate::middleware::OutputLimitRecoveryMiddleware,
            tools: &Arc<[querymt::chat::Tool]>,
            context: Arc<crate::middleware::ConversationContext>,
        ) -> ExecutionState {
            use crate::middleware::MiddlewareDriver;
            let state = middleware
                .on_step_start(ExecutionState::BeforeLlmCall { context }, None)
                .await
                .unwrap();
            let ExecutionState::BeforeLlmCall { context } = state else {
                panic!("expected BeforeLlmCall");
            };
            let state = transition_before_llm_call(config, &context, exec)
                .await
                .expect("before llm call");
            let ExecutionState::CallLlm { context, request } = state else {
                panic!("expected CallLlm, got {}", state.name());
            };
            let mut probe = request.messages.to_vec();
            assert!(
                crate::model::answer_unanswered_tool_calls(&mut probe).is_empty(),
                "request must answer every call"
            );
            let mut request = (*request).clone();
            request.tools = tools.clone();
            let state = transition_call_llm(config, &context, &Arc::new(request), exec)
                .await
                .expect("call llm");
            middleware.on_after_llm(state, None).await.unwrap()
        }

        /// Replays session 01a16d18: escalation succeeds, a later step of the
        /// same turn is cut off inside a flushed (invalid) `write_file`, and
        /// the continuation must keep the escalated cap and answer the call.
        #[tokio::test]
        async fn later_cutoff_in_an_escalated_turn_keeps_the_cap_and_answers_the_call() {
            use crate::middleware::MiddlewareDriver;
            use querymt::chat::GenerationOptions;

            let (fixture, factory) = timed_fixture(
                true,
                vec![
                    text_reply("first, cut off", FinishReason::Length),
                    text_reply("researching", FinishReason::Stop),
                    Ok(vec![
                        (0, Ok(StreamChunk::Text("Writing reconcile.rs now.".into()))),
                        (
                            0,
                            Ok(StreamChunk::ToolUseStart {
                                index: 1,
                                id: "toolu_cut".into(),
                                name: "write_file".into(),
                            }),
                        ),
                        (
                            0,
                            Ok(flushed_call(
                                1,
                                "toolu_cut",
                                r#"{"path": "reconcile.rs""#,
                                true,
                            )),
                        ),
                        (
                            0,
                            Ok(StreamChunk::Done {
                                finish_reason: FinishReason::Length,
                            }),
                        ),
                    ]),
                    text_reply("continued", FinishReason::Stop),
                ],
                "openai",
                None,
            )
            .await;
            *factory.provider.support.lock().unwrap() = querymt::chat::GenerationOptionSupport::ALL;
            let mut exec = fixture.execution_context().await;
            let middleware = crate::middleware::OutputLimitRecoveryMiddleware::default();
            let tools: Arc<[querymt::chat::Tool]> = vec![write_file_tool()].into();
            let context = profiled_context(
                &exec.session_id,
                profile_with(
                    querymt::chat::GenerationOptionSupport::ALL,
                    Some(crate::model_heuristics::OutputCaps {
                        initial: 32_000,
                        ceiling: 128_000,
                    }),
                ),
            );

            let config = fixture.config.clone();
            let mw = &middleware;

            // 1: cut off at 32k -> escalate.
            let ExecutionState::BeforeLlmCall { context } =
                model_step(&config, &exec, mw, &tools, context).await
            else {
                panic!("expected escalation");
            };
            // 2: escalated retry succeeds.
            let ExecutionState::AfterLlm { response, context } =
                model_step(&config, &exec, mw, &tools, context).await
            else {
                panic!("expected AfterLlm");
            };
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .unwrap();
            let context = state.context().expect("context").clone();
            // 3: a later step of the same turn starts without overrides, is sent at
            // 128k, and is cut off inside a flushed invalid call.
            assert!(context.request_overrides.is_empty());
            let ExecutionState::AfterLlm { response, context } =
                model_step(&config, &exec, mw, &tools, context).await
            else {
                panic!("a second cut-off must not escalate again");
            };
            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .unwrap();
            assert!(matches!(
                state,
                ExecutionState::Stopped {
                    stop_type: StopType::ModelTokenLimit,
                    ..
                }
            ));
            // 4: continuation at the escalated cap, with the call answered.
            let ExecutionState::BeforeLlmCall { context } =
                middleware.on_stopped(state, None).await.unwrap()
            else {
                panic!("expected continuation");
            };
            let ExecutionState::AfterLlm { .. } =
                model_step(&config, &exec, mw, &tools, context).await
            else {
                panic!("expected AfterLlm");
            };

            let max_tokens: Vec<_> = factory
                .provider
                .options_seen
                .lock()
                .unwrap()
                .iter()
                .map(|options: &GenerationOptions| options.max_tokens)
                .collect();
            assert_eq!(
                max_tokens,
                vec![None, Some(128_000), Some(128_000), Some(128_000)]
            );
        }

        fn reasoning_item(text: &str) -> ChatOutputItem {
            ChatOutputItem::Reasoning(querymt::chat::ChatReasoningItem {
                id: None,
                summary: Vec::new(),
                content: vec![querymt::chat::ChatReasoningPart::text(text.to_string())],
                encrypted_content: None,
                signature: None,
                status: None,
                extensions: Default::default(),
            })
        }

        /// Feed a `Length` reply with the given items through `transition_after_llm`.
        /// Returns the resulting state and whether `AssistantMessageStored` was emitted.
        async fn after_length_reply(
            items: Vec<ChatOutputItem>,
            content: &str,
            thinking: Option<&str>,
        ) -> (ExecutionState, bool) {
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))
            .await;
            let mut events = fixture.config.subscribe_events();
            let mut exec = fixture.execution_context().await;
            let context = crate::test_utils::test_context(&exec.session_id, 0);
            let usage = Usage {
                input_tokens: 10,
                output_tokens: 32_000,
                ..Usage::default()
            };
            let output = ChatOutput {
                items,
                usage: Some(usage.clone()),
                finish_reason: Some(FinishReason::Length),
                ..ChatOutput::default()
            };
            let response = Arc::new(
                LlmResponse::new(
                    content.to_string(),
                    Vec::new(),
                    Some(usage),
                    Some(FinishReason::Length),
                )
                .with_thinking(thinking.map(str::to_owned))
                .with_output(Some(output)),
            );

            let state = transition_after_llm(&fixture.config, &response, &context, &mut exec)
                .await
                .expect("after llm");

            let mut stored = false;
            while let Ok(Ok(envelope)) =
                tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            {
                stored |= matches!(
                    envelope.kind(),
                    crate::events::AgentEventKind::AssistantMessageStored { .. }
                );
            }
            (state, stored)
        }

        #[tokio::test]
        async fn thinking_only_length_reply_is_not_stored_but_is_counted() {
            let (state, stored) =
                after_length_reply(vec![reasoning_item("pondering")], "", Some("pondering")).await;

            assert!(
                !stored,
                "no AssistantMessageStored for a thinking-only cut-off"
            );
            let ExecutionState::Stopped {
                stop_type, context, ..
            } = state
            else {
                panic!("expected Stopped, got {}", state.name());
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
            let context = context.expect("context kept");
            assert!(
                context.messages.is_empty(),
                "the reply must not enter history"
            );
            assert_eq!(context.stats.steps, 1);
            assert_eq!(context.stats.total_output_tokens, 32_000);
            assert_eq!(context.stats.total_input_tokens, 10);
        }

        #[tokio::test]
        async fn mixed_length_reply_with_text_is_still_stored() {
            let message = ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![querymt::chat::ChatMessagePart::Text {
                    text: "partial".into(),
                    annotations: Vec::new(),
                    extensions: Default::default(),
                }],
                extensions: Default::default(),
            });
            let (state, stored) = after_length_reply(
                vec![reasoning_item("pondering"), message],
                "partial",
                Some("pondering"),
            )
            .await;

            assert!(stored, "a reply with visible text is kept");
            let ExecutionState::Stopped {
                stop_type, context, ..
            } = state
            else {
                panic!("expected Stopped, got {}", state.name());
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
            assert_eq!(context.expect("context kept").messages.len(), 1);
        }

        #[test]
        fn thinking_only_classification_requires_length_and_no_visible_output() {
            let reasoning_only = ChatOutput {
                items: vec![reasoning_item("x")],
                ..ChatOutput::default()
            };
            let cut = |finish| {
                LlmResponse::new(String::new(), Vec::new(), None, Some(finish))
                    .with_output(Some(reasoning_only.clone()))
            };
            assert!(cut(FinishReason::Length).is_thinking_only_length_cutoff());
            assert!(!cut(FinishReason::Stop).is_thinking_only_length_cutoff());

            let with_call = LlmResponse::new(
                String::new(),
                vec![middleware_call("call_1", "{}")],
                None,
                Some(FinishReason::Length),
            );
            assert!(!with_call.is_thinking_only_length_cutoff());
        }

        #[tokio::test]
        async fn non_length_incomplete_tool_call_keeps_the_generic_skip_text() {
            let text = skipped_call_text(Some(FinishReason::ContentFilter), "detail");
            assert!(text.contains("response was incomplete"));
            assert!(!text.contains("split the write"));
        }

        #[tokio::test]
        async fn genai_stream_first_chunk_counts_setup_and_retries_but_not_provider_resolution() {
            for mode in [
                "metadata",
                "empty",
                "usage",
                "terminal",
                "setup_retry",
                "metadata_retry",
                "controls",
                "error",
                "cancel_before",
                "cancel_after",
                "drop_before",
                "drop_after",
            ] {
                let retry = || LLMError::RateLimited {
                    message: "SECRET_ERROR".into(),
                    retry_after_secs: Some(1),
                };
                let mut chunks = vec![(
                    20,
                    Ok(match mode {
                        "empty" => StreamChunk::Text(String::new()),
                        "usage" => StreamChunk::Usage(Default::default()),
                        "terminal" => StreamChunk::Done {
                            finish_reason: FinishReason::Stop,
                        },
                        _ => metadata_event(),
                    }),
                )];
                let mut attempts = vec![];
                if mode == "setup_retry" {
                    attempts.push(Err(retry()));
                }
                if mode == "metadata_retry" {
                    attempts.push(Ok(vec![(20, Ok(metadata_event())), (0, Err(retry()))]));
                }
                if mode == "controls" {
                    chunks.insert(
                        0,
                        (
                            0,
                            Err(LLMError::RemoteStreamDisconnected {
                                message: "SECRET_ERROR".into(),
                            }),
                        ),
                    );
                    chunks.insert(
                        1,
                        (
                            0,
                            Err(LLMError::RemoteStreamReconnected {
                                message: "SECRET_ERROR".into(),
                            }),
                        ),
                    );
                }
                if mode == "error" {
                    chunks = vec![(0, Err(LLMError::AuthError("SECRET_ERROR".into())))];
                }
                if matches!(mode, "cancel_before" | "drop_before") {
                    chunks.clear();
                }
                if !mode.starts_with("cancel")
                    && !mode.starts_with("drop")
                    && !matches!(mode, "error" | "terminal")
                {
                    chunks.extend([
                        (100, Ok(StreamChunk::Text("SECRET_RESPONSE".into()))),
                        (
                            0,
                            Ok(if matches!(mode, "empty" | "usage") {
                                StreamChunk::Done {
                                    finish_reason: FinishReason::Stop,
                                }
                            } else {
                                terminal_event(
                                    ChatOutputStatus::Completed,
                                    Some(FinishReason::Stop),
                                )
                            }),
                        ),
                        (
                            0,
                            Ok(StreamChunk::Done {
                                finish_reason: FinishReason::Stop,
                            }),
                        ),
                    ]);
                }
                attempts.push(Ok(chunks));
                let (fixture, factory) = timed_fixture(
                    true,
                    attempts,
                    "openai",
                    Some("oci://ghcr.io/querymt/openai:latest"),
                )
                .await;
                let exec = fixture.execution_context().await;
                let context = crate::test_utils::test_context(&exec.session_id, 0);
                let mut request = (*request()).clone();
                request.tools = vec![serde_json::from_value(serde_json::json!({"type":"function", "function":{"name":"safe", "description":"", "parameters":{"type":"object"}}})).unwrap()].into();
                let (_, spans) = capture(async {
                    let parent = info_span!("timing-parent");
                    async {
                        let request = Arc::new(request);
                        let mut future = Box::pin(transition_call_llm(
                            &fixture.config,
                            &context,
                            &request,
                            &exec,
                        ));
                        if mode.starts_with("cancel") || mode.starts_with("drop") {
                            assert!(
                                tokio::time::timeout(Duration::from_millis(180), &mut future)
                                    .await
                                    .is_err()
                            );
                            if mode.starts_with("cancel") {
                                exec.cancellation_token.cancel();
                                assert!(matches!(future.await.unwrap(), ExecutionState::Cancelled));
                            }
                        } else {
                            let result = future.await;
                            assert_eq!(result.is_err(), mode == "error");
                        }
                    }
                    .instrument(parent)
                    .await
                })
                .await;
                let chat = spans
                    .iter()
                    .find(|span| span.name == "chat mock-model")
                    .unwrap();
                assert_eq!(
                    chat.parent_span_id,
                    spans
                        .iter()
                        .find(|span| span.name == "timing-parent")
                        .unwrap()
                        .span_context
                        .span_id()
                );
                let expected = factory.provider.clock.lock().unwrap().1;
                match (attr(chat, "gen_ai.response.time_to_first_chunk"), expected) {
                    (Some(Value::F64(actual)), Some(expected)) => {
                        assert!(
                            (actual - expected).abs() < 0.03,
                            "{mode}: actual={actual} expected={expected}"
                        );
                        if mode == "setup_retry" {
                            assert!(*actual >= 1.0);
                        }
                    }
                    (None, None) => {}
                    other => panic!("{mode}: {other:?}"),
                }
                assert_eq!(
                    chat.attributes
                        .iter()
                        .filter(|attr| attr.key.as_str() == "gen_ai.response.time_to_first_chunk")
                        .count(),
                    usize::from(expected.is_some())
                );
                assert_eq!(attr(chat, "gen_ai.agent.id"), Some(&Value::from("parent")));
                for diagnostic in spans
                    .iter()
                    .filter(|span| span.span_context.span_id() != chat.span_context.span_id())
                {
                    assert!(attr(diagnostic, "gen_ai.response.time_to_first_chunk").is_none());
                    for key in [
                        "gen_ai.request.max_tokens",
                        "gen_ai.request.temperature",
                        "gen_ai.request.top_p",
                        "gen_ai.request.reasoning.level",
                    ] {
                        assert!(attr(diagnostic, key).is_none(), "{mode}: {key}");
                    }
                }
                if mode.starts_with("cancel") || mode.starts_with("drop") {
                    assert_eq!(chat.status, Status::Unset);
                }
                assert_private(&spans);
            }
        }

        #[tokio::test]
        async fn genai_effective_settings_stay_bound_to_the_actual_cached_provider() {
            for (with_tools, streaming) in [(false, false), (true, true)] {
                for (provider, identity) in [
                    ("openai", Some("oci://ghcr.io/querymt/openai:latest")),
                    ("openai", Some("static:openai")),
                    ("openai", None),
                    ("xai", Some("oci://ghcr.io/querymt/xai:latest")),
                    ("google", Some("oci://ghcr.io/querymt/google:latest")),
                    ("xai", Some("oci://ghcr.io/querymt/xai-custom:latest")),
                    ("google", Some("/tmp/google.wasm")),
                ] {
                    let official = format!("oci://ghcr.io/querymt/{provider}:latest");
                    let item = ChatOutput::from_projections(
                        None,
                        Some("SECRET_RESPONSE".into()),
                        None,
                        None,
                        Some(FinishReason::Stop),
                    )
                    .items
                    .remove(0);
                    let chunks = || {
                        Ok(vec![
                            metadata_event(),
                            StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
                                output_index: 0,
                                item: item.clone(),
                            }),
                            terminal_event(ChatOutputStatus::Completed, Some(FinishReason::Stop)),
                            StreamChunk::Usage(querymt::Usage {
                                input_tokens: 10,
                                output_tokens: 5,
                                ..Default::default()
                            }),
                        ]
                        .into_iter()
                        .map(|chunk| (0, Ok(chunk)))
                        .collect())
                    };
                    let attempts = if streaming {
                        vec![chunks(), chunks()]
                    } else {
                        vec![]
                    };
                    let (fixture, factory) =
                        timed_fixture(streaming, attempts, provider, identity).await;
                    factory.binding.lock().unwrap()["reasoning_effort"] = serde_json::json!("high");
                    let exec = fixture.execution_context().await;
                    let context = crate::test_utils::test_context(&exec.session_id, 0);
                    let mut prepared = request();
                    if with_tools {
                        Arc::make_mut(&mut prepared).tools = vec![serde_json::from_value(serde_json::json!({
                            "type":"function", "function":{"name":"telemetry_tool", "description":"",
                            "parameters":{"type":"object"}}
                        })).unwrap()].into();
                    }
                    // The actual transition must initialize the turn handle, not a prewarm.
                    assert_eq!(factory.resolutions.load(Ordering::Relaxed), 0);
                    assert!(factory.configs.lock().unwrap().is_empty());
                    let (_, spans) = capture(async {
                        async {
                            for call in 0..2 {
                                let ExecutionState::AfterLlm { response, .. } =
                                    transition_call_llm(&fixture.config, &context, &prepared, &exec)
                                        .await
                                        .unwrap()
                                else {
                                    panic!("expected completed generation");
                                };
                                let output = response.output.as_ref().unwrap();
                                assert_eq!(output.text().as_deref(), Some("SECRET_RESPONSE"));
                                assert_eq!(output.finish_reason, Some(FinishReason::Stop));
                                if streaming {
                                    let usage = output.usage.as_ref().unwrap();
                                    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
                                }
                                assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);
                                let configs = factory.configs.lock().unwrap();
                                assert_eq!(configs.len(), 1);
                                assert_eq!(configs[0]["temperature"], 1.5); // Session override.
                                assert_eq!(configs[0]["max_tokens"], 123); // Binding override.
                                assert_eq!(configs[0]["reasoning_effort"], "high");
                                assert!(configs[0].get("top_p").is_none()); // Schema-pruned.
                                if call == 0 {
                                    *factory.binding.lock().unwrap() = serde_json::json!({
                                        "temperature":0.5, "max_tokens":999, "reasoning_effort":"low"
                                    });
                                }
                            }
                        }
                        .instrument(info_span!("settings-parent"))
                        .await
                    })
                    .await;
                    let chats: Vec<_> = spans
                        .iter()
                        .filter(|span| {
                            attr(span, "gen_ai.operation.name") == Some(&Value::from("chat"))
                        })
                        .collect();
                    assert_eq!(chats.len(), 2);
                    for chat in chats {
                        let verified = identity == Some(official.as_str());
                        for (key, value) in [
                            ("gen_ai.request.temperature", Value::F64(1.5)),
                            ("gen_ai.request.max_tokens", Value::I64(123)),
                        ] {
                            assert_eq!(
                                attr(chat, key),
                                (verified && provider == "openai").then_some(&value)
                            );
                        }
                        assert_eq!(
                            attr(chat, "gen_ai.request.reasoning.level"),
                            verified.then_some(&Value::from("high"))
                        );
                        assert!(attr(chat, "gen_ai.request.top_p").is_none());
                        assert_eq!(
                            attr(chat, "gen_ai.response.time_to_first_chunk").is_some(),
                            streaming
                        );
                        // TTFT and TTFB use the names Otelite reads, never the server-side spec name.
                        assert_eq!(attr(chat, "llm.time_to_first_token").is_some(), streaming);
                        assert_eq!(attr(chat, "llm.time_to_first_byte").is_some(), streaming);
                        assert!(attr(chat, "gen_ai.server.time_to_first_token").is_none());
                        if let (Some(Value::F64(chunk)), Some(Value::F64(token))) = (
                            attr(chat, "gen_ai.response.time_to_first_chunk"),
                            attr(chat, "llm.time_to_first_token"),
                        ) {
                            assert!(token >= chunk, "token={token} chunk={chunk}");
                        }
                        assert_eq!(
                            attr(chat, "gen_ai.request.stream"),
                            streaming.then_some(&Value::Bool(true))
                        );
                        assert_eq!(
                            attr(chat, "gen_ai.response.finish_reasons"),
                            Some(&Value::Array(Array::String(vec!["stop".into()])))
                        );
                        if streaming {
                            assert_eq!(
                                attr(chat, "gen_ai.usage.input_tokens"),
                                Some(&Value::I64(10))
                            );
                            assert_eq!(
                                attr(chat, "gen_ai.usage.output_tokens"),
                                Some(&Value::I64(5))
                            );
                        }
                    }
                    for diagnostic in spans.iter().filter(|span| {
                        attr(span, "gen_ai.operation.name") != Some(&Value::from("chat"))
                    }) {
                        for key in [
                            "gen_ai.usage.reasoning_tokens",
                            "gen_ai.usage.reasoning.output_tokens",
                            "gen_ai.request.max_tokens",
                            "gen_ai.request.temperature",
                            "gen_ai.request.top_p",
                            "gen_ai.request.reasoning.level",
                        ] {
                            assert!(attr(diagnostic, key).is_none());
                        }
                    }
                    assert_private(&spans);
                }
            }
        }

        #[tokio::test]
        async fn cloned_session_handles_share_one_provider_build() {
            let (fixture, factory) = timed_fixture(
                true,
                vec![],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let exec = fixture.execution_context().await;
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 0);

            // The state machine clones the handle on every step; each clone must
            // reuse the turn's provider instead of rebuilding it.
            let first = exec.session_handle.provider().await.unwrap();
            for _ in 0..3 {
                let clone = exec.session_handle.clone();
                let again = clone.provider().await.unwrap();
                assert!(Arc::ptr_eq(&first, &again));
            }
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);

            // A clone taken before the first build shares the same cell.
            let (fixture, factory) = timed_fixture(
                true,
                vec![],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let exec = fixture.execution_context().await;
            let early_clone = exec.session_handle.clone();
            let from_clone = early_clone.provider().await.unwrap();
            let from_original = exec.session_handle.provider().await.unwrap();
            assert!(Arc::ptr_eq(&from_clone, &from_original));
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn pre_request_phases_are_traced_and_provider_init_is_not_a_chat_retry() {
            let (fixture, _factory) = timed_fixture(
                true,
                vec![],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let exec = fixture.execution_context().await;
            let context = crate::test_utils::test_context(&exec.session_id, 0);
            let (_, spans) = capture(async {
                let _ = transition_before_llm_call(&fixture.config, &context, &exec).await;
            })
            .await;
            let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
            for expected in [
                "agent.transition.before_llm_call",
                "agent.provider.build",
                "agent.tools.collect",
                "agent.hooks.context",
                "agent.request.cache_breakpoints",
            ] {
                assert!(names.contains(&expected), "missing {expected}: {names:?}");
            }
            let init = spans
                .iter()
                .find(|s| s.name == "agent.llm.call_with_retry")
                .expect("provider init retry span");
            assert_eq!(attr(init, "operation"), Some(&Value::from("provider_init")));
            let tools = spans
                .iter()
                .find(|s| s.name == "agent.tools.collect")
                .unwrap();
            assert!(attr(tools, "tool_count").is_some());
            assert!(attr(tools, "tools_bytes").is_some());
        }

        #[tokio::test]
        async fn genai_nonstream_retry_exports_one_logical_chat_and_typed_totals() {
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = attempts.clone();
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(move |_| {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Err(LLMError::RateLimited {
                        message: "SECRET_ERROR".into(),
                        retry_after_secs: Some(0),
                    });
                }
                let mut output = ChatOutput::from_projections(
                    None,
                    Some("SECRET_RESPONSE".into()),
                    None,
                    None,
                    Some(FinishReason::Stop),
                );
                output.response_id = Some("response-1".into());
                output.usage = Some(querymt::Usage {
                    input_tokens: 100,
                    output_tokens: 200,
                    cache_read: 5,
                    cache_write: 7,
                    reasoning_tokens: 9,
                    cache_write_tiers: None,
                });
                output.provenance = Some(ChatOutputProvenance {
                    provider: "codex".into(),
                    model: "effective-model".into(),
                    endpoint: "SECRET_ENDPOINT".into(),
                    protocol: "responses".into(),
                });
                Ok(output)
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            let context = crate::test_utils::test_context(&exec.session_id, 0);
            let (result, spans) = capture(async {
                transition_call_llm(&fixture.config, &context, &request(), &exec)
                    .instrument(info_span!("test-parent"))
                    .await
            })
            .await;
            assert!(matches!(result.unwrap(), ExecutionState::AfterLlm { .. }));
            assert_eq!(attempts.load(Ordering::Relaxed), 2);
            let chats: Vec<_> = spans
                .iter()
                .filter(|s| attr(s, "gen_ai.operation.name") == Some(&Value::from("chat")))
                .collect();
            assert_eq!(chats.len(), 1);
            let chat = chats[0];
            assert_eq!(chat.name, "chat mock-model");
            assert_eq!(
                attr(chat, "gen_ai.request.model"),
                Some(&Value::from("mock-model"))
            );
            assert_eq!(chat.span_kind, SpanKind::Client);
            assert_eq!(chat.status, Status::Unset);
            assert_eq!(
                chat.parent_span_id,
                spans
                    .iter()
                    .find(|s| s.name == "test-parent")
                    .unwrap()
                    .span_context
                    .span_id()
            );
            assert_eq!(
                attr(chat, "gen_ai.provider.name"),
                Some(&Value::from("openai"))
            );
            assert_eq!(
                attr(chat, "gen_ai.conversation.id"),
                Some(&Value::from(exec.session_id.clone()))
            );
            assert_eq!(
                attr(chat, "session.id"),
                attr(chat, "gen_ai.conversation.id")
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.input_tokens"),
                Some(&Value::I64(112))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.output_tokens"),
                Some(&Value::I64(209))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning.output_tokens"),
                Some(&Value::I64(9))
            );
            assert_eq!(
                attr(chat, "gen_ai.usage.reasoning_tokens"),
                Some(&Value::I64(9))
            );
            assert_eq!(
                attr(chat, "gen_ai.response.finish_reasons"),
                Some(&Value::Array(Array::String(vec!["stop".into()])))
            );
            assert_eq!(
                attr(chat, "gen_ai.response.model"),
                Some(&Value::from("effective-model"))
            );
            assert!(attr(chat, "error.type").is_none());
            assert!(attr(chat, "gen_ai.agent.id").is_none());
            assert!(attr(chat, "gen_ai.response.time_to_first_chunk").is_none());
            assert_private(&spans);
        }

        #[tokio::test]
        async fn genai_error_and_early_cancel_close_without_error_messages() {
            for cancelled in [false, true] {
                let mut mock = MockLlmProvider::new();
                mock.expect_chat()
                    .returning(|_| Err(LLMError::AuthError("SECRET_ERROR".into())));
                let fixture =
                    TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
                let exec = fixture.execution_context().await;
                if cancelled {
                    exec.cancellation_token.cancel();
                }
                let context = crate::test_utils::test_context(&exec.session_id, 0);
                let (result, spans) = capture(transition_call_llm(
                    &fixture.config,
                    &context,
                    &request(),
                    &exec,
                ))
                .await;
                let chat = spans.iter().find(|s| s.name == "chat mock-model").unwrap();
                assert_eq!(
                    attr(chat, "session.id"),
                    Some(&Value::from(exec.session_id.clone()))
                );
                assert_eq!(
                    attr(chat, "session.id"),
                    attr(chat, "gen_ai.conversation.id")
                );
                if cancelled {
                    assert!(matches!(result.unwrap(), ExecutionState::Cancelled));
                    assert_eq!(chat.status, Status::Unset);
                    assert!(attr(chat, "error.type").is_none());
                } else {
                    assert!(result.is_err());
                    assert_eq!(chat.status, Status::error(""));
                    assert_eq!(
                        attr(chat, "error.type"),
                        Some(&Value::from("authentication"))
                    );
                }
                assert_private(&spans);
            }
        }

        #[tokio::test]
        async fn genai_polled_then_dropped_request_closes_unset() {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat()
                .returning(|_| panic!("provider must not be reached while its lock is held"));
            let shared = SharedLlmProvider::new(mock, vec![]);
            let lock = shared.inner.clone();
            let fixture = TestAgent::with_mock_provider(shared).await;
            let exec = fixture.execution_context().await;
            // Initialize the provider before blocking the mock's call.
            exec.session_handle.provider().await.unwrap();
            let _locked = lock.lock().await;
            let context = crate::test_utils::test_context(&exec.session_id, 0);
            let (_, spans) = capture(async {
                let request = request();
                let mut future = Box::pin(transition_call_llm(
                    &fixture.config,
                    &context,
                    &request,
                    &exec,
                ));
                assert!(futures_util::poll!(&mut future).is_pending());
                drop(future);
            })
            .await;
            let chat = spans.iter().find(|s| s.name == "chat mock-model").unwrap();
            assert_eq!(
                attr(chat, "session.id"),
                Some(&Value::from(exec.session_id.clone()))
            );
            assert_eq!(
                attr(chat, "session.id"),
                attr(chat, "gen_ai.conversation.id")
            );
            assert_eq!(chat.status, Status::Unset);
            assert!(attr(chat, "error.type").is_none());
            assert_eq!(
                attr(chat, "gen_ai.response.finish_reasons"),
                Some(&Value::Array(Array::String(vec!["error".into()])))
            );
            assert_private(&spans);
        }
    }

    use crate::middleware::ConversationContext;
    use querymt::chat::{
        ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOpaqueItem, ChatOutputItem,
        ChatReasoningItem, ChatReasoningPart,
    };
    use querymt::chat::{ChatMessage, ChatRole};

    fn make_message(role: ChatRole, content: &str) -> ChatMessage {
        match role {
            ChatRole::User => ChatMessage::user().text(content).build(),
            ChatRole::Assistant => ChatMessage::assistant().text(content).build(),
        }
    }

    /// Fixed tool overhead must fail instead of requesting repeated history compaction.
    #[test]
    fn oversized_tool_definitions_fail_without_compaction() {
        for tokens in [10_000, 10_001, usize::MAX] {
            let error = validate_tool_context_budget(tokens, 10_000).unwrap_err();
            assert!(error.to_string().contains("Reduce the available tools"));
        }
    }

    /// Tool overhead below the window still permits history-reducible admission checks.
    #[test]
    fn fitting_tool_definitions_pass_the_fixed_overhead_guard() {
        assert!(validate_tool_context_budget(0, 10_000).is_ok());
        assert!(validate_tool_context_budget(9_999, 10_000).is_ok());
    }

    // ── Item-aware accumulation (task 4.1) ───────────────────────────────────

    use querymt::Usage;
    use querymt::chat::ChatOutputStatus;

    fn fn_call_item(item_id: &str, call_id: &str, args: &str) -> ChatOutputItem {
        ChatOutputItem::FunctionCall(ChatFunctionCallItem {
            item_id: Some(item_id.into()),
            call_id: call_id.into(),
            name: "lookup".into(),
            arguments: args.into(),
            status: Some(ChatOutputStatus::Completed),
            extensions: Default::default(),
        })
    }

    fn metadata_event() -> StreamChunk {
        StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
            response_id: Some("resp_1".into()),
            status: Some(ChatOutputStatus::InProgress),
            usage: None,
            finish_reason: None,
            provenance: None,
        })
    }

    fn terminal_event(status: ChatOutputStatus, finish: Option<FinishReason>) -> StreamChunk {
        StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status,
            usage: None,
            finish_reason: finish,
            detail: None,
        })
    }

    /// Ordered items must survive agent accumulation without flattening into
    /// text/thinking/call buckets.
    #[test]
    fn ordered_items_survive_accumulation_without_flattening() {
        let output = ChatOutput {
            response_id: Some("resp_1".into()),
            status: Some(ChatOutputStatus::Completed),
            finish_reason: Some(FinishReason::ToolCalls),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning_1".into()),
                    summary: vec![ChatReasoningPart::text("thinking first")],
                    content: Vec::new(),
                    encrypted_content: Some("opaque-continuation".into()),
                    signature: Some("sig".into()),
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: Some("message_1".into()),
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text: "answer".into(),
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    }],
                    extensions: Default::default(),
                }),
                fn_call_item("item_1", "call_1", "{\"city\":\"oslo\"}"),
                fn_call_item("item_2", "call_2", "{\"city\":\"rome\"}"),
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning_2".into()),
                    summary: Vec::new(),
                    content: Vec::new(),
                    encrypted_content: Some("encrypted-only".into()),
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                }),
                ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type: "future_action".into(),
                    payload: serde_json::json!({"required": true}),
                }),
            ],
            ..ChatOutput::default()
        };

        // Flattened projections used by the agent execution paths.
        assert_eq!(output.text().as_deref(), Some("answer"));
        assert_eq!(output.thinking().as_deref(), Some("thinking first"));
        assert_eq!(output.signature().as_deref(), Some("sig"));
        let calls = dedupe_tool_calls_by_call_id(output.tool_calls().unwrap_or_default());
        assert_eq!(calls.len(), 2, "both calls survive in order");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.arguments, "{\"city\":\"oslo\"}");
        assert_eq!(calls[1].id, "call_2");
        // Encrypted-only reasoning and opaque items are never flattened away or
        // exposed as executable calls.
        assert!(matches!(output.items[4], ChatOutputItem::Reasoning(_)));
        assert!(matches!(output.items[5], ChatOutputItem::Opaque(_)));
    }

    /// A structured stream accompanied by legacy projection events yields one
    /// canonical call sequence: the legacy ToolUseComplete duplicate is ignored.
    #[test]
    fn mixed_structured_legacy_stream_projects_each_call_once() {
        let mut accumulator = ChatStreamAccumulator::new();
        let chunks = [
            metadata_event(),
            StreamChunk::Structured(StructuredStreamEvent::ItemStarted {
                output_index: 1,
                item: fn_call_item("item_1", "call_1", "{"),
            }),
            StreamChunk::Structured(StructuredStreamEvent::FunctionArgumentsDelta {
                output_index: 1,
                delta: "\"city\":\"oslo\"}".into(),
            }),
            StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
                output_index: 1,
                item: fn_call_item("item_1", "call_1", "{\"city\":\"oslo\"}"),
            }),
            // Legacy compatibility projection of the same call.
            StreamChunk::ToolUseComplete {
                index: 1,
                tool_call: ToolCall {
                    id: "call_1".into(),
                    call_type: "function".into(),
                    function: querymt::FunctionCall {
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    },
                },
                extensions: Default::default(),
            },
            terminal_event(ChatOutputStatus::Completed, Some(FinishReason::ToolCalls)),
        ];
        for chunk in &chunks {
            accumulator.push(chunk).unwrap();
        }
        assert!(accumulator.is_structured());

        // Post-terminal legacy framing (Done) is drained by the agent and never
        // pushed: the accumulator rejects events after the semantic terminal.
        assert_eq!(
            accumulator.push(&StreamChunk::Done {
                finish_reason: FinishReason::ToolCalls
            }),
            Err(ChatStreamAccumulatorError::EventAfterTerminal)
        );

        let output = accumulator
            .finish_success()
            .expect("structured stream validates");
        let calls = dedupe_tool_calls_by_call_id(output.tool_calls().unwrap_or_default());
        assert_eq!(calls.len(), 1, "legacy projection must not duplicate calls");
        assert_eq!(calls[0].function.arguments, "{\"city\":\"oslo\"}");
        assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
    }

    /// Structured text/reasoning deltas surface on the same UI buffers as
    /// legacy display chunks.
    #[test]
    fn structured_deltas_map_onto_ui_buffers() {
        let text = StructuredStreamEvent::MessagePartDelta {
            output_index: 0,
            content_index: 0,
            delta: ChatMessagePartDelta::Text {
                delta: "hello".into(),
            },
        };
        let refusal = StructuredStreamEvent::MessagePartDelta {
            output_index: 0,
            content_index: 1,
            delta: ChatMessagePartDelta::Refusal { delta: "no".into() },
        };
        let reasoning = StructuredStreamEvent::ReasoningPartDelta {
            output_index: 2,
            part: querymt::chat::ReasoningPartKind::Summary,
            part_index: 0,
            delta: "why".into(),
        };

        assert_eq!(
            structured_delta_for_ui(&text),
            Some(UiStreamDelta::Text("hello"))
        );
        assert_eq!(
            structured_delta_for_ui(&refusal),
            Some(UiStreamDelta::Text("no"))
        );
        assert_eq!(
            structured_delta_for_ui(&reasoning),
            Some(UiStreamDelta::Reasoning {
                output_index: 2,
                part_index: 0,
                kind: UiReasoningKind::Summary,
                delta: "why",
            })
        );
        let raw = StructuredStreamEvent::ReasoningPartDelta {
            output_index: 2,
            part: ReasoningPartKind::Content,
            part_index: 0,
            delta: "hidden".into(),
        };
        assert_eq!(
            structured_delta_for_ui(&raw),
            Some(UiStreamDelta::Reasoning {
                output_index: 2,
                part_index: 0,
                kind: UiReasoningKind::Content,
                delta: "hidden",
            })
        );
    }

    #[test]
    fn pinned_reasoning_part_id_does_not_change_after_item_id_arrives() {
        let mut pinned = HashMap::new();
        let mut ids = HashMap::new();
        let before = pinned_reasoning_part_id(&mut pinned, &ids, 0, 1, UiReasoningKind::Summary);
        ids.insert(0, "rs_1".to_string());
        let after = pinned_reasoning_part_id(&mut pinned, &ids, 0, 1, UiReasoningKind::Summary);
        assert_eq!(before, "output:0:summary:1");
        assert_eq!(after, before);
        assert_eq!(
            pinned_reasoning_part_id(&mut pinned, &ids, 0, 2, UiReasoningKind::Summary),
            "rs_1:summary:2"
        );
        assert_eq!(
            pinned_reasoning_part_id(&mut pinned, &ids, 0, 0, UiReasoningKind::Content),
            "rs_1:content:0"
        );
    }

    #[test]
    fn summary_part_change_flushes_the_previous_part() {
        let mut buffer = ThinkingPartBuffer::new();
        assert!(
            buffer
                .push(Some("rs_1:summary:0".into()), "first")
                .is_none()
        );
        let flushed = buffer
            .push(Some("rs_1:summary:1".into()), "second")
            .expect("part change flushes");
        assert_eq!(flushed.0.as_deref(), Some("rs_1:summary:0"));
        assert_eq!(flushed.1, "first");
        assert_eq!(
            buffer.take(),
            (Some("rs_1:summary:1".into()), "second".into())
        );
    }

    #[test]
    fn reasoning_parts_keep_summary_and_content_identity() {
        let output = ChatOutput {
            items: vec![ChatOutputItem::Reasoning(
                querymt::chat::ChatReasoningItem {
                    id: Some("rs_1".into()),
                    summary: vec![
                        querymt::chat::ChatReasoningPart::text("one"),
                        querymt::chat::ChatReasoningPart::text(""),
                        querymt::chat::ChatReasoningPart::text("two"),
                    ],
                    content: vec![querymt::chat::ChatReasoningPart::text("raw")],
                    encrypted_content: Some("secret".into()),
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                },
            )],
            ..ChatOutput::default()
        };
        let parts = reasoning_parts_from_output(&output);
        assert_eq!(
            parts,
            vec![
                ReasoningPartStored {
                    id: "rs_1:summary:0".into(),
                    text: "one".into(),
                },
                ReasoningPartStored {
                    id: "rs_1:summary:2".into(),
                    text: "two".into(),
                },
                ReasoningPartStored {
                    id: "rs_1:content:0".into(),
                    text: "raw".into(),
                },
            ]
        );
    }

    #[test]
    fn drained_usage_merges_fieldwise_without_double_counting() {
        let output = ChatOutput {
            usage: Some(Usage {
                input_tokens: 10,
                ..Usage::default()
            }),
            ..ChatOutput::default()
        };
        let merged = merge_drained_usage(
            output,
            Some(Usage {
                output_tokens: 4,
                ..Usage::default()
            }),
        );
        let usage = merged.usage.expect("usage present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);

        // None + drained yields the drained value.
        let merged = merge_drained_usage(ChatOutput::default(), Some(Usage::default()));
        assert!(merged.usage.is_some());
    }

    // ── Execution gating (task 4.3) ──────────────────────────────────────────

    fn middleware_call(id: &str, arguments: &str) -> MiddlewareToolCall {
        MiddlewareToolCall {
            id: id.into(),
            function: ToolFunction {
                name: "lookup".into(),
                arguments: arguments.into(),
            },
            invalid_input: false,
        }
    }

    #[test]
    fn invalid_argument_calls_are_gated_not_executed() {
        let calls = vec![
            middleware_call("call_ok", "{\"q\":1}"),
            middleware_call("call_bad", "{not json"),
            middleware_call("call_array", "[1,2,3]"),
        ];
        let (executable, gated) = gate_function_calls_for_execution(&calls);

        assert_eq!(
            executable
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_ok"]
        );
        assert_eq!(
            gated
                .iter()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_bad", "call_array"]
        );
        assert!(gated.iter().all(|result| result.is_error));
    }

    #[test]
    fn duplicate_call_identities_dispatch_once() {
        let calls = vec![
            middleware_call("call_1", "{}"),
            middleware_call("call_1", "{\"other\":1}"),
            middleware_call("call_2", "{}"),
        ];
        let (executable, gated) = gate_function_calls_for_execution(&calls);

        assert_eq!(
            executable
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["call_1", "call_2"]
        );
        assert!(gated.is_empty(), "duplicates are skipped without results");
    }

    /// Opaque output items never enter the executable function-call projection.
    #[test]
    fn opaque_items_never_enter_the_executor_projection() {
        let output = ChatOutput {
            items: vec![
                ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type: "code_interpreter_call".into(),
                    payload: serde_json::json!({"arguments": {"command": "rm -rf /"}}),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("item_1".into()),
                    call_id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{invalid".into(),
                    status: Some(ChatOutputStatus::Completed),
                    extensions: Default::default(),
                }),
            ],
            ..ChatOutput::default()
        };

        // Projection drops the opaque item entirely and keeps the invalid call
        // only as structured data with exact raw arguments.
        let projected = output.tool_calls().unwrap_or_default();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].function.arguments, "{invalid");

        let middleware_calls: Vec<MiddlewareToolCall> = dedupe_tool_calls_by_call_id(projected)
            .into_iter()
            .map(|tc| MiddlewareToolCall {
                id: tc.id,
                function: ToolFunction {
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                },
                invalid_input: false,
            })
            .collect();
        let (executable, gated) = gate_function_calls_for_execution(&middleware_calls);
        assert!(executable.is_empty(), "invalid call must not execute");
        assert_eq!(gated.len(), 1);
    }

    /// Assemble a stream the way the provider emits it, then project to middleware calls.
    fn assembled_calls(chunks: Vec<querymt::chat::StreamChunk>) -> Vec<MiddlewareToolCall> {
        let mut accumulator = querymt::chat::streaming::ChatStreamAccumulator::new();
        for chunk in &chunks {
            accumulator.push(chunk).unwrap();
        }
        middleware_calls_from_output(&accumulator.output())
    }

    fn complete_chunk(id: &str, arguments: &str, invalid: bool) -> querymt::chat::StreamChunk {
        let mut extensions = querymt::chat::Extensions::new();
        if invalid {
            extensions.insert(
                querymt::chat::INVALID_INPUT_EXTENSION.to_string(),
                serde_json::Value::Bool(true),
            );
        }
        querymt::chat::StreamChunk::ToolUseComplete {
            index: 0,
            tool_call: querymt::ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: querymt::FunctionCall {
                    name: "lookup".into(),
                    arguments: arguments.into(),
                },
            },
            extensions,
        }
    }

    #[test]
    fn invalid_marker_survives_assembly_even_when_the_buffer_is_valid_json() {
        let calls = assembled_calls(vec![complete_chunk("c1", r#"{"q":1}"#, true)]);
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].invalid_input,
            "marker carried onto middleware call"
        );
        assert_eq!(
            calls[0].function.arguments, r#"{"q":1}"#,
            "raw arguments kept"
        );

        // The gate rejects it with one INVALID_JSON result and does not dispatch it.
        let (executable, gated) = gate_function_calls_for_execution(&calls);
        assert!(executable.is_empty());
        assert_eq!(gated.len(), 1);
        assert!(gated[0].is_error);
    }

    #[test]
    fn unmarked_valid_call_is_executable_and_marker_is_per_call() {
        let calls = assembled_calls(vec![
            complete_chunk("ok", r#"{"q":1}"#, false),
            complete_chunk("bad", r#"{"q":1}"#, true),
        ]);
        let (executable, gated) = gate_function_calls_for_execution(&calls);
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].id, "ok");
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].call_id, "bad");
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
    fn cache_defaults_preserve_explicit_hints_and_ttls() {
        let hint = CacheHint::Ephemeral {
            ttl_seconds: Some(1800),
        };
        let messages = vec![
            ChatMessage::user()
                .text("stable")
                .cache(hint.clone())
                .build(),
            ChatMessage::user().text("current").build(),
        ];
        let result = apply_cache_breakpoints(&messages);
        assert_eq!(result[0].cache, Some(hint));
        assert_eq!(
            result[1].cache,
            Some(CacheHint::Ephemeral { ttl_seconds: None })
        );
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(apply_cache_breakpoints(&result)).unwrap()
        );
    }

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

    mod cost_accounting_tests {
        use super::*;
        use crate::middleware::AgentStats;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use querymt::Usage;
        use querymt::chat::{ChatOutput, FinishReason};
        use querymt::providers::{ModelInfo, ModelPricing, ProviderInfo, ProvidersRegistry};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Serializes mutation of the process-wide provider-cache location.
        pub(super) static CACHE_ENV_LOCK: tokio::sync::Mutex<()> =
            tokio::sync::Mutex::const_new(());

        /// Points the provider cache at a temporary registry that prices
        /// `mock/mock-model`, so no user cache or network fetch is involved.
        pub(super) struct InjectedPricing {
            _dir: tempfile::TempDir,
            previous: Option<std::ffi::OsString>,
        }

        impl InjectedPricing {
            pub(super) fn install(pricing: ModelPricing) -> Self {
                Self::install_for("mock", pricing)
            }

            pub(super) fn install_for(provider_id: &str, pricing: ModelPricing) -> Self {
                let dir = tempfile::TempDir::new().unwrap();
                let model = ModelInfo {
                    id: "mock-model".into(),
                    name: "mock-model".into(),
                    pricing,
                    ..Default::default()
                };
                let provider = ProviderInfo {
                    id: provider_id.into(),
                    name: provider_id.into(),
                    models: [("mock-model".to_string(), model)].into(),
                    ..Default::default()
                };
                let registry = ProvidersRegistry {
                    providers: [(provider_id.to_string(), provider)].into(),
                };
                std::fs::write(
                    dir.path().join("models.dev.json"),
                    serde_json::to_string(&registry).unwrap(),
                )
                .unwrap();
                let previous = std::env::var_os("QMT_PROVIDER_CACHE_DIR");
                // SAFETY: test-only mutation serialized by CACHE_ENV_LOCK and
                // restored in Drop.
                unsafe { std::env::set_var("QMT_PROVIDER_CACHE_DIR", dir.path()) };
                Self {
                    _dir: dir,
                    previous,
                }
            }
        }

        impl Drop for InjectedPricing {
            fn drop(&mut self) {
                // SAFETY: see `install`.
                unsafe {
                    match self.previous.take() {
                        Some(value) => std::env::set_var("QMT_PROVIDER_CACHE_DIR", value),
                        None => std::env::remove_var("QMT_PROVIDER_CACHE_DIR"),
                    }
                }
            }
        }

        fn usage_response(usage: Usage) -> ChatOutput {
            let mut output = ChatOutput::from_projections(
                None,
                Some("ok".into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            output.usage = Some(usage);
            output
        }

        async fn request_end_costs(
            events: &mut tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>,
        ) -> (Option<f64>, Option<f64>) {
            loop {
                let envelope = tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .expect("timed out waiting for LlmRequestEnd")
                    .expect("event channel closed");
                if let AgentEventKind::LlmRequestEnd {
                    cost_usd,
                    cumulative_cost_usd,
                    ..
                } = envelope.kind()
                {
                    return (*cost_usd, *cumulative_cost_usd);
                }
            }
        }

        /// Records a mismatch instead of panicking so one run reports every
        /// accounting defect.
        fn expect_usd(failures: &mut Vec<String>, actual: Option<f64>, expected: f64, what: &str) {
            match actual {
                Some(actual) if (actual - expected).abs() < 1e-9 => {}
                other => failures.push(format!("{what}: expected {expected}, got {other:?}")),
            }
        }

        /// Rates per million tokens: input 3, output 15, cache read 0.3,
        /// aggregate cache write 3.75.
        #[tokio::test]
        async fn request_end_costs_price_reasoning_once_and_keep_cache_and_history() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install(ModelPricing {
                input: Some(3.0),
                output: Some(15.0),
                cache_read: Some(0.3),
                cache_write: Some(3.75),
                ..Default::default()
            });

            let call = Arc::new(AtomicUsize::new(0));
            let counter = call.clone();
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(move |_| {
                Ok(usage_response(
                    match counter.fetch_add(1, Ordering::SeqCst) {
                        0 => Usage {
                            input_tokens: 1_000,
                            output_tokens: 100,
                            reasoning_tokens: 900,
                            cache_read: 2_000,
                            cache_write: 500,
                            cache_write_tiers: None,
                        },
                        _ => Usage {
                            input_tokens: 200,
                            output_tokens: 100,
                            reasoning_tokens: 0,
                            cache_read: 3_000,
                            cache_write: 0,
                            cache_write_tiers: None,
                        },
                    },
                ))
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            let mut events = fixture.config.subscribe_events();
            let request = Arc::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("hi").build()].into(),
                tools: Arc::from([]),
                estimated_tokens: 1,
                options: querymt::chat::GenerationOptions::NONE,
            });
            let context_with = |stats: AgentStats| {
                Arc::new(ConversationContext::new(
                    exec.session_id.as_str().into(),
                    Arc::from([]),
                    Arc::new(stats),
                    "mock".into(),
                    "mock-model".into(),
                ))
            };

            // First request: 1000 input + (100 + 900) output + 2000 reads + 500 writes.
            transition_call_llm(
                &fixture.config,
                &context_with(AgentStats::default()),
                &request,
                &exec,
            )
            .await
            .unwrap();
            let first = request_end_costs(&mut events).await;
            let first_cost = (3_000.0 + 15_000.0 + 600.0 + 1_875.0) / 1e6;
            let mut failures = Vec::new();
            expect_usd(
                &mut failures,
                first.0,
                first_cost,
                "first request cost includes cache",
            );
            expect_usd(
                &mut failures,
                first.1,
                first_cost,
                "first cumulative prices reasoning once, not twice",
            );

            // Second request starts from the stats the first request leaves behind.
            let after_first = AgentStats {
                total_input_tokens: 1_000,
                total_output_tokens: 100,
                reasoning_tokens: 900,
                cache_read_tokens: 2_000,
                cache_write_tokens: 500,
                ..Default::default()
            };
            transition_call_llm(&fixture.config, &context_with(after_first), &request, &exec)
                .await
                .unwrap();
            let second = request_end_costs(&mut events).await;
            let second_cost = (600.0 + 1_500.0 + 900.0) / 1e6;
            expect_usd(
                &mut failures,
                second.0,
                second_cost,
                "second request cost includes cache",
            );
            expect_usd(
                &mut failures,
                second.1,
                first_cost + second_cost,
                "second cumulative keeps earlier reasoning and cache charges",
            );
            assert!(
                failures.is_empty(),
                "accounting defects:\n{}",
                failures.join("\n")
            );
        }
    }

    mod attempt_accounting_tests {
        use super::cost_accounting_tests::{CACHE_ENV_LOCK, InjectedPricing};
        use super::genai_trace_tests::{TimedChunks, timed_fixture};
        use super::*;
        use crate::accounting::{AccountingReason, AttemptOutcome, UsageDisposition};
        use crate::session::backend::StorageBackend;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use querymt::Usage;
        use querymt::providers::ModelPricing;
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn pricing() -> ModelPricing {
            ModelPricing {
                input: Some(3.0),
                output: Some(15.0),
                cache_read: Some(0.3),
                cache_write: Some(3.75),
                ..Default::default()
            }
        }

        fn cost_value() -> f64 {
            cost()
        }

        fn usage() -> Usage {
            Usage {
                input_tokens: 100,
                output_tokens: 10,
                reasoning_tokens: 5,
                cache_read: 50,
                cache_write: 20,
                cache_write_tiers: None,
            }
        }

        fn cost() -> f64 {
            (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
        }

        fn output() -> ChatOutput {
            let mut out = ChatOutput::from_projections(
                None,
                Some("ok".into()),
                None,
                None,
                Some(FinishReason::Stop),
            );
            out.usage = Some(usage());
            out
        }

        fn prepared(with_tools: bool) -> Arc<PreparedModelRequest> {
            let tools: Vec<querymt::chat::Tool> = if with_tools {
                vec![
                    serde_json::from_value(serde_json::json!({"type":"function","function":{
                        "name":"safe","description":"","parameters":{"type":"object"}}}))
                    .unwrap(),
                ]
            } else {
                Vec::new()
            };
            Arc::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("hi").build()].into(),
                tools: tools.into(),
                estimated_tokens: 1,
                options: querymt::chat::GenerationOptions::NONE,
            })
        }

        fn context(exec: &ExecutionContext, provider: &str) -> Arc<ConversationContext> {
            Arc::new(ConversationContext::new(
                exec.session_id.as_str().into(),
                Arc::from([]),
                Arc::new(crate::middleware::AgentStats::default()),
                provider.into(),
                "mock-model".into(),
            ))
        }

        /// Accounting records persisted for the session, in journal order.
        async fn ledger(agent: &TestAgent, session_id: &str) -> Vec<AgentEventKind> {
            agent
                .storage
                .event_journal()
                .load_session_stream(session_id, None, None)
                .await
                .unwrap()
                .into_iter()
                .map(|e| e.kind)
                .filter(|k| {
                    matches!(
                        k,
                        AgentEventKind::LlmAttemptStarted { .. }
                            | AgentEventKind::LlmAttemptSettled { .. }
                    )
                })
                .collect()
        }

        fn settlements(ledger: &[AgentEventKind]) -> Vec<&crate::accounting::AttemptSettlement> {
            ledger
                .iter()
                .filter_map(|k| match k {
                    AgentEventKind::LlmAttemptSettled { settlement } => Some(settlement),
                    _ => None,
                })
                .collect()
        }

        fn starts(ledger: &[AgentEventKind]) -> usize {
            ledger
                .iter()
                .filter(|k| matches!(k, AgentEventKind::LlmAttemptStarted { .. }))
                .count()
        }

        async fn session_totals(exec: &ExecutionContext) -> crate::accounting::ScopeTotals {
            exec.cycle_accounting
                .get()
                .expect("cycle accounting is created on first dispatch")
                .session_totals()
        }

        fn rate_limited() -> LLMError {
            LLMError::RateLimited {
                message: "slow down".into(),
                retry_after_secs: Some(0),
            }
        }

        #[tokio::test]
        async fn non_streaming_retry_records_two_attempts_and_prices_only_the_success() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install(pricing());
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(move |_| {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(rate_limited())
                } else {
                    Ok(output())
                }
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();

            let ledger = ledger(&fixture, &exec.session_id).await;
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(starts(&ledger), 2, "each retry is its own attempt");
            let settled = settlements(&ledger);
            assert_eq!(settled.len(), 2);
            assert_eq!(settled[0].disposition, UsageDisposition::NotBillable);
            assert_eq!(settled[0].attempt.attempt_index, 0);
            assert_eq!(settled[1].disposition, UsageDisposition::Final);
            assert_eq!(settled[1].attempt.attempt_index, 1);
            assert_eq!(
                settled[0].attempt.operation_id,
                settled[1].attempt.operation_id
            );
            assert_ne!(settled[0].attempt.attempt_id, settled[1].attempt.attempt_id);

            let totals = session_totals(&exec).await;
            assert_eq!(totals.priced_attempts, 1);
            assert_eq!(totals.unresolved_attempts, 0);
            assert!((totals.complete_total_usd().unwrap() - cost()).abs() < 1e-12);
        }

        #[tokio::test]
        async fn non_streaming_with_tools_is_recorded_on_the_same_boundary() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install(pricing());
            let mut mock = MockLlmProvider::new();
            mock.expect_chat_with_tools().returning(|_, _| Ok(output()));
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(true),
                &exec,
            )
            .await
            .unwrap();
            let totals = session_totals(&exec).await;
            assert!((totals.complete_total_usd().unwrap() - cost()).abs() < 1e-12);
        }

        #[tokio::test]
        async fn unconfirmed_failure_stays_uncertain_and_confirmed_rejection_does_not() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install(pricing());
            for (error, uncertain) in [
                (LLMError::ProviderError("connection reset".into()), true),
                (LLMError::AuthError("bad key".into()), false),
            ] {
                let mut mock = MockLlmProvider::new();
                let message = error.to_string();
                mock.expect_chat().returning(move |_| {
                    Err(if uncertain {
                        LLMError::ProviderError(message.clone())
                    } else {
                        LLMError::AuthError(message.clone())
                    })
                });
                let fixture =
                    TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
                let exec = fixture.execution_context().await;
                let result = transition_call_llm(
                    &fixture.config,
                    &context(&exec, "mock"),
                    &prepared(false),
                    &exec,
                )
                .await;
                assert!(result.is_err());
                let totals = session_totals(&exec).await;
                assert_eq!(totals.unresolved_attempts, 0, "failure is settled");
                // An unconfirmed failure is a metered attempt with unknown
                // cost (incomplete); a confirmed rejection adds nothing.
                assert_eq!(totals.priced_attempts, u32::from(uncertain));
                assert_eq!(totals.incomplete_attempts, u32::from(uncertain));
                assert_eq!(totals.known_subtotal_usd, 0.0);
                assert_eq!(totals.complete_total_usd(), None);
                assert_eq!(
                    totals
                        .reasons()
                        .contains(&AccountingReason::UsageUnavailable),
                    uncertain
                );
            }
        }

        #[tokio::test]
        async fn cancelled_before_dispatch_creates_no_paid_attempt() {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().times(0);
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            exec.cancellation_token.cancel();
            let state = transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();
            assert!(matches!(state, ExecutionState::Cancelled));
            assert!(ledger(&fixture, &exec.session_id).await.is_empty());
        }

        #[tokio::test]
        async fn budget_refusal_ends_the_main_path_as_a_price_limit_stop() {
            for with_tools in [false, true] {
                let mut mock = MockLlmProvider::new();
                mock.expect_chat().times(0);
                mock.expect_chat_with_tools().times(0);
                let fixture =
                    TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
                // A zero cap is already reached by a cycle with no spend.
                let mut config = (*fixture.config).clone();
                config
                    .middleware_drivers
                    .push(Arc::new(crate::middleware::LimitsMiddleware::new(
                        crate::middleware::LimitsConfig::default().max_price_usd(0.0),
                    )));
                let exec = fixture.execution_context().await;
                let state = transition_call_llm(
                    &config,
                    &context(&exec, "mock"),
                    &prepared(with_tools),
                    &exec,
                )
                .await
                .expect("a refused dispatch is a stop, not an error");
                let ExecutionState::Stopped {
                    stop_type, message, ..
                } = state
                else {
                    panic!("expected a price-limit stop, got {state:?}");
                };
                assert_eq!(stop_type, StopType::PriceLimit, "tools={with_tools}");
                assert!(message.contains("Price limit reached"), "{message}");
                assert!(
                    ledger(&fixture, &exec.session_id).await.is_empty(),
                    "a refused dispatch records no paid attempt"
                );
            }
        }

        #[tokio::test]
        async fn catalog_refresh_and_model_switch_never_reprice_earlier_attempts() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(|_| Ok(output()));
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;

            let first_pricing = InjectedPricing::install(pricing());
            transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();
            let first_cost = cost();
            drop(first_pricing);

            // The catalog is refreshed with doubled rates before the next call.
            let doubled = ModelPricing {
                input: Some(6.0),
                output: Some(30.0),
                cache_read: Some(0.6),
                cache_write: Some(7.5),
                ..Default::default()
            };
            let _second_pricing = InjectedPricing::install(doubled);
            transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();

            let ledger = ledger(&fixture, &exec.session_id).await;
            let settled = settlements(&ledger);
            assert_eq!(settled.len(), 2);
            assert_eq!(
                settled[0].pricing,
                Some(pricing()),
                "first keeps its snapshot"
            );
            assert_eq!(settled[1].pricing.as_ref().unwrap().input, Some(6.0));

            let totals = session_totals(&exec).await;
            assert!(
                (totals.known_subtotal_usd - (first_cost + 2.0 * first_cost)).abs() < 1e-12,
                "earlier attempt must not be revalued: {}",
                totals.known_subtotal_usd
            );

            // Replay from the journal alone reproduces the same totals.
            let replayed = crate::accounting::SessionAccounting::hydrate(
                &exec.session_id,
                fixture.storage.event_journal(),
            )
            .await
            .unwrap();
            assert_eq!(replayed.read(|r| r.session().clone()), totals);
        }

        #[tokio::test]
        async fn unpriced_model_is_recorded_as_incomplete_not_free() {
            // No registry entry for this provider: billing class is unknown.
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("someone-else", pricing());
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(|_| Ok(output()));
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();
            let totals = session_totals(&exec).await;
            assert_eq!(totals.known_subtotal_usd, 0.0);
            assert_eq!(totals.complete_total_usd(), None);
            assert!(totals.reasons().contains(&AccountingReason::Valuation {
                reason: querymt::providers::IncompleteReason::PricingUnavailable
            }));
        }

        #[tokio::test]
        async fn history_save_failure_after_settlement_keeps_the_recorded_charge() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install(pricing());
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(|_| Ok(output()));
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let mut exec = fixture.execution_context().await;

            let state = transition_call_llm(
                &fixture.config,
                &context(&exec, "mock"),
                &prepared(false),
                &exec,
            )
            .await
            .unwrap();
            let ExecutionState::AfterLlm { response, context } = state else {
                panic!("expected AfterLlm");
            };
            let recorded = session_totals(&exec).await;
            assert!((recorded.complete_total_usd().unwrap() - cost()).abs() < 1e-12);

            // Make storing the assistant message fail.
            fixture
                .storage
                .session_store()
                .delete_session(&exec.session_id)
                .await
                .unwrap();
            let result =
                transition_after_llm(&fixture.config, &response, &context, &mut exec).await;
            assert!(result.is_err(), "history save must fail");

            // The charge is unaffected, live and from the journal alone.
            assert_eq!(session_totals(&exec).await, recorded);
            let replayed = crate::accounting::SessionAccounting::hydrate(
                &exec.session_id,
                fixture.storage.event_journal(),
            )
            .await
            .unwrap();
            assert_eq!(replayed.read(|r| r.session().clone()), recorded);
        }

        #[tokio::test]
        async fn accounting_storage_failure_is_an_execution_error_not_a_budget_outcome() {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().times(0);
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            // A cap is configured, so a budget stop would be possible.
            let mut config = (*fixture.config).clone();
            config
                .middleware_drivers
                .push(Arc::new(crate::middleware::LimitsMiddleware::new(
                    crate::middleware::LimitsConfig::default().max_price_usd(100.0),
                )));
            let exec = fixture.execution_context().await;
            // Open accounting first, then make the journal unwritable so the
            // durable attempt start cannot be recorded.
            exec.cycle_accounting(&config.event_sink).await.unwrap();
            fixture
                .storage
                .conn_for_test()
                .lock()
                .unwrap()
                .execute_batch("DROP TABLE event_journal")
                .unwrap();
            let result =
                transition_call_llm(&config, &context(&exec, "mock"), &prepared(false), &exec)
                    .await;
            let error = result.expect_err("storage failure must be an execution error");
            assert!(error.to_string().contains("accounting"), "{error:#}");
        }

        type Events = tokio::sync::broadcast::Receiver<crate::events::EventEnvelope>;

        /// The next `LlmRequestEnd` as (cost, accounting), or `None` if none
        /// arrives within the window.
        async fn next_request_end(
            events: &mut Events,
            wait: Duration,
        ) -> Option<(Option<f64>, Option<crate::accounting::RequestAccounting>)> {
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let envelope = tokio::time::timeout(remaining, events.recv())
                    .await
                    .ok()?
                    .ok()?;
                if let AgentEventKind::LlmRequestEnd {
                    cost_usd,
                    accounting,
                    ..
                } = envelope.kind()
                {
                    return Some((*cost_usd, accounting.clone()));
                }
            }
        }

        #[tokio::test]
        async fn request_end_cost_spans_retries_and_includes_cache_charges() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![
                Err(rate_limited()),
                Ok(vec![
                    (0, Ok(StreamChunk::Text("hello".into()))),
                    (0, Ok(chunk_usage())),
                    (
                        0,
                        Ok(StreamChunk::Done {
                            finish_reason: FinishReason::Stop,
                        }),
                    ),
                ]),
            ])
            .await;
            let mut events = fixture.config.subscribe_events();
            transition_call_llm(
                &fixture.config,
                &context(&exec, "alias"),
                &prepared(true),
                &exec,
            )
            .await
            .unwrap();

            let (cost, accounting) = next_request_end(&mut events, Duration::from_secs(1))
                .await
                .expect("request end");
            // cost() includes cache read and write charges; the rejected retry adds none.
            assert!((cost.unwrap() - cost_value()).abs() < 1e-12);
            let accounting = accounting.expect("accounting detail");
            assert!(accounting.complete);
            assert_eq!(accounting.attempt_ids.len(), 2);
            assert!((accounting.known_subtotal_usd - cost_value()).abs() < 1e-12);

            // The identifiers match the durable attempt records.
            let ledger = ledger(&fixture, &exec.session_id).await;
            let recorded: Vec<_> = settlements(&ledger)
                .into_iter()
                .map(|s| s.attempt.attempt_id.clone())
                .collect();
            assert_eq!(accounting.attempt_ids, recorded);
            let operation_ids: std::collections::HashSet<_> = settlements(&ledger)
                .into_iter()
                .map(|s| s.attempt.operation_id.clone())
                .collect();
            assert_eq!(operation_ids.len(), 1);
            assert_eq!(operation_ids.iter().next(), Some(&accounting.operation_id));
        }

        #[tokio::test]
        async fn usage_bearing_retry_counts_once_and_marks_operation_partial() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![
                // First attempt reports usage, then the connection drops.
                Ok(vec![
                    (0, Ok(chunk_usage())),
                    (0, Err(LLMError::HttpError("reset".into()))),
                ]),
                Ok(vec![
                    (0, Ok(StreamChunk::Text("hello".into()))),
                    (0, Ok(chunk_usage())),
                    (
                        0,
                        Ok(StreamChunk::Done {
                            finish_reason: FinishReason::Stop,
                        }),
                    ),
                ]),
            ])
            .await;
            let mut events = fixture.config.subscribe_events();
            transition_call_llm(
                &fixture.config,
                &context(&exec, "alias"),
                &prepared(true),
                &exec,
            )
            .await
            .unwrap();

            let (cost, accounting) = next_request_end(&mut events, Duration::from_secs(1))
                .await
                .expect("request end");
            let accounting = accounting.unwrap();
            assert_eq!(accounting.attempt_ids.len(), 2);
            // Both attempts' usage is charged exactly once each ...
            assert!((accounting.known_subtotal_usd - 2.0 * cost_value()).abs() < 1e-12);
            // ... but the interrupted attempt keeps the operation partial.
            assert_eq!(
                cost, None,
                "a partial operation total is never published as complete"
            );
            assert!(!accounting.complete);
            assert!(accounting.reasons.contains(&AccountingReason::PartialUsage));
        }

        #[tokio::test]
        async fn cancelled_execution_is_accounted_without_a_request_end_event() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
            ])])
            .await;
            let mut events = fixture.config.subscribe_events();
            let context = context(&exec, "alias");
            let request = prepared(true);
            let mut future = Box::pin(transition_call_llm(
                &fixture.config,
                &context,
                &request,
                &exec,
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(400), &mut future)
                    .await
                    .is_err()
            );
            exec.cancellation_token.cancel();
            assert!(matches!(future.await.unwrap(), ExecutionState::Cancelled));

            assert!(
                next_request_end(&mut events, Duration::from_millis(300))
                    .await
                    .is_none(),
                "no normal request end for cancelled execution"
            );
            let totals = session_totals(&exec).await;
            assert!((totals.known_subtotal_usd - cost_value()).abs() < 1e-12);
            assert_eq!(totals.unresolved_attempts, 0);
            assert_eq!(totals.complete_total_usd(), None);
        }

        fn chunk_usage() -> StreamChunk {
            StreamChunk::Usage(usage())
        }

        async fn stream_fixture(attempts: Vec<TimedChunks>) -> (TestAgent, ExecutionContext) {
            let (fixture, _factory) = timed_fixture(true, attempts, "openai", None).await;
            let exec = fixture.execution_context().await;
            (fixture, exec)
        }

        #[tokio::test]
        async fn streaming_retry_after_setup_rejection_keeps_separate_accumulators() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![
                Err(rate_limited()),
                Ok(vec![
                    (0, Ok(StreamChunk::Text("hello".into()))),
                    (0, Ok(chunk_usage())),
                    (
                        0,
                        Ok(StreamChunk::Done {
                            finish_reason: FinishReason::Stop,
                        }),
                    ),
                ]),
            ])
            .await;
            transition_call_llm(
                &fixture.config,
                &context(&exec, "alias"),
                &prepared(true),
                &exec,
            )
            .await
            .unwrap();

            let ledger = ledger(&fixture, &exec.session_id).await;
            assert_eq!(starts(&ledger), 2);
            let settled = settlements(&ledger);
            assert_eq!(settled[0].disposition, UsageDisposition::NotBillable);
            assert!(settled[0].usage.is_none(), "retry must not inherit usage");
            assert_eq!(settled[1].disposition, UsageDisposition::Final);
            assert_eq!(settled[1].usage.as_ref(), Some(&usage()));
            let totals = session_totals(&exec).await;
            assert!((totals.complete_total_usd().unwrap() - cost()).abs() < 1e-12);
        }

        #[tokio::test]
        async fn streaming_failure_after_usage_is_partial_and_priced() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
                (0, Err(LLMError::ProviderError("stream broke".into()))),
            ])])
            .await;
            let result = transition_call_llm(
                &fixture.config,
                &context(&exec, "alias"),
                &prepared(true),
                &exec,
            )
            .await;
            assert!(result.is_err());

            let ledger = ledger(&fixture, &exec.session_id).await;
            let settled = settlements(&ledger);
            assert_eq!(settled.len(), 1);
            assert_eq!(settled[0].disposition, UsageDisposition::Partial);
            assert_eq!(settled[0].usage.as_ref(), Some(&usage()));
            let totals = session_totals(&exec).await;
            assert!((totals.known_subtotal_usd - cost()).abs() < 1e-12);
            assert_eq!(totals.complete_total_usd(), None);
            assert!(totals.reasons().contains(&AccountingReason::PartialUsage));
        }

        #[tokio::test]
        async fn streaming_cancellation_preserves_available_partial_usage() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            // No terminal chunk: the provider stream stays pending.
            let (fixture, exec) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
            ])])
            .await;
            let context = context(&exec, "alias");
            let request = prepared(true);
            let mut future = Box::pin(transition_call_llm(
                &fixture.config,
                &context,
                &request,
                &exec,
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(400), &mut future)
                    .await
                    .is_err()
            );
            exec.cancellation_token.cancel();
            assert!(matches!(future.await.unwrap(), ExecutionState::Cancelled));

            let ledger = ledger(&fixture, &exec.session_id).await;
            let settled = settlements(&ledger);
            assert_eq!(settled.len(), 1);
            assert_eq!(settled[0].outcome, AttemptOutcome::Cancelled);
            assert_eq!(settled[0].disposition, UsageDisposition::Partial);
            assert_eq!(settled[0].usage.as_ref(), Some(&usage()));
            assert_eq!(session_totals(&exec).await.unresolved_attempts, 0);
        }

        #[tokio::test]
        async fn dropped_streaming_request_settles_instead_of_staying_unresolved() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec) = stream_fixture(vec![Ok(vec![(0, Ok(chunk_usage()))])]).await;
            {
                let context = context(&exec, "alias");
                let request = prepared(true);
                let mut future = Box::pin(transition_call_llm(
                    &fixture.config,
                    &context,
                    &request,
                    &exec,
                ));
                assert!(
                    tokio::time::timeout(Duration::from_millis(400), &mut future)
                        .await
                        .is_err()
                );
                // Owning future dropped, as when a turn task is aborted.
            }
            for _ in 0..200 {
                if session_totals(&exec).await.unresolved_attempts == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let ledger = ledger(&fixture, &exec.session_id).await;
            assert_eq!(starts(&ledger), 1);
            assert_eq!(settlements(&ledger)[0].outcome, AttemptOutcome::Cancelled);
            assert_eq!(settlements(&ledger)[0].usage.as_ref(), Some(&usage()));
        }
    }
}
