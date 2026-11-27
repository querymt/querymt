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
use crate::agent::execution::TurnError;
use crate::agent::execution::turn::{Turn, TurnEnv};
use crate::agent::execution::{CycleOutcome, Stage, Step};
use crate::agent::session_actor::ensure_pre_turn_snapshot_ready;
use crate::agent::utils::{genai, u32_from_usize};
use crate::middleware::{
    LlmResponse, PreparedBatch, PreparedCallVerdict, PreparedModelRequest, PreparedToolCall,
    ToolCall as MiddlewareToolCall, ToolFunction, ToolResult, TurnStack, apply_usage_to_stats,
    calculate_context_tokens,
};
use anyhow::Context as _;
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::future::join_all;
use log::{debug, trace, warn};
use querymt::ToolCall;
use querymt::chat::{
    ChatCall, ChatMessage, ChatMessagePartDelta, ChatOutput, ChatOutputItem, ChatOutputStatus,
    ChatRole, ChatStreamAccumulator, ChatStreamAccumulatorError, ChatStreamFinish, FinishReason,
    ReasoningPartKind, StreamChunk, StructuredStreamEvent,
};
use querymt::error::LLMError;
use querymt::error::{ProviderErrorKind, ProviderFailure};
use querymt_agent_core::events::{
    AgentEventKind, ExecutionMetrics, ReasoningPartStored, StopType, reasoning_content_part_id,
    reasoning_summary_part_id,
};
use querymt_agent_core::model::{AgentMessage, MessagePart};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug_span, info_span, instrument};
use uuid::Uuid;

/// Whether the request follows a tool batch or a user prompt, for the context hook.
///
/// Read before the request's notices are stored, so a notice stored after a
/// tool batch does not make the request a user-prompt request. Tool records
/// stored after the tool results do not change what the request follows.
pub(super) fn context_hook_trigger(history: &[AgentMessage]) -> ContextTrigger {
    if history
        .iter()
        .rfind(|message| !message.is_tool_record())
        .is_some_and(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::ToolResult { .. }))
        })
    {
        ContextTrigger::AfterToolBatch
    } else {
        ContextTrigger::UserPrompt
    }
}

/// What a request follows, as the context hook's `trigger` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum ContextTrigger {
    /// The request follows the results of a tool batch.
    AfterToolBatch,
    /// The request follows a user prompt.
    UserPrompt,
}

/// Tell the session's clients about the skill problems of its workspace when
/// the list differs from the one last announced to this session.
///
/// Runs right after tool collection, where the `skill` tool's definition has
/// just refreshed the catalog, and only when that tool is offered. An empty
/// list is announced only to a session that was told about a problem.
fn announce_skill_problems(
    config: &AgentConfig,
    env: &TurnEnv,
    tools: &[querymt::chat::Tool],
    collect_span: &tracing::Span,
) {
    let Some(catalog) = &config.skill_catalog else {
        return;
    };
    if !tools
        .iter()
        .any(|tool| tool.function.name == crate::skills::SkillTool::NAME)
    {
        return;
    }
    let workspace = env
        .runtime
        .cwd
        .as_deref()
        .unwrap_or_else(|| catalog.project_root());
    if let Some(stats) = catalog.last_refresh_stats(workspace) {
        collect_span.record("skill_refresh_us", stats.elapsed.as_micros() as u64);
        collect_span.record("skill_refresh_entries", stats.visited_entries as u64);
    }
    let problems: Vec<querymt_agent_core::events::SkillProblemInfo> = catalog
        .problems(workspace)
        .into_iter()
        .map(|problem| querymt_agent_core::events::SkillProblemInfo {
            path: problem.path.display().to_string(),
            source: problem.source.display().to_string(),
            kind: problem.kind.as_str().to_string(),
            message: problem.message,
        })
        .collect();
    let current = (!problems.is_empty())
        .then(|| serde_json::to_vec(&problems).ok())
        .flatten()
        .map(|bytes| querymt_agent_core::hash::RapidHash::new(&bytes));
    {
        let mut announced = env.runtime.announced_skill_problems.lock();
        if *announced == current {
            return;
        }
        *announced = current;
    }
    config.emit_event(
        &env.session_id,
        AgentEventKind::SkillProblemsChanged { problems },
    );
}

/// Transition from BeforeLlmCall to CallLlm.
///
/// This collects available tools, computes their hash, and emits a ToolsAvailable event
/// if the tool set has changed. `trigger` is the context-hook trigger, from
/// [`context_hook_trigger`] over the history before notices were stored.
/// `objective` is the request-scoped run objective, sent in the request tail.
/// The provider and its capabilities are the run's model (`env.model`),
/// resolved once before the run's environment was built.
#[instrument(
    name = "agent.transition.before_llm_call",
    skip(config, env, turn, trigger, objective),
    fields(
        session_id = %env.session_id,
        provider = %env.provider_name(),
        model = %env.model_name(),
        steps = turn.stats.steps
    )
)]
pub(super) async fn transition_before_llm_call(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
    trigger: ContextTrigger,
    objective: Option<&str>,
) -> Result<Step, TurnError> {
    debug!(
        "BeforeLlmCall: session={}, steps={}",
        env.session_id, turn.stats.steps
    );

    if env.cancellation_token.is_cancelled() {
        return Ok(Step::Break(CycleOutcome::Cancelled));
    }

    env.require_turn_authority()
        .await
        .map_err(anyhow::Error::from)?;
    let trigger: &'static str = trigger.into();

    let collect_span = info_span!(
        "agent.tools.collect",
        tool_count = tracing::field::Empty,
        tools_bytes = tracing::field::Empty,
        skill_refresh_us = tracing::field::Empty,
        skill_refresh_entries = tracing::field::Empty
    );
    let capabilities = env.model.profile.capabilities;
    // Peer turns have a filtered tool set. Do not replay a host turn's tool
    // baseline, which may include definitions this peer cannot use.
    let tool_updates = capabilities.tool_updates
        && matches!(
            env.turn_authority(),
            crate::agent::session_actor::TurnAuthority::Local
        );
    // Default message hints request the configured lifetime, capped at what
    // the provider offers; the provider rounds it to one it supports.
    let hint_ttl_seconds = capabilities
        .prompt_cache
        .default_hint_ttl(config.execution_policy.prompt_cache.ttl_secs)
        .map(u64::from);
    let (tools, tools_json, new_hash) = {
        // Synchronous block: the guard never crosses an await.
        let _guard = collect_span.enter();
        let mut tools = config.collect_tools(
            Arc::clone(&env.model.provider),
            Some(env.runtime.as_ref()),
            Some(&env.tool_config),
            env.tool_set(),
        );
        tools.retain(|tool| env.allows_workflow_tool(&tool.function.name));
        announce_skill_problems(config, env, &tools, &collect_span);
        if env.restricted_mesh_turn() {
            tools = tools
                .into_iter()
                .filter_map(|tool| {
                    super::tool_calls::restricted_mesh_tool(config, env, &tool.function.name)
                        .map(|(_, definition)| definition)
                })
                .collect();
        }
        let tools_json =
            serde_json::to_vec(&tools).context("Failed to serialize tools for hash computation")?;
        let new_hash = querymt_agent_core::hash::RapidHash::new(&tools_json);
        collect_span.record("tool_count", tools.len() as u64);
        collect_span.record("tools_bytes", tools_json.len() as u64);
        (tools, tools_json, new_hash)
    };

    let current_hash = env.runtime.mcp_tool_state.load().tools_hash;
    let changed = current_hash.is_none_or(|h| h != new_hash);
    if changed {
        env.runtime.mcp_tool_state.rcu(|snap| {
            let mut s = snap.clone();
            s.tools_hash = Some(new_hash);
            s
        });
    }

    if changed {
        config.emit_event(
            &env.session_id,
            querymt_agent_core::events::AgentEventKind::ToolsAvailable {
                tools: tools.clone(),
                tools_hash: new_hash,
            },
        );
    }

    // The request's tool list and history. For a provider that declares
    // `tool_updates` this stores a baseline or a tool change when due, and the
    // list is the most recent baseline's; otherwise it is the tools offered now
    // and the stored tool records are left out.
    let tool_changes = super::tool_history::resolve(
        env,
        turn,
        &super::tool_history::origin(env.provider_name(), env.model_name()),
        tool_updates,
        &tools,
    )
    .await?;
    let target = env
        .session_handle
        .output_target(&env.model, turn.history.as_slice());
    let projected = querymt_agent_core::projection::project_request(
        turn.history.as_slice(),
        &querymt_agent_core::projection::RequestTarget {
            output: target.as_ref(),
            max_prompt_bytes: env
                .execution_config()
                .and_then(|config| config.max_prompt_bytes),
            tool_changes,
        },
    )
    .map_err(|error| anyhow::anyhow!("Invalid stored message content: {error}"))?;
    // The request lists share the tools offered now whenever their contents
    // match, so the schema list is copied only for a baseline that differs.
    let tools: Arc<[querymt::chat::Tool]> = Arc::from(tools);
    let (leading, leading_json): (Arc<[querymt::chat::Tool]>, Vec<u8>) =
        match projected.baseline_tools {
            Some(baseline) if baseline != &tools[..] => (
                Arc::from(baseline),
                serde_json::to_vec(baseline)
                    .context("Failed to serialize tools for hash computation")?,
            ),
            _ => (Arc::clone(&tools), tools_json),
        };
    let offered_tools: Arc<[querymt::chat::Tool]> = if tool_changes {
        let offered = super::tool_history::offered_refs(turn.history.as_slice());
        if offered.iter().copied().eq(leading.iter()) {
            Arc::clone(&leading)
        } else {
            offered.into_iter().cloned().collect()
        }
    } else {
        Arc::clone(&tools)
    };
    let leading_hash = querymt_agent_core::hash::RapidHash::new(&leading_json);

    // The context hook sees durable history only (stored notices included).
    let mut messages = projected.messages;
    let context_window =
        querymt_agent_services::model_info::get_model_info(env.provider_name(), env.model_name())
            .and_then(|info| info.limits.context)
            .unwrap_or(u32::MAX as u64)
            .min(u32::MAX as u64) as u32;
    let hook_span = info_span!(
        "agent.hooks.context",
        trigger,
        message_count = messages.len() as u64
    );
    env.require_turn_authority()
        .await
        .map_err(anyhow::Error::from)?;
    let hook_result = if env.restricted_mesh_turn() {
        crate::hooks::ContextHookResult {
            estimated_tokens: crate::session::pruning::estimate_chat_tokens(&messages),
            messages: Some(messages),
            additional_context: Vec::new(),
            notices: Vec::new(),
        }
    } else {
        config
            .hooks
            .run_context(crate::hooks::ContextHookRequest {
                session_id: env.session_id.clone(),
                mcp_tool_state: Some(env.runtime.mcp_tool_state.clone()),
                turn_id: env.turn_id().to_string(),
                cwd: env.cwd().map(|path| path.to_path_buf()),
                model: env.model_name().to_string(),
                permission_mode: env.permission_mode().to_string(),
                trigger: trigger.to_string(),
                context_window,
                messages,
            })
            .instrument(hook_span)
            .await?
    };
    for notice in hook_result.notices {
        config.emit_event(
            &env.session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    messages = hook_result.messages.unwrap_or_default();
    let request_tail = request_tail(objective, &hook_result.additional_context);
    // Providers reject a tool call without its result. Reaching this point
    // with one is a bug upstream; answer it so the run survives, and make it loud.
    let unanswered = querymt_agent_core::model::answer_unanswered_tool_calls(&mut messages);
    if !unanswered.is_empty() {
        log::error!(
            "Session {}: request had unanswered tool call(s) {:?}; answered them as cancelled",
            env.session_id,
            unanswered
        );
        debug_assert!(
            unanswered.is_empty(),
            "request had unanswered tool calls: {unanswered:?}"
        );
    }
    // Attribute any cache prefix change against the last completed request.
    // Durable history only: the request tail is rebuilt every time.
    let prefix_fingerprint = super::prefix::PrefixFingerprint::new(
        env.provider_name(),
        env.model_name(),
        leading_hash,
        system_prompt_hash(config, env),
        &messages,
    );
    env.runtime
        .cache_prefix
        .lock()
        .begin(prefix_fingerprint, std::time::Instant::now());
    // Tool definitions are model input; reuse the serialization already used for their hash.
    let tool_tokens = if leading.is_empty() {
        0
    } else {
        crate::session::pruning::tool_json_tokens(leading_json.len())
    };
    validate_tool_context_budget(tool_tokens, context_window)?;
    let tail_tokens = request_tail.as_ref().map_or(0, |tail| {
        crate::session::pruning::estimate_chat_tokens(std::slice::from_ref(tail))
    });
    let estimated_tokens = hook_result
        .estimated_tokens
        .saturating_add(tool_tokens)
        .saturating_add(tail_tokens);
    if estimated_tokens >= context_window as usize {
        return Ok(super::stopping(
            StopType::ContextThreshold,
            format!(
                "Prepared request is approximately {} tokens, exceeding the {} token context window",
                estimated_tokens, context_window
            ),
        ));
    }
    // The two cache hints go in place on the messages as they are sent, after
    // any context hook has restructured them.
    debug_span!(
        "agent.request.cache_breakpoints",
        message_count = messages.len() as u64
    )
    .in_scope(|| super::cache_hints::apply_cache_breakpoints(&mut messages, hint_ttl_seconds));
    // The tail follows the hinted durable messages and carries no hint.
    messages.reserve(usize::from(request_tail.is_some()));
    messages.extend(request_tail);
    // Re-evaluate room after history, steering, tools, and context hooks are prepared.
    let mut options = turn.request_overrides;
    if let Some(cap) = options.max_tokens {
        let room = context_window.saturating_sub(estimated_tokens as u32);
        options.max_tokens = Some(cap.min(room));
    }
    Ok(Step::Continue(Stage::CallModel(Box::new(
        PreparedModelRequest {
            messages: Arc::from(messages.into_boxed_slice()),
            tools: leading,
            offered_tools,
            estimated_tokens,
            options,
        },
    ))))
}

/// Hash of the system prompt the session's provider is configured with: the
/// turn-pinned session config when it carries one, else the agent's initial
/// config.
fn system_prompt_hash(config: &AgentConfig, env: &TurnEnv) -> querymt_agent_core::hash::RapidHash {
    let pinned = env
        .llm_config()
        .and_then(|llm| llm.params.as_ref())
        .and_then(|params| params.get("system"));
    // A JSON value and an optional string always serialize.
    let bytes = match pinned {
        Some(system) => serde_json::to_vec(system),
        None => serde_json::to_vec(&config.provider.initial_config().system),
    }
    .expect("system prompt serializes to JSON");
    querymt_agent_core::hash::RapidHash::new(&bytes)
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

/// The request tail: at most one user message holding the run objective (when
/// enabled) followed by hook `additional_context`. Never stored, rebuilt for
/// every request, and the only place text that is not in history may go.
fn request_tail(objective: Option<&str>, additional_context: &[String]) -> Option<ChatMessage> {
    let mut parts = objective
        .into_iter()
        .map(str::to_owned)
        .chain(additional_context.iter().cloned())
        .filter(|text| !text.trim().is_empty());
    let first = parts.next()?;
    let mut tail = ChatMessage::user().text(first).build();
    for part in parts {
        tail.push_input_part(querymt::chat::ChatInputPart::text(part));
    }
    Some(tail)
}

pub(super) fn contextualize_llm_error(
    error: LLMError,
    operation: &'static str,
    env: &TurnEnv,
) -> TurnError {
    provider_error(error, operation, env.provider_name(), env.model_name())
}

/// A provider failure of `operation` for `provider` and `model`, recorded on
/// the current span. [`contextualize_llm_error`] for callers that have no
/// [`TurnEnv`] yet (resolving the model).
pub(super) fn provider_error(
    error: LLMError,
    operation: &'static str,
    provider: &str,
    model: &str,
) -> TurnError {
    genai::llm_error(&error);
    TurnError::Provider {
        operation,
        provider: provider.to_string(),
        model: model.to_string(),
        source: error,
    }
}

/// Map a failed LLM setup/call into either `Cancelled` or a contextualized error.
///
/// Cancel must never bubble as `Err`; it is an execution state, not a provider failure.
fn accounting_failure(error: querymt_agent_services::accounting::AccountingError) -> anyhow::Error {
    anyhow::anyhow!("accounting failure: {error}")
}

/// The logical accounting operation for one main model request. Its retries
/// are separate attempts. Failing to open accounting is an execution error:
/// no paid request may be sent without a ledger.
fn main_attempt_operation(
    config: &AgentConfig,
    env: &TurnEnv,
) -> querymt_agent_services::accounting::AttemptOperation {
    let cycle = &env.cycle_accounting;
    let recorder = Arc::new(
        querymt_agent_services::accounting::AttemptRecorder::with_projection(
            config.event_sink.session_stream(&env.session_id),
            cycle.session_accounting().clone(),
        ),
    );
    let (provider, model) = env
        .llm_config()
        .map(|c| (c.provider.clone(), c.model.clone()))
        .unwrap_or_default();
    querymt_agent_services::accounting::AttemptOperation::new(
        recorder,
        cycle,
        querymt_agent_services::accounting::AttemptPurpose::MainResponse,
        provider,
        model,
    )
    .with_budget_pricing(config.budget_pricing())
    .with_budget_cap(config.budget_cap_usd())
}

/// A paid dispatch the cycle budget gate refused ends the cycle as a
/// price-limit stop, not an execution failure. Nothing was sent.
fn budget_stop(operation: &querymt_agent_services::accounting::AttemptOperation) -> Option<Step> {
    operation.budget_refusal().map(|(cap, spent)| {
        super::stopping(
            StopType::PriceLimit,
            format!("Price limit reached: ${spent:.4} >= ${cap:.2}"),
        )
    })
}

fn map_failed_llm_call(error: LLMError, streaming: bool, env: &TurnEnv) -> Result<Step, TurnError> {
    match error {
        LLMError::Cancelled => Ok(Step::Break(CycleOutcome::Cancelled)),
        error => Err(contextualize_llm_error(
            error,
            if streaming { "streaming" } else { "chat" },
            env,
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
    results: &[ToolResult],
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Step, TurnError> {
    let next = super::tool_calls::store_all_tool_results(
        config,
        results,
        env,
        turn,
        crate::agent::execution::DrainedSteering::none(),
    )
    .await?;
    Ok(match next {
        Stage::BeforeRequest => super::stopping(StopType::ModelTokenLimit, "Model hit token limit"),
        waiting => Step::Continue(waiting),
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
    skip(config, request, env, turn),
    fields(
        otel.name = %format!("chat {}", env.model_name()),
        otel.kind = "client",
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = %genai::provider_name(env.provider_name()),
        gen_ai.request.model = %env.model_name(),
        gen_ai.conversation.id = %env.session_id,
        session.id = %env.session_id,
        gen_ai.request.stream = tracing::field::Empty,
        qmt.cache.prefix_change = tracing::field::Empty,
        qmt.cache.idle_secs = tracing::field::Empty,
        session_id = %env.session_id,
        provider = %env.provider_name(),
        model = %env.model_name(),
        message_count = request.messages.len(),
        tool_count = request.offered_tools.len()
    )
)]
pub(super) async fn transition_call_llm(
    config: &AgentConfig,
    request: &PreparedModelRequest,
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Step, TurnError> {
    // A single expected generation; early cancellation/drop retains an abnormal finish
    // without marking the span as an error. Successful canonical output replaces it.
    genai::finish_reason("error");
    let chat_span = tracing::Span::current();
    // Attempts of this request, streaming or not, are reported on the span.
    let request_attempts = genai::RequestAttempts::new(&chat_span);
    genai::agent_id(&chat_span, config.provider.agent_id.as_deref());
    let session_id = &env.session_id;
    let request_messages = request.messages.as_ref();
    genai::conversation_compacted(&chat_span, turn.history.as_slice(), request_messages);
    let tools = &request.tools;
    let prefix = env.runtime.cache_prefix.lock().pending_attribution();
    if let Some(prefix) = prefix {
        chat_span.record("qmt.cache.prefix_change", prefix.change.as_str());
        if let Some(idle) = prefix.idle_secs {
            chat_span.record("qmt.cache.idle_secs", idle);
        }
    }
    debug!(
        "CallLlm: session={}, messages={}",
        session_id,
        request_messages.len()
    );

    if env.cancellation_token.is_cancelled() {
        return Ok(Step::Break(CycleOutcome::Cancelled));
    }

    env.require_turn_authority()
        .await
        .map_err(anyhow::Error::from)?;

    // Overrides last for exactly this request. Options the provider does not
    // honour are dropped (and logged); `applied` records what was really sent.
    let options = env
        .model
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

    let session_handle = &env.session_handle;
    let provider = Arc::clone(&env.model.provider);
    let messages_with_cache = request_messages;
    // Recorded on every exit path (success, error, cancel) when dropped.
    let chat_timer = crate::agent::utils::llm_metrics::ChatTimer::start(
        genai::provider_name(env.provider_name()),
        env.model_name(),
        &env.cancellation_token,
    );
    let operation =
        info_span!("agent.accounting.open").in_scope(|| main_attempt_operation(config, env));

    // Pre-allocated message_id for streaming path so that delta events and the
    // final AssistantMessageStored share the same ID.
    let mut streaming_message_id: Option<String> = None;

    // Determine response via streaming or non-streaming path.
    // Each arm produces canonical ordered output so the rest of the function is
    // uniform and item-aware data survives without flattening.
    // Tool changes can offer tools even with an empty baseline. Mesh calls
    // keep the streaming path available even when no tools are offered.
    let mesh_streaming = env.restricted_mesh_turn() || session_handle.uses_explicit_mesh_provider();
    let call_tools = (!mesh_streaming || !tools.is_empty()).then_some(tools.as_ref());
    let output: ChatOutput = if tools.is_empty()
        && request.offered_tools.is_empty()
        && !mesh_streaming
    {
        let cancel = env.cancellation_token.clone();
        match super::llm_retry::call_chat_with_retry(
            config,
            session_id,
            &request_attempts,
            &env.cancellation_token,
            || {
                let messages_with_cache = &messages_with_cache;
                let cancel = cancel.clone();
                let operation = &operation;
                let provider = &provider;
                async move {
                    // `options` already holds only what the provider honours.
                    let call = ChatCall::new(messages_with_cache).options(options);
                    querymt_agent_services::accounting::run_attempt(
                        operation,
                        session_handle.billing_basis(),
                        &cancel,
                        || async {
                            env.require_turn_authority()
                                .await
                                .map_err(|error| LLMError::InvalidRequest(error.to_string()))?;
                            provider.chat(&call).await
                        },
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
                if let Some(stop) = budget_stop(&operation) {
                    return Ok(stop);
                }
                return map_failed_llm_call(e, false, env);
            }
        }
    } else {
        session_handle.record_generation_settings(&chat_span, options);
        if env.model.profile.capabilities.streaming {
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
            )
            .with_attempts(request_attempts.clone());
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
                if env.cancellation_token.is_cancelled() {
                    return Ok(Step::Break(CycleOutcome::Cancelled));
                }

                env.require_turn_authority()
                    .await
                    .map_err(anyhow::Error::from)?;
                // Durable start before the request is sent; a failure here
                // means nothing was dispatched.
                let mut attempt = match operation
                    .begin(
                        querymt_agent_services::accounting::DispatchMode::Streaming,
                        session_handle.billing_basis(),
                    )
                    .await
                {
                    Ok(attempt) => attempt,
                    Err(error) => {
                        if let Some(stop) = budget_stop(&operation) {
                            return Ok(stop);
                        }
                        return Err(accounting_failure(error).into());
                    }
                };
                if let Err(error) = env.require_turn_authority().await {
                    attempt
                        .settle_error(&LLMError::InvalidRequest(error.to_string()), false, None)
                        .await
                        .map_err(accounting_failure)?;
                    return Err(anyhow::Error::from(error).into());
                }
                let mut response_started = false;

                timing.start();
                let mut stream = match provider
                    .chat_stream(
                        &ChatCall::new(messages_with_cache)
                            .maybe_tools(call_tools)
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
                        &env.cancellation_token,
                    )
                    .await
                    {
                        super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                        super::llm_retry::StreamFailureAction::Cancelled => {
                            return Ok(Step::Break(CycleOutcome::Cancelled));
                        }
                        super::llm_retry::StreamFailureAction::Terminal(error) => {
                            return map_failed_llm_call(error, true, env);
                        }
                    },
                };

                // ── Inner consume loop ───────────────────────────────────────
                loop {
                    timing.wait_begin();
                    let item =
                        super::llm_retry::next_stream_chunk(&mut stream, &env.cancellation_token)
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
                            &env.cancellation_token,
                        )
                        .await
                        {
                            super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                            super::llm_retry::StreamFailureAction::Cancelled => {
                                return Ok(Step::Break(CycleOutcome::Cancelled));
                            }
                            super::llm_retry::StreamFailureAction::Terminal(error) => {
                                return Err(contextualize_llm_error(error, "streaming", env));
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
                            &env.cancellation_token,
                        )
                        .await
                        {
                            super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                            super::llm_retry::StreamFailureAction::Cancelled => {
                                return Ok(Step::Break(CycleOutcome::Cancelled));
                            }
                            super::llm_retry::StreamFailureAction::Terminal(error) => {
                                return Err(contextualize_llm_error(error, "streaming", env));
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
                                    drain_trailing_usage(&mut stream, &env.cancellation_token)
                                        .await;
                                if env.cancellation_token.is_cancelled() {
                                    let partial = querymt_agent_services::accounting::merge_usage(
                                        accumulator.output().usage,
                                        drained_usage.take(),
                                    );
                                    attempt
                                        .settle_error(&LLMError::Cancelled, true, partial)
                                        .await
                                        .map_err(accounting_failure)?;
                                    return Ok(Step::Break(CycleOutcome::Cancelled));
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
                                    drain_trailing_usage(&mut stream, &env.cancellation_token)
                                        .await;
                                if env.cancellation_token.is_cancelled() {
                                    let partial = querymt_agent_services::accounting::merge_usage(
                                        accumulator.output().usage,
                                        drained_usage.take(),
                                    );
                                    attempt
                                        .settle_error(&LLMError::Cancelled, true, partial)
                                        .await
                                        .map_err(accounting_failure)?;
                                    return Ok(Step::Break(CycleOutcome::Cancelled));
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
                                        &env.cancellation_token,
                                    )
                                    .await
                                    {
                                        super::llm_retry::StreamFailureAction::Retry => {
                                            continue 'stream;
                                        }
                                        super::llm_retry::StreamFailureAction::Cancelled => {
                                            return Ok(Step::Break(CycleOutcome::Cancelled));
                                        }
                                        super::llm_retry::StreamFailureAction::Terminal(error) => {
                                            return Err(contextualize_llm_error(
                                                error,
                                                "streaming",
                                                env,
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
                            &env.cancellation_token,
                        )
                        .await
                        {
                            super::llm_retry::StreamFailureAction::Retry => continue 'stream,
                            super::llm_retry::StreamFailureAction::Cancelled => {
                                return Ok(Step::Break(CycleOutcome::Cancelled));
                            }
                            super::llm_retry::StreamFailureAction::Terminal(error) => {
                                return Err(contextualize_llm_error(error, "streaming", env));
                            }
                        }
                    }
                }
            }; // end outer retry loop ('stream)

            request_attempts.succeeded();
            timing.finish(&chat_span);
            crate::agent::utils::llm_metrics::stream_latency(
                genai::provider_name(env.provider_name()),
                env.model_name(),
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
            if env.cancellation_token.is_cancelled() {
                return Ok(Step::Break(CycleOutcome::Cancelled));
            }

            streamed_output
        } else {
            // === NON-STREAMING FALLBACK ===
            let cancel = env.cancellation_token.clone();
            match super::llm_retry::call_chat_with_retry(
                config,
                session_id,
                &request_attempts,
                &env.cancellation_token,
                || {
                    let provider = &provider;
                    let messages_with_cache = &messages_with_cache;
                    let cancel = cancel.clone();
                    let operation = &operation;
                    async move {
                        let call = ChatCall::new(messages_with_cache)
                            .maybe_tools(call_tools)
                            .options(options);
                        querymt_agent_services::accounting::run_attempt(
                            operation,
                            session_handle.billing_basis(),
                            &cancel,
                            || async {
                                env.require_turn_authority()
                                    .await
                                    .map_err(|error| LLMError::InvalidRequest(error.to_string()))?;
                                provider.chat(&call).await
                            },
                        )
                        .await
                    }
                },
            )
            .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    if let Some(stop) = budget_stop(&operation) {
                        return Ok(stop);
                    }
                    return map_failed_llm_call(e, false, env);
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
    let cumulative_cost = env.cycle_accounting.session_totals().complete_total_usd();

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
                    turn.stats.steps.saturating_add(1),
                    "stats.steps + 1",
                    Some(session_id),
                ),
                turns: u32_from_usize(turn.stats.turns, "stats.turns", Some(session_id)),
            },
            accounting: Some(operation.request_accounting()),
            prefix_change: prefix.map(|p| p.change),
            idle_secs: prefix.and_then(|p| p.idle_secs),
        },
    );
    // The request completed, so its prefix is now what the provider cached.
    env.runtime
        .cache_prefix
        .lock()
        .commit(std::time::Instant::now());

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
    turn.request_overrides = querymt::chat::GenerationOptions::NONE;
    turn.last_applied = options;

    Ok(Step::Continue(Stage::AfterReply(Box::new(llm_response))))
}

/// Transition from AfterLlm to ProcessingToolCalls or Complete.
///
/// This stores the assistant's response, updates statistics, sends client updates,
/// and determines next state based on finish reason and tool calls.
#[instrument(
    name = "agent.transition.after_llm",
    skip(config, response, env, turn),
    fields(session_id = %env.session_id, has_tool_calls = response.has_tool_calls())
)]
pub(super) async fn transition_after_llm(
    config: &AgentConfig,
    response: &LlmResponse,
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Step, TurnError> {
    debug!(
        "AfterLlm: session={}, has_tool_calls={}",
        env.session_id,
        response.has_tool_calls()
    );

    if env.cancellation_token.is_cancelled() {
        return Ok(Step::Break(CycleOutcome::Cancelled));
    }

    // The pre-turn snapshot is deliberately NOT awaited here: the response is
    // stored and shown immediately. It is resolved right before the first tool
    // call that can change workspace files (see `transition_processing_tool_calls`)
    // and, at the latest, before the post-turn snapshot.

    let progress_description = if response.has_tool_calls() {
        format!(
            "Received response with {} tool call(s)",
            response.tool_calls.len()
        )
    } else {
        "Received response from LLM".to_string()
    };

    let progress_entry = turn
        .state
        .record_progress(
            crate::session::domain::ProgressKind::Note,
            progress_description,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to record progress: {}", e))?;

    config.emit_event(
        &env.session_id,
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
        session_id: env.session_id.clone(),
        role: ChatRole::Assistant,
        parts,
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: Some(env.provider_name().to_string()),
        source_model: Some(env.model_name().to_string()),
    };

    // A thinking-only reply cut off by the output limit is not history: it is
    // neither persisted nor announced, but its usage below is still counted.
    let store_reply = !response.is_thinking_only_length_cutoff();
    if store_reply {
        let stored_id = assistant_msg.id.clone();
        turn.history
            .commit(&env.session_handle, vec![assistant_msg])
            .await
            .map_err(TurnError::history("Failed to store assistant message"))?;

        let reasoning_parts = response
            .output
            .as_ref()
            .map(reasoning_parts_from_output)
            .unwrap_or_default();
        config.emit_event(
            &env.session_id,
            AgentEventKind::AssistantMessageStored {
                content: response.content.clone(),
                thinking: response.thinking.clone(),
                reasoning_parts,
                message_id: Some(stored_id),
            },
        );
    } else {
        debug!(
            "Not storing thinking-only reply cut off by the output limit: session={}",
            env.session_id
        );
    }

    turn.stats.steps += 1;
    apply_usage_to_stats(&mut turn.stats, response.usage.as_ref());
    // Monetary fields are a view of this cycle's recorded components, never a
    // revaluation of token counters at current rates. Applied even when this
    // response reported no usage, so recorded charges stay visible.
    turn.stats
        .apply_recorded_costs(&env.cycle_accounting.cycle_totals());
    turn.last_applied = response.applied;

    let output_status = structured_output.as_ref().and_then(|output| output.status);

    if output_status == Some(ChatOutputStatus::Failed) {
        return Err(anyhow::anyhow!(
            "Provider reported a failed response (provider={}, model={})",
            env.provider_name(),
            env.model_name()
        )
        .into());
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
            return Ok(super::stopping(stop_type, message));
        }

        if response.finish_reason == Some(FinishReason::Length) {
            return store_results_then_stop_on_length(config, &gated_results, env, turn).await;
        }

        // Store the synthesized results (no execution) by routing through the
        // tool-result storage step with no remaining calls.
        return Ok(Step::Continue(Stage::ToolCalls {
            calls: Vec::new(),
            results: gated_results,
        }));
    }

    let has_dispatchable_calls = !executable_calls.is_empty() || !gated_results.is_empty();

    match response.finish_reason {
        Some(FinishReason::ToolCalls) => {
            if has_dispatchable_calls {
                Ok(Step::Continue(Stage::ToolCalls {
                    calls: executable_calls,
                    results: gated_results,
                }))
            } else {
                Ok(Step::Continue(Stage::Ending))
            }
        }

        Some(FinishReason::Stop) => Ok(Step::Continue(Stage::Ending)),

        Some(FinishReason::Length) => {
            // Cut-offs with tool calls were answered and stopped above.
            debug_assert!(!has_dispatchable_calls);
            Ok(super::stopping(
                StopType::ModelTokenLimit,
                "Model hit token limit",
            ))
        }

        Some(FinishReason::ContentFilter) => Ok(super::stopping(
            StopType::ContentFilter,
            "Response blocked by content filter",
        )),

        Some(FinishReason::Error)
        | Some(FinishReason::Unknown)
        | Some(FinishReason::Other)
        | None => {
            if has_dispatchable_calls {
                Ok(Step::Continue(Stage::ToolCalls {
                    calls: executable_calls,
                    results: gated_results,
                }))
            } else {
                Ok(Step::Continue(Stage::Ending))
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
    skip(config, stack, remaining_calls, results, env, turn, steering),
    fields(
        session_id = %env.session_id,
        remaining_calls = remaining_calls.len(),
        completed_results = results.len(),
        wave_count = tracing::field::Empty,
        serialized_calls = tracing::field::Empty,
        state_reload_ms = tracing::field::Empty,
    )
)]
pub(super) async fn transition_processing_tool_calls(
    config: &AgentConfig,
    stack: &mut TurnStack,
    remaining_calls: &[MiddlewareToolCall],
    results: &[ToolResult],
    env: &TurnEnv,
    turn: &mut Turn,
    steering: super::DrainedSteering,
) -> Result<Step, TurnError> {
    debug!(
        "ProcessingToolCalls: session={}, remaining={}, completed={}",
        env.session_id,
        remaining_calls.len(),
        results.len()
    );

    // If already cancelled before we even start, we still need to store synthetic
    // results for every pending call to keep history consistent.
    let already_cancelled = env.cancellation_token.is_cancelled();

    if remaining_calls.is_empty() {
        let session_id = env.session_id.clone();
        let next_state =
            super::tool_calls::store_all_tool_results(config, results, env, turn, steering)
                .instrument(info_span!(
                    "agent.tools.store_results",
                    session_id = %session_id,
                    result_count = results.len()
                ))
                .await?;

        if already_cancelled {
            return Ok(Step::Break(CycleOutcome::Cancelled));
        }
        return Ok(Step::Continue(next_state));
    }

    // Calls are prepared (normalised, hooked, validated, access-declared) one
    // execution-barrier-delimited segment at a time, in model order. A segment is
    // a run of `ParallelSafe` calls or a single barrier call; nothing beyond a
    // stateful or clarification barrier is prepared until the barrier has
    // completed, so later hooks observe its effects.
    let classes: Vec<crate::tools::ToolExecutionClass> = remaining_calls
        .iter()
        .map(|call| env.tool_set().execution_class(&call.function.name))
        .collect();
    let segments = split_segments(&classes);
    debug!(
        "Executing {} tool calls in {} segment(s) for session {}",
        remaining_calls.len(),
        segments.len(),
        env.session_id
    );

    // Middleware reviewing the batch may queue notices on the run; they are
    // stored with the next request, after the tool results, and go with the
    // run when it is cancelled.

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
        if env.cancellation_token.is_cancelled() {
            cancelled = true;
            break;
        }

        let indices: Vec<usize> = match segment {
            Segment::Parallel(indices) => indices.clone(),
            Segment::Barrier(index) => vec![*index],
        };

        // The pre-turn snapshot must exist before anything can modify the
        // workspace. Read-only segments never wait for it, unless hooks are
        // active: a command hook runs in the workspace and can modify it
        // whatever the tool declares, and `prepare_segment` runs them.
        if (config.hooks.is_enabled()
            || indices.iter().any(|&index| {
                !super::tool_calls::call_is_read_only(env, &remaining_calls[index].function.name)
            }))
            && let Err(e) = ensure_pre_turn_snapshot_ready(env, turn, "before_mutating_tool").await
        {
            warn!(
                "Failed to resolve pre-turn snapshot before tool execution: {}",
                e
            );
        }
        let segment_plan =
            prepare_segment(config, stack, turn, remaining_calls, &indices, env).await?;
        // Preparation may resolve a call to a different target than the
        // pre-check saw (e.g. the MCP tool list reloaded); re-check the
        // prepared targets. A no-op when the snapshot is already resolved.
        if segment_plan
            .ready
            .iter()
            .any(|(_, call, _)| !call.target.is_read_only())
            && let Err(e) = ensure_pre_turn_snapshot_ready(env, turn, "before_mutating_tool").await
        {
            warn!(
                "Failed to resolve pre-turn snapshot before tool execution: {}",
                e
            );
        }
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
                    if env.cancellation_token.is_cancelled() {
                        cancelled = true;
                        break;
                    }
                    // Reborrow immutably so the run can be shared across the
                    // wave; a tool call only reads it.
                    let turn_ref: &Turn = turn;
                    let futures = wave.iter().map(|&position| {
                        let index = ready_indices[position];
                        let call = &remaining_calls[index];
                        let (prepared_call, _) =
                            prepared[index].take().expect("prepared call executes once");
                        async move {
                            (
                                index,
                                run_prepared_tool_call(config, call, prepared_call, env, turn_ref)
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
                        run_prepared_tool_call(config, call, prepared_call, env, turn).await;
                    if class == crate::tools::ToolExecutionClass::ClarificationBoundary
                        && !result.is_error
                    {
                        clarification_applied =
                            apply_clarification_answer(turn, call, &result).await;
                    }
                    slots[index] = Some(result);
                    if class == crate::tools::ToolExecutionClass::SerialStateful {
                        // Following calls are prepared against the updated state.
                        state_reload_ms += reload_working_context(turn).await;
                    }
                }
            }
        }

        if env.cancellation_token.is_cancelled() {
            cancelled = true;
        }
    }

    let span = tracing::Span::current();
    span.record("wave_count", wave_count);
    span.record("serialized_calls", serialized_calls);
    span.record("state_reload_ms", state_reload_ms);

    let was_cancelled = cancelled || env.cancellation_token.is_cancelled();

    // Exactly one result per call: calls that never started (cancellation, or
    // a clarification that changed the run objective) get a synthetic result.
    let mut all_results = results.to_vec();
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
        env.session_id,
        was_cancelled,
    );

    // Always store results — even on cancellation — to maintain the
    // tool_use → tool_result history invariant required by LLM APIs.
    let session_id = env.session_id.clone();
    let next_state = super::tool_calls::store_all_tool_results(
        config,
        &all_results,
        env,
        turn,
        crate::agent::execution::DrainedSteering::none(),
    )
    .instrument(info_span!(
        "agent.tools.store_results",
        session_id = %session_id,
        cancelled = was_cancelled,
    ))
    .await?;

    if was_cancelled {
        return Ok(Step::Break(CycleOutcome::Cancelled));
    }

    Ok(Step::Continue(next_state))
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
    stack: &mut TurnStack,
    turn: &mut Turn,
    calls: &[MiddlewareToolCall],
    indices: &[usize],
    env: &TurnEnv,
) -> Result<SegmentPlan, anyhow::Error> {
    let mut immediate: Vec<(usize, ToolResult)> = Vec::new();
    let mut prepared: Vec<(usize, PreparedCall)> = Vec::new();

    for &index in indices {
        if env.cancellation_token.is_cancelled() {
            break;
        }
        let call = &calls[index];
        let outcome = tokio::select! {
            biased;
            _ = env.cancellation_token.cancelled() => break,
            outcome = super::tool_calls::prepare_tool_call(config, call, env) => outcome,
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
    let cwd = env.cwd().map(|path| path.to_path_buf());
    let accesses = env
        .tool_set()
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
        let mut cx = turn.hook_cx(env);
        stack
            .prepared_tool_calls(
                &mut cx,
                &PreparedBatch {
                    session_id: &env.session_id,
                    cwd: cwd.as_deref(),
                    calls: &views,
                },
            )
            .await?
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
async fn reload_working_context(turn: &mut Turn) -> u64 {
    let started = Instant::now();
    match turn.state.load_working_context().await {
        Ok(()) => {
            if let Some(objective) = turn.run_objective.as_mut() {
                objective.set_task(
                    turn.state.active_task.as_ref(),
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
    env: &TurnEnv,
    turn: &Turn,
) -> ToolResult {
    let cancel = env.cancellation_token.clone();
    tokio::select! {
        result = super::tool_calls::execute_prepared_tool_call(config, call, prepared, env, turn) => {
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
    turn: &mut Turn,
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

    let summary = turn
        .state
        .current_intent
        .as_ref()
        .map(|intent| intent.summary.clone())
        .unwrap_or_else(|| clarification.clone());
    match turn
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
            if let Some(objective) = turn.run_objective.as_mut() {
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

/// [`transition_before_llm_call`] over a history that has no notices stored
/// for this request, so the trigger can be read from it.
#[cfg(test)]
pub(super) async fn transition_before_llm_call_in_test(
    config: &AgentConfig,
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Step, TurnError> {
    let trigger = context_hook_trigger(turn.history.as_slice());
    transition_before_llm_call(config, env, turn, trigger, None).await
}

/// [`transition_call_llm`] over the model the fixture's environment holds.
#[cfg(test)]
pub(super) async fn call_llm_in_test(
    config: &AgentConfig,
    request: &PreparedModelRequest,
    env: &TurnEnv,
    turn: &mut Turn,
) -> Result<Step, TurnError> {
    transition_call_llm(config, request, env, turn).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::execution::{CycleOutcome, Stage, Step};
    use crate::middleware::Stop;
    use querymt::chat::CacheHint;

    /// The stored message a provider message stands for: what a test that
    /// arranges its history as chat messages stores.
    fn stored_of(session_id: &str, message: &ChatMessage) -> AgentMessage {
        use querymt::chat::ChatInputPart;
        let mut stored = AgentMessage::new(session_id.to_string(), message.role);
        if message.role == ChatRole::Assistant {
            stored.parts.push(MessagePart::Output {
                output: message.output().expect("assistant output").clone(),
            });
            return stored;
        }
        for part in message.input().unwrap_or_default() {
            if let Some(change) = part.as_tool_change() {
                stored.parts.push(MessagePart::ToolChange {
                    added: change.added.clone(),
                    removed: change.removed.clone(),
                });
            } else if let ChatInputPart::ToolResult(result) = part {
                stored.parts.push(MessagePart::ToolResult {
                    call_id: result.call_id.clone(),
                    content: result.parts.clone(),
                    is_error: result.is_error,
                    tool_name: result.name.clone(),
                    tool_arguments: None,
                    compacted_at: None,
                });
            } else if let Some(text) = part.as_text() {
                stored.parts.push(MessagePart::Text {
                    content: text.to_string(),
                });
            } else {
                panic!("a test history holds text, tool results and tool changes only");
            }
        }
        stored
    }

    /// The run's history as `messages` (not stored: a request is prepared
    /// from what the run holds).
    fn hold(turn: &mut Turn, session_id: &str, messages: &[ChatMessage]) {
        turn.history.replace(
            messages
                .iter()
                .map(|message| stored_of(session_id, message))
                .collect(),
        );
    }

    /// The hints the request projection puts on `messages`, as a new vector.
    fn apply_cache_breakpoints(
        messages: &[ChatMessage],
        ttl_seconds: Option<u64>,
    ) -> Vec<ChatMessage> {
        let mut hinted = messages.to_vec();
        crate::agent::execution::cache_hints::apply_cache_breakpoints(&mut hinted, ttl_seconds);
        hinted
    }

    /// The pre-turn snapshot is awaited only before a call that can change the
    /// workspace; read-only calls and text replies never wait for it.
    mod snapshot_wait_tests {
        use super::super::*;
        use crate::agent::execution::turn::PreTurnSnapshot;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
        use std::time::Duration;

        fn turn_id() -> querymt_agent_core::model::TurnId {
            querymt_agent_core::model::TurnId::from(uuid::Uuid::from_u128(0x1111))
        }

        async fn pending_snapshot_exec(agent: &TestAgent) -> (TurnEnv, Turn) {
            let (exec, mut turn) = agent.turn().await;
            turn.snapshot = PreTurnSnapshot::Pending(tokio::spawn(async {
                (turn_id(), Ok("snapshot-1".into()))
            }));
            (exec, turn)
        }

        async fn agent() -> TestAgent {
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await
        }

        #[tokio::test]
        async fn read_only_tool_call_leaves_the_snapshot_pending() {
            let agent = agent().await;
            let (exec, mut turn) = pending_snapshot_exec(&agent).await;
            let calls: Arc<[MiddlewareToolCall]> = Arc::from(vec![mock_tool_call(
                "c1",
                "read_tool",
                r#"{"path":"/definitely/not/here.txt"}"#,
            )]);
            let results: Arc<[ToolResult]> = Arc::from(Vec::new());
            let mut stack = TurnStack::default();

            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                transition_processing_tool_calls(
                    &agent.config,
                    &mut stack,
                    &calls,
                    &results,
                    &exec,
                    &mut turn,
                    crate::agent::execution::DrainedSteering::none(),
                ),
            )
            .await
            .expect("read-only call must complete without waiting for the snapshot")
            .expect("transition succeeds");

            assert!(
                matches!(turn.snapshot, PreTurnSnapshot::Pending(_)),
                "a read-only call must not wait for the pre-turn snapshot"
            );
        }

        #[tokio::test]
        async fn mutating_tool_call_resolves_the_snapshot_first() {
            let agent = agent().await;
            let (exec, mut turn) = pending_snapshot_exec(&agent).await;
            let calls: Arc<[MiddlewareToolCall]> = Arc::from(vec![mock_tool_call(
                "c1",
                "write_file",
                r#"{"path":"a.txt","content":"x"}"#,
            )]);
            let results: Arc<[ToolResult]> = Arc::from(Vec::new());
            let mut stack = TurnStack::default();

            // The call itself may be blocked or fail (no workspace, approval);
            // only the ordering matters: the snapshot is resolved before it.
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                transition_processing_tool_calls(
                    &agent.config,
                    &mut stack,
                    &calls,
                    &results,
                    &exec,
                    &mut turn,
                    crate::agent::execution::DrainedSteering::none(),
                ),
            )
            .await;

            let PreTurnSnapshot::Ready(snapshot) = &turn.snapshot else {
                panic!("the snapshot must be resolved before the call");
            };
            assert_eq!(
                *snapshot,
                crate::agent::core::TurnSnapshot {
                    turn_id: turn_id(),
                    snapshot_id: "snapshot-1".into(),
                }
            );
        }

        #[tokio::test]
        async fn text_only_reply_does_not_wait_for_the_snapshot() {
            let agent = agent().await;
            let (exec, mut turn) = pending_snapshot_exec(&agent).await;
            // A task that never finishes would hang the turn if it were awaited.
            turn.snapshot = PreTurnSnapshot::Pending(tokio::spawn(async {
                std::future::pending::<()>().await;
                (turn_id(), Ok("".into()))
            }));
            let response = Arc::new(LlmResponse::new(
                "hello".into(),
                Vec::new(),
                None,
                Some(FinishReason::Stop),
            ));

            let state = tokio::time::timeout(
                Duration::from_secs(10),
                transition_after_llm(&agent.config, &response, &exec, &mut turn),
            )
            .await
            .expect("after_llm must not block on the pending snapshot")
            .expect("after llm");

            assert!(!matches!(state, Step::Continue(Stage::ToolCalls { .. })));
            let PreTurnSnapshot::Pending(pending) = std::mem::take(&mut turn.snapshot) else {
                panic!("snapshot task still pending");
            };
            pending.abort();
        }

        #[tokio::test]
        async fn read_only_check_matches_snapshot_preparation() {
            let agent = agent().await;
            let (exec, _turn) = agent.turn().await;
            use super::super::super::tool_calls::call_is_read_only;
            assert!(call_is_read_only(&exec, "read_tool"));
            assert!(!call_is_read_only(&exec, "write_file"));
        }
    }

    mod mcp_cancel_tests {
        use super::*;
        use crate::test_utils::mcp_harness::{HarnessOptions, McpHarness, TaskScript};
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
        use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
        use serde_json::json;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        /// Run `tool` through the real cancel race, cancel once the server is
        /// busy, and return the result plus every event published afterwards.
        async fn cancel_run(
            harness: &McpHarness,
            tool: &str,
            args: serde_json::Value,
        ) -> (ToolResult, Vec<EventEnvelope>, Vec<EventEnvelope>) {
            let agent = TestAgent::with_mock_provider(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))
            .await;
            let token = CancellationToken::new();
            let (mut exec, turn) = agent.turn_with_mcp(harness.tool_state.clone()).await;
            exec.cancellation_token = token.clone();
            let mut events = agent.config.event_sink.fanout().subscribe();

            let call = mock_tool_call("c1", tool, &args.to_string());
            let prepared =
                crate::agent::execution::tool_calls::prepare_tool_call(&agent.config, &call, &exec)
                    .await
                    .unwrap();

            let mut before_cancel = Vec::new();
            let canceller = async {
                tokio::time::timeout(Duration::from_secs(5), harness.log.started.notified())
                    .await
                    .expect("server started the call");
                // Let the task id / progress reach the client before cancelling.
                tokio::time::sleep(Duration::from_millis(150)).await;
                // Drain with no await between this and the cancel, so
                // everything left in `events` was published afterwards.
                while let Ok(e) = events.try_recv() {
                    before_cancel.push(std::sync::Arc::unwrap_or_clone(e));
                }
                token.cancel();
            };
            let (result, ()) = tokio::join!(
                run_prepared_tool_call(&agent.config, &call, prepared, &exec, &turn),
                canceller
            );

            // A cancelled call is dropped and ends with one cancellation
            // ToolCallEnd; no progress for it is published after the cancel.
            let mut after_cancel = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
            while let Ok(Ok(e)) = tokio::time::timeout_at(deadline, events.recv()).await {
                after_cancel.push(std::sync::Arc::unwrap_or_clone(e));
            }
            (result, before_cancel, after_cancel)
        }

        fn assert_cancelled_once(result: &ToolResult) {
            assert!(result.is_error);
            let text: String = result.content.iter().filter_map(|b| b.as_text()).collect();
            assert!(text.contains("Cancelled by user"), "{text}");
        }

        fn no_progress(after_cancel: &[EventEnvelope]) {
            assert!(
                !after_cancel
                    .iter()
                    .any(|e| matches!(e.kind(), AgentEventKind::ToolCallProgress { .. })),
                "no ToolCallProgress after the run was cancelled"
            );
        }

        #[tokio::test]
        async fn mcp_cancel_sends_notifications_cancelled_and_one_error_result() {
            let harness = McpHarness::start(HarnessOptions::default()).await;
            // `progress` with a long hold emits a burst, then blocks.
            let (result, _before, after) = cancel_run(
                &harness,
                "progress",
                json!({"count": 10, "hold_ms": 30_000}),
            )
            .await;

            assert_cancelled_once(&result);
            tokio::time::timeout(Duration::from_secs(2), async {
                while harness.log.cancelled.lock().is_empty() {
                    harness.log.cancel_seen.notified().await;
                }
            })
            .await
            .expect("server received notifications/cancelled");
            assert!(
                harness.request_id_of_first_cancel().is_some(),
                "cancellation names the request id"
            );
            no_progress(&after);
        }

        #[tokio::test]
        async fn mcp_cancel_during_a_task_sends_tasks_cancel() {
            let harness = McpHarness::start(HarnessOptions {
                tasks: true,
                ..HarnessOptions::default()
            })
            .await;
            harness.set_task_script(TaskScript::default()); // working forever
            let (result, _before, after) = cancel_run(&harness, "task", json!({})).await;

            assert_cancelled_once(&result);
            tokio::time::timeout(Duration::from_secs(2), async {
                while harness.log.task_cancels.lock().is_empty() {
                    harness.log.cancel_seen.notified().await;
                }
            })
            .await
            .expect("server received tasks/cancel");
            assert_eq!(*harness.log.task_cancels.lock(), ["t1"]);
            no_progress(&after);
        }
    }

    /// Every started `shell` call gets exactly one end event, however the run
    /// is cancelled.
    #[cfg(unix)]
    mod shell_cancel_tests {
        use super::*;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
        use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        /// Cancel the batch of `(tool, arguments)` calls, with ids `c1`, `c2`, ...,
        /// once a command has created `{marker}`, and return every event
        /// published for the batch.
        async fn cancel_batch(batch: &[(&str, serde_json::Value)]) -> Vec<EventEnvelope> {
            let agent = TestAgent::with_mock_provider(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))
            .await;
            let dir = tempfile::TempDir::new().unwrap();
            let token = CancellationToken::new();
            let (mut exec, mut turn) = agent.turn_in(dir.path().to_path_buf()).await;
            exec.cancellation_token = token.clone();
            let mut events = agent.config.event_sink.fanout().subscribe();
            let pid_file = dir.path().join("running");
            let calls: Arc<[MiddlewareToolCall]> = batch
                .iter()
                .enumerate()
                .map(|(index, (tool, args))| {
                    let args = args
                        .to_string()
                        .replace("{marker}", &pid_file.display().to_string());
                    mock_tool_call(&format!("c{}", index + 1), tool, &args)
                })
                .collect();
            let results: Arc<[ToolResult]> = Arc::from(Vec::new());
            let mut stack = TurnStack::default();

            let canceller = async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !pid_file.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("first command started");
                token.cancel();
            };
            let (state, ()) = tokio::join!(
                tokio::time::timeout(
                    Duration::from_secs(10),
                    transition_processing_tool_calls(
                        &agent.config,
                        &mut stack,
                        &calls,
                        &results,
                        &exec,
                        &mut turn,
                        crate::agent::execution::DrainedSteering::none(),
                    ),
                ),
                canceller
            );
            let state = state.expect("cancelled batch returns").expect("transition");
            assert!(
                matches!(state, Step::Break(CycleOutcome::Cancelled)),
                "{state:?}"
            );

            let mut published = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
            while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
                published.push(std::sync::Arc::unwrap_or_clone(event));
            }
            published
        }

        fn starts(events: &[EventEnvelope], id: &str) -> usize {
            events
                .iter()
                .filter(|e| matches!(e.kind(), AgentEventKind::ToolCallStart { tool_call_id, .. } if tool_call_id == id))
                .count()
        }

        fn ends<'a>(events: &'a [EventEnvelope], id: &str) -> Vec<(&'a bool, &'a String)> {
            events
                .iter()
                .filter_map(|e| match e.kind() {
                    AgentEventKind::ToolCallEnd {
                        tool_call_id,
                        is_error,
                        result,
                        ..
                    } if tool_call_id == id => Some((is_error, result)),
                    _ => None,
                })
                .collect()
        }

        /// The cancel signal races the tool's own cancellation, so the
        /// batch is repeated to hit both outcomes.
        #[tokio::test]
        async fn cancelled_while_running_has_one_end_event() {
            for attempt in 0..10 {
                let events = cancel_batch(&[sleeping_shell()]).await;

                assert_eq!(starts(&events, "c1"), 1, "attempt {attempt}");
                let ends = ends(&events, "c1");
                assert_eq!(ends.len(), 1, "attempt {attempt}: {ends:?}");
                let (is_error, result) = ends[0];
                assert!(*is_error, "attempt {attempt}");
                assert!(
                    result.contains("Cancelled by user"),
                    "attempt {attempt}: {result}"
                );
            }
        }

        fn sleeping_shell() -> (&'static str, serde_json::Value) {
            (
                "shell",
                serde_json::json!({"command": "touch {marker}; exec sleep 600"}),
            )
        }

        /// `create_task` is an execution barrier, so the last call waits in a
        /// later segment while the first one runs.
        #[tokio::test]
        async fn call_cancelled_before_its_command_started_has_no_events() {
            let events = cancel_batch(&[
                sleeping_shell(),
                ("create_task", serde_json::json!({"title": "t"})),
                ("shell", serde_json::json!({"command": "echo never-runs"})),
            ])
            .await;

            assert_eq!(starts(&events, "c1"), 1);
            assert_eq!(ends(&events, "c1").len(), 1);
            for waiting in ["c2", "c3"] {
                assert_eq!(starts(&events, waiting), 0, "{waiting} never started");
                assert!(ends(&events, waiting).is_empty(), "{waiting}");
            }
        }
    }

    mod genai_trace_tests {
        use super::*;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use opentelemetry::{
            Array, Value,
            trace::{SpanKind, Status},
        };
        use querymt::chat::ChatOutputProvenance;
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn request() -> Box<PreparedModelRequest> {
            Box::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("SECRET_PROMPT").build()].into(),
                tools: Arc::from([]),
                offered_tools: Arc::from([]),
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
            use querymt_agent_services::tools::trace_support::genai_trace::capture_info;

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
                let (exec, mut turn) = fixture.turn().await;
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
                    exec.session_handle.add_message(boundary).await.unwrap();
                    if mode != "request_only" {
                        exec.session_handle.add_message(summary).await.unwrap();
                    }
                }
                if mode == "orphan_request" {
                    let (orphan, _) = SessionCompaction::create_compaction_messages(
                        &exec.session_id,
                        "unused",
                        100,
                    );
                    exec.session_handle.add_message(orphan).await.unwrap();
                }
                turn.history.replace(
                    exec.session_handle
                        .get_effective_agent_history()
                        .await
                        .unwrap(),
                );
                let mut messages = querymt_agent_core::projection::project_request(
                    turn.history.as_slice(),
                    &querymt_agent_core::projection::RequestTarget {
                        output: None,
                        max_prompt_bytes: None,
                        tool_changes: false,
                    },
                )
                .unwrap()
                .messages;
                if mode == "reload_removed" {
                    assert!(
                        turn.history
                            .as_slice()
                            .iter()
                            .flat_map(|message| &message.parts)
                            .any(|part| matches!(part, MessagePart::Compaction { .. }))
                    );
                    fixture
                        .config
                        .provider
                        .history_store()
                        .delete_messages_after(&exec.session_id, &boundary_id)
                        .await
                        .unwrap();
                    turn.history.replace(
                        exec.session_handle
                            .get_effective_agent_history()
                            .await
                            .unwrap(),
                    );
                }
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
                let exec = exec.tools_from(&config);
                match mode {
                    "cached" => messages = apply_cache_breakpoints(&messages, None),
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
                        let Step::Continue(Stage::CallModel(request)) =
                            transition_before_llm_call_in_test(&config, &exec, &mut turn)
                                .await
                                .unwrap()
                        else {
                            panic!("expected prepared request");
                        };
                        request
                    } else {
                        Box::new(PreparedModelRequest {
                            messages: Arc::from(messages),
                            tools: Arc::from([]),
                            offered_tools: Arc::from([]),
                            estimated_tokens: 1,
                            options: Default::default(),
                        })
                    };
                    call_llm_in_test(&config, &request, &exec, &mut turn).await
                })
                .await;
                assert!(
                    matches!(result.unwrap(), Step::Continue(Stage::AfterReply(_))),
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
            /// Whether the provider declares `tool_updates`.
            tool_updates: Arc<std::sync::atomic::AtomicBool>,
        }

        #[async_trait::async_trait]
        impl querymt::chat::ChatProvider for TimedProvider {
            fn capabilities(&self) -> querymt::chat::ChatCapabilities {
                querymt::chat::ChatCapabilities::NONE
                    .with_streaming(self.streaming)
                    .with_options(*self.support.lock().unwrap())
                    .with_tool_updates(self.tool_updates.load(std::sync::atomic::Ordering::Relaxed))
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
                    tool_updates: Default::default(),
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
            config.execution_policy.rate_limit.transport_wait_secs = 0;
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
            Step,
            Vec<querymt_agent_core::events::AgentEventKind>,
            Arc<TimedFactory>,
            Vec<ChatMessage>,
        ) {
            let attempts = vec![Ok(chunks.into_iter().map(|chunk| (0, Ok(chunk))).collect())];
            let (fixture, factory) = timed_fixture(true, attempts, "openai", None).await;
            let mut events = fixture.config.subscribe_events();
            let (exec, mut turn) = fixture.turn().await;

            // Tools must be present: a tool-less request takes the non-streaming path.
            let mut prepared = (*request()).clone();
            prepared.tools = vec![serde_json::from_value(serde_json::json!({
                "type": "function",
                "function": {"name": "write_file", "description": "", "parameters": {"type": "object"}}
            }))
            .unwrap()]
            .into();
            let state = call_llm_in_test(&fixture.config, &Arc::new(prepared), &exec, &mut turn)
                .await
                .expect("call llm");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
            };
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .expect("after llm");

            let mut seen = Vec::new();
            while let Ok(Ok(envelope)) =
                tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            {
                seen.push(envelope.kind().clone());
            }
            // The run's history as the model that produced it would be sent it:
            // an assistant reply keeps its native output.
            let held = turn
                .history
                .as_slice()
                .iter()
                .map(|message| match message.parts.first() {
                    Some(MessagePart::Output { output }) => {
                        ChatMessage::from_assistant_output(output.clone())
                    }
                    _ => message.to_chat_message().unwrap(),
                })
                .collect();
            (state, seen, factory, held)
        }

        #[tokio::test]
        async fn length_cutoff_inside_tool_call_skips_the_call_and_stops_without_retry() {
            // One call completes, a second is cut off mid-arguments. The
            // incomplete reply must not execute the completed call either.
            let (state, events, factory, held) = run_streamed_reply(vec![
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
                !events.iter().any(|kind| matches!(
                    kind,
                    querymt_agent_core::events::AgentEventKind::LlmRetryWait { .. }
                )),
                "a length cut-off must not be retried as a transient stream failure"
            );
            assert!(
                factory.provider.attempts.lock().unwrap().is_empty(),
                "exactly one physical attempt is expected"
            );

            let Step::Continue(Stage::Stopping(Stop { stop_type, .. })) = state else {
                panic!("expected Stopped, got {state:?}");
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);

            let result_text = held
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
            state: Step,
            messages: Vec<querymt::chat::ChatMessage>,
        ) -> (Vec<querymt::chat::ChatMessage>, HashMap<String, String>) {
            let Step::Continue(Stage::Stopping(Stop { stop_type, .. })) = state else {
                panic!("expected Stopped, got {state:?}");
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
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
            let (state, _, _, held) = run_streamed_reply(vec![
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

            let (messages, results) = stopped_results(state, held);
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
            let (state, _, factory, held) = run_streamed_reply(vec![
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
            let (_, results) = stopped_results(state, held);
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

        fn has_retry_wait(events: &[querymt_agent_core::events::AgentEventKind]) -> bool {
            events.iter().any(|kind| {
                matches!(
                    kind,
                    querymt_agent_core::events::AgentEventKind::LlmRetryWait { .. }
                )
            })
        }

        /// Session 01a18903: an opaque server-tool block alongside local calls
        /// used to fail every turn with unfinished tool call indexes and burn
        /// retries. Both local calls dispatch once; the opaque block never does.
        #[tokio::test]
        async fn anthropic_structured_stream_dispatches_local_calls_once_and_stores_history() {
            let (state, events, factory, held) =
                run_streamed_reply(anthropic_fixture("structured_tool_turn")).await;

            assert!(!has_retry_wait(&events), "no llm_retry_wait: {events:?}");
            assert!(factory.provider.attempts.lock().unwrap().is_empty());
            let Step::Continue(Stage::ToolCalls {
                calls: remaining_calls,
                ..
            }) = state
            else {
                panic!("expected ToolCalls, got {state:?}");
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
            let assistant = held
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
            let (_, events, _, _) =
                run_streamed_reply(anthropic_fixture("structured_tool_turn")).await;

            let text: String = events
                .iter()
                .filter_map(|kind| match kind {
                    querymt_agent_core::events::AgentEventKind::AssistantContentDelta {
                        content,
                        ..
                    } => Some(content.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "Checking both files.");

            let thinking: Vec<(&str, Option<&str>)> = events
                .iter()
                .filter_map(|kind| match kind {
                    querymt_agent_core::events::AgentEventKind::AssistantThinkingDelta {
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
            let (state, events, factory, held) =
                run_streamed_reply(anthropic_fixture("structured_max_tokens")).await;

            assert!(!has_retry_wait(&events), "a length cut-off must not retry");
            assert!(factory.provider.attempts.lock().unwrap().is_empty());
            // `stopped_results` also asserts the stop type is the model token limit.
            let (_, results) = stopped_results(state, held);
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
            caps: Option<querymt_agent_services::model_heuristics::OutputCaps>,
        ) -> Arc<crate::middleware::ModelProfile> {
            Arc::new(crate::middleware::ModelProfile {
                caps,
                capabilities: querymt::chat::ChatCapabilities::NONE
                    .with_streaming(true)
                    .with_options(support),
                ..Default::default()
            })
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
            let (exec, mut turn) = fixture.turn().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            turn.request_overrides = overrides;

            let state = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                .await
                .expect("before llm call");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            assert_eq!(request.options, overrides);
            assert_eq!(
                exec.model.profile.capabilities,
                exec.model.provider.capabilities(),
                "the profile holds what the resolved provider declares"
            );
        }

        fn mcp_tool(name: &str, description: &str) -> querymt::chat::Tool {
            querymt::chat::Tool {
                tool_type: "function".into(),
                function: querymt::chat::FunctionTool {
                    name: name.into(),
                    description: description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: None,
                },
            }
        }

        async fn declared_run(
            tool_updates: bool,
        ) -> (
            Turn,
            Box<PreparedModelRequest>,
            Box<PreparedModelRequest>,
            TestAgent,
            TurnEnv,
            &'static str,
        ) {
            let (fixture, factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            factory
                .provider
                .tool_updates
                .store(tool_updates, std::sync::atomic::Ordering::Relaxed);
            let state = crate::agent::core::McpToolState::new(
                Default::default(),
                vec![mcp_tool("alpha", "A")],
            );
            let (exec, mut turn) = fixture.turn_with_mcp(state).await;
            hold(
                &mut turn,
                &exec.session_id,
                &[ChatMessage::user().text("go").build()],
            );
            let Step::Continue(Stage::CallModel(request)) =
                transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                    .await
                    .expect("first request")
            else {
                panic!("expected CallLlm");
            };
            // The first request completed, so it is the reference for the next.
            exec.runtime
                .cache_prefix
                .lock()
                .commit(std::time::Instant::now());
            // An MCP server adds a tool between the two requests.
            exec.runtime.mcp_tool_state.rcu(|snapshot| {
                let mut next = snapshot.clone();
                next.tool_defs.push(mcp_tool("beta", "B"));
                next
            });
            let Step::Continue(Stage::CallModel(second)) =
                transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                    .await
                    .expect("second request")
            else {
                panic!("expected CallLlm");
            };
            let prefix_change = exec
                .runtime
                .cache_prefix
                .lock()
                .pending_attribution()
                .expect("the second request is attributed")
                .change
                .as_str();
            (turn, request, second, fixture, exec, prefix_change)
        }

        #[tokio::test]
        async fn mcp_tool_added_mid_session_is_sent_as_a_change_when_the_provider_declares_it() {
            let (turn, first, second, _fixture, _exec, prefix_change) = declared_run(true).await;
            assert_eq!(prefix_change, "none", "the request keeps the cached prefix");

            let names = |request: &PreparedModelRequest| -> Vec<String> {
                request
                    .tools
                    .iter()
                    .map(|t| t.function.name.clone())
                    .collect()
            };
            assert!(names(&first).contains(&"alpha".to_string()));
            assert_eq!(
                names(&second),
                names(&first),
                "the tool list does not change"
            );
            let last = second.messages.last().expect("messages");
            let change = last.input().unwrap()[0]
                .as_tool_change()
                .expect("a tool change");
            assert_eq!(change.added.len(), 1);
            assert_eq!(change.added[0].function.name, "beta");
            assert_eq!(
                second
                    .messages
                    .iter()
                    .filter(|m| m.has_tool_change())
                    .count(),
                1,
                "the change is the only tool record sent: a baseline is not a message"
            );
            // The model can call the added tool although the list does not
            // name it.
            assert!(
                second
                    .offered_tools
                    .iter()
                    .any(|t| t.function.name == "beta")
            );
            assert_eq!(second.offered_tools.len(), second.tools.len() + 1);
            assert_eq!(first.offered_tools, first.tools);
            assert!(
                turn.history
                    .as_slice()
                    .iter()
                    .flat_map(|message| &message.parts)
                    .any(|part| matches!(part, MessagePart::ToolChange { .. }))
            );
            assert!(turn.history.tool_baseline().is_some());
        }

        #[tokio::test]
        async fn mcp_tool_added_mid_session_changes_the_list_without_tool_updates() {
            let (turn, first, second, fixture, exec, prefix_change) = declared_run(false).await;
            assert_eq!(prefix_change, "tools", "the list change rewrites the cache");

            assert!(
                second.tools.iter().any(|t| t.function.name == "beta"),
                "the current list carries the new tool"
            );
            assert_eq!(second.tools.len(), first.tools.len() + 1);
            assert_eq!(second.offered_tools, second.tools);
            assert!(!second.messages.iter().any(ChatMessage::has_tool_change));
            assert!(turn.history.tool_baseline().is_none());
            let stored = fixture
                .config
                .provider
                .history_store()
                .get_history(&exec.session_id)
                .await
                .unwrap();
            assert!(stored.iter().flat_map(|m| &m.parts).all(|part| !matches!(
                part,
                MessagePart::ToolBaseline { .. } | MessagePart::ToolChange { .. }
            )));
        }

        const OBJECTIVE: &str = "[Run objective revision 1]\nOBJECTIVE";

        /// Prepare one request for `messages`, as the execution loop does before
        /// every model request. `objective` is `Some` when `run_objective = true`.
        async fn prepared(
            fixture: &TestAgent,
            exec: &TurnEnv,
            turn: &mut Turn,
            messages: Vec<ChatMessage>,
            objective: Option<&str>,
        ) -> Vec<ChatMessage> {
            prepared_in(&fixture.config, exec, turn, messages, objective).await
        }

        async fn prepared_in(
            config: &AgentConfig,
            exec: &TurnEnv,
            turn: &mut Turn,
            messages: Vec<ChatMessage>,
            objective: Option<&str>,
        ) -> Vec<ChatMessage> {
            hold(turn, &exec.session_id, &messages);
            let trigger = context_hook_trigger(turn.history.as_slice());
            let state = transition_before_llm_call(config, exec, turn, trigger, objective)
                .await
                .expect("before llm call");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            request.messages.to_vec()
        }

        /// A config whose only context hook prints `additional_context`, naming
        /// whether the objective text reached the hook's input.
        fn config_with_context_hook(config: &AgentConfig) -> AgentConfig {
            let command = r#"if grep -q OBJECTIVE; then t='saw objective'; else t='durable only'; fi; printf '{"hook_specific_output":{"hook_event_name":"context","additional_context":"%s"}}' "$t""#;
            let hooks = crate::hooks::Hooks::new(
                serde_json::from_value::<crate::hooks::HooksConfig>(serde_json::json!({
                    "enabled": true,
                    "context": [{"hooks": [{
                        "type": "command", "command": command, "timeout_sec": 5
                    }]}]
                }))
                .unwrap(),
            )
            .unwrap();
            let mut config = config.clone();
            config.hooks = hooks;
            config
        }

        fn hinted(message: &ChatMessage) -> bool {
            message.cache.is_some()
        }

        /// Assert that `next` begins with every durable message of `previous`,
        /// compared without cache hints: the prefix a provider cache written by
        /// `previous` could serve to `next`. `previous_tail` says whether the
        /// last message of `previous` is request-scoped context.
        fn assert_durable_prefix(
            previous: &[ChatMessage],
            previous_tail: bool,
            next: &[ChatMessage],
        ) {
            let strip = |messages: &[ChatMessage]| -> Vec<serde_json::Value> {
                messages
                    .iter()
                    .map(|message| {
                        let mut message = message.clone();
                        message.cache = None;
                        serde_json::to_value(message).unwrap()
                    })
                    .collect()
            };
            let durable_len = previous.len() - usize::from(previous_tail);
            let (previous, next) = (strip(&previous[..durable_len]), strip(next));
            assert!(next.len() >= previous.len(), "next request is shorter");
            for (i, (before, after)) in previous.iter().zip(&next).enumerate() {
                assert_eq!(
                    before, after,
                    "durable prefix differs at message {i} of {durable_len}"
                );
            }
        }

        fn tool_step(call_id: &str) -> [ChatMessage; 2] {
            [
                ChatMessage::assistant()
                    .tool_use(call_id, "read_tool", serde_json::json!({"path": call_id}))
                    .build(),
                ChatMessage::user()
                    .part(querymt::chat::ChatInputPart::tool_result(
                        querymt::chat::ToolResult::text(call_id, format!("contents of {call_id}")),
                    ))
                    .build(),
            ]
        }

        /// First request of a turn, then one tool step.
        async fn prompt_to_first_tool_step(objective: Option<&str>) {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let prompt = ChatMessage::user().text("fix the build").build();

            let first = prepared(&fixture, &exec, &mut turn, vec![prompt.clone()], objective).await;
            let mut history = vec![prompt];
            history.extend(tool_step("call-1"));
            let second = prepared(&fixture, &exec, &mut turn, history, objective).await;

            assert_durable_prefix(&first, objective.is_some(), &second);
        }

        /// Two consecutive tool steps of one turn.
        async fn across_a_tool_step(objective: Option<&str>) {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));

            let first = prepared(&fixture, &exec, &mut turn, history.clone(), objective).await;
            history.extend(tool_step("call-2"));
            let second = prepared(&fixture, &exec, &mut turn, history, objective).await;

            assert_durable_prefix(&first, objective.is_some(), &second);
        }

        #[tokio::test]
        async fn durable_prefix_is_stable_from_prompt_to_first_tool_step() {
            prompt_to_first_tool_step(None).await;
        }

        #[tokio::test]
        async fn durable_prefix_is_stable_across_a_tool_step() {
            across_a_tool_step(None).await;
        }

        /// "Prefix is stable with the run objective enabled".
        #[tokio::test]
        async fn durable_prefix_is_stable_with_the_run_objective_enabled() {
            prompt_to_first_tool_step(Some(OBJECTIVE)).await;
            across_a_tool_step(Some(OBJECTIVE)).await;
        }

        #[tokio::test]
        async fn durable_prefix_is_stable_across_turns() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));

            let last_of_turn_one =
                prepared(&fixture, &exec, &mut turn, history.clone(), None).await;
            history.push(ChatMessage::assistant().text("done").build());
            history.push(ChatMessage::user().text("now the tests").build());
            let first_of_turn_two = prepared(&fixture, &exec, &mut turn, history, None).await;

            assert_durable_prefix(&last_of_turn_one, false, &first_of_turn_two);
        }

        #[tokio::test]
        async fn durable_prefix_is_stable_across_a_mode_switch() {
            use querymt_agent_core::model::NoticeSource;
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));

            let before = prepared(&fixture, &exec, &mut turn, history.clone(), None).await;
            // Build -> plan between requests: the reminder is stored as a notice,
            // appended after the tool results.
            hold(&mut turn, &exec.session_id, &history);
            turn.queue_notice(NoticeSource::AgentMode, "plan reminder".into());
            crate::agent::execution::store_pending_notices(&exec, &mut turn)
                .await
                .unwrap();
            let stored: Vec<ChatMessage> = turn
                .history
                .as_slice()
                .iter()
                .map(|message| message.to_chat_message().unwrap())
                .collect();
            let after = prepared(&fixture, &exec, &mut turn, stored, None).await;

            assert_durable_prefix(&before, false, &after);
            assert_eq!(after.last().unwrap().text(), "plan reminder");
        }

        /// A notice stored after a tool batch does not turn the request into a
        /// user-prompt request for context hooks.
        #[tokio::test]
        async fn notice_after_a_tool_batch_keeps_the_after_tool_batch_trigger() {
            use querymt_agent_core::model::NoticeSource;
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));
            hold(&mut turn, &exec.session_id, &history);
            turn.queue_notice(NoticeSource::DuplicateToolCall, "same".into());

            let (_, spans) = capture(async {
                let _ = crate::agent::execution::prepare_model_request(
                    &fixture.config,
                    &exec,
                    &mut turn,
                )
                .await
                .expect("prepare request");
            })
            .await;
            let hook = spans
                .iter()
                .find(|span| span.name == "agent.hooks.context")
                .expect("context hook span");
            assert_eq!(
                attr(hook, "trigger"),
                Some(&Value::from("after_tool_batch"))
            );
        }

        #[tokio::test]
        async fn durable_prefix_is_stable_for_a_provider_without_breakpoints() {
            // A local provider: hints are compared away, content must still match.
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "llama_cpp", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));

            let first = prepared(&fixture, &exec, &mut turn, history.clone(), None).await;
            history.extend(tool_step("call-2"));
            let second = prepared(&fixture, &exec, &mut turn, history, None).await;

            assert_durable_prefix(&first, false, &second);
        }

        #[tokio::test]
        async fn request_without_request_scoped_context_ends_with_a_durable_message() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let prompt = ChatMessage::user().text("fix the build").build();

            let request = prepared(&fixture, &exec, &mut turn, vec![prompt], None).await;

            assert_eq!(request.len(), 1);
            assert_eq!(request[0].text(), "fix the build");
        }

        #[tokio::test]
        async fn context_after_the_user_prompt_follows_it() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let prompt = ChatMessage::user().text("fix the build").build();

            let request = prepared(&fixture, &exec, &mut turn, vec![prompt], Some(OBJECTIVE)).await;

            assert_eq!(request.len(), 2);
            assert_eq!(request[0].text(), "fix the build");
            assert_eq!(request[1].text(), OBJECTIVE);
        }

        #[tokio::test]
        async fn context_after_a_tool_batch_leaves_the_tool_result_message_unchanged() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let config = config_with_context_hook(&fixture.config);
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));
            let stored_result = serde_json::to_value(history.last().unwrap()).unwrap();

            let request = prepared_in(&config, &exec, &mut turn, history, None).await;

            assert_eq!(request.len(), 4);
            let mut result = request[2].clone();
            result.cache = None;
            assert_eq!(serde_json::to_value(result).unwrap(), stored_result);
            assert!(request[3].text().contains("durable only"));
            assert!(request[3].text().contains("<hook-context event=context"));
        }

        #[tokio::test]
        async fn context_hook_sees_durable_history_only() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let config = config_with_context_hook(&fixture.config);
            let prompt = ChatMessage::user().text("fix the build").build();

            let request =
                prepared_in(&config, &exec, &mut turn, vec![prompt], Some(OBJECTIVE)).await;

            let tail = request.last().unwrap().text();
            assert!(tail.contains("durable only"), "{tail}");
            assert!(!tail.contains("saw objective"), "{tail}");
        }

        #[tokio::test]
        async fn objective_and_hook_text_share_one_message_objective_first() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let config = config_with_context_hook(&fixture.config);
            let prompt = ChatMessage::user().text("fix the build").build();

            let request =
                prepared_in(&config, &exec, &mut turn, vec![prompt], Some(OBJECTIVE)).await;

            assert_eq!(request.len(), 2, "one tail message");
            let tail = request[1].text();
            assert!(tail.find("OBJECTIVE").unwrap() < tail.find("durable only").unwrap());
        }

        #[tokio::test]
        async fn context_hook_contribution_is_request_scoped() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let with_hook = config_with_context_hook(&fixture.config);
            let prompt = ChatMessage::user().text("fix the build").build();

            let first = prepared_in(&with_hook, &exec, &mut turn, vec![prompt.clone()], None).await;
            let second = prepared_in(&fixture.config, &exec, &mut turn, vec![prompt], None).await;

            assert_eq!(first.len(), 2);
            assert_eq!(second.len(), 1, "the next request does not carry the text");
            assert!(!second[0].text().contains("durable only"));
        }

        #[tokio::test]
        async fn last_two_durable_messages_are_hinted_and_the_tail_is_not() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));
            history.extend(tool_step("call-2"));

            let request = prepared(&fixture, &exec, &mut turn, history, Some(OBJECTIVE)).await;

            // Five durable messages followed by the unhinted tail.
            assert_eq!(request.len(), 6);
            let flags: Vec<bool> = request.iter().map(hinted).collect();
            assert_eq!(flags, [false, false, false, true, true, false]);
        }

        #[tokio::test]
        async fn existing_history_hints_are_kept_and_hinting_is_idempotent() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            let mut history = vec![ChatMessage::user().text("fix the build").build()];
            history.extend(tool_step("call-1"));
            history.extend(tool_step("call-2"));

            let request = prepared(&fixture, &exec, &mut turn, history, None).await;

            let flags: Vec<bool> = request.iter().map(hinted).collect();
            assert_eq!(flags, [false, false, false, true, true]);
            assert_eq!(
                serde_json::to_value(apply_cache_breakpoints(&request, None)).unwrap(),
                serde_json::to_value(&request).unwrap(),
                "applying hints twice gives the same request"
            );
        }

        /// Task 2.3: the session-bound `skill` tool reaches `ToolsAvailable`
        /// for an agent with a catalog, but never the agent-level registry; an
        /// agent without a catalog is unchanged.
        #[tokio::test]
        async fn tools_available_includes_session_skill_tool_only_with_a_catalog() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let project = tempfile::tempdir().unwrap();

            async fn tools_event(
                config: &AgentConfig,
                fixture: &TestAgent,
            ) -> Vec<querymt::chat::Tool> {
                let (exec, mut turn) = fixture.turn().await;
                let exec = exec.tools_from(config);
                let mut events = config.subscribe_events();
                let _ = transition_before_llm_call_in_test(config, &exec, &mut turn)
                    .await
                    .expect("before llm call");
                let deadline = std::time::Duration::from_secs(2);
                while let Ok(Ok(event)) = tokio::time::timeout(deadline, events.recv()).await {
                    if let querymt_agent_core::events::AgentEventKind::ToolsAvailable {
                        tools,
                        ..
                    } = event.kind()
                    {
                        return tools.clone();
                    }
                }
                panic!("no ToolsAvailable event emitted");
            }

            // No catalog: unchanged, no skill tool.
            let without = tools_event(&fixture.config, &fixture).await;
            assert!(!without.iter().any(|t| t.function.name == "skill"));

            // Catalog: session tool is offered, agent registry still lacks it.
            let mut config = (*fixture.config).clone();
            config.skill_catalog = Some(Arc::new(crate::skills::SkillCatalog::from_parts(
                crate::skills::SkillRegistry::new(),
                Arc::new(crate::skills::SkillPermissions::default()),
                vec![],
                false,
                project.path().to_path_buf(),
            )));
            let with = tools_event(&config, &fixture).await;
            assert!(with.iter().any(|t| t.function.name == "skill"));
            let handle = crate::agent::LocalAgentHandle::from_config(Arc::new(config));
            assert!(handle.tool_registry().find("skill").is_none());
        }

        /// Skill problem announcements: a fixture whose catalog reads one
        /// configured folder, and the problem lists announced per request.
        struct SkillProblemRun {
            folder: tempfile::TempDir,
            config: AgentConfig,
            exec: crate::agent::execution::turn::TurnEnv,
            turn: crate::agent::execution::turn::Turn,
            events: tokio::sync::broadcast::Receiver<
                std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
            >,
        }

        impl SkillProblemRun {
            async fn new() -> Self {
                let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
                let folder = tempfile::tempdir().unwrap();
                let mut config = (*fixture.config).clone();
                config.skill_catalog = Some(Arc::new(crate::skills::SkillCatalog::from_parts(
                    crate::skills::SkillRegistry::new(),
                    Arc::new(crate::skills::SkillPermissions::default()),
                    vec![crate::skills::SkillSource::Configured(
                        folder.path().to_path_buf(),
                    )],
                    false,
                    folder.path().to_path_buf(),
                )));
                let (exec, turn) = fixture.turn().await;
                let exec = exec.tools_from(&config);
                let events = config.subscribe_events();
                Self {
                    folder,
                    config,
                    exec,
                    turn,
                    events,
                }
            }

            /// A skill definition without a description: an `invalid_definition`.
            fn add_malformed(&self, name: &str) {
                let dir = self.folder.path().join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\n---\nBody\n"),
                )
                .unwrap();
            }

            fn remove(&self, name: &str) {
                std::fs::remove_dir_all(self.folder.path().join(name)).unwrap();
            }

            /// Prepare one model request and return the skill problem lists
            /// announced while doing so.
            async fn request(&mut self) -> Vec<Vec<querymt_agent_core::events::SkillProblemInfo>> {
                let _ =
                    transition_before_llm_call_in_test(&self.config, &self.exec, &mut self.turn)
                        .await
                        .expect("before llm call");
                let mut announced = Vec::new();
                // Delivery through the fanout is asynchronous: collect until
                // the stream stays quiet.
                let quiet = std::time::Duration::from_millis(150);
                while let Ok(Ok(event)) = tokio::time::timeout(quiet, self.events.recv()).await {
                    if let AgentEventKind::SkillProblemsChanged { problems } = event.kind() {
                        announced.push(problems.clone());
                    }
                }
                announced
            }
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn problems_are_announced_on_the_first_model_request() {
            let mut run = SkillProblemRun::new().await;
            std::os::unix::fs::symlink(
                run.folder.path().join("missing"),
                run.folder.path().join("dangling"),
            )
            .unwrap();

            let announced = run.request().await;

            assert_eq!(announced.len(), 1, "{announced:?}");
            assert_eq!(announced[0].len(), 1);
            assert_eq!(announced[0][0].kind, "broken_link");
            assert!(announced[0][0].path.ends_with("dangling"));
        }

        #[tokio::test]
        async fn unchanged_problems_are_not_announced_again() {
            let mut run = SkillProblemRun::new().await;
            run.add_malformed("bad");

            assert_eq!(run.request().await.len(), 1);
            assert!(run.request().await.is_empty());
            assert!(run.request().await.is_empty());
        }

        #[tokio::test]
        async fn changed_problems_are_announced_again() {
            let mut run = SkillProblemRun::new().await;
            run.add_malformed("first");
            assert_eq!(run.request().await.len(), 1);

            run.add_malformed("second");
            let announced = run.request().await;

            assert_eq!(announced.len(), 1, "{announced:?}");
            assert_eq!(announced[0].len(), 2);
            assert!(announced[0].iter().any(|p| p.path.contains("first")));
            assert!(announced[0].iter().any(|p| p.path.contains("second")));
        }

        #[tokio::test]
        async fn resolved_problems_are_announced_with_an_empty_list() {
            let mut run = SkillProblemRun::new().await;
            run.add_malformed("bad");
            assert_eq!(run.request().await.len(), 1);

            run.remove("bad");
            let announced = run.request().await;

            assert_eq!(announced, vec![Vec::new()]);
            assert!(
                run.request().await.is_empty(),
                "the empty list is announced once"
            );
        }

        #[tokio::test]
        async fn session_without_problems_receives_no_skill_notice() {
            let mut run = SkillProblemRun::new().await;
            let dir = run.folder.path().join("fine");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                "---\nname: fine\ndescription: A fine skill\n---\nBody\n",
            )
            .unwrap();

            assert!(run.request().await.is_empty());
            assert!(run.request().await.is_empty());
        }

        #[tokio::test]
        async fn recovery_cap_is_clamped_after_preparing_the_request() {
            let (fixture, _factory) = timed_fixture(true, Vec::new(), "openai", None).await;
            let (exec, mut turn) = fixture.turn().await;
            hold(
                &mut turn,
                &exec.session_id,
                &[ChatMessage::user().text("continue the report").build()],
            );
            turn.request_overrides = querymt::chat::GenerationOptions::NONE
                .with_max_tokens(u32::MAX)
                .with_reasoning_effort(querymt::chat::ReasoningEffort::Low);
            let window = querymt_agent_services::model_info::get_model_info(
                exec.provider_name(),
                exec.model_name(),
            )
            .and_then(|info| info.limits.context)
            .unwrap_or(u64::from(u32::MAX))
            .min(u64::from(u32::MAX)) as u32;
            let state = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                .await
                .unwrap();
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
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
            let (mut exec, mut turn) = fixture.turn().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            exec.model.profile = profile_with(querymt::chat::GenerationOptionSupport::ALL, None);
            turn.request_overrides = overrides;
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            prepared.options = overrides;

            let state = call_llm_in_test(
                &fixture.config,
                &Arc::new(prepared.clone()),
                &exec,
                &mut turn,
            )
            .await
            .expect("first request");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
            };
            assert_eq!(response.applied, overrides);
            assert_eq!(turn.last_applied, overrides);
            assert!(
                turn.request_overrides.is_empty(),
                "the consumed override must not carry into the next state"
            );

            // The follow-up request is prepared from the cleared run.
            prepared.options = turn.request_overrides;
            let state = call_llm_in_test(&fixture.config, &Arc::new(prepared), &exec, &mut turn)
                .await
                .expect("second request");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
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
            let (exec, mut turn) = fixture.turn().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            prepared.options = overrides;

            let state = call_llm_in_test(&fixture.config, &Arc::new(prepared), &exec, &mut turn)
                .await
                .expect("request");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
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
            let (exec, _turn) = fixture.turn().await;

            let profile = exec.model.profile.clone();

            // The binding's explicit max_tokens is its own ceiling.
            assert_eq!(
                profile.caps,
                Some(querymt_agent_services::model_heuristics::OutputCaps {
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
            let (before, _turn) = fixture.turn().await;
            let old = before.model.profile.clone();
            assert!(old.capabilities.options.max_tokens);

            // A switch rebinds the session to another model: the next turn builds a
            // new handle, and with it a new profile (new caps, new capabilities).
            *factory.provider.support.lock().unwrap() =
                querymt::chat::GenerationOptionSupport::NONE;
            factory.binding.lock().unwrap()["max_tokens"] = serde_json::json!(999);
            fixture.config.provider.clear_provider_cache().await;
            let (after, _turn) = fixture.turn().await;
            let new = after.model.profile.clone();

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
            let (mut exec, mut turn) = fixture.turn().await;
            let overrides = querymt::chat::GenerationOptions::NONE.with_max_tokens(64_000);
            exec.model.profile = profile_with(support, None);
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            let plain_request = Arc::new(prepared.clone());
            prepared.options = overrides;
            let escalated_request = Arc::new(prepared);

            let (_, spans) = capture(async {
                let _ = call_llm_in_test(&fixture.config, &escalated_request, &exec, &mut turn)
                    .await
                    .expect("escalated request");
                let _ = call_llm_in_test(&fixture.config, &plain_request, &exec, &mut turn)
                    .await
                    .expect("plain request");
            })
            .await;

            let chats: Vec<_> = spans
                .iter()
                .filter(|span| attr(span, "gen_ai.operation.name") == Some(&Value::from("chat")))
                .collect();
            // A reply asked for again at a higher cap is a new request, not a
            // retry: each request has its own span with one attempt.
            for chat in &chats {
                assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(1)));
                let events = chat
                    .events
                    .iter()
                    .filter(|event| event.name == "gen_ai.request.attempt")
                    .count();
                assert_eq!(events, 1);
            }
            let max_tokens: Vec<_> = chats
                .iter()
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
            use crate::test_utils::HookRig;
            use crate::test_utils::hook_rig::in_run;

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
            let (mut exec, mut turn) = fixture.turn().await;
            let mut stack =
                HookRig::stack_of(crate::middleware::OutputLimitRecoveryMiddleware::default());
            let mut prepared = (*request()).clone();
            prepared.tools = vec![write_file_tool()].into();
            exec.model.profile = profile_with(
                querymt::chat::GenerationOptionSupport::ALL,
                Some(querymt_agent_services::model_heuristics::OutputCaps {
                    initial: 32_000,
                    ceiling: 128_000,
                }),
            );

            // First request: cut off at the initial cap.
            let state = call_llm_in_test(
                &fixture.config,
                &Arc::new(prepared.clone()),
                &exec,
                &mut turn,
            )
            .await
            .expect("first request");
            assert!(
                matches!(state, Step::Continue(Stage::AfterReply(_))),
                "{state:?}"
            );
            let state = in_run::after_llm(&mut stack, &exec, &mut turn, state)
                .await
                .expect("middleware");
            assert!(
                matches!(state, Step::Continue(Stage::BeforeRequest)),
                "expected escalation to BeforeLlmCall, got {state:?}"
            );
            assert_eq!(turn.request_overrides.max_tokens, Some(128_000));

            // Retry: same request at the ceiling.
            let state = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                .await
                .expect("before llm call");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            // The fixture config has no tools; restore the first request's so the
            // retry takes the same streaming path.
            let mut retry = (*request).clone();
            retry.tools = prepared.tools.clone();
            let state = call_llm_in_test(&fixture.config, &Arc::new(retry), &exec, &mut turn)
                .await
                .expect("second request");
            let state = in_run::after_llm(&mut stack, &exec, &mut turn, state)
                .await
                .expect("middleware");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
            };
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .expect("after llm");

            assert_eq!(
                *factory.provider.options_seen.lock().unwrap(),
                vec![
                    querymt::chat::GenerationOptions::NONE,
                    querymt::chat::GenerationOptions::NONE.with_max_tokens(128_000),
                ]
            );
            assert!(matches!(state, Step::Continue(Stage::Ending)));
            let history = turn
                .history
                .as_slice()
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
            use crate::test_utils::HookRig;
            use crate::test_utils::hook_rig::in_run;
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
            let (mut exec, mut turn) = fixture.turn().await;
            let mut middleware =
                HookRig::stack_of(crate::middleware::OutputLimitRecoveryMiddleware::default());
            let tools: Arc<[querymt::chat::Tool]> = vec![write_file_tool()].into();
            let instruction_marker = "cut off by the output-token limit";

            // Request 1: initial cap, thinking only, cut off -> escalate.
            exec.model.profile = profile_with(
                querymt::chat::GenerationOptionSupport::ALL,
                Some(querymt_agent_services::model_heuristics::OutputCaps {
                    initial: 32_000,
                    ceiling: 128_000,
                }),
            );
            let mut prepared = (*request()).clone();
            prepared.tools = tools.clone();
            let state = call_llm_in_test(&fixture.config, &Arc::new(prepared), &exec, &mut turn)
                .await
                .expect("request 1");
            let state = in_run::after_llm(&mut middleware, &exec, &mut turn, state)
                .await
                .expect("middleware 1");
            assert!(
                matches!(state, Step::Continue(Stage::BeforeRequest)),
                "expected escalation, got {state:?}"
            );
            assert_eq!(turn.request_overrides.max_tokens, Some(128_000));

            // Request 2: ceiling, thinking only again -> not escalated twice, stops.
            let state = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                .await
                .expect("before 2");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            let mut retry = (*request).clone();
            retry.tools = tools.clone();
            let state = call_llm_in_test(&fixture.config, &Arc::new(retry), &exec, &mut turn)
                .await
                .expect("request 2");
            let state = in_run::after_llm(&mut middleware, &exec, &mut turn, state)
                .await
                .expect("middleware 2");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
            };
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .expect("after 2");
            assert!(
                matches!(
                    state,
                    Step::Continue(Stage::Stopping(Stop {
                        stop_type: StopType::ModelTokenLimit,
                        ..
                    }))
                ),
                "got {state:?}"
            );

            // Stop -> continuation with instruction, ceiling and lowered effort.
            let state = in_run::stopped(&mut middleware, &exec, &mut turn, state)
                .await
                .expect("on_stopped");
            assert!(
                matches!(state, Step::Continue(Stage::BeforeRequest)),
                "expected continuation, got {state:?}"
            );
            assert_eq!(
                turn.request_overrides,
                GenerationOptions::NONE
                    .with_max_tokens(128_000)
                    .with_reasoning_effort(ReasoningEffort::Medium)
            );

            // Request 3: carries the stored instruction and succeeds. The loop
            // stores queued notices right before the request.
            crate::agent::execution::store_pending_notices(&exec, &mut turn)
                .await
                .expect("store notice");
            let state = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn)
                .await
                .expect("before 3");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
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
            let state = call_llm_in_test(&fixture.config, &Arc::new(last), &exec, &mut turn)
                .await
                .expect("request 3");
            let state = in_run::after_llm(&mut middleware, &exec, &mut turn, state)
                .await
                .expect("middleware 3");
            let Step::Continue(Stage::AfterReply(response)) = state else {
                panic!("expected AfterLlm, got {state:?}");
            };
            assert_eq!(
                turn.history
                    .as_slice()
                    .iter()
                    .filter(|message| format!("{message:?}").contains(instruction_marker))
                    .count(),
                1,
                "instruction stays in history after the normal reply, once"
            );
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .expect("after 3");

            assert!(matches!(state, Step::Continue(Stage::Ending)), "{state:?}");
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
            let history = turn
                .history
                .as_slice()
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
            exec: &TurnEnv,
            turn: &mut Turn,
            middleware: &mut crate::middleware::TurnStack,
            tools: &Arc<[querymt::chat::Tool]>,
        ) -> Step {
            let state = crate::test_utils::hook_rig::in_run::step_start(middleware, exec, turn)
                .await
                .unwrap();
            assert!(matches!(state, Step::Continue(Stage::BeforeRequest)));
            let state = transition_before_llm_call_in_test(config, exec, turn)
                .await
                .expect("before llm call");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            let mut probe = request.messages.to_vec();
            assert!(
                querymt_agent_core::model::answer_unanswered_tool_calls(&mut probe).is_empty(),
                "request must answer every call"
            );
            let mut request = (*request).clone();
            request.tools = tools.clone();
            request.offered_tools = tools.clone();
            let state = call_llm_in_test(config, &Arc::new(request), exec, turn)
                .await
                .expect("call llm");
            crate::test_utils::hook_rig::in_run::after_llm(middleware, exec, turn, state)
                .await
                .unwrap()
        }

        /// Replays session 01a16d18: escalation succeeds, a later step of the
        /// same turn is cut off inside a flushed (invalid) `write_file`, and
        /// the continuation must keep the escalated cap and answer the call.
        #[tokio::test]
        async fn later_cutoff_in_an_escalated_turn_keeps_the_cap_and_answers_the_call() {
            use crate::test_utils::HookRig;
            use crate::test_utils::hook_rig::in_run;
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
            let (mut exec, mut turn) = fixture.turn().await;
            let mut middleware =
                HookRig::stack_of(crate::middleware::OutputLimitRecoveryMiddleware::default());
            let tools: Arc<[querymt::chat::Tool]> = vec![write_file_tool()].into();
            exec.model.profile = profile_with(
                querymt::chat::GenerationOptionSupport::ALL,
                Some(querymt_agent_services::model_heuristics::OutputCaps {
                    initial: 32_000,
                    ceiling: 128_000,
                }),
            );

            let config = fixture.config.clone();
            let mw = &mut middleware;

            // 1: cut off at 32k -> escalate.
            let state = model_step(&config, &exec, &mut turn, mw, &tools).await;
            assert!(
                matches!(state, Step::Continue(Stage::BeforeRequest)),
                "expected escalation"
            );
            // 2: escalated retry succeeds.
            let Step::Continue(Stage::AfterReply(response)) =
                model_step(&config, &exec, &mut turn, mw, &tools).await
            else {
                panic!("expected AfterLlm");
            };
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .unwrap();
            assert!(matches!(state, Step::Continue(Stage::Ending)));
            // 3: a later step of the same turn starts without overrides, is sent at
            // 128k, and is cut off inside a flushed invalid call.
            assert!(turn.request_overrides.is_empty());
            let Step::Continue(Stage::AfterReply(response)) =
                model_step(&config, &exec, &mut turn, mw, &tools).await
            else {
                panic!("a second cut-off must not escalate again");
            };
            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .unwrap();
            assert!(matches!(
                state,
                Step::Continue(Stage::Stopping(Stop {
                    stop_type: StopType::ModelTokenLimit,
                    ..
                }))
            ));
            // 4: continuation at the escalated cap, with the call answered.
            let state = in_run::stopped(mw, &exec, &mut turn, state).await.unwrap();
            assert!(
                matches!(state, Step::Continue(Stage::BeforeRequest)),
                "expected continuation"
            );
            let Step::Continue(Stage::AfterReply(_)) =
                model_step(&config, &exec, &mut turn, mw, &tools).await
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
        ) -> (Step, bool, Turn) {
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))
            .await;
            let mut events = fixture.config.subscribe_events();
            let (exec, mut turn) = fixture.turn().await;
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

            let state = transition_after_llm(&fixture.config, &response, &exec, &mut turn)
                .await
                .expect("after llm");

            let mut stored = false;
            while let Ok(Ok(envelope)) =
                tokio::time::timeout(Duration::from_millis(50), events.recv()).await
            {
                stored |= matches!(
                    envelope.kind(),
                    querymt_agent_core::events::AgentEventKind::AssistantMessageStored { .. }
                );
            }
            (state, stored, turn)
        }

        #[tokio::test]
        async fn thinking_only_length_reply_is_not_stored_but_is_counted() {
            let (state, stored, turn) =
                after_length_reply(vec![reasoning_item("pondering")], "", Some("pondering")).await;

            assert!(
                !stored,
                "no AssistantMessageStored for a thinking-only cut-off"
            );
            let Step::Continue(Stage::Stopping(Stop { stop_type, .. })) = state else {
                panic!("expected Stopped, got {state:?}");
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
            assert!(
                turn.history.as_slice().is_empty(),
                "the reply must not enter history"
            );
            assert_eq!(turn.stats.steps, 1);
            assert_eq!(turn.stats.total_output_tokens, 32_000);
            assert_eq!(turn.stats.total_input_tokens, 10);
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
            let (state, stored, turn) = after_length_reply(
                vec![reasoning_item("pondering"), message],
                "partial",
                Some("pondering"),
            )
            .await;

            assert!(stored, "a reply with visible text is kept");
            let Step::Continue(Stage::Stopping(Stop { stop_type, .. })) = state else {
                panic!("expected Stopped, got {state:?}");
            };
            assert_eq!(stop_type, StopType::ModelTokenLimit);
            assert_eq!(turn.history.as_slice().len(), 1);
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
                let (exec, mut turn) = fixture.turn().await;
                let mut request = (*request()).clone();
                request.tools = vec![serde_json::from_value(serde_json::json!({"type":"function", "function":{"name":"safe", "description":"", "parameters":{"type":"object"}}})).unwrap()].into();
                request.offered_tools = request.tools.clone();
                let (_, spans) = capture(async {
                    let parent = info_span!("timing-parent");
                    async {
                        let request = Arc::new(request);
                        let mut future = Box::pin(call_llm_in_test(
                            &fixture.config,
                            &request,
                            &exec,
                            &mut turn,
                        ));
                        if mode.starts_with("cancel") || mode.starts_with("drop") {
                            assert!(
                                tokio::time::timeout(Duration::from_millis(180), &mut future)
                                    .await
                                    .is_err()
                            );
                            if mode.starts_with("cancel") {
                                exec.cancellation_token.cancel();
                                assert!(matches!(
                                    future.await.unwrap(),
                                    Step::Break(CycleOutcome::Cancelled)
                                ));
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
                    let (exec, mut turn) = fixture.turn().await;
                    let mut prepared = request();
                    if with_tools {
                        prepared.tools = vec![serde_json::from_value(serde_json::json!({
                            "type":"function", "function":{"name":"telemetry_tool", "description":"",
                            "parameters":{"type":"object"}}
                        })).unwrap()].into();
                    }
                    // The run resolved its provider once, before the transitions
                    // below; they reuse it.
                    assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);
                    assert_eq!(factory.configs.lock().unwrap().len(), 1);
                    let (_, spans) = capture(async {
                        async {
                            for call in 0..2 {
                                let Step::Continue(Stage::AfterReply(response)) =
                                    call_llm_in_test(&fixture.config, &prepared, &exec, &mut turn)
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
            let (exec, _turn) = fixture.turn().await;
            // The run resolves its provider once, when it starts.
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);

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
            // `fixture.turn()` resolves the provider, so take a handle that has
            // not: nothing is built until the first `provider()` call.
            let handle = fixture
                .config
                .provider
                .create_session(None, None, &Default::default())
                .await
                .unwrap();
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 0);
            let early_clone = handle.clone();
            let from_clone = early_clone.provider().await.unwrap();
            let from_original = handle.provider().await.unwrap();
            assert!(Arc::ptr_eq(&from_clone, &from_original));
            assert_eq!(factory.resolutions.load(Ordering::Relaxed), 1);
        }

        #[tokio::test]
        async fn pre_request_phases_are_traced_and_resolve_no_provider() {
            let (fixture, _factory) = timed_fixture(
                true,
                vec![],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let (exec, mut turn) = fixture.turn().await;
            let (_, spans) = capture(async {
                let _ = transition_before_llm_call_in_test(&fixture.config, &exec, &mut turn).await;
            })
            .await;
            let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
            for expected in [
                "agent.transition.before_llm_call",
                "agent.tools.collect",
                "agent.hooks.context",
                "agent.request.cache_breakpoints",
            ] {
                assert!(names.contains(&expected), "missing {expected}: {names:?}");
            }
            // The provider was resolved when the run started: preparing a
            // request builds and retries nothing.
            for absent in ["agent.provider.build", "agent.llm.call_with_retry"] {
                assert!(!names.contains(&absent), "unexpected {absent}: {names:?}");
            }
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
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(async {
                call_llm_in_test(&fixture.config, &request(), &exec, &mut turn)
                    .instrument(info_span!("test-parent"))
                    .await
            })
            .await;
            assert!(matches!(
                result.unwrap(),
                Step::Continue(Stage::AfterReply(_))
            ));
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
                let (exec, mut turn) = fixture.turn().await;
                if cancelled {
                    exec.cancellation_token.cancel();
                }
                let (result, spans) = capture(call_llm_in_test(
                    &fixture.config,
                    &request(),
                    &exec,
                    &mut turn,
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
                    assert!(matches!(
                        result.unwrap(),
                        Step::Break(CycleOutcome::Cancelled)
                    ));
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

        // ── gen_ai.request.attempt ───────────────────────────────────────────

        /// `(attempt, error.type)` of each `gen_ai.request.attempt` event, in order.
        fn attempt_events(span: &opentelemetry_sdk::trace::SpanData) -> Vec<(i64, Option<String>)> {
            span.events
                .iter()
                .filter(|event| event.name == "gen_ai.request.attempt")
                .map(|event| {
                    let attribute = |key: &str| {
                        event
                            .attributes
                            .iter()
                            .find(|kv| kv.key.as_str() == key)
                            .map(|kv| kv.value.clone())
                    };
                    let attempt = match attribute("gen_ai.request.attempt") {
                        Some(Value::I64(attempt)) => attempt,
                        other => panic!("event without an integer attempt: {other:?}"),
                    };
                    (attempt, attribute("error.type").map(|v| v.to_string()))
                })
                .collect()
        }

        fn chat_span(
            spans: &[opentelemetry_sdk::trace::SpanData],
        ) -> &opentelemetry_sdk::trace::SpanData {
            let mut chats = spans
                .iter()
                .filter(|s| attr(s, "gen_ai.operation.name") == Some(&Value::from("chat")));
            let chat = chats.next().expect("a model request span");
            assert!(chats.next().is_none(), "exactly one model request span");
            chat
        }

        /// A request with a tool, so the streaming path is used.
        fn streaming_request() -> Box<PreparedModelRequest> {
            let mut request = (*request()).clone();
            request.tools = vec![serde_json::from_value(serde_json::json!({"type":"function", "function":{"name":"safe", "description":"", "parameters":{"type":"object"}}})).unwrap()].into();
            request.offered_tools = request.tools.clone();
            Box::new(request)
        }

        fn rate_limited_now() -> LLMError {
            LLMError::RateLimited {
                message: "SECRET_ERROR".into(),
                retry_after_secs: Some(0),
            }
        }

        #[tokio::test]
        async fn request_without_a_retry_reports_one_attempt() {
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
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(result.is_ok());

            // Provider setup (`provider_init`) is not an attempt of the request.
            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(1)));
            assert_eq!(attempt_events(chat), [(1, None)]);
            assert_private(&spans);
        }

        /// Whether the one request `prepared` leads to was sent with a tool
        /// list (`Some(len)`) or without one (`None`).
        async fn sent_tool_list(prepared: Box<PreparedModelRequest>) -> Option<usize> {
            let sent = Arc::new(std::sync::Mutex::new(None));
            let seen = sent.clone();
            let reply = || {
                Ok(ChatOutput::from_projections(
                    None,
                    Some("ok".into()),
                    None,
                    None,
                    Some(FinishReason::Stop),
                ))
            };
            // The mock takes a call without a tool list as `chat` and one
            // with a list as `chat_with_tools`.
            let mut mock = MockLlmProvider::new();
            let without = seen.clone();
            mock.expect_chat().returning(move |_| {
                *without.lock().unwrap() = Some(None);
                reply()
            });
            mock.expect_chat_with_tools().returning(move |_, tools| {
                *seen.lock().unwrap() = Some(tools.map(<[_]>::len));
                reply()
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let (exec, mut turn) = fixture.turn().await;
            let _ = call_llm_in_test(&fixture.config, &prepared, &exec, &mut turn)
                .await
                .expect("call llm");
            sent.lock().unwrap().take().expect("one request was sent")
        }

        #[tokio::test]
        async fn tools_offered_only_by_a_change_are_sent_with_a_tool_list() {
            // The request's tool list is the baseline's: it can be empty while
            // a tool change in the messages offers tools.
            let mut offered_by_change = (*request()).clone();
            offered_by_change.offered_tools = streaming_request().tools.clone();
            assert!(offered_by_change.tools.is_empty());

            assert_eq!(
                sent_tool_list(Box::new(offered_by_change)).await,
                Some(0),
                "sent as a request with tools, whose list is empty"
            );
            assert_eq!(
                sent_tool_list(request()).await,
                None,
                "no tools at all: no tool list"
            );
            assert_eq!(sent_tool_list(streaming_request()).await, Some(1));
        }

        #[tokio::test]
        async fn retry_after_an_overloaded_error_reports_two_attempts() {
            use querymt::error::{ProviderErrorKind, ProviderFailure};

            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(move |_| {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Err(LLMError::from(
                        ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "SECRET_ERROR")
                            .with_retry_after_secs(Some(0)),
                    ));
                }
                Ok(ChatOutput::from_projections(
                    None,
                    Some("SECRET_RESPONSE".into()),
                    None,
                    None,
                    Some(FinishReason::Stop),
                ))
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(result.is_ok());
            assert_eq!(calls.load(Ordering::Relaxed), 2);

            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(2)));
            let events = attempt_events(chat);
            assert_eq!(events.len(), 2, "{events:?}");
            assert_eq!(events[0].0, 1);
            // The failure is identified by its type; the message is never exported.
            assert!(events[0].1.is_some(), "{events:?}");
            assert_eq!(events[1], (2, None));
            assert_eq!(chat.status, Status::Unset);
            assert_private(&spans);
        }

        #[tokio::test]
        async fn request_failing_every_attempt_reports_all_three_and_error_status() {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(rate_limited_now())
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(result.is_err());
            assert_eq!(calls.load(Ordering::Relaxed), 3);

            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(3)));
            let limited = || Some("rate_limited".to_string());
            assert_eq!(
                attempt_events(chat),
                [(1, limited()), (2, limited()), (3, limited())]
            );
            assert_eq!(chat.status, Status::error(""));
            assert_private(&spans);
        }

        #[tokio::test]
        async fn stream_recreated_after_a_failure_reports_two_attempts() {
            let chunks = vec![
                (20, Ok(metadata_event())),
                (100, Ok(StreamChunk::Text("SECRET_RESPONSE".into()))),
                (
                    0,
                    Ok(terminal_event(
                        ChatOutputStatus::Completed,
                        Some(FinishReason::Stop),
                    )),
                ),
                (
                    0,
                    Ok(StreamChunk::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ),
            ];
            let (fixture, _factory) = timed_fixture(
                true,
                vec![Err(rate_limited_now()), Ok(chunks)],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &streaming_request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(result.is_ok());

            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(2)));
            assert_eq!(
                attempt_events(chat),
                [(1, Some("rate_limited".to_string())), (2, None)]
            );
            assert_private(&spans);
        }

        #[tokio::test]
        async fn stream_failing_every_attempt_reports_all_three_and_error_status() {
            let (fixture, _factory) = timed_fixture(
                true,
                vec![
                    Err(rate_limited_now()),
                    Err(rate_limited_now()),
                    Err(rate_limited_now()),
                ],
                "openai",
                Some("oci://ghcr.io/querymt/openai:latest"),
            )
            .await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &streaming_request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(result.is_err());

            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(3)));
            assert_eq!(attempt_events(chat).len(), 3);
            assert_eq!(chat.status, Status::error(""));
            assert_private(&spans);
        }

        #[tokio::test]
        async fn cancelled_request_adds_no_attempt_event() {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat().returning(|_| Err(LLMError::Cancelled));
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let (exec, mut turn) = fixture.turn().await;
            let (result, spans) = capture(call_llm_in_test(
                &fixture.config,
                &request(),
                &exec,
                &mut turn,
            ))
            .await;
            assert!(matches!(
                result.unwrap(),
                Step::Break(CycleOutcome::Cancelled)
            ));

            // The attempt was started, so the count is right; it did not end in
            // a response or a failure, so there is no event.
            let chat = chat_span(&spans);
            assert_eq!(attr(chat, "gen_ai.request.attempt"), Some(&Value::I64(1)));
            assert!(attempt_events(chat).is_empty());
            assert_eq!(chat.status, Status::Unset);
        }

        #[tokio::test]
        async fn request_attempts_helper_skips_cancellation() {
            let (_, spans) = capture(async {
                let span = info_span!("chat holder");
                let attempts = crate::agent::utils::genai::RequestAttempts::new(&span);
                attempts.start();
                attempts.failed(&LLMError::Cancelled);
                attempts.start();
                attempts.succeeded();
                drop(span);
            })
            .await;
            let holder = spans.iter().find(|s| s.name == "chat holder").unwrap();
            assert_eq!(attr(holder, "gen_ai.request.attempt"), Some(&Value::I64(2)));
            assert_eq!(attempt_events(holder), [(2, None)]);
        }

        #[tokio::test]
        async fn genai_polled_then_dropped_request_closes_unset() {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat()
                .returning(|_| panic!("provider must not be reached while its lock is held"));
            let shared = SharedLlmProvider::new(mock, vec![]);
            let lock = shared.inner.clone();
            let fixture = TestAgent::with_mock_provider(shared).await;
            let (exec, mut turn) = fixture.turn().await;
            // Initialize the provider before blocking the mock's call.
            exec.session_handle.provider().await.unwrap();
            let _locked = lock.lock().await;
            let (_, spans) = capture(async {
                let request = request();
                let mut future = Box::pin(call_llm_in_test(
                    &fixture.config,
                    &request,
                    &exec,
                    &mut turn,
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

    async fn llm_env() -> TurnEnv {
        let fixture = crate::test_utils::TestAgent::new().await.with_inert_model();
        fixture.turn().await.0
    }

    #[tokio::test]
    async fn setup_cancel_maps_to_cancelled_not_error() {
        let env = llm_env().await;
        let non_stream = map_failed_llm_call(LLMError::Cancelled, false, &env)
            .expect("cancel must be Ok(Cancelled)");
        assert!(matches!(non_stream, Step::Break(CycleOutcome::Cancelled)));

        let stream = map_failed_llm_call(LLMError::Cancelled, true, &env)
            .expect("stream setup cancel must be Ok(Cancelled)");
        assert!(matches!(stream, Step::Break(CycleOutcome::Cancelled)));
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

    #[tokio::test]
    async fn llm_failures_add_operation_and_model_context_without_erasing_source() {
        let env = llm_env().await;
        let chat_err = map_failed_llm_call(
            LLMError::InvalidRequest("missing api_key".into()),
            false,
            &env,
        )
        .expect_err("non-stream failure is Err");
        let chat_context = chat_err.to_string();
        assert!(chat_context.contains("LLM chat error"));
        assert!(chat_context.contains("provider=mock"));
        assert!(chat_context.contains("model=mock"));
        assert!(chat_context.contains("Invalid Request: missing api_key"));
        assert!(matches!(
            chat_err.llm_error(),
            Some(LLMError::InvalidRequest(message)) if message == "missing api_key"
        ));

        let stream_err = map_failed_llm_call(LLMError::GenericError("boom".into()), true, &env)
            .expect_err("stream failure is Err");
        assert!(stream_err.to_string().contains("LLM streaming error"));
        assert!(matches!(
            stream_err.llm_error(),
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
        let result = apply_cache_breakpoints(&messages, None);
        assert_eq!(result[0].cache, Some(hint));
        assert_eq!(
            result[1].cache,
            Some(CacheHint::Ephemeral { ttl_seconds: None })
        );
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(apply_cache_breakpoints(&result, None)).unwrap()
        );
    }

    #[test]
    fn tool_change_message_is_not_hinted_and_does_not_count() {
        use querymt::chat::ToolChange;
        let change = ChatMessage::from_tool_change(ToolChange::new(Vec::new(), vec!["x".into()]));
        let msgs = vec![
            make_message(ChatRole::User, "prompt"),
            make_message(ChatRole::Assistant, "call"),
            make_message(ChatRole::User, "result"),
            change,
        ];
        let result = apply_cache_breakpoints(&msgs, None);
        assert!(result[0].cache.is_none());
        assert!(result[1].cache.is_some(), "the call is hinted");
        assert!(result[2].cache.is_some(), "the result is hinted");
        assert!(result[3].cache.is_none(), "the tool change is not hinted");
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(apply_cache_breakpoints(&result, None)).unwrap(),
            "applying twice changes nothing"
        );
    }

    #[test]
    fn test_cache_breakpoints_empty_slice() {
        let result = apply_cache_breakpoints(&[], None);
        assert!(result.is_empty());
    }

    #[test]
    fn test_cache_breakpoints_single_message_gets_cache_hint() {
        let msgs = vec![make_message(ChatRole::User, "hello")];
        let result = apply_cache_breakpoints(&msgs, None);
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
        let result = apply_cache_breakpoints(&msgs, None);
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
        let result = apply_cache_breakpoints(&msgs, None);
        assert_eq!(result.len(), 3);
        assert!(
            result[0].cache.is_none(),
            "first message should NOT be cached"
        );
        assert!(result[1].cache.is_some(), "second-to-last should be cached");
        assert!(result[2].cache.is_some(), "last should be cached");
    }

    #[test]
    fn last_two_messages_are_hinted() {
        let msgs: Vec<ChatMessage> = (0..5)
            .map(|i| make_message(ChatRole::User, &format!("msg-{i}")))
            .collect();
        let result = apply_cache_breakpoints(&msgs, None);
        assert_eq!(result.len(), 5);
        for (i, msg) in result.iter().enumerate().take(3) {
            assert!(msg.cache.is_none(), "msg-{i} should NOT have cache hint");
        }
        assert!(result[3].cache.is_some(), "msg-3 should be cached");
        assert!(result[4].cache.is_some(), "msg-4 should be cached");
    }

    #[test]
    fn existing_history_hints_are_kept() {
        let hint = CacheHint::Ephemeral {
            ttl_seconds: Some(1800),
        };
        let mut msgs: Vec<ChatMessage> = (0..5)
            .map(|i| make_message(ChatRole::User, &format!("msg-{i}")))
            .collect();
        msgs[1].cache = Some(hint.clone());

        let result = apply_cache_breakpoints(&msgs, None);
        let hinted: Vec<bool> = result.iter().map(|m| m.cache.is_some()).collect();
        assert_eq!(hinted, [false, true, false, true, true]);
        assert_eq!(result[1].cache, Some(hint));

        // Applying twice yields the same request as applying once.
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(apply_cache_breakpoints(&result, None)).unwrap()
        );
    }

    #[test]
    fn test_cache_breakpoints_preserves_content() {
        let msgs = vec![
            make_message(ChatRole::User, "important content"),
            make_message(ChatRole::Assistant, "response text"),
            make_message(ChatRole::User, "follow-up"),
        ];
        let result = apply_cache_breakpoints(&msgs, None);
        assert_eq!(result[0].text(), "important content");
        assert_eq!(result[1].text(), "response text");
        assert_eq!(result[2].text(), "follow-up");
    }

    #[test]
    fn default_hints_request_the_given_lifetime_and_keep_explicit_ones() {
        let explicit = CacheHint::Ephemeral {
            ttl_seconds: Some(300),
        };
        let mut msgs: Vec<ChatMessage> = (0..4)
            .map(|i| make_message(ChatRole::User, &format!("msg-{i}")))
            .collect();
        msgs[3].cache = Some(explicit.clone());

        let result = apply_cache_breakpoints(&msgs, Some(1800));

        // Message 2 gets the default hint with the requested lifetime;
        // message 3 keeps the lifetime it already had.
        assert_eq!(
            result[2].cache,
            Some(CacheHint::Ephemeral {
                ttl_seconds: Some(1800)
            })
        );
        assert_eq!(result[3].cache, Some(explicit));
        assert!(result[0].cache.is_none() && result[1].cache.is_none());
    }

    #[test]
    fn test_cache_breakpoints_cache_hint_is_ephemeral() {
        // With 2 messages the last one should get an Ephemeral hint
        let msgs = vec![
            make_message(ChatRole::User, "test"),
            make_message(ChatRole::Assistant, "reply"),
        ];
        let result = apply_cache_breakpoints(&msgs, None);
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
            events: &mut tokio::sync::broadcast::Receiver<
                std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
            >,
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
            let (exec, mut turn) = fixture.turn().await;
            let mut events = fixture.config.subscribe_events();
            let request = Arc::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("hi").build()].into(),
                tools: Arc::from([]),
                offered_tools: Arc::from([]),
                estimated_tokens: 1,
                options: querymt::chat::GenerationOptions::NONE,
            });
            // First request: 1000 input + (100 + 900) output + 2000 reads + 500 writes.
            turn.stats = AgentStats::default();
            let _ = call_llm_in_test(&fixture.config, &request, &exec, &mut turn)
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
            turn.stats = after_first;
            let _ = call_llm_in_test(&fixture.config, &request, &exec, &mut turn)
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

    mod prefix_attribution_tests {
        use super::*;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use querymt::chat::{ChatOutput, FinishReason};
        use querymt_agent_core::events::CachePrefixChange;

        fn ok_response() -> ChatOutput {
            ChatOutput::from_projections(
                None,
                Some("ok".into()),
                None,
                None,
                Some(FinishReason::Stop),
            )
        }

        /// Prepare and send one request; returns what its `LlmRequestEnd`
        /// recorded.
        async fn send(
            fixture: &TestAgent,
            exec: &mut TurnEnv,
            turn: &mut Turn,
            events: &mut tokio::sync::broadcast::Receiver<
                std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
            >,
            model: &str,
            messages: Vec<ChatMessage>,
        ) -> (Option<CachePrefixChange>, Option<u64>) {
            if exec.model_name() != model {
                exec.session_handle = exec.session_handle.with_model_for_test(model);
            }
            hold(turn, &exec.session_id, &messages);
            let state = transition_before_llm_call_in_test(&fixture.config, exec, turn)
                .await
                .expect("before llm call");
            let Step::Continue(Stage::CallModel(request)) = state else {
                panic!("expected CallLlm, got {state:?}");
            };
            let _ = call_llm_in_test(&fixture.config, &request, exec, turn)
                .await
                .expect("call llm");
            loop {
                let envelope = tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .expect("timed out waiting for LlmRequestEnd")
                    .expect("event channel closed");
                if let AgentEventKind::LlmRequestEnd {
                    prefix_change,
                    idle_secs,
                    ..
                } = envelope.kind()
                {
                    return (*prefix_change, *idle_secs);
                }
            }
        }

        fn user(text: &str) -> ChatMessage {
            ChatMessage::user().text(text).build()
        }

        async fn fixture() -> TestAgent {
            let mut mock = MockLlmProvider::new();
            mock.expect_chat_with_tools()
                .returning(|_, _| Ok(ok_response()));
            TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await
        }

        #[tokio::test]
        async fn request_end_attributes_each_prefix_change() {
            let fixture = fixture().await;
            let (mut exec, mut turn) = fixture.turn().await;
            let mut events = fixture.config.subscribe_events();
            let base = vec![user("a"), user("b")];

            let (change, idle) = send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "mock-model",
                base.clone(),
            )
            .await;
            assert_eq!(change, Some(CachePrefixChange::FirstRequest));
            assert_eq!(idle, None, "no idle for the first request");

            let mut grown = base.clone();
            grown.push(user("c"));
            let (change, idle) = send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "mock-model",
                grown.clone(),
            )
            .await;
            assert_eq!(change, Some(CachePrefixChange::Unchanged));
            assert!(idle.is_some_and(|secs| secs < 5), "idle: {idle:?}");

            let (change, _) = send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "other-model",
                grown.clone(),
            )
            .await;
            assert_eq!(change, Some(CachePrefixChange::Model));

            // Compaction replaced the older messages.
            let compacted = vec![user("summary"), user("c"), user("d")];
            let (change, _) = send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "other-model",
                compacted,
            )
            .await;
            assert_eq!(change, Some(CachePrefixChange::History));
        }

        #[tokio::test]
        async fn a_new_runtime_reports_first_request_again() {
            let fixture = fixture().await;
            let mut events = fixture.config.subscribe_events();
            let messages = vec![user("a")];

            let (mut exec, mut turn) = fixture.turn().await;
            send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "mock-model",
                messages.clone(),
            )
            .await;

            // A loaded session gets a fresh runtime, and with it no fingerprint.
            let (mut reloaded, mut reloaded_turn) = fixture.turn().await;
            let (change, idle) = send(
                &fixture,
                &mut reloaded,
                &mut reloaded_turn,
                &mut events,
                "mock-model",
                messages,
            )
            .await;
            assert_eq!(change, Some(CachePrefixChange::FirstRequest));
            assert_eq!(idle, None);
        }

        #[tokio::test]
        async fn the_model_request_span_carries_the_attribution() {
            use std::sync::Mutex;
            use tracing_subscriber::layer::SubscriberExt;

            #[derive(Default)]
            struct Recorded(Mutex<Vec<(String, String)>>);
            struct Capture(Arc<Recorded>);
            struct Visit<'a>(&'a mut Vec<(String, String)>);
            impl tracing::field::Visit for Visit<'_> {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    self.0.push((f.name().to_string(), format!("{v:?}")));
                }
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    self.0.push((f.name().to_string(), v.to_string()));
                }
                fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
                    self.0.push((f.name().to_string(), v.to_string()));
                }
            }
            impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
                fn on_record(
                    &self,
                    _id: &tracing::span::Id,
                    values: &tracing::span::Record<'_>,
                    _ctx: tracing_subscriber::layer::Context<'_, S>,
                ) {
                    values.record(&mut Visit(&mut self.0.0.lock().unwrap()));
                }
            }

            let recorded = Arc::new(Recorded::default());
            let subscriber = tracing_subscriber::registry().with(Capture(recorded.clone()));
            let _guard = tracing::subscriber::set_default(subscriber);

            let fixture = fixture().await;
            let (mut exec, mut turn) = fixture.turn().await;
            let mut events = fixture.config.subscribe_events();
            send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "mock-model",
                vec![user("a")],
            )
            .await;
            send(
                &fixture,
                &mut exec,
                &mut turn,
                &mut events,
                "mock-model",
                vec![user("a"), user("b")],
            )
            .await;

            let fields = recorded.0.lock().unwrap().clone();
            let value = |name: &str| -> Vec<&str> {
                fields
                    .iter()
                    .filter(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
                    .collect()
            };
            assert_eq!(value("qmt.cache.prefix_change"), ["first_request", "none"]);
            assert_eq!(
                value("qmt.cache.idle_secs").len(),
                1,
                "only the second request"
            );
        }
    }

    mod attempt_accounting_tests {
        use super::cost_accounting_tests::{CACHE_ENV_LOCK, InjectedPricing};
        use super::genai_trace_tests::{TimedChunks, timed_fixture};
        use super::*;
        use crate::session::backend::StorageBackend;
        use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};
        use querymt::Usage;
        use querymt::providers::ModelPricing;
        use querymt_agent_services::accounting::{
            AccountingReason, AttemptOutcome, UsageDisposition,
        };
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

        fn prepared(with_tools: bool) -> Box<PreparedModelRequest> {
            let tools: Vec<querymt::chat::Tool> = if with_tools {
                vec![
                    serde_json::from_value(serde_json::json!({"type":"function","function":{
                        "name":"safe","description":"","parameters":{"type":"object"}}}))
                    .unwrap(),
                ]
            } else {
                Vec::new()
            };
            Box::new(PreparedModelRequest {
                messages: vec![ChatMessage::user().text("hi").build()].into(),
                tools: tools.clone().into(),
                offered_tools: tools.into(),
                estimated_tokens: 1,
                options: querymt::chat::GenerationOptions::NONE,
            })
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

        fn settlements(
            ledger: &[AgentEventKind],
        ) -> Vec<&querymt_agent_services::accounting::AttemptSettlement> {
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

        async fn session_totals(exec: &TurnEnv) -> querymt_agent_services::accounting::ScopeTotals {
            exec.cycle_accounting.session_totals()
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
            let (exec, mut turn) = fixture.turn().await;
            let _ = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
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
            let (exec, mut turn) = fixture.turn().await;
            let _ = call_llm_in_test(&fixture.config, &prepared(true), &exec, &mut turn)
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
                let (exec, mut turn) = fixture.turn().await;
                let result =
                    call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn).await;
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
            let (exec, mut turn) = fixture.turn().await;
            exec.cancellation_token.cancel();
            let state = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
                .await
                .unwrap();
            assert!(matches!(state, Step::Break(CycleOutcome::Cancelled)));
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
                let (exec, mut turn) = fixture.turn().await;
                let state = call_llm_in_test(&config, &prepared(with_tools), &exec, &mut turn)
                    .await
                    .expect("a refused dispatch is a stop, not an error");
                let Step::Continue(Stage::Stopping(Stop { stop_type, message })) = state else {
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
            let (exec, mut turn) = fixture.turn().await;

            let first_pricing = InjectedPricing::install(pricing());
            let _ = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
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
            let _ = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
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
            let replayed = querymt_agent_services::accounting::SessionAccounting::hydrate(
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
            let (exec, mut turn) = fixture.turn().await;
            let _ = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
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
            let (exec, mut turn) = fixture.turn().await;

            let state = call_llm_in_test(&fixture.config, &prepared(false), &exec, &mut turn)
                .await
                .unwrap();
            let Step::Continue(Stage::AfterReply(response)) = state else {
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
            let result = transition_after_llm(&fixture.config, &response, &exec, &mut turn).await;
            assert!(result.is_err(), "history save must fail");

            // The charge is unaffected, live and from the journal alone.
            assert_eq!(session_totals(&exec).await, recorded);
            let replayed = querymt_agent_services::accounting::SessionAccounting::hydrate(
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
            let (exec, mut turn) = fixture.turn().await;
            // Open accounting first, then make the journal unwritable so the
            // durable attempt start cannot be recorded.
            fixture
                .storage
                .conn_for_test()
                .lock()
                .execute_batch("DROP TABLE event_journal")
                .unwrap();
            let result = call_llm_in_test(&config, &prepared(false), &exec, &mut turn).await;
            let error = result.expect_err("storage failure must be an execution error");
            assert!(error.to_string().contains("accounting"), "{error:#}");
        }

        type Events = tokio::sync::broadcast::Receiver<
            std::sync::Arc<querymt_agent_core::events::EventEnvelope>,
        >;

        /// The next `LlmRequestEnd` as (cost, accounting), or `None` if none
        /// arrives within the window.
        async fn next_request_end(
            events: &mut Events,
            wait: Duration,
        ) -> Option<(
            Option<f64>,
            Option<querymt_agent_services::accounting::RequestAccounting>,
        )> {
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
            let (fixture, exec, mut turn) = stream_fixture(vec![
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
            let _ = call_llm_in_test(&fixture.config, &prepared(true), &exec, &mut turn)
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
            let (fixture, exec, mut turn) = stream_fixture(vec![
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
            let _ = call_llm_in_test(&fixture.config, &prepared(true), &exec, &mut turn)
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
            let (fixture, exec, mut turn) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
            ])])
            .await;
            let mut events = fixture.config.subscribe_events();
            let request = prepared(true);
            let mut future = Box::pin(call_llm_in_test(
                &fixture.config,
                &request,
                &exec,
                &mut turn,
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(400), &mut future)
                    .await
                    .is_err()
            );
            exec.cancellation_token.cancel();
            assert!(matches!(
                future.await.unwrap(),
                Step::Break(CycleOutcome::Cancelled)
            ));

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

        async fn stream_fixture(attempts: Vec<TimedChunks>) -> (TestAgent, TurnEnv, Turn) {
            let (fixture, _factory) = timed_fixture(true, attempts, "openai", None).await;
            let (exec, turn) = fixture.turn().await;
            (fixture, exec, turn)
        }

        #[tokio::test]
        async fn streaming_retry_after_setup_rejection_keeps_separate_accumulators() {
            let _guard = CACHE_ENV_LOCK.lock().await;
            let _pricing = InjectedPricing::install_for("alias", pricing());
            let (fixture, exec, mut turn) = stream_fixture(vec![
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
            let _ = call_llm_in_test(&fixture.config, &prepared(true), &exec, &mut turn)
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
            let (fixture, exec, mut turn) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
                (0, Err(LLMError::ProviderError("stream broke".into()))),
            ])])
            .await;
            let result = call_llm_in_test(&fixture.config, &prepared(true), &exec, &mut turn).await;
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
            let (fixture, exec, mut turn) = stream_fixture(vec![Ok(vec![
                (0, Ok(chunk_usage())),
                (0, Ok(StreamChunk::Text("part".into()))),
            ])])
            .await;
            let request = prepared(true);
            let mut future = Box::pin(call_llm_in_test(
                &fixture.config,
                &request,
                &exec,
                &mut turn,
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(400), &mut future)
                    .await
                    .is_err()
            );
            exec.cancellation_token.cancel();
            assert!(matches!(
                future.await.unwrap(),
                Step::Break(CycleOutcome::Cancelled)
            ));

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
            let (fixture, exec, mut turn) =
                stream_fixture(vec![Ok(vec![(0, Ok(chunk_usage()))])]).await;
            {
                let request = prepared(true);
                let mut future = Box::pin(call_llm_in_test(
                    &fixture.config,
                    &request,
                    &exec,
                    &mut turn,
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
