//! Tool execution, permission checking, and result storage
//!
//! This module handles the complete lifecycle of tool calls: execution, permission
//! checking, snapshotting, and storing results back into conversation history.

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SnapshotPolicy;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::snapshots::{SnapshotState, snapshot_metadata};
use crate::agent::utils::genai;
use crate::config::{ApprovalNeed, ToolRef};
use crate::events::AgentEventKind;
use crate::hooks::{
    PermissionRequestDecision, PostToolUseRequest, PreDelegationRequest, PreToolUseRequest,
    UpdatedDelegation,
};
use crate::middleware::ToolCall as MiddlewareToolCall;
use crate::middleware::{ExecutionState, ToolResult, WaitCondition};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use crate::tools::Tool;
use crate::tools::args::{Repair, normalize_arguments};
use log::debug;
use querymt::chat::ChatRole;
use querymt::mcp::adapter::McpToolAdapter;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{Instrument, Span, info_span, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

/// Open the `agent.tool.execute` span of one call.
///
/// The span is created when the call is prepared and travels with the
/// [`PreparedCall`], so preparation and execution are reported on one span
/// per call. Its duration therefore includes any time the prepared call waits
/// for earlier conflicting calls to finish.
fn open_tool_span(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
) -> Span {
    let span = info_span!(
        "agent.tool.execute",
        otel.name = %format!("execute_tool {}", call.function.name),
        otel.kind = "internal",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.name = %call.function.name,
        gen_ai.tool.call.id = %call.id,
        gen_ai.conversation.id = %exec_ctx.session_id,
        session.id = %exec_ctx.session_id,
        session_id = %exec_ctx.session_id,
        tool_name = %call.function.name,
        tool_call_id = %call.id,
        tool_source = tracing::field::Empty,
        is_error = tracing::field::Empty,
        has_snapshot = tracing::field::Empty
    );
    genai::agent_id(&span, config.provider.agent_id.as_deref());
    span.set_attribute("querymt.tool.execution", "not_started");
    span
}

/// Prepare and execute a single tool call in one step.
///
/// The scheduler prepares and executes separately (see
/// [`prepare_tool_call`] and [`execute_prepared_tool_call`]); this convenience
/// composes the two for tests.
#[cfg(test)]
pub(super) async fn execute_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
) -> Result<ToolResult, anyhow::Error> {
    let prepared = prepare_tool_call(config, call, exec_ctx).await?;
    execute_prepared_tool_call(config, call, prepared, exec_ctx).await
}

/// Why a prepared call will not reach its tool.
pub(super) enum Disposition {
    /// Arguments are valid; the call may be scheduled and executed.
    Ready,
    /// The call was turned into its final error result during preparation
    /// (conflicting spellings, hook block, or validation failure). Its tool is
    /// never invoked.
    Rejected(Box<ToolResult>),
}

/// One tool call after argument preparation.
///
/// Owns the *effective* arguments (normalised, hook-adjusted, validated), the
/// hook decisions and context contributions, and the preparation outcome. The
/// model-emitted arguments stay untouched on the middleware call and in stored
/// assistant history; they are not part of this value.
pub(super) struct PreparedCall {
    /// Final effective arguments. Governs validation, scheduling, permissions,
    /// invocation, events and result records.
    pub args: serde_json::Value,
    /// `args` serialised once, for events and tool-result records.
    pub arguments_json: String,
    /// Changes applied by normalisation, from the initial arguments and from any
    /// pre-tool hook replacement. Reported once, by `prepare_tool_call`.
    pub repairs: Vec<Repair>,
    /// What the call resolves to, decided once during preparation.
    pub target: ToolTarget,
    /// Approval the call needs before it runs: a hook allow means none, a hook
    /// ask means every call, otherwise the agent's permission lists decide.
    pub approval: ApprovalNeed,
    /// Context contributed by pre-tool hooks, delivered with the call's result.
    pub hook_contexts: Vec<crate::hooks::HookContextContribution>,
    pub disposition: Disposition,
    /// The call's `agent.tool.execute` span, shared by preparation and
    /// execution.
    pub span: Span,
}

/// Maximum number of Unicode scalar values of raw input echoed in an
/// `INVALID_JSON` diagnostic.
pub(super) const INVALID_JSON_MAX_CHARS: usize = 2000;

/// Whether the call's outer input must not execute: the provider marked it
/// invalid, or its raw arguments are not a JSON object. A parser-normalised
/// zero-argument `{}` is valid.
pub(super) fn has_invalid_input(call: &MiddlewareToolCall) -> bool {
    call.invalid_input
        || !matches!(
            serde_json::from_str::<serde_json::Value>(&call.function.arguments),
            Ok(serde_json::Value::Object(_))
        )
}

/// The error text of a rejected invalid call: `{"INVALID_JSON": "<raw>"}` with
/// the raw input cut to [`INVALID_JSON_MAX_CHARS`] Unicode scalar values.
pub(super) fn invalid_input_text(raw: &str) -> String {
    let shown: String = raw.chars().take(INVALID_JSON_MAX_CHARS).collect();
    serde_json::json!({ "INVALID_JSON": shown }).to_string()
}

/// The single error result of a call with invalid outer input. The raw
/// arguments are recorded as emitted: there are no effective arguments.
pub(super) fn invalid_input_result(call: &MiddlewareToolCall) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        vec![querymt::chat::ToolResultPart::Text {
            text: invalid_input_text(&call.function.arguments),
        }],
        true,
        Some(call.function.name.clone()),
        Some(call.function.arguments.clone()),
    )
    .with_execution(true, "invalid_input")
}

/// Final error result for a call rejected during preparation.
fn rejection_result(
    call: &MiddlewareToolCall,
    args: &serde_json::Value,
    text: String,
    execution: &'static str,
    hook_contexts: Vec<crate::hooks::HookContextContribution>,
) -> ToolResult {
    ToolResult::new(
        call.id.clone(),
        vec![querymt::chat::ToolResultPart::Text { text }],
        true,
        Some(call.function.name.clone()),
        Some(serde_json::to_string(args).unwrap_or_else(|_| call.function.arguments.clone())),
    )
    .with_execution(true, execution)
    .with_hook_contexts(hook_contexts)
}

/// Prepare one call: normalise its arguments, run the pre-tool hooks exactly
/// once, normalise any hook replacement, and validate the final value.
///
/// Conflicts, hook blocks and validation failures come back as
/// [`Disposition::Rejected`] carrying the call's single error result; the tool is
/// not invoked for them.
pub(super) async fn prepare_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
) -> Result<PreparedCall, anyhow::Error> {
    debug!(
        "Preparing tool: session={}, tool={}",
        exec_ctx.session_id, call.function.name
    );
    let span = open_tool_span(config, call, exec_ctx);
    let mut prepared = prepare_tool_call_inner(config, call, exec_ctx)
        .instrument(span.clone())
        .await
        .inspect_err(|_| {
            if !exec_ctx.cancellation_token.is_cancelled() {
                span.in_scope(|| genai::error("tool_pipeline_error"));
            }
        })?;
    // Each applied mutation is reported exactly once, here: scheduling,
    // middleware review and execution never re-normalise or re-report.
    let rules: Vec<&'static str> = prepared
        .repairs
        .iter()
        .map(|repair| repair.rule.as_str())
        .collect();
    genai::args_repaired(&span, &call.function.name, &rules);
    prepared.span = span;
    Ok(prepared)
}

/// Record the outcome of a call that never ran (rejected during preparation or
/// by middleware review), keeping the repaired indicator of its arguments.
pub(super) fn record_rejected_outcome(call: &MiddlewareToolCall, repaired: bool) {
    genai::tool_call_outcome(&call.function.name, repaired, "rejected");
}

async fn prepare_tool_call_inner(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
) -> Result<PreparedCall, anyhow::Error> {
    // Invalid outer input never executes and never degrades to `{}`. It is
    // rejected before hooks, permissions or invocation.
    if has_invalid_input(call) {
        Span::current().set_attribute("querymt.tool.execution", "invalid_input");
        genai::error("invalid_input");
        return Ok(PreparedCall {
            args: serde_json::Value::Null,
            arguments_json: call.function.arguments.clone(),
            repairs: Vec::new(),
            target: ToolTarget::Provider,
            approval: ApprovalNeed::None,
            hook_contexts: Vec::new(),
            disposition: Disposition::Rejected(Box::new(invalid_input_result(call))),
            span: Span::none(),
        });
    }
    let mut args: serde_json::Value = serde_json::from_str(&call.function.arguments)?;
    let tool = config.tool_registry.find(&call.function.name);
    let target = ToolTarget::resolve(tool.clone(), exec_ctx, &call.function.name);
    let schema = resolve_tool_schema(config, exec_ctx, &call.function.name).await;
    let mut repairs: Vec<Repair> = Vec::new();

    let reject = |args: serde_json::Value,
                  repairs: Vec<Repair>,
                  text: String,
                  execution: &'static str,
                  hook_contexts: Vec<crate::hooks::HookContextContribution>| {
        let result = rejection_result(call, &args, text, execution, hook_contexts);
        let arguments_json = result.tool_arguments.clone().unwrap_or_default();
        PreparedCall {
            args,
            arguments_json,
            repairs,
            target: ToolTarget::Provider,
            approval: ApprovalNeed::None,
            hook_contexts: Vec::new(),
            disposition: Disposition::Rejected(Box::new(result)),
            span: Span::none(),
        }
    };

    // Hooks must see canonical names, so normalise before running them.
    match normalize_arguments(tool.as_deref(), schema.as_ref(), &mut args) {
        Ok(applied) => repairs.extend(applied),
        Err(conflict) => {
            Span::current().set_attribute("querymt.tool.execution", "validation_rejected");
            genai::error("invalid_tool_arguments");
            return Ok(reject(
                args,
                repairs,
                format!("Error: invalid tool arguments: {conflict}"),
                "validation",
                Vec::new(),
            ));
        }
    }

    let hook_result = config
        .hooks
        .run_pre_tool_use(PreToolUseRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_server: target.server().map(str::to_owned),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: exec_ctx.turn_id().to_string(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: exec_ctx
                .llm_config()
                .map(|cfg| cfg.model.clone())
                .unwrap_or_default(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            tool_name: call.function.name.clone(),
            tool_input: args.clone(),
            tool_use_id: call.id.clone(),
        })
        .await
        .inspect_err(|_| {
            Span::current().set_attribute("querymt.tool.execution", "hook_failed");
        })?;
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

    let approval = match hook_result.permission_decision {
        Some(crate::hooks::engine::PreToolPermissionDecision::Allow) => ApprovalNeed::None,
        Some(crate::hooks::engine::PreToolPermissionDecision::Ask { reason }) => {
            ApprovalNeed::EveryCall { reason }
        }
        None => config
            .permissions
            .need(target.tool_ref(&call.function.name))
            .stricter(target.default_approval()),
    };
    let hook_contexts = hook_result.context_contributions;

    // A hook replacement is normalised like model input. A conflict inside it
    // rejects the call unless the hook also blocked it (the block wins).
    let mut replacement_conflict = None;
    if let Some(updated_input) = hook_result.updated_input {
        args = updated_input;
        match normalize_arguments(tool.as_deref(), schema.as_ref(), &mut args) {
            Ok(applied) => repairs.extend(applied),
            Err(conflict) => replacement_conflict = Some(conflict),
        }
    }

    if hook_result.should_block {
        Span::current().set_attribute("querymt.tool.execution", "hook_blocked");
        genai::error("hook_blocked");
        let reason = hook_result
            .block_reason
            .unwrap_or_else(|| "tool blocked by hook".to_string());
        return Ok(reject(
            args,
            repairs,
            format!("Error: {reason}"),
            "blocked",
            hook_contexts,
        ));
    }

    if let Some(conflict) = replacement_conflict {
        Span::current().set_attribute("querymt.tool.execution", "validation_rejected");
        genai::error("invalid_tool_arguments");
        return Ok(reject(
            args,
            repairs,
            format!("Error: invalid tool arguments: {conflict}"),
            "validation",
            hook_contexts,
        ));
    }

    if let Err(error) = validate_tool_arguments(exec_ctx, schema.as_ref(), &args).await {
        Span::current().set_attribute("querymt.tool.execution", "validation_rejected");
        genai::error("invalid_tool_arguments");
        return Ok(reject(
            args,
            repairs,
            format!("Error: invalid tool arguments: {error}"),
            "validation",
            hook_contexts,
        ));
    }

    let arguments_json =
        serde_json::to_string(&args).unwrap_or_else(|_| call.function.arguments.clone());
    Ok(PreparedCall {
        args,
        arguments_json,
        repairs,
        target,
        approval,
        hook_contexts,
        disposition: Disposition::Ready,
        span: Span::none(),
    })
}

/// Execute a prepared call on the span opened at preparation. Never re-parses
/// arguments or re-runs hooks.
///
/// This runs the invocation steps in order: emit the start event, snapshot,
/// record progress, check permissions, invoke the tool, run post-tool hooks,
/// truncate output, emit the end event, and build the result.
pub(super) async fn execute_prepared_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    prepared: PreparedCall,
    exec_ctx: &ExecutionContext,
) -> Result<ToolResult, anyhow::Error> {
    let span = prepared.span.clone();
    let repaired = !prepared.repairs.is_empty();
    execute_prepared_inner(config, call, prepared, exec_ctx)
        .instrument(span.clone())
        .await
        .inspect_err(|_| {
            if !exec_ctx.cancellation_token.is_cancelled() {
                span.in_scope(|| genai::error("tool_pipeline_error"));
                genai::tool_call_outcome(&call.function.name, repaired, "error");
            }
        })
}

async fn execute_prepared_inner(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    prepared: PreparedCall,
    exec_ctx: &ExecutionContext,
) -> Result<ToolResult, anyhow::Error> {
    let repaired = !prepared.repairs.is_empty();
    let PreparedCall {
        args,
        arguments_json,
        approval,
        target,
        hook_contexts,
        disposition,
        ..
    } = prepared;
    if let Disposition::Rejected(result) = disposition {
        record_rejected_outcome(call, repaired);
        return Ok(*result);
    }

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ToolCallStart {
            tool_call_id: call.id.clone(),
            tool_name: call.function.name.clone(),
            arguments: arguments_json.clone(),
        },
    );

    // Progress recording is observational — a failure here must not abort the
    // tool call itself, otherwise the tool_use block ends up without a matching
    // tool_result and the session becomes permanently broken.
    match exec_ctx
        .state
        .record_progress(
            crate::session::domain::ProgressKind::ToolCall,
            format!("Calling tool: {}", call.function.name),
            Some(args.clone()),
        )
        .await
    {
        Ok(progress_entry) => {
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::ProgressRecorded { progress_entry },
            );
        }
        Err(e) => {
            log::warn!(
                "Failed to record progress for tool {} (session {}): {}",
                call.function.name,
                exec_ctx.session_id,
                e
            );
        }
    }

    let (outcome, snapshot) = dispatch(config, exec_ctx, call, &args, &target, approval).await?;
    let ToolOutcome {
        parts: raw_result_blocks,
        is_error,
        source: tool_source,
        execution,
    } = outcome;

    let span = Span::current();
    span.record("tool_source", tool_source.as_str());
    span.record("is_error", is_error);
    span.set_attribute("querymt.tool.execution", execution.as_str());
    if is_error && !exec_ctx.cancellation_token.is_cancelled() {
        genai::error(execution.error_type());
    }

    // Post hooks inspect the complete canonical output; truncation is applied once
    // after the transformation pipeline.
    let execution_is_error = is_error;
    let mut result_blocks = raw_result_blocks;
    let mut is_error = is_error;
    let post_hook = config
        .hooks
        .run_post_tool_use(PostToolUseRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_server: target.server().map(str::to_owned),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: exec_ctx.turn_id().to_string(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: exec_ctx
                .llm_config()
                .map(|cfg| cfg.model.clone())
                .unwrap_or_default(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            tool_name: call.function.name.clone(),
            tool_input: args.clone(),
            content: result_blocks.clone(),
            is_error,
            execution_is_error,
            tool_source: tool_source.as_str().to_string(),
            tool_use_id: call.id.clone(),
        })
        .await?;
    // A blocked or suppressed result must not regain information through
    // enrichment; decided before the hook result is consumed.
    // Redaction (a rewritten output or error flag) counts the same as a block:
    // the model-visible result is no longer the tool's own output.
    // The engine reports the (possibly unchanged) final content and flag
    // whenever hooks ran, so only a difference from the tool's own output
    // counts as redaction.
    let output_suppressed = post_hook.should_block
        || post_hook.stop_reason.is_some()
        || post_hook
            .content
            .as_ref()
            .is_some_and(|content| *content != result_blocks)
        || post_hook.is_error.is_some_and(|flag| flag != is_error);
    for notice in post_hook.notices {
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    if let Some(content) = post_hook.content {
        result_blocks = content;
    }
    if let Some(model_is_error) = post_hook.is_error {
        is_error = model_is_error;
    }
    result_blocks = truncate_model_tool_output(
        config,
        exec_ctx,
        &call.id,
        &call.function.name,
        result_blocks,
        is_error,
    )
    .await;

    // Extract text summary for event (display purposes)
    let result_text: String = result_blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n");

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ToolCallEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.function.name.clone(),
            is_error,
            result: result_text,
        },
    );

    let snapshot_part = match snapshot {
        SnapshotState::Diff { pre_tree, root } => {
            let (post_tree, changed_paths) = info_span!(
                "agent.tool.snapshot.diff",
                session_id = %exec_ctx.session_id,
                tool_name = %call.function.name,
                tool_call_id = %call.id,
            )
            .in_scope(|| {
                let post_tree = crate::index::merkle::MerkleTree::scan_with_previous(
                    root.as_path(),
                    Some(&pre_tree),
                );
                let changed_paths = post_tree.diff_paths(&pre_tree);
                (post_tree, changed_paths)
            });
            let root_hash = post_tree.root_hash;
            *exec_ctx.runtime.last_snapshot_tree.lock() = Some((root, Arc::new(post_tree)));
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::SnapshotEnd {
                    summary: Some(changed_paths.summary()),
                },
            );
            Some(MessagePart::Snapshot {
                root_hash,
                changed_paths,
            })
        }
        SnapshotState::Metadata { root } => {
            let (part, summary) = info_span!(
                "agent.tool.snapshot.metadata",
                session_id = %exec_ctx.session_id,
                tool_name = %call.function.name,
                tool_call_id = %call.id,
            )
            .in_scope(|| snapshot_metadata(root.as_path()));
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::SnapshotEnd { summary },
            );
            Some(part)
        }
        SnapshotState::None => None,
    };

    let mut hook_contexts = hook_contexts;
    hook_contexts.extend(post_hook.context_contributions);
    // Native index enrichment: after execution, post-tool hooks and output
    // truncation. Purely additive context; the result blocks and error flag
    // above are never touched. Contributions already present (for example from
    // a command-hook wrapper) are not delivered again.
    let enrichment = crate::agent::repository_delivery::index_contexts(
        config,
        exec_ctx,
        &crate::agent::repository_delivery::FinishedCall {
            tool_name: &call.function.name,
            tool_source: tool_source.as_str(),
            execution_is_error,
            result_is_error: is_error,
            output_suppressed,
            tool_use_id: &call.id,
            args: &args,
        },
        &hook_contexts,
    )
    .await;
    hook_contexts.extend(enrichment);
    let mut tool_result = ToolResult::new(
        call.id.clone(),
        result_blocks,
        is_error,
        Some(call.function.name.clone()),
        Some(arguments_json.clone()),
    )
    .with_execution(execution_is_error, tool_source.as_str())
    .with_hook_contexts(hook_contexts);
    if let Some(part) = snapshot_part {
        tool_result = tool_result.with_snapshot(part);
    }

    Span::current().record("has_snapshot", tool_result.snapshot_part.is_some());
    genai::tool_call_outcome(
        &call.function.name,
        repaired,
        if execution_is_error {
            "error"
        } else {
            "success"
        },
    );

    Ok(tool_result)
}

/// Parameter schema of a tool: builtin registry, then MCP tool state, then
/// provider tools. `None` when the tool is unknown (nothing to validate).
async fn resolve_tool_schema(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    tool_name: &str,
) -> Option<serde_json::Value> {
    let schema = config
        .tool_registry
        .definition_for_cwd(tool_name, exec_ctx.cwd())
        .map(|tool| tool.function.parameters)
        .or_else(|| {
            exec_ctx
                .runtime
                .mcp_tool_state
                .load()
                .tool_defs
                .iter()
                .find(|tool| tool.function.name == tool_name)
                .map(|tool| tool.function.parameters.clone())
        });
    match schema {
        Some(schema) => Some(schema),
        None => exec_ctx
            .session_handle
            .provider()
            .await
            .ok()
            .and_then(|provider| {
                provider.tools().and_then(|tools| {
                    tools
                        .iter()
                        .find(|tool| tool.function.name == tool_name)
                        .map(|tool| tool.function.parameters.clone())
                })
            }),
    }
}

/// Validate effective arguments against an already-resolved schema.
async fn validate_tool_arguments(
    exec_ctx: &ExecutionContext,
    schema: Option<&serde_json::Value>,
    arguments: &serde_json::Value,
) -> anyhow::Result<()> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let schema_key = serde_json::to_string(schema)?;
    let validator = {
        let cache = exec_ctx.runtime.tool_validator_cache.lock();
        cache.get(&schema_key).cloned()
    };
    let validator = match validator {
        Some(validator) => validator,
        None => {
            let validator = Arc::new(
                jsonschema::validator_for(schema)
                    .map_err(|error| anyhow::anyhow!("invalid tool schema: {}", error))?,
            );
            exec_ctx
                .runtime
                .tool_validator_cache
                .lock()
                .insert(schema_key, validator.clone());
            validator
        }
    };
    match crate::tools::validation::report(&validator, schema, arguments) {
        None => Ok(()),
        Some(report) => Err(anyhow::anyhow!(report)),
    }
}

async fn truncate_model_tool_output(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call_id: &str,
    tool_name: &str,
    blocks: Vec<querymt::chat::ToolResultPart>,
    is_error: bool,
) -> Vec<querymt::chat::ToolResultPart> {
    if is_error {
        return blocks;
    }
    use crate::tools::builtins::helpers::{
        TruncationDirection, format_truncation_message_with_overflow, save_overflow_output,
        truncate_output,
    };
    let policy = &config.execution_policy.tool_output;
    let raw_text = blocks
        .iter()
        .filter_map(querymt::chat::ToolResultPart::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    let truncation = truncate_output(
        &raw_text,
        policy.max_lines,
        policy.max_bytes,
        TruncationDirection::Head,
    );
    if !truncation.was_truncated {
        return blocks;
    }
    let overflow_content = raw_text.clone();
    let overflow_storage = policy.overflow_storage.clone();
    let overflow_session_id = exec_ctx.session_id.clone();
    let overflow_call_id = call_id.to_string();
    let overflow = tokio::task::spawn_blocking(move || {
        save_overflow_output(
            &overflow_content,
            &overflow_storage,
            &overflow_session_id,
            &overflow_call_id,
            None,
        )
    })
    .await
    .unwrap_or_else(
        |error| crate::tools::builtins::helpers::OverflowSaveResult {
            path: None,
            error: Some(format!("Failed to join overflow writer: {}", error)),
        },
    );
    let hint = config
        .tool_registry
        .find(tool_name)
        .and_then(|tool| tool.truncation_hint());
    let suffix = format_truncation_message_with_overflow(
        &truncation,
        TruncationDirection::Head,
        Some(&overflow),
        hint,
    );
    let mut result: Vec<querymt::chat::ToolResultPart> = blocks
        .into_iter()
        .filter(|block| block.as_text().is_none())
        .collect();
    result.insert(
        0,
        querymt::chat::ToolResultPart::Text {
            text: format!("{}{}", truncation.content, suffix),
        },
    );
    result
}

/// What a call resolves to, decided once during preparation in dispatch
/// order: built-in registry, then the MCP snapshot, then the provider.
pub(super) enum ToolTarget {
    Builtin(Arc<dyn Tool>),
    Mcp(Arc<McpToolAdapter>),
    Provider,
}

/// Whether a call to `name` can leave workspace files untouched, using the same
/// target resolution (built-in, then MCP, then provider) as snapshot
/// preparation, so both agree on what counts as read-only.
pub(super) fn call_is_read_only(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    name: &str,
) -> bool {
    ToolTarget::resolve(config.tool_registry.find(name), exec_ctx, name).is_read_only()
}

impl ToolTarget {
    fn resolve(builtin: Option<Arc<dyn Tool>>, exec_ctx: &ExecutionContext, name: &str) -> Self {
        if let Some(tool) = builtin {
            return Self::Builtin(tool);
        }
        let snapshot = exec_ctx.runtime.mcp_tool_state.load();
        match snapshot.tools.get(name) {
            Some(adapter) => Self::Mcp(Arc::clone(adapter)),
            None => Self::Provider,
        }
    }

    /// MCP server serving the tool, if it is an MCP tool.
    fn server(&self) -> Option<&str> {
        match self {
            Self::Mcp(adapter) => Some(adapter.server_name()),
            _ => None,
        }
    }

    /// Server-qualified identity used by allow, deny and approval lists.
    fn tool_ref<'a>(&'a self, name: &'a str) -> ToolRef<'a> {
        ToolRef {
            name,
            server: self.server(),
        }
    }

    /// Approval the tool itself demands (built-ins only).
    fn default_approval(&self) -> ApprovalNeed {
        match self {
            Self::Builtin(tool) => tool.default_approval(),
            _ => ApprovalNeed::None,
        }
    }

    /// Whether the call needs no workspace snapshot. The provider fallback is
    /// never registered by the agent (it only reaches tools added to the LLM
    /// client directly), so it is not snapshotted.
    pub(super) fn is_read_only(&self) -> bool {
        match self {
            Self::Builtin(tool) => tool.is_read_only(),
            Self::Mcp(adapter) => adapter.is_read_only(),
            Self::Provider => true,
        }
    }

    fn source(&self) -> ToolSource {
        match self {
            Self::Builtin(_) => ToolSource::Builtin,
            Self::Mcp(_) => ToolSource::Mcp,
            Self::Provider => ToolSource::Provider,
        }
    }
}

/// Where a result came from; `Blocked` means the tool never ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSource {
    Builtin,
    Mcp,
    Provider,
    Blocked,
}

impl ToolSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Mcp => "mcp",
            Self::Provider => "provider",
            Self::Blocked => "blocked",
        }
    }
}

/// How the call ended, as reported on the tool span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Execution {
    Executed,
    PolicyBlocked,
    PermissionDenied,
}

impl Execution {
    fn as_str(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::PolicyBlocked => "policy_blocked",
            Self::PermissionDenied => "permission_denied",
        }
    }

    /// `error.type` recorded when the call ended in an error.
    fn error_type(self) -> &'static str {
        match self {
            Self::Executed => "tool_error",
            Self::PolicyBlocked => "policy_blocked",
            Self::PermissionDenied => "permission_denied",
        }
    }
}

/// Result of dispatching one call, before post-tool hooks.
struct ToolOutcome {
    parts: Vec<querymt::chat::ToolResultPart>,
    is_error: bool,
    source: ToolSource,
    execution: Execution,
}

impl ToolOutcome {
    fn new(
        parts: Vec<querymt::chat::ToolResultPart>,
        is_error: bool,
        source: ToolSource,
        execution: Execution,
    ) -> Self {
        Self {
            parts,
            is_error,
            source,
            execution,
        }
    }

    fn error_text(text: String, source: ToolSource, execution: Execution) -> Self {
        Self::new(
            vec![querymt::chat::ToolResultPart::Text { text }],
            true,
            source,
            execution,
        )
    }
}

/// Allow check, approval, snapshot, then invocation, for every tool source.
///
/// A denied or blocked call yields an error outcome and never reaches its
/// tool; no snapshot is taken for it.
async fn dispatch(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &MiddlewareToolCall,
    args: &serde_json::Value,
    target: &ToolTarget,
    approval: ApprovalNeed,
) -> Result<(ToolOutcome, SnapshotState), anyhow::Error> {
    let tool = target.tool_ref(&call.function.name);

    // Per-session config so that runtime SetAllowedTools / SetDeniedTools apply.
    if !exec_ctx.tool_config.allows(tool) {
        let text = format!("Error: tool '{}' is not allowed", call.function.name);
        return Ok((
            ToolOutcome::error_text(text, ToolSource::Blocked, Execution::PolicyBlocked),
            SnapshotState::None,
        ));
    }

    if !matches!(approval, ApprovalNeed::None) {
        let approved = request_approval(config, exec_ctx, &call.id, tool, args, approval)
            .instrument(info_span!(
                "agent.tool.permission_wait",
                tool_name = %call.function.name,
                tool_call_id = %call.id,
                session_id = %exec_ctx.session_id,
            ))
            .await
            .map_err(|e| anyhow::anyhow!("Permission check failed: {}", e))?;
        if let Approval::Denied(denial) = approved {
            let text = format!("Error: {denial}; the tool was not run");
            return Ok((
                ToolOutcome::error_text(text, ToolSource::Blocked, Execution::PermissionDenied),
                SnapshotState::None,
            ));
        }
    }

    let snapshot = if target.is_read_only() {
        SnapshotState::None
    } else {
        begin_snapshot(config, exec_ctx, call)
    };
    let outcome = run_tool(config, exec_ctx, call, args, target).await;
    Ok((outcome, snapshot))
}

/// Capture the pre-call state of the workspace, per the snapshot policy.
fn begin_snapshot(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &MiddlewareToolCall,
) -> SnapshotState {
    info_span!(
        "agent.tool.snapshot.prepare",
        session_id = %exec_ctx.session_id,
        tool_name = %call.function.name,
        tool_call_id = %call.id,
    )
    .in_scope(|| {
        config
            .prepare_snapshot(exec_ctx.cwd())
            .map(|(root, policy)| {
                config.emit_event(
                    &exec_ctx.session_id,
                    AgentEventKind::SnapshotStart {
                        policy: policy.to_string(),
                    },
                );
                match policy {
                    SnapshotPolicy::Diff => {
                        // Seed with the previous post-call tree so only files whose
                        // size or mtime changed since then are re-hashed.
                        let previous = exec_ctx
                            .runtime
                            .last_snapshot_tree
                            .lock()
                            .as_ref()
                            .filter(|(cached_root, _)| *cached_root == root)
                            .map(|(_, tree)| Arc::clone(tree));
                        let pre_tree = crate::index::merkle::MerkleTree::scan_with_previous(
                            root.as_path(),
                            previous.as_deref(),
                        );
                        SnapshotState::Diff { pre_tree, root }
                    }
                    SnapshotPolicy::Metadata => SnapshotState::Metadata { root },
                    SnapshotPolicy::None => SnapshotState::None,
                }
            })
            .unwrap_or(SnapshotState::None)
    })
}

/// Invoke the resolved target.
async fn run_tool(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &MiddlewareToolCall,
    args: &serde_json::Value,
    target: &ToolTarget,
) -> ToolOutcome {
    let source = target.source();
    match target {
        ToolTarget::Builtin(tool) => {
            let tool_context = tool_context_for_call(config, exec_ctx, call);
            let result = genai::tool_scope(
                Span::current(),
                tool.call(args.clone(), &tool_context)
                    .instrument(info_span!(
                        "agent.tool.invoke",
                        source = "builtin",
                        tool_name = %call.function.name,
                        tool_call_id = %call.id,
                    )),
            )
            .await;
            match result {
                Ok(parts) => ToolOutcome::new(parts, false, source, Execution::Executed),
                Err(e) => {
                    let execution = if matches!(&e, crate::tools::ToolError::PermissionDenied(_)) {
                        Execution::PermissionDenied
                    } else {
                        Execution::Executed
                    };
                    ToolOutcome::error_text(format!("Error: {e}"), source, execution)
                }
            }
        }
        ToolTarget::Mcp(adapter) => run_mcp_tool(config, exec_ctx, call, args, adapter).await,
        ToolTarget::Provider => {
            let result = exec_ctx
                .session_handle
                .call_tool(&call.function.name, args.clone())
                .instrument(info_span!(
                    "agent.tool.invoke",
                    source = "provider",
                    tool_name = %call.function.name,
                    tool_call_id = %call.id,
                ))
                .await;
            match result {
                Ok(parts) => ToolOutcome::new(parts, false, source, Execution::Executed),
                Err(e) => {
                    ToolOutcome::error_text(format!("Error: {e}"), source, Execution::Executed)
                }
            }
        }
    }
}

async fn run_mcp_tool(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &MiddlewareToolCall,
    args: &serde_json::Value,
    adapter: &McpToolAdapter,
) -> ToolOutcome {
    use querymt::tool_decorator::CallFunctionTool;
    const SOURCE: ToolSource = ToolSource::Mcp;

    // Progress is throttled and closed when the call ends or is dropped, so
    // no update can follow this call's `ToolCallEnd`.
    let (progress_sink, _close_progress) = {
        let event_sink = config.event_sink.clone();
        let (session_id, tool_call_id, tool_name) = (
            exec_ctx.session_id.clone(),
            call.id.clone(),
            call.function.name.clone(),
        );
        super::progress_sink::AgentProgressSink::new(super::progress_sink::THROTTLE, move |u| {
            event_sink.session_stream(&session_id).emit_ephemeral(
                AgentEventKind::ToolCallProgress {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    progress: u.progress,
                    total: u.total,
                    message: u.message,
                },
            );
        })
    };
    let result = querymt::mcp::progress::CURRENT_PROGRESS
        .scope(
            Arc::new(progress_sink) as Arc<dyn querymt::mcp::progress::ProgressSink>,
            adapter.call(args.clone()),
        )
        .instrument(info_span!(
            "agent.tool.invoke",
            source = "mcp",
            tool_name = %call.function.name,
            tool_call_id = %call.id,
        ))
        .await;

    let e = match result {
        Ok(parts) => return ToolOutcome::new(parts, false, SOURCE, Execution::Executed),
        Err(e) => e,
    };
    // A rejected/missing OAuth login tells the user which provider to log in
    // with instead of surfacing a transport error.
    let server = adapter.server_name();
    if let Some(text) = crate::agent::mcp::report_auth_required(
        &e,
        server,
        exec_ctx.runtime.mcp_tool_state.is_oauth_server(server),
        &config.event_sink,
        &exec_ctx.session_id,
    ) {
        return ToolOutcome::error_text(text, SOURCE, Execution::Executed);
    }
    // In-band `isError` results keep the server's content verbatim.
    match e.downcast::<querymt::mcp::adapter::McpToolError>() {
        Ok(querymt::mcp::adapter::McpToolError::Reported { parts }) => {
            ToolOutcome::new(parts, true, SOURCE, Execution::Executed)
        }
        Ok(other) => {
            ToolOutcome::error_text(format!("Error: {other}"), SOURCE, Execution::Executed)
        }
        Err(e) => ToolOutcome::error_text(format!("Error: {e}"), SOURCE, Execution::Executed),
    }
}

/// Build the context handed to a built-in tool, wiring its elicitation channel
/// to the pending-interaction registry for this call.
fn tool_context_for_call(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call: &MiddlewareToolCall,
) -> crate::tools::AgentToolContext {
    let (elicitation_tx, mut elicitation_rx) =
        tokio::sync::mpsc::channel::<crate::tools::ElicitationRequest>(1);

    let event_sink = config.event_sink.clone();
    let session_id_clone = exec_ctx.session_id.clone();
    let run_id_clone = exec_ctx.turn_id().to_string();
    let tool_call_id_clone = call.id.clone();
    let pending_elicitations = config.pending_elicitations.clone();
    let cancellation_token = exec_ctx.cancellation_token.clone();
    tokio::spawn(async move {
        while let Some(request) = elicitation_rx.recv().await {
            let elicitation_id = request.elicitation_id.clone();
            crate::elicitation::register_pending_elicitation(
                &pending_elicitations,
                elicitation_id.clone(),
                crate::elicitation::PendingElicitationRegistration {
                    session_id: session_id_clone.clone(),
                    form: crate::elicitation::PendingElicitationForm {
                        message: request.message.clone(),
                        requested_schema: request.requested_schema.clone(),
                        source: request.source.clone(),
                    },
                    owner_authority: None,
                    run_id: Some(run_id_clone.clone()),
                    tool_call_id: Some(tool_call_id_clone.clone()),
                },
                request.response_tx,
            )
            .await;
            // Close the race where cancellation happens just before this request
            // reaches the shared pending map.
            if cancellation_token.is_cancelled() {
                crate::elicitation::cancel_pending_elicitations_for_session(
                    &pending_elicitations,
                    &session_id_clone,
                )
                .await;
                continue;
            }
            // Durable: elicitation must be visible in UI replay.
            if let Err(err) = event_sink
                .emit_durable(
                    &session_id_clone,
                    crate::events::AgentEventKind::ElicitationRequested {
                        elicitation_id,
                        session_id: session_id_clone.clone(),
                        message: request.message,
                        requested_schema: request.requested_schema,
                        source: request.source,
                    },
                )
                .await
            {
                log::warn!("failed to emit ElicitationRequested: {}", err);
            }
        }

        crate::elicitation::remove_pending_elicitations_for_tool(
            &pending_elicitations,
            &session_id_clone,
            &run_id_clone,
            &tool_call_id_clone,
        )
        .await;
    });

    exec_ctx
        .tool_context(config.agent_registry.clone(), Some(elicitation_tx))
        .with_workspace_service(config.workspace_service())
        .with_todo_store(config.todo_store.clone())
        .with_session_store(exec_ctx.state.store.clone())
        .with_task_service(crate::session::TaskService::new(
            exec_ctx.state.store.clone(),
            exec_ctx.session_id.clone(),
            call.id.clone(),
        ))
}

/// Outcome of an approval request.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Approval {
    Granted,
    Denied(Denial),
}

/// Why a call that needed approval will not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum Denial {
    #[error("rejected by the user")]
    Rejected,
    #[error("no answer within {0:?}")]
    TimedOut(Duration),
    #[error("the run was cancelled")]
    Cancelled,
    #[error("the task is not active")]
    TaskInactive,
    #[error("denied by hook: {0}")]
    Hook(String),
}

const OPTION_ALLOW_ONCE: &str = "allow_once";
const OPTION_ALLOW_SESSION: &str = "allow_session";
const OPTION_REJECT: &str = "reject";

/// Options offered for `need`: the session option only for list-driven asks.
fn permission_options(need: &ApprovalNeed) -> Vec<crate::acp::protocol::PermissionOption> {
    use crate::acp::protocol::{PermissionOption, PermissionOptionId, PermissionOptionKind};
    let mut options = vec![PermissionOption::new(
        PermissionOptionId::from(OPTION_ALLOW_ONCE),
        "Allow once",
        PermissionOptionKind::AllowOnce,
    )];
    if matches!(need, ApprovalNeed::Session) {
        options.push(PermissionOption::new(
            PermissionOptionId::from(OPTION_ALLOW_SESSION),
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ));
    }
    options.push(PermissionOption::new(
        PermissionOptionId::from(OPTION_REJECT),
        "Reject",
        PermissionOptionKind::RejectOnce,
    ));
    options
}

/// Resolves after `timeout`, or never when there is none.
async fn sleep_or_pending(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

/// Decide whether `tool` may run, asking the user when `need` requires it.
///
/// Order: no need, session grant, inactive task, `permission_request` hooks,
/// then the user prompt. The prompt waits for an answer, run cancellation, or
/// the configured timeout, and always retires its pending entry.
#[instrument(
    name = "agent.tool.permission",
    skip_all,
    fields(
        session_id = %exec_ctx.session_id,
        tool_name = %tool.name,
        tool_call_id = %call_id,
        session_grant = tracing::field::Empty,
        granted = tracing::field::Empty
    )
)]
pub(super) async fn request_approval(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call_id: &str,
    tool: ToolRef<'_>,
    args: &serde_json::Value,
    need: ApprovalNeed,
) -> Result<Approval, agent_client_protocol::Error> {
    let span = Span::current();
    let approval = decide_approval(config, exec_ctx, call_id, tool, args, need).await?;
    span.record("granted", matches!(approval, Approval::Granted));
    Ok(approval)
}

async fn decide_approval(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call_id: &str,
    tool: ToolRef<'_>,
    args: &serde_json::Value,
    need: ApprovalNeed,
) -> Result<Approval, agent_client_protocol::Error> {
    if matches!(need, ApprovalNeed::None) {
        return Ok(Approval::Granted);
    }

    if matches!(need, ApprovalNeed::Session) {
        let granted = exec_ctx.runtime.session_grants.lock().contains(tool);
        Span::current().record("session_grant", granted);
        if granted {
            return Ok(Approval::Granted);
        }
    }

    if let Some(task) = &exec_ctx.state.active_task
        && task.status != TaskStatus::Active
    {
        return Ok(Approval::Denied(Denial::TaskInactive));
    }

    let turn_id = exec_ctx.turn_id().to_string();
    let hook_decision = config
        .hooks
        .run_permission_request(crate::hooks::PermissionRequestRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: turn_id.clone(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: exec_ctx
                .llm_config()
                .map(|cfg| cfg.model.clone())
                .unwrap_or_default(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            tool_name: tool.name.to_string(),
            tool_input: args.clone(),
        })
        .await
        .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
    for notice in hook_decision.notices {
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
    match hook_decision.decision {
        Some(PermissionRequestDecision::Allow) => return Ok(Approval::Granted),
        Some(PermissionRequestDecision::Deny { message }) => {
            log::debug!(
                "Session {}: permission hook denied {}: {}",
                exec_ctx.session_id,
                tool,
                message
            );
            return Ok(Approval::Denied(Denial::Hook(message)));
        }
        None => {}
    }

    Ok(prompt_user(config, exec_ctx, call_id, tool, args, &need, &turn_id).await)
}

/// Register a permission interaction, announce it, and wait for the outcome.
async fn prompt_user(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    call_id: &str,
    tool: ToolRef<'_>,
    args: &serde_json::Value,
    need: &ApprovalNeed,
    turn_id: &str,
) -> Approval {
    use crate::acp::protocol::{
        RequestPermissionRequest, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };
    use crate::agent::utils::{extract_locations, tool_kind_for_tool};

    let hook_reason = match need {
        ApprovalNeed::EveryCall { reason } => reason.as_deref().filter(|r| !r.is_empty()),
        _ => None,
    };

    let locations = extract_locations(args);
    let tool_update_fields = ToolCallUpdateFields::new()
        .title(
            hook_reason
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Run {tool}")),
        )
        .kind(tool_kind_for_tool(tool.name))
        .status(ToolCallStatus::Pending)
        .locations(if locations.is_empty() {
            None
        } else {
            Some(locations)
        })
        .raw_input(args.clone());

    // Unique interaction ID; the tool-call ID is carried separately in the
    // request. The ID also rides on the wire request so any client can key its
    // card by it and match it against the recovery pending set.
    let permission_id = uuid::Uuid::now_v7().to_string();
    let mut request_meta = serde_json::Map::new();
    request_meta.insert(
        "querymt".to_string(),
        serde_json::json!({ "interaction_id": permission_id }),
    );

    let request = RequestPermissionRequest::new(
        exec_ctx.session_id.clone(),
        ToolCallUpdate::new(ToolCallId::from(call_id.to_string()), tool_update_fields),
        permission_options(need),
    )
    .meta(request_meta);

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    crate::elicitation::register_pending_permission(
        &config.pending_elicitations,
        permission_id.clone(),
        exec_ctx.session_id.clone(),
        request.clone(),
        Some(turn_id.to_string()),
        Some(call_id.to_string()),
        response_tx,
    )
    .await;
    // Retire the pending entry if this future is dropped mid-wait.
    let _cleanup = PendingPermissionGuard {
        pending_map: config.pending_elicitations.clone(),
        interaction_id: permission_id.clone(),
    };

    // Durable so any attached client (local or remote) or a later attach
    // can discover and answer the request.
    if let Err(err) = config
        .event_sink
        .emit_durable(
            &exec_ctx.session_id,
            AgentEventKind::PermissionRequested {
                permission_id: permission_id.clone(),
                task_id: exec_ctx
                    .state
                    .active_task
                    .as_ref()
                    .map(|task| task.public_id.clone()),
                tool_name: tool.name.to_string(),
                reason: hook_reason
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Tool {tool} requires explicit permission")),
                tool_call_id: call_id.to_string(),
                request: Some(request),
            },
        )
        .await
    {
        // The pending entry stays discoverable through ListPendingInteractions.
        log::warn!("failed to emit PermissionRequested: {}", err);
    }

    // Wait for an explicit answer, run cancellation or the timeout. No client
    // attached, delivery failure and disconnect all keep the request pending;
    // only an explicit selected option grants.
    let timeout = config.permissions.timeout;
    let answer = tokio::select! {
        response = response_rx => response.map_err(|_| Denial::Cancelled),
        _ = exec_ctx.cancellation_token.cancelled() => Err(Denial::Cancelled),
        _ = sleep_or_pending(timeout) => Err(Denial::TimedOut(timeout.unwrap_or_default())),
    };

    // Retire the request before reporting, so a late answer is never pending.
    config
        .pending_elicitations
        .lock()
        .await
        .remove(&permission_id);

    // `Ok(true)` = allowed for the session. Decline, cancel and unknown
    // options never grant.
    let decision = answer.and_then(|response| match response.selected_permission_option() {
        Some(OPTION_ALLOW_ONCE) => Ok(false),
        Some(OPTION_ALLOW_SESSION) => Ok(true),
        _ => Err(Denial::Rejected),
    });

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::PermissionGranted {
            permission_id,
            granted: decision.is_ok(),
        },
    );

    match decision {
        Ok(session_wide) => {
            if session_wide && matches!(need, ApprovalNeed::Session) {
                exec_ctx.runtime.session_grants.lock().insert(tool);
            }
            Approval::Granted
        }
        Err(denial) => Approval::Denied(denial),
    }
}

/// Removes a pending permission interaction when the waiting tool call exits.
struct PendingPermissionGuard {
    pending_map: crate::elicitation::PendingElicitationMap,
    interaction_id: String,
}

impl Drop for PendingPermissionGuard {
    fn drop(&mut self) {
        let pending_map = self.pending_map.clone();
        let interaction_id = std::mem::take(&mut self.interaction_id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                pending_map.lock().await.remove(&interaction_id);
            });
        }
    }
}

/// Record side effects of a tool execution (artifacts, delegations).
///
/// Returns a wait condition if the tool initiated an action that requires waiting
/// (e.g., delegation).
#[instrument(
    name = "agent.tool.side_effects",
    skip(config, result, exec_ctx),
    fields(
        session_id = %exec_ctx.session_id,
        tool_call_id = %result.call_id,
        tool_name = result.tool_name.as_deref().unwrap_or("unknown")
    )
)]
pub(super) async fn record_tool_side_effects(
    config: &AgentConfig,
    result: &mut ToolResult,
    exec_ctx: &ExecutionContext,
    batch_id: Option<&str>,
) -> Result<Option<WaitCondition>, anyhow::Error> {
    if result.execution_is_error {
        return Ok(None);
    }

    let Some(tool_name) = result.tool_name.as_ref() else {
        return Ok(None);
    };

    if tool_name == "write_file" || tool_name == "apply_patch" {
        let args: serde_json::Value =
            serde_json::from_str(result.tool_arguments.as_deref().unwrap_or("{}"))
                .unwrap_or_default();
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Ok(artifact) = exec_ctx
            .state
            .record_artifact(
                "file".to_string(),
                None,
                path.clone(),
                Some(format!("Produced by {}", tool_name)),
            )
            .await
        {
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::ArtifactRecorded { artifact },
            );
        }
    }

    if tool_name == "delegate" {
        let args: serde_json::Value =
            serde_json::from_str(result.tool_arguments.as_deref().unwrap_or("{}"))
                .unwrap_or_default();
        let target_agent_id = args
            .get("target_agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let objective = args
            .get("objective")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let context_val = args
            .get("context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let constraints = args
            .get("constraints")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let expected_output = args
            .get("expected_output")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let isolation = args
            .get("isolation")
            .and_then(|v| v.as_str())
            .map(|s| s.parse::<crate::worktree::DelegationIsolation>())
            .transpose()
            .unwrap_or_default()
            .unwrap_or_default();

        let pre_hook = config
            .hooks
            .run_pre_delegation(PreDelegationRequest {
                session_id: exec_ctx.session_id.clone(),
                turn_id: exec_ctx.turn_id().to_string(),
                cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
                model: exec_ctx
                    .llm_config()
                    .map(|cfg| cfg.model.clone())
                    .unwrap_or_default(),
                permission_mode: exec_ctx.permission_mode().to_string(),
                tool_use_id: result.call_id.clone(),
                target_agent_id: target_agent_id.clone(),
                objective: objective.clone(),
                context: context_val.clone(),
                constraints: constraints.clone(),
                expected_output: expected_output.clone(),
                isolation,
            })
            .await?;
        for notice in pre_hook.notices {
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::HookNotice {
                    event_name: notice.event_name,
                    message: notice.message,
                    is_error: notice.is_error,
                },
            );
        }

        let UpdatedDelegation {
            target_agent_id: updated_target_agent_id,
            objective: updated_objective,
            context: updated_context,
            constraints: updated_constraints,
            expected_output: updated_expected_output,
            isolation: updated_isolation,
        } = pre_hook.updated_delegation.unwrap_or_default();

        let target_agent_id = updated_target_agent_id.unwrap_or(target_agent_id);
        let objective = updated_objective.unwrap_or(objective);
        let mut context_val = updated_context.or(context_val);
        let constraints = updated_constraints.or(constraints);
        let expected_output = updated_expected_output.or(expected_output);
        let requested_isolation = updated_isolation.unwrap_or(isolation);

        if !pre_hook.additional_contexts.is_empty() {
            let extra_context = pre_hook.additional_contexts.join("\n\n");
            context_val = Some(match context_val {
                Some(existing) if !existing.is_empty() => {
                    format!("{}\n\n{}", existing, extra_context)
                }
                _ => extra_context,
            });
        }

        if pre_hook.should_block {
            let reason = pre_hook
                .block_reason
                .unwrap_or_else(|| "delegation blocked by hook".to_string());
            result.content = vec![querymt::chat::ToolResultPart::Text {
                text: format!("Delegation blocked by hook: {}", reason),
            }];
            result.is_error = false;
            return Ok(None);
        }

        // Resolve the delegation isolation policy (design decision 2): an
        // explicit request wins; an inherited request follows the parent's
        // execution mode. The resolved policy is persisted with the request
        // and disclosed to the caller.
        let parent_mode = match config.workspace_service() {
            Some(service) => match service.session_execution_mode(&exec_ctx.session_id).await {
                Ok(mode) => mode,
                Err(err) => {
                    return Err(anyhow::anyhow!(
                        "cannot resolve delegation workspace policy: {err}"
                    ));
                }
            },
            None => crate::worktree::ExecutionMode::Local,
        };
        let configured_isolation = config
            .agent_registry
            .get_agent(&target_agent_id)
            .map(|info| info.isolation)
            .unwrap_or_default();
        let (resolved_isolation, isolation_source) = crate::worktree::resolve_delegation_policy(
            parent_mode,
            requested_isolation,
            configured_isolation,
        );

        if let Ok(serialized_args) = serde_json::to_string(&serde_json::json!({
            "target_agent_id": target_agent_id,
            "objective": objective,
            "context": context_val,
            "constraints": constraints,
            "expected_output": expected_output,
            "isolation": resolved_isolation_label(resolved_isolation),
            "isolation_source": isolation_source.label(),
        })) {
            result.tool_arguments = Some(serialized_args);
        }

        // Disclose the resolved execution mode: shared execution is never
        // presented as isolated and explicit overrides are visible.
        result.content.push(querymt::chat::ToolResultPart::Text {
            text: format!(
                "Execution workspace ({} policy): {}.",
                isolation_source.label(),
                match resolved_isolation {
                    crate::worktree::ResolvedIsolation::Isolated => {
                        "isolated (private Git worktree checkout seeded from this session's current code state)"
                    }
                    crate::worktree::ResolvedIsolation::Shared => {
                        "shared (this session's checkout; an explicit shared override is disclosed as shared, never isolated)"
                    }
                }
            ),
        });

        if let Ok(delegation) = exec_ctx
            .state
            .record_delegation(
                target_agent_id.clone(),
                objective.clone(),
                context_val.clone(),
                constraints,
                expected_output,
                match resolved_isolation {
                    crate::worktree::ResolvedIsolation::Isolated => {
                        crate::worktree::DelegationIsolation::Isolated
                    }
                    crate::worktree::ResolvedIsolation::Shared => {
                        crate::worktree::DelegationIsolation::Shared
                    }
                },
            )
            .await
        {
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::DelegationRequested {
                    delegation: delegation.clone(),
                    tool_call_id: Some(result.call_id.clone()),
                    batch_id: batch_id.map(str::to_owned),
                    cycle_id: exec_ctx
                        .cycle_accounting
                        .get()
                        .map(|cycle| cycle.cycle_id().to_string()),
                },
            );
            return Ok(Some(WaitCondition::delegation(
                delegation.public_id.clone(),
            )));
        }
    }

    Ok(None)
}

/// Return `results` stable-sorted by the position of each `call_id` in the last
/// assistant message's tool calls.
///
/// Results whose id is not among those calls (or all results, when there is no
/// assistant tool-call message) keep their relative order and go at the end.
fn order_results_by_call_position(
    results: &[ToolResult],
    messages: &[querymt::chat::ChatMessage],
) -> Vec<ToolResult> {
    let mut ordered = results.to_vec();
    let Some(assistant) = messages
        .iter()
        .rfind(|message| message.role == ChatRole::Assistant)
    else {
        return ordered;
    };

    let mut positions: HashMap<&str, usize> = HashMap::new();
    for (position, call) in assistant.function_calls().enumerate() {
        // First occurrence wins if an id is (unexpectedly) repeated.
        positions.entry(call.call_id.as_str()).or_insert(position);
    }
    // `sort_by_key` is stable, so unknown ids (usize::MAX) keep their order.
    ordered.sort_by_key(|result| {
        positions
            .get(result.call_id.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });
    ordered
}

/// Serialize the resolved isolation for persisted tool arguments.
fn resolved_isolation_label(resolved: crate::worktree::ResolvedIsolation) -> &'static str {
    match resolved {
        crate::worktree::ResolvedIsolation::Shared => "shared",
        crate::worktree::ResolvedIsolation::Isolated => "isolated",
    }
}

/// Store all completed tool results back into conversation history.
///
/// This function:
/// 1. Creates user messages with tool results
/// 2. Stores them in the database
/// 3. Records side effects (artifacts, delegations)
/// 4. Aggregates file changes for deduplication
/// 5. Returns either WaitingForEvent (if delegation) or BeforeLlmCall
#[instrument(
    name = "agent.tools.store_all_results",
    skip(config, results, context, exec_ctx),
    fields(session_id = %exec_ctx.session_id, result_count = results.len())
)]
pub(super) async fn store_all_tool_results(
    config: &AgentConfig,
    results: &Arc<[ToolResult]>,
    context: &Arc<crate::middleware::ConversationContext>,
    exec_ctx: &mut ExecutionContext,
) -> Result<ExecutionState, anyhow::Error> {
    debug!(
        "Storing all tool results: session={}, count={}",
        exec_ctx.session_id,
        results.len()
    );

    let mut messages = (*context.messages).to_vec();
    let mut wait_conditions = Vec::new();

    // Middleware may pre-fill `results` (e.g. rejected calls), so the incoming
    // order is not necessarily the order of the assistant's tool calls. Store
    // results in call order.
    let ordered_results = order_results_by_call_position(results, &context.messages);

    // Record per-result side effects (delegations, artifacts, etc.) before
    // persisting the combined tool_result message so hook-driven rewrites are
    // visible to the model in the stored transcript.
    let batch_id = Uuid::new_v4().to_string();
    crate::agent::repository_delivery::restore_state(config, exec_ctx).await;
    let mut adjusted_results = Vec::with_capacity(ordered_results.len());
    for mut result in ordered_results.iter().cloned() {
        if let Some(wait_condition) =
            record_tool_side_effects(config, &mut result, exec_ctx, Some(&batch_id)).await?
        {
            wait_conditions.push(wait_condition);
        }
        // Native repository cards draw from the turn's shared budget in
        // stable tool-call order (not completion order). Cards that do not
        // fit are dropped whole, so they are never recorded as delivered.
        let contexts = std::mem::take(&mut result.hook_contexts);
        result.hook_contexts =
            crate::agent::repository_delivery::allocate(config, exec_ctx, contexts);
        adjusted_results.push(result);
    }

    // Collect all tool results into a single User message so that the LLM API
    // sees every tool_result block immediately after the assistant's tool_use
    // blocks. Splitting them across multiple consecutive User messages violates
    // the Anthropic API contract (and potentially other providers').
    let mut result_parts: Vec<_> = adjusted_results
        .iter()
        .map(|result| MessagePart::ToolResult {
            call_id: result.call_id.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
            tool_name: result.tool_name.clone(),
            tool_arguments: result.tool_arguments.clone(),
            compacted_at: None,
        })
        .collect();
    let hook_context_parts = adjusted_results.iter().flat_map(|result| {
        result
            .hook_contexts
            .iter()
            .map(|context| MessagePart::HookContext {
                event_name: context.event_name.clone(),
                handler_id: context.handler_id.clone(),
                tool_use_id: context.tool_use_id.clone(),
                content: context.content.clone(),
            })
    });
    result_parts.extend(hook_context_parts);
    let snapshot_parts: Vec<_> = adjusted_results
        .iter()
        .filter_map(|result| result.snapshot_part.clone())
        .collect();
    result_parts.extend(snapshot_parts);

    let result_msg = AgentMessage {
        id: Uuid::new_v4().to_string(),
        session_id: exec_ctx.session_id.clone(),
        role: ChatRole::User,
        parts: result_parts,
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };

    exec_ctx
        .add_message(result_msg.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to store tool results: {}", e))?;

    // Steering may have been accepted after the assistant requested these tools.
    // Persist its original order, but keep the request's result batch adjacent
    // to the assistant instead of inserting a user message between them.
    let result_index = messages
        .iter()
        .rposition(|message| message.role == ChatRole::Assistant)
        .map_or(messages.len(), |index| index + 1);
    messages.insert(
        result_index,
        result_msg
            .to_chat_message()
            .map_err(|error| anyhow::anyhow!("Invalid stored tool result content: {error}"))?,
    );

    let new_context = Arc::new(
        crate::middleware::ConversationContext::new(
            context.session_id.clone(),
            Arc::from(messages.into_boxed_slice()),
            context.stats.clone(),
            context.provider.clone(),
            context.model.clone(),
        )
        .with_session_mode(context.session_mode)
        .with_profile(context.profile.clone())
        .with_last_applied(context.last_applied),
    );

    // Aggregate changed file paths from tool results for dedup check
    let mut combined = crate::index::DiffPaths::default();
    for result in ordered_results.iter() {
        if let Some(ref snapshot) = result.snapshot_part
            && let Some(paths) = snapshot.changed_paths()
        {
            combined.merge(paths.clone());
        }
    }

    if !combined.is_empty() {
        exec_ctx.runtime.turn_diffs.lock().merge(combined);
    }

    if let Some(wait_condition) = WaitCondition::merge(wait_conditions) {
        return Ok(ExecutionState::WaitingForEvent {
            context: new_context,
            wait: wait_condition,
        });
    }

    Ok(ExecutionState::BeforeLlmCall {
        context: new_context,
    })
}

#[cfg(test)]
mod result_order_tests {
    use super::*;
    use querymt::chat::ChatMessage;

    fn result(call_id: &str) -> ToolResult {
        ToolResult::new(
            call_id.to_string(),
            vec![querymt::chat::ToolResultPart::text(call_id)],
            false,
            Some("tool".to_string()),
            None,
        )
    }

    fn ids(results: &[ToolResult]) -> Vec<&str> {
        results.iter().map(|r| r.call_id.as_str()).collect()
    }

    fn assistant_with_calls(ids: &[&str]) -> ChatMessage {
        let mut builder = ChatMessage::assistant();
        for id in ids {
            builder = builder.tool_use(*id, "edit", serde_json::json!({}));
        }
        builder.build()
    }

    #[test]
    fn prefilled_out_of_order_results_follow_call_order() {
        let messages = [assistant_with_calls(&["c1", "c2", "c3"])];
        // Deny-mode style: the rejected c2 was pre-filled before c1/c3 ran.
        let results = [result("c2"), result("c3"), result("c1")];

        let ordered = order_results_by_call_position(&results, &messages);

        assert_eq!(ids(&ordered), ["c1", "c2", "c3"]);
    }

    #[test]
    fn unknown_ids_keep_relative_order_at_the_end() {
        let messages = [assistant_with_calls(&["c1", "c2"])];
        let results = [result("x2"), result("c2"), result("x1"), result("c1")];

        let ordered = order_results_by_call_position(&results, &messages);

        assert_eq!(ids(&ordered), ["c1", "c2", "x2", "x1"]);
    }

    #[test]
    fn only_the_last_assistant_message_defines_order() {
        let messages = [
            assistant_with_calls(&["old1", "old2"]),
            assistant_with_calls(&["b", "a"]),
        ];
        let results = [result("a"), result("b")];

        let ordered = order_results_by_call_position(&results, &messages);

        assert_eq!(ids(&ordered), ["b", "a"]);
    }

    #[test]
    fn without_an_assistant_message_order_is_unchanged() {
        let results = [result("b"), result("a")];

        let ordered = order_results_by_call_position(&results, &[]);

        assert_eq!(ids(&ordered), ["b", "a"]);
    }
}

#[cfg(test)]
mod genai_trace_tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::hooks::{Hooks, HooksConfig};
    use crate::session::backend::StorageBackend;
    use crate::test_utils::helpers::genai_trace::{assert_private, attr, capture};
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
    use opentelemetry::{
        Value,
        trace::{SpanKind, Status},
    };

    fn hooks(event: &str, output: serde_json::Value) -> Hooks {
        let output = serde_json::to_string(&output).unwrap();
        Hooks::new(serde_json::from_value::<HooksConfig>(serde_json::json!({
            "enabled": true,
            event: [{"matcher": "^telemetry_tool$", "hooks": [{
                "type": "command", "command": format!("printf '%s' '{output}'"), "timeout_sec": 5
            }]}]
        })).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn genai_skill_identity_is_verified_after_hooks_on_the_existing_tool_span() {
        use crate::skills::{
            permissions::{PermissionLevel, SkillPermissions},
            registry::SkillRegistry,
            tool::SkillTool,
            types::SkillSource,
        };
        use crate::test_utils::helpers::genai_trace::capture_info;
        use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry};

        struct UnrelatedSkill;
        #[async_trait::async_trait]
        impl Tool for UnrelatedSkill {
            fn name(&self) -> &str {
                "skill"
            }
            fn definition(&self) -> querymt::chat::Tool {
                querymt::chat::Tool {
                    tool_type: "function".into(),
                    function: querymt::chat::FunctionTool {
                        name: "skill".into(),
                        description: "".into(),
                        parameters: serde_json::json!({"type":"object"}),
                        strict: None,
                    },
                }
            }
            async fn call(
                &self,
                _: serde_json::Value,
                _: &dyn ToolContext,
            ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError> {
                Ok(vec![querymt::chat::ToolResultPart::text("SECRET_RESPONSE")])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("catalog");
        std::fs::create_dir(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        std::fs::write(&path, "---\nid: catalog.review\nname: Code Review\ndescription: SECRET_ARGUMENT\n---\nSECRET_RESPONSE\n").unwrap();
        let mut readonly = std::fs::metadata(&path).unwrap().permissions();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly).unwrap();
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let exec = fixture.execution_context().await;
        for mode in ["success", "rewrite", "unknown", "blocked", "custom"] {
            let mut permissions = SkillPermissions::default();
            if mode == "blocked" {
                permissions
                    .patterns
                    .insert("catalog.review".into(), PermissionLevel::Deny);
            }
            let mut registry = ToolRegistry::new();
            if mode == "custom" {
                registry.add(Arc::new(UnrelatedSkill));
            } else {
                registry.add(Arc::new(SkillTool::new_with_fallback(
                    Arc::new(std::sync::Mutex::new(SkillRegistry::new())),
                    Arc::new(permissions),
                    vec![SkillSource::Configured(dir.path().to_owned())],
                    false,
                    dir.path().to_owned(),
                )));
            }
            let mut builder = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                Arc::new(
                    (*fixture.config.provider)
                        .clone()
                        .with_agent_id(Some("reader".into())),
                ),
                fixture.storage.event_journal(),
            )
            .with_tool_registry(registry);
            if mode == "rewrite" {
                builder = builder.with_hooks(Hooks::new(serde_json::from_value::<HooksConfig>(serde_json::json!({
                    "enabled": true, "pre_tool_use": [{"matcher":"^skill$", "hooks":[{
                        "type":"command", "command":"printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"pre_tool_use\",\"permission_decision\":\"allow\",\"updated_input\":{\"name\":\"catalog.review\"}}}'", "timeout_sec":5
                    }]}]
                })).unwrap()).unwrap());
            }
            let config = builder.build();
            let requested = if matches!(mode, "unknown" | "rewrite") {
                "SECRET_PROMPT"
            } else {
                "catalog.review"
            };
            let call = mock_tool_call(
                mode,
                "skill",
                &serde_json::json!({"name":requested}).to_string(),
            );
            let (result, spans) = capture_info(execute_tool_call(&config, &call, &exec)).await;
            let verified = matches!(mode, "success" | "rewrite");
            let result = result.unwrap();
            assert_eq!(
                result.is_error,
                matches!(mode, "unknown" | "blocked"),
                "mode={mode} source={}",
                result.tool_source
            );
            let semantic: Vec<_> = spans
                .iter()
                .filter(|span| {
                    attr(span, "gen_ai.operation.name") == Some(&Value::from("execute_tool"))
                })
                .collect();
            assert_eq!(semantic.len(), 1);
            let tool = semantic[0];
            assert_eq!(
                attr(tool, "querymt.tool.execution"),
                Some(&Value::from(match mode {
                    "blocked" => "permission_denied",
                    "unknown" => "validation_rejected",
                    _ => "executed",
                }))
            );
            assert_eq!(
                attr(tool, "error.type"),
                match mode {
                    "blocked" => Some(Value::from("permission_denied")),
                    "unknown" => Some(Value::from("invalid_tool_arguments")),
                    _ => None,
                }
                .as_ref()
            );
            assert_eq!(
                tool.name,
                if verified {
                    "execute_tool skill Code Review"
                } else {
                    "execute_tool skill"
                }
            );
            assert_eq!(
                attr(tool, "gen_ai.skill.name"),
                verified.then_some(&Value::from("Code Review"))
            );
            assert_eq!(attr(tool, "gen_ai.agent.id"), Some(&Value::from("reader")));
            assert!(attr(tool, "gen_ai.agent.name").is_none());
            for diagnostic in spans
                .iter()
                .filter(|span| span.span_context.span_id() != tool.span_context.span_id())
            {
                assert!(attr(diagnostic, "gen_ai.skill.name").is_none());
            }
            assert_private(&spans);
            assert!(
                !format!("{spans:?}").contains(dir.path().to_str().unwrap()),
                "mode={mode}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn genai_shell_refines_only_spawned_builtin_on_existing_span() {
        use crate::test_utils::helpers::genai_trace::capture_info;
        use crate::tools::{
            Tool, ToolContext, ToolError, ToolRegistry, builtins::shell::ShellTool,
        };
        use std::os::unix::fs::PermissionsExt;

        struct CustomShell;
        #[async_trait::async_trait]
        impl Tool for CustomShell {
            fn name(&self) -> &str {
                "shell"
            }
            fn definition(&self) -> querymt::chat::Tool {
                ShellTool::new().definition()
            }
            async fn call(
                &self,
                _: serde_json::Value,
                _: &dyn ToolContext,
            ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError> {
                Ok(vec![querymt::chat::ToolResultPart::text("SECRET_RESPONSE")])
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("sh"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        let executable = shell.file_name().unwrap().to_str().unwrap();
        let symlink = dir.path().join("SECRET_ARGUMENT-link");
        std::os::unix::fs::symlink(&shell, &symlink).unwrap();
        let script = dir.path().join("SECRET_ARGUMENT-script");
        std::fs::write(
            &script,
            format!(
                "#!{}\nsleep 0.2\nprintf SECRET_RESPONSE\nprintf SECRET_ERROR >&2\nexit 7\n",
                shell.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let mut exec = fixture.execution_context().await;
        for mode in [
            "direct",
            "symlink",
            "script",
            "wrapper",
            "signal",
            "spawn_failure",
            "blocked",
            "policy_blocked",
            "custom",
        ] {
            let mut registry = ToolRegistry::new();
            if mode == "custom" {
                registry.add(Arc::new(CustomShell));
            } else {
                registry.add(Arc::new(ShellTool::new()));
            }
            exec.tool_config.deny = Default::default();
            if mode == "policy_blocked" {
                exec.tool_config.deny = ["shell".parse().unwrap()].into_iter().collect();
            }
            let mut builder = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                fixture.config.provider.clone(),
                fixture.storage.event_journal(),
            )
            .with_tool_registry(registry);
            if mode == "blocked" {
                builder = builder.with_hooks(Hooks::new(serde_json::from_value::<HooksConfig>(serde_json::json!({
                    "enabled": true, "pre_tool_use": [{"matcher": "^shell$", "hooks": [{"type": "command",
                    "command": "printf '%s' '{\"hook_specific_output\":{\"hook_event_name\":\"pre_tool_use\",\"permission_decision\":\"deny\"}}'", "timeout_sec": 5}]}],
                })).unwrap()).unwrap());
            }
            let config = builder.build();
            let command = match mode {
                "symlink" => symlink.to_str().unwrap(),
                "script" => script.to_str().unwrap(),
                "spawn_failure" => "SECRET_ARGUMENT-missing-executable",
                _ => shell.to_str().unwrap(),
            };
            let mut args = serde_json::json!({"command": command, "args": ["-c", "sleep 0.2; printf SECRET_RESPONSE; printf SECRET_ERROR >&2; exit 0"], "workdir": dir.path()});
            if mode == "script" {
                args["args"] = serde_json::json!([]);
            }
            if mode == "wrapper" {
                args = serde_json::json!({"command": "sleep 0.2; printf SECRET_RESPONSE; printf SECRET_ERROR >&2; exit 7", "workdir": dir.path()});
            }
            if mode == "signal" {
                args["args"] = serde_json::json!(["-c", "sleep 0.2; kill -TERM $$"]);
            }
            let call = mock_tool_call(mode, "shell", &args.to_string());
            let (result, spans) = capture_info(execute_tool_call(&config, &call, &exec)).await;
            let result = result.unwrap();
            let ran = matches!(mode, "direct" | "symlink" | "script" | "wrapper" | "signal");
            assert_eq!(
                result.is_error,
                matches!(mode, "spawn_failure" | "blocked" | "policy_blocked"),
                "mode={mode}"
            );
            let semantic: Vec<_> = spans
                .iter()
                .filter(|span| {
                    attr(span, "gen_ai.operation.name") == Some(&Value::from("execute_tool"))
                })
                .collect();
            assert_eq!(semantic.len(), 1, "mode={mode}");
            let tool = semantic[0];
            assert_eq!(tool.span_kind, SpanKind::Internal);
            assert_eq!(
                tool.name,
                if ran {
                    format!("execute_tool shell {executable}")
                } else {
                    "execute_tool shell".into()
                },
                "mode={mode}"
            );
            assert_eq!(attr(tool, "gen_ai.tool.name"), Some(&Value::from("shell")));
            assert_eq!(
                attr(tool, "process.executable.name"),
                ran.then_some(&Value::from(executable.to_owned())),
                "mode={mode}"
            );
            let exit = match mode {
                "direct" | "symlink" => Some(0),
                "script" | "wrapper" => Some(7),
                _ => None,
            };
            assert_eq!(
                attr(tool, "process.exit.code"),
                exit.map(Value::I64).as_ref(),
                "mode={mode}"
            );
            if ran {
                assert_eq!(tool.status, Status::Unset);
                let text = result.content[0].as_text().unwrap();
                let output: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(output["exit_code"], exit.unwrap_or(-1));
            }
            for diagnostic in spans
                .iter()
                .filter(|span| span.span_context.span_id() != tool.span_context.span_id())
            {
                assert!(attr(diagnostic, "process.executable.name").is_none());
                assert!(attr(diagnostic, "process.exit.code").is_none());
            }
            assert_private(&spans);
            let exported = format!("{spans:?}");
            assert!(
                !exported.contains(dir.path().to_str().unwrap()),
                "mode={mode}"
            );
            assert!(!exported.contains(shell.to_str().unwrap()), "mode={mode}");
        }
    }

    #[tokio::test]
    async fn genai_tool_status_uses_execution_truth_not_post_hook_projection() {
        for execution_failed in [false, true] {
            let mut mock = MockLlmProvider::new();
            mock.expect_call_tool().times(1).returning(move |_, _| {
                if execution_failed {
                    Err(querymt::error::LLMError::ProviderError(
                        "SECRET_ERROR".into(),
                    ))
                } else {
                    Ok(vec![querymt::chat::ToolResultPart::text("SECRET_RESPONSE")])
                }
            });
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            let config = AgentConfigBuilder::from_provider(
                fixture.storage.clone(), fixture.config.provider.clone(), fixture.storage.event_journal(),
            ).with_hooks(hooks("post_tool_use", serde_json::json!({
                "hook_specific_output": {"hook_event_name": "post_tool_use", "updated_output": {"is_error": !execution_failed}}
            }))).build();
            let call = mock_tool_call(
                "call-1",
                "telemetry_tool",
                r#"{"secret":"SECRET_ARGUMENT"}"#,
            );
            let (result, spans) = capture(async {
                execute_tool_call(&config, &call, &exec)
                    .instrument(info_span!("test-parent"))
                    .await
            })
            .await;
            let result = result.unwrap();
            assert_eq!(result.is_error, !execution_failed);
            assert_eq!(result.execution_is_error, execution_failed);
            let tool = spans
                .iter()
                .find(|s| s.name == "execute_tool telemetry_tool")
                .unwrap();
            assert_eq!(tool.span_kind, SpanKind::Internal);
            assert_eq!(
                attr(tool, "session.id"),
                Some(&Value::from(exec.session_id.clone()))
            );
            assert_eq!(
                attr(tool, "session.id"),
                attr(tool, "gen_ai.conversation.id")
            );
            assert_eq!(
                attr(tool, "gen_ai.tool.call.id"),
                Some(&Value::from("call-1"))
            );
            assert_eq!(
                attr(tool, "querymt.tool.execution"),
                Some(&Value::from("executed"))
            );
            assert_eq!(
                tool.parent_span_id,
                spans
                    .iter()
                    .find(|s| s.name == "test-parent")
                    .unwrap()
                    .span_context
                    .span_id()
            );
            assert_eq!(
                tool.status,
                if execution_failed {
                    Status::error("")
                } else {
                    Status::Unset
                }
            );
            assert_eq!(
                attr(tool, "error.type"),
                execution_failed.then_some(&Value::from("tool_error"))
            );
            assert!(attr(tool, "call").is_none());
            assert_private(&spans);
        }
    }

    #[tokio::test]
    async fn genai_pre_tool_hook_spawn_failure_is_not_tool_execution() {
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let mut exec = fixture.execution_context().await;
        exec.runtime = crate::agent::core::SessionRuntime::new(
            Some(fixture._tempdir.path().join("nonexistent-hook-cwd")),
            Default::default(),
            crate::agent::core::McpToolState::empty(),
        );
        let config = AgentConfigBuilder::from_provider(
            fixture.storage.clone(),
            fixture.config.provider.clone(),
            fixture.storage.event_journal(),
        )
        .with_hooks(hooks("pre_tool_use", serde_json::json!({})))
        .build();
        let call = mock_tool_call(
            "call-1",
            "telemetry_tool",
            r#"{"secret":"SECRET_ARGUMENT"}"#,
        );
        let (result, spans) = capture(execute_tool_call(&config, &call, &exec)).await;
        assert!(result.is_err());
        let tool = spans
            .iter()
            .find(|s| s.name == "execute_tool telemetry_tool")
            .unwrap();
        assert_eq!(
            attr(tool, "querymt.tool.execution"),
            Some(&Value::from("hook_failed"))
        );
        assert_eq!(
            attr(tool, "error.type"),
            Some(&Value::from("tool_pipeline_error"))
        );
        assert_eq!(tool.status, Status::error(""));
        assert_eq!(
            attr(tool, "session.id"),
            Some(&Value::from(exec.session_id.clone()))
        );
        assert_eq!(
            attr(tool, "session.id"),
            attr(tool, "gen_ai.conversation.id")
        );
        assert_private(&spans);
    }

    #[tokio::test]
    async fn genai_rejected_tools_are_distinguished_from_execution() {
        for hook_blocked in [false, true] {
            let mock = MockLlmProvider::new(); // No execution expectation: rejection must not call it.
            let fixture = TestAgent::with_mock_provider(SharedLlmProvider::new(mock, vec![])).await;
            let exec = fixture.execution_context().await;
            let mut builder = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                fixture.config.provider.clone(),
                fixture.storage.event_journal(),
            )
            .with_permissions(
                crate::config::Permissions::new()
                    .ask(["telemetry_tool".parse().unwrap()])
                    .timeout(std::time::Duration::from_millis(20)),
            );
            if hook_blocked {
                builder = builder.with_hooks(hooks("pre_tool_use", serde_json::json!({
                    "hook_specific_output": {"hook_event_name": "pre_tool_use", "permission_decision": "deny"}
                })));
            }
            let config = builder.build();
            let call = mock_tool_call(
                "call-1",
                "telemetry_tool",
                r#"{"secret":"SECRET_ARGUMENT"}"#,
            );
            let (result, spans) = capture(execute_tool_call(&config, &call, &exec)).await;
            assert!(result.unwrap().is_error);
            let tool = spans
                .iter()
                .find(|s| s.name == "execute_tool telemetry_tool")
                .unwrap();
            let reason = if hook_blocked {
                "hook_blocked"
            } else {
                "permission_denied"
            };
            assert_eq!(
                attr(tool, "querymt.tool.execution"),
                Some(&Value::from(reason))
            );
            assert_eq!(attr(tool, "error.type"), Some(&Value::from(reason)));
            assert_eq!(tool.status, Status::error(""));
            assert_eq!(
                attr(tool, "session.id"),
                Some(&Value::from(exec.session_id.clone()))
            );
            assert_eq!(
                attr(tool, "session.id"),
                attr(tool, "gen_ai.conversation.id")
            );
            assert_private(&spans);
        }
    }
}

#[cfg(test)]
mod permission_interaction_tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::elicitation::{ElicitationResponse, InteractionKind};
    use crate::hooks::{Hooks, HooksConfig};
    use crate::session::backend::StorageBackend;
    use crate::session::domain::{Task, TaskKind};
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent};

    const SHELL: ToolRef<'static> = ToolRef::builtin("shell");

    async fn fixture() -> (TestAgent, ExecutionContext) {
        let fixture =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let exec = fixture.execution_context().await;
        (fixture, exec)
    }

    fn every_call() -> ApprovalNeed {
        ApprovalNeed::EveryCall { reason: None }
    }

    async fn approve(
        config: &AgentConfig,
        exec: &ExecutionContext,
        call_id: &str,
        tool: ToolRef<'_>,
        need: ApprovalNeed,
    ) -> Approval {
        request_approval(config, exec, call_id, tool, &serde_json::json!({}), need)
            .await
            .unwrap()
    }

    /// Wait until a permission interaction for `tool_call_id` is pending and
    /// return its (unique) interaction ID.
    async fn wait_pending(config: &AgentConfig, tool_call_id: &str) -> String {
        for _ in 0..200 {
            if let Some((id, entry)) = config
                .pending_elicitations
                .lock()
                .await
                .iter()
                .find(|(_, entry)| entry.tool_call_id.as_deref() == Some(tool_call_id))
            {
                assert!(matches!(entry.kind, InteractionKind::Permission { .. }));
                assert_ne!(id, tool_call_id, "interaction id must be unique");
                return id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("permission interaction for tool call {tool_call_id} never became pending");
    }

    async fn answer(config: &AgentConfig, session_id: &str, id: &str, option: &str) {
        let resolution = crate::elicitation::resolve_in_map(
            &config.pending_elicitations,
            Some(session_id),
            id,
            "conn-1",
            None,
            ElicitationResponse::permission_selected(option),
        )
        .await
        .unwrap();
        assert_eq!(
            resolution,
            crate::elicitation::ElicitationResolution::Resolved
        );
    }

    /// Option IDs of the pending request `id`, in presentation order.
    async fn option_ids(config: &AgentConfig, id: &str) -> Vec<String> {
        let pending = config.pending_elicitations.lock().await;
        let InteractionKind::Permission { request } = &pending[id].kind else {
            panic!("expected a permission interaction");
        };
        request
            .options
            .iter()
            .map(|option| option.option_id.to_string())
            .collect()
    }

    /// Prompt for `call_id`, answer it with `option` and return the outcome.
    async fn prompt_and_answer(
        config: &AgentConfig,
        exec: &ExecutionContext,
        call_id: &str,
        tool: ToolRef<'_>,
        need: ApprovalNeed,
        option: &str,
    ) -> Approval {
        let request = approve(config, exec, call_id, tool, need);
        let driver = async {
            let id = wait_pending(config, call_id).await;
            answer(config, &exec.session_id, &id, option).await;
        };
        let (approval, ()) = tokio::join!(request, driver);
        approval
    }

    async fn pending_is_empty(config: &AgentConfig) -> bool {
        for _ in 0..200 {
            if config.pending_elicitations.lock().await.is_empty() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn assert_prompts(
        config: &AgentConfig,
        exec: &ExecutionContext,
        call_id: &str,
        tool: ToolRef<'_>,
        need: ApprovalNeed,
    ) {
        let outcome = prompt_and_answer(config, exec, call_id, tool, need, OPTION_REJECT).await;
        assert!(matches!(outcome, Approval::Denied(Denial::Rejected)));
    }

    #[tokio::test]
    async fn no_need_is_granted_without_a_prompt() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        let outcome = approve(config, &exec, "call-n", SHELL, ApprovalNeed::None).await;
        assert!(matches!(outcome, Approval::Granted));
        assert!(config.pending_elicitations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn waits_without_client_then_answer_decides() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        let request = approve(config, &exec, "call-1", SHELL, ApprovalNeed::Session);
        let driver = async {
            let id = wait_pending(config, "call-1").await;
            // No client answers yet: the call must still be waiting, not granted.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(config.pending_elicitations.lock().await.contains_key(&id));
            answer(config, &exec.session_id, &id, OPTION_ALLOW_ONCE).await;
        };
        let (approval, ()) = tokio::join!(request, driver);
        assert!(matches!(approval, Approval::Granted));
        assert!(pending_is_empty(config).await);
    }

    #[tokio::test]
    async fn request_carries_its_interaction_id_in_meta() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        let request = approve(config, &exec, "call-i", SHELL, ApprovalNeed::Session);
        let driver = async {
            let id = wait_pending(config, "call-i").await;
            let wire_id = {
                let pending = config.pending_elicitations.lock().await;
                let InteractionKind::Permission { request } = &pending[&id].kind else {
                    panic!("expected a permission interaction");
                };
                let wire = serde_json::to_value(request.as_ref()).unwrap();
                wire["_meta"]["querymt"]["interaction_id"]
                    .as_str()
                    .map(str::to_string)
            };
            assert_eq!(
                wire_id.as_deref(),
                Some(id.as_str()),
                "the wire request must carry the pending interaction ID"
            );
            answer(config, &exec.session_id, &id, OPTION_ALLOW_ONCE).await;
        };
        let (approval, ()) = tokio::join!(request, driver);
        assert!(matches!(approval, Approval::Granted));
    }

    #[tokio::test]
    async fn options_follow_the_need() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        let needs = [
            (
                ApprovalNeed::Session,
                vec!["allow_once", "allow_session", "reject"],
            ),
            (every_call(), vec!["allow_once", "reject"]),
            (
                ApprovalNeed::EveryCall {
                    reason: Some("EDIT a.rs".into()),
                },
                vec!["allow_once", "reject"],
            ),
        ];
        for (index, (need, expected)) in needs.into_iter().enumerate() {
            let call_id = format!("call-o{index}");
            let request = approve(config, &exec, &call_id, SHELL, need);
            let driver = async {
                let id = wait_pending(config, &call_id).await;
                assert_eq!(option_ids(config, &id).await, expected);
                answer(config, &exec.session_id, &id, OPTION_ALLOW_ONCE).await;
            };
            let (approval, ()) = tokio::join!(request, driver);
            assert!(matches!(approval, Approval::Granted));
        }
    }

    #[tokio::test]
    async fn hook_reason_is_the_request_title() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        let need = ApprovalNeed::EveryCall {
            reason: Some("EDIT a.rs".into()),
        };

        let request = approve(config, &exec, "call-t", SHELL, need);
        let driver = async {
            let id = wait_pending(config, "call-t").await;
            let title = {
                let pending = config.pending_elicitations.lock().await;
                let InteractionKind::Permission { request } = &pending[&id].kind else {
                    panic!("expected a permission interaction");
                };
                request.tool_call.fields.title.clone()
            };
            assert_eq!(title.as_deref(), Some("EDIT a.rs"));
            answer(config, &exec.session_id, &id, OPTION_ALLOW_ONCE).await;
        };
        let (approval, ()) = tokio::join!(request, driver);
        assert!(matches!(approval, Approval::Granted));
    }

    #[tokio::test]
    async fn reject_option_denies_call_and_is_not_remembered() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        assert_prompts(config, &exec, "call-r1", SHELL, ApprovalNeed::Session).await;
        // The same tool prompts again, and can then be allowed.
        let second = prompt_and_answer(
            config,
            &exec,
            "call-r2",
            SHELL,
            ApprovalNeed::Session,
            OPTION_ALLOW_ONCE,
        )
        .await;
        assert!(matches!(second, Approval::Granted));
    }

    #[tokio::test]
    async fn unknown_option_stays_pending() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        let request = approve(config, &exec, "call-m", SHELL, ApprovalNeed::Session);
        let driver = async {
            let id = wait_pending(config, "call-m").await;
            let err = crate::elicitation::resolve_in_map(
                &config.pending_elicitations,
                Some(&exec.session_id),
                &id,
                "conn-1",
                None,
                ElicitationResponse::permission_selected("not-an-option"),
            )
            .await;
            assert!(err.is_err(), "malformed answer must be rejected");
            assert!(config.pending_elicitations.lock().await.contains_key(&id));
            answer(config, &exec.session_id, &id, OPTION_ALLOW_ONCE).await;
        };
        let (approval, ()) = tokio::join!(request, driver);
        assert!(matches!(approval, Approval::Granted));
    }

    #[tokio::test]
    async fn cancel_while_waiting_does_not_grant() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        let request = approve(config, &exec, "call-c", SHELL, ApprovalNeed::Session);
        let driver = async {
            let id = wait_pending(config, "call-c").await;
            exec.cancellation_token.cancel();
            id
        };
        let (approval, id) = tokio::join!(request, driver);
        assert!(matches!(approval, Approval::Denied(Denial::Cancelled)));
        assert!(pending_is_empty(config).await);
        // A late answer is rejected as not pending.
        let late = crate::elicitation::resolve_in_map(
            &config.pending_elicitations,
            Some(&exec.session_id),
            &id,
            "conn-1",
            None,
            ElicitationResponse::permission_selected(OPTION_ALLOW_ONCE),
        )
        .await
        .unwrap();
        assert_eq!(late, crate::elicitation::ElicitationResolution::NotFound);
    }

    #[tokio::test]
    async fn allow_session_skips_later_prompts_for_the_same_tool() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();

        let first = prompt_and_answer(
            config,
            &exec,
            "call-a",
            SHELL,
            ApprovalNeed::Session,
            OPTION_ALLOW_SESSION,
        )
        .await;
        assert!(matches!(first, Approval::Granted));

        let second = approve(config, &exec, "call-b", SHELL, ApprovalNeed::Session).await;
        assert!(matches!(second, Approval::Granted));
        assert!(config.pending_elicitations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn allow_session_does_not_cover_every_call_needs() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        prompt_and_answer(
            config,
            &exec,
            "call-a",
            SHELL,
            ApprovalNeed::Session,
            OPTION_ALLOW_SESSION,
        )
        .await;

        // always_ask and hook asks still prompt, and never record a grant.
        assert_prompts(config, &exec, "call-e1", SHELL, every_call()).await;
        let hook_ask = ApprovalNeed::EveryCall {
            reason: Some("EDIT a.rs".into()),
        };
        assert_prompts(config, &exec, "call-e2", SHELL, hook_ask).await;
    }

    #[tokio::test]
    async fn every_call_never_records_a_session_grant() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        let first = prompt_and_answer(
            config,
            &exec,
            "call-1",
            SHELL,
            every_call(),
            OPTION_ALLOW_ONCE,
        )
        .await;
        assert!(matches!(first, Approval::Granted));
        // Even a plain `ask` need prompts: nothing was remembered.
        assert_prompts(config, &exec, "call-2", SHELL, ApprovalNeed::Session).await;
    }

    #[tokio::test]
    async fn allow_session_is_per_server_qualified_tool() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        let alpha = ToolRef::mcp("alpha", "lookup");
        let beta = ToolRef::mcp("beta", "lookup");
        prompt_and_answer(
            config,
            &exec,
            "call-a",
            alpha,
            ApprovalNeed::Session,
            OPTION_ALLOW_SESSION,
        )
        .await;

        let again = approve(config, &exec, "call-a2", alpha, ApprovalNeed::Session).await;
        assert!(matches!(again, Approval::Granted));
        assert_prompts(config, &exec, "call-b", beta, ApprovalNeed::Session).await;
    }

    #[tokio::test]
    async fn allow_session_does_not_leak_into_a_new_session() {
        let (fixture, exec) = fixture().await;
        let config = fixture.config.as_ref();
        prompt_and_answer(
            config,
            &exec,
            "call-a",
            SHELL,
            ApprovalNeed::Session,
            OPTION_ALLOW_SESSION,
        )
        .await;

        let other = fixture.execution_context().await;
        assert_prompts(config, &other, "call-o", SHELL, ApprovalNeed::Session).await;
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_request_times_out_and_is_retired() {
        let (fixture, exec) = fixture().await;
        let mut config = fixture.config.as_ref().clone();
        config.permissions = crate::config::Permissions::new().timeout(Duration::from_secs(120));

        let started = tokio::time::Instant::now();
        let approval = approve(&config, &exec, "call-t", SHELL, ApprovalNeed::Session).await;

        assert!(matches!(
            approval,
            Approval::Denied(Denial::TimedOut(d)) if d == Duration::from_secs(120)
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(120));
        // Retired on every client: nothing is left to answer late.
        assert!(config.pending_elicitations.lock().await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_applies_to_every_call_needs_with_empty_lists() {
        let (fixture, exec) = fixture().await;
        let mut config = fixture.config.as_ref().clone();
        config.permissions = crate::config::Permissions::new().timeout(Duration::from_secs(30));
        let need = ApprovalNeed::EveryCall {
            reason: Some("hook says ask".into()),
        };

        let approval = approve(&config, &exec, "call-t", SHELL, need).await;

        assert!(matches!(approval, Approval::Denied(Denial::TimedOut(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn answer_before_the_timeout_wins() {
        let (fixture, exec) = fixture().await;
        let mut config = fixture.config.as_ref().clone();
        config.permissions = crate::config::Permissions::new().timeout(Duration::from_secs(120));

        let approval = prompt_and_answer(
            &config,
            &exec,
            "call-t",
            SHELL,
            ApprovalNeed::Session,
            OPTION_ALLOW_ONCE,
        )
        .await;

        assert!(matches!(approval, Approval::Granted));
    }

    #[tokio::test]
    async fn paused_task_denies_without_prompting() {
        let (fixture, mut exec) = fixture().await;
        let config = fixture.config.as_ref();
        let now = time::OffsetDateTime::now_utc();
        exec.state.active_task = Some(Task {
            id: 1,
            public_id: "task-1".into(),
            session_id: 1,
            kind: TaskKind::Finite,
            status: TaskStatus::Paused,
            expected_deliverable: None,
            acceptance_criteria: None,
            revision: 1,
            creation_key: None,
            completion_evidence: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
        });

        let approval = approve(config, &exec, "call-p", SHELL, ApprovalNeed::Session).await;

        assert!(matches!(approval, Approval::Denied(Denial::TaskInactive)));
        assert!(config.pending_elicitations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn permission_request_hook_decision_skips_prompt() {
        let (fixture, exec) = fixture().await;
        for behavior in ["allow", "deny"] {
            let output = serde_json::json!({
                "continue": true,
                "hook_specific_output": {
                    "hook_event_name": "permission_request",
                    "decision": {"behavior": behavior, "message": "outside trading hours"}
                }
            })
            .to_string();
            let hooks = Hooks::new(
                serde_json::from_value::<HooksConfig>(serde_json::json!({
                    "enabled": true,
                    "permission_request": [{"matcher": "^shell$", "hooks": [{
                        "type": "command",
                        "command": format!("printf '%s' '{output}'"),
                        "timeout_sec": 5
                    }]}]
                }))
                .unwrap(),
            )
            .unwrap();
            let config = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                fixture.config.provider.clone(),
                fixture.storage.event_journal(),
            )
            .with_hooks(hooks)
            .build();
            let approval = tokio::time::timeout(
                Duration::from_secs(10),
                approve(&config, &exec, "call-h", SHELL, ApprovalNeed::Session),
            )
            .await
            .expect("hook decision must not wait for a user");
            match (behavior, approval) {
                ("allow", Approval::Granted) => {}
                ("deny", Approval::Denied(Denial::Hook(message))) => {
                    assert_eq!(message, "outside trading hours");
                }
                (behavior, other) => panic!("behavior={behavior}: {other:?}"),
            }
            assert!(config.pending_elicitations.lock().await.is_empty());
        }
    }
}

#[cfg(test)]
mod prepared_call_tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::hooks::{Hooks, HooksConfig};
    use crate::session::backend::StorageBackend;
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
    use crate::tools::{
        AliasScope, ArgAlias, Tool, ToolContext, ToolError, ToolRegistry, WrapRule,
    };
    use serde_json::{Value, json};
    use std::sync::Mutex;

    const ALIASES: &[ArgAlias] = &[
        ArgAlias {
            scope: AliasScope::TopLevel,
            from: "filePath",
            to: "file_path",
        },
        ArgAlias {
            scope: AliasScope::TopLevel,
            from: "newString",
            to: "new_string",
        },
    ];

    /// Builtin-style tool that records the arguments it was invoked with.
    struct Recorder {
        invocations: Arc<Mutex<Vec<Value>>>,
        read_only: bool,
        /// Approval the tool itself demands (`Tool::default_approval`).
        approval: ApprovalNeed,
    }

    #[async_trait::async_trait]
    impl Tool for Recorder {
        fn name(&self) -> &str {
            "rec_edit"
        }
        fn definition(&self) -> querymt::chat::Tool {
            querymt::chat::Tool {
                tool_type: "function".into(),
                function: querymt::chat::FunctionTool {
                    name: "rec_edit".into(),
                    description: String::new(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "file_path": {"type": "string"},
                            "new_string": {"type": "string"}
                        },
                        "required": ["file_path", "new_string"],
                        "additionalProperties": false
                    }),
                    strict: None,
                },
            }
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        fn default_approval(&self) -> ApprovalNeed {
            self.approval.clone()
        }
        fn arg_aliases(&self) -> &'static [ArgAlias] {
            ALIASES
        }
        fn wrap_single_item(&self) -> Option<WrapRule> {
            None
        }
        async fn call(
            &self,
            args: Value,
            _: &dyn ToolContext,
        ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError> {
            self.invocations.lock().unwrap().push(args);
            Ok(vec![querymt::chat::ToolResultPart::text("ok")])
        }
    }

    struct Fixture {
        agent: TestAgent,
        invocations: Arc<Mutex<Vec<Value>>>,
        hook_log: std::path::PathBuf,
    }

    impl Fixture {
        async fn new() -> Self {
            let agent = TestAgent::with_mock_provider(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))
            .await;
            let hook_log = agent._tempdir.path().join("hook.log");
            Self {
                agent,
                invocations: Arc::new(Mutex::new(Vec::new())),
                hook_log,
            }
        }

        /// Hook that appends its stdin (one JSON line per invocation) to the log
        /// and prints `output`.
        fn hooks(&self, output: Value) -> Hooks {
            let output = serde_json::to_string(&output).unwrap();
            let log = self.hook_log.display();
            Hooks::new(
                serde_json::from_value::<HooksConfig>(json!({
                    "enabled": true,
                    "pre_tool_use": [{"matcher": "^rec_edit$", "hooks": [{
                        "type": "command",
                        "command": format!("cat >> '{log}'; echo >> '{log}'; printf '%s' '{output}'"),
                        "timeout_sec": 5
                    }]}]
                }))
                .unwrap(),
            )
            .unwrap()
        }

        fn config(&self, hooks: Option<Hooks>) -> crate::agent::agent_config::AgentConfig {
            let mut registry = ToolRegistry::new();
            registry.add(Arc::new(Recorder {
                invocations: self.invocations.clone(),
                read_only: false,
                approval: ApprovalNeed::None,
            }));
            let mut builder = AgentConfigBuilder::from_provider(
                self.agent.storage.clone(),
                self.agent.config.provider.clone(),
                self.agent.storage.event_journal(),
            )
            .with_tool_registry(registry);
            if let Some(hooks) = hooks {
                builder = builder.with_hooks(hooks);
            }
            builder.build()
        }

        /// `tool_input` of every recorded hook invocation, in order.
        fn hook_inputs(&self) -> Vec<Value> {
            std::fs::read_to_string(&self.hook_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<Value>(line).unwrap()["tool_input"].clone())
                .collect()
        }

        fn invoked(&self) -> Vec<Value> {
            self.invocations.lock().unwrap().clone()
        }
    }

    fn text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|part| part.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn run(
        fixture: &Fixture,
        config: &crate::agent::agent_config::AgentConfig,
        args: Value,
    ) -> ToolResult {
        let exec = fixture.agent.execution_context().await;
        let call = mock_tool_call("call-1", "rec_edit", &args.to_string());
        execute_tool_call(config, &call, &exec).await.unwrap()
    }

    #[tokio::test]
    async fn hook_receives_canonical_names_and_runs_exactly_once() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        let result = run(&fx, &config, json!({"filePath": "a.rs", "newString": "y"})).await;

        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(
            fx.hook_inputs(),
            vec![json!({"file_path": "a.rs", "new_string": "y"})],
            "exactly one hook invocation, canonical input"
        );
        assert_eq!(
            fx.invoked(),
            vec![json!({"file_path": "a.rs", "new_string": "y"})]
        );
        assert_eq!(
            result.tool_arguments.as_deref(),
            Some(r#"{"file_path":"a.rs","new_string":"y"}"#),
            "result record carries effective arguments"
        );
    }

    #[tokio::test]
    async fn hook_replacement_is_normalised_without_rerunning_the_hook() {
        let fx = Fixture::new().await;
        let hooks = fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "allow",
            "updated_input": {"filePath": "redirected.rs", "newString": "z"}
        }}));
        let config = fx.config(Some(hooks));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(fx.hook_inputs().len(), 1, "hook must not run again");
        assert_eq!(
            fx.invoked(),
            vec![json!({"file_path": "redirected.rs", "new_string": "z"})],
            "tool sees the canonical replacement"
        );
        assert_eq!(
            result.tool_arguments.as_deref(),
            Some(r#"{"file_path":"redirected.rs","new_string":"z"}"#)
        );
    }

    #[tokio::test]
    async fn unanswered_approval_keeps_a_listed_tool_from_running() {
        let fx = Fixture::new().await;
        let mut config = fx.config(None);
        config.permissions = crate::config::Permissions::new()
            .ask(["rec_edit".parse().unwrap()])
            .timeout(std::time::Duration::from_millis(50));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(result.is_error);
        assert!(
            text(&result).contains("no answer within")
                && text(&result).contains("the tool was not run"),
            "{}",
            text(&result)
        );
        assert!(fx.invoked().is_empty(), "tool must not run");
    }

    #[tokio::test]
    async fn hook_ask_requires_confirmation_before_the_tool_runs() {
        let fx = Fixture::new().await;
        let hooks = fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "ask",
            "permission_decision_reason": "EDIT a.rs"
        }}));
        let mut config = fx.config(Some(hooks));
        config.permissions =
            crate::config::Permissions::new().timeout(std::time::Duration::from_millis(50));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(result.is_error);
        assert!(fx.invoked().is_empty(), "tool must not run unconfirmed");
    }

    #[tokio::test]
    async fn unlisted_tool_runs_without_confirmation() {
        let fx = Fixture::new().await;
        let mut config = fx.config(None);
        config.permissions = crate::config::Permissions::new().ask(["other_tool".parse().unwrap()]);

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(fx.invoked().len(), 1);
    }

    /// Config whose only tool is `rec_edit` (read-only or not), with a
    /// metadata snapshot policy enabled.
    fn snapshot_config(fx: &Fixture, read_only: bool) -> crate::agent::agent_config::AgentConfig {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Recorder {
            invocations: fx.invocations.clone(),
            read_only,
            approval: ApprovalNeed::None,
        }));
        AgentConfigBuilder::from_provider(
            fx.agent.storage.clone(),
            fx.agent.config.provider.clone(),
            fx.agent.storage.event_journal(),
        )
        .with_tool_registry(registry)
        .with_snapshot_policy(SnapshotPolicy::Metadata)
        .build()
    }

    async fn run_in_workspace(fx: &Fixture, read_only: bool) -> ToolResult {
        let dir = tempfile::tempdir().unwrap();
        let exec = fx
            .agent
            .execution_context_in(dir.path().to_path_buf())
            .await;
        let config = snapshot_config(fx, read_only);
        let call = mock_tool_call(
            "call-1",
            "rec_edit",
            &json!({"file_path": "a.rs", "new_string": "y"}).to_string(),
        );
        execute_tool_call(&config, &call, &exec).await.unwrap()
    }

    #[tokio::test]
    async fn read_only_tool_takes_no_snapshot() {
        let fx = Fixture::new().await;
        let result = run_in_workspace(&fx, true).await;
        assert!(!result.is_error, "{}", text(&result));
        assert!(result.snapshot_part.is_none());
        assert_eq!(fx.invoked().len(), 1);
    }

    #[tokio::test]
    async fn mutating_tool_takes_a_snapshot_when_the_policy_is_enabled() {
        let fx = Fixture::new().await;
        let result = run_in_workspace(&fx, false).await;
        assert!(!result.is_error, "{}", text(&result));
        assert!(result.snapshot_part.is_some());
    }

    fn config_with_approval(
        fx: &Fixture,
        approval: ApprovalNeed,
        hooks: Option<Hooks>,
    ) -> crate::agent::agent_config::AgentConfig {
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Recorder {
            invocations: fx.invocations.clone(),
            read_only: false,
            approval,
        }));
        let mut builder = AgentConfigBuilder::from_provider(
            fx.agent.storage.clone(),
            fx.agent.config.provider.clone(),
            fx.agent.storage.event_journal(),
        )
        .with_tool_registry(registry)
        .with_permissions(
            crate::config::Permissions::new().timeout(std::time::Duration::from_millis(50)),
        );
        if let Some(hooks) = hooks {
            builder = builder.with_hooks(hooks);
        }
        builder.build()
    }

    #[tokio::test]
    async fn tool_declared_approval_prompts_without_any_config() {
        let fx = Fixture::new().await;
        let config = config_with_approval(&fx, ApprovalNeed::EveryCall { reason: None }, None);

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(result.is_error);
        assert!(
            text(&result).contains("the tool was not run"),
            "{}",
            text(&result)
        );
        assert!(fx.invoked().is_empty(), "tool must not run unconfirmed");
    }

    /// Runs `rec_edit` once in `exec` under `config`.
    async fn run_in(
        config: &crate::agent::agent_config::AgentConfig,
        exec: &crate::agent::execution_context::ExecutionContext,
        id: &str,
    ) -> ToolResult {
        let call = mock_tool_call(id, "rec_edit", r#"{"file_path":"a.rs","new_string":"y"}"#);
        execute_tool_call(config, &call, exec).await.unwrap()
    }

    #[tokio::test]
    async fn session_default_approval_prompts_until_granted_for_the_session() {
        let fx = Fixture::new().await;
        let config = config_with_approval(&fx, ApprovalNeed::Session, None);
        let exec = fx.agent.execution_context().await;

        // No grant yet: the prompt is raised and (unanswered) times out.
        let denied = run_in(&config, &exec, "c1").await;
        assert!(denied.is_error, "{}", text(&denied));
        assert!(fx.invoked().is_empty());

        // "Allow for this session" is what records this grant.
        exec.runtime
            .session_grants
            .lock()
            .insert(ToolRef::builtin("rec_edit"));
        let allowed = run_in(&config, &exec, "c2").await;
        assert!(!allowed.is_error, "{}", text(&allowed));
        assert_eq!(fx.invoked().len(), 1);
    }

    #[tokio::test]
    async fn always_ask_overrides_a_session_grant_of_a_session_default() {
        let fx = Fixture::new().await;
        let mut config = config_with_approval(&fx, ApprovalNeed::Session, None);
        config.permissions = crate::config::Permissions::new()
            .always_ask(["rec_edit".parse().unwrap()])
            .timeout(std::time::Duration::from_millis(50));
        let exec = fx.agent.execution_context().await;
        exec.runtime
            .session_grants
            .lock()
            .insert(ToolRef::builtin("rec_edit"));

        let result = run_in(&config, &exec, "c1").await;

        assert!(result.is_error, "every-call need ignores session grants");
        assert!(fx.invoked().is_empty());
    }

    #[tokio::test]
    async fn pre_tool_use_allow_skips_the_tool_declared_approval() {
        let fx = Fixture::new().await;
        let hooks = fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "allow"
        }}));
        let config =
            config_with_approval(&fx, ApprovalNeed::EveryCall { reason: None }, Some(hooks));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(fx.invoked().len(), 1);
    }

    /// Snapshot of a tool call in a workspace whose only tool is a recorder.
    async fn run_named_snapshot(
        fx: &Fixture,
        read_only: bool,
        dir: &std::path::Path,
    ) -> ToolResult {
        let exec = fx.agent.execution_context_in(dir.to_path_buf()).await;
        let config = snapshot_config(fx, read_only);
        let call = mock_tool_call(
            "call-1",
            "rec_edit",
            &json!({"file_path": "a.rs", "new_string": "y"}).to_string(),
        );
        execute_tool_call(&config, &call, &exec).await.unwrap()
    }

    #[tokio::test]
    async fn diff_snapshot_reuses_the_previous_tree_and_reports_new_changes() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one").unwrap();
        let exec = fx
            .agent
            .execution_context_in(dir.path().to_path_buf())
            .await;
        let mut registry = ToolRegistry::new();
        registry.add(Arc::new(Recorder {
            invocations: fx.invocations.clone(),
            read_only: false,
            approval: ApprovalNeed::None,
        }));
        let config = AgentConfigBuilder::from_provider(
            fx.agent.storage.clone(),
            fx.agent.config.provider.clone(),
            fx.agent.storage.event_journal(),
        )
        .with_tool_registry(registry)
        .with_snapshot_policy(SnapshotPolicy::Diff)
        .build();
        let call = |id: &str| {
            mock_tool_call(
                id,
                "rec_edit",
                &json!({"file_path": "a.rs", "new_string": "y"}).to_string(),
            )
        };

        let first = execute_tool_call(&config, &call("c1"), &exec)
            .await
            .unwrap();
        assert!(first.snapshot_part.is_some());
        assert!(exec.runtime.last_snapshot_tree.lock().is_some());

        // A change between calls is still reported by the next snapshot.
        let before = exec
            .runtime
            .last_snapshot_tree
            .lock()
            .as_ref()
            .map(|(_, tree)| tree.root_hash)
            .unwrap();
        std::fs::write(dir.path().join("b.rs"), "new file").unwrap();
        let second = execute_tool_call(&config, &call("c2"), &exec)
            .await
            .unwrap();
        assert!(second.snapshot_part.is_some());
        let after = exec
            .runtime
            .last_snapshot_tree
            .lock()
            .as_ref()
            .map(|(_, tree)| tree.root_hash)
            .unwrap();
        assert_ne!(before, after, "cached tree is refreshed after each call");
    }

    #[tokio::test]
    async fn read_only_declaration_takes_no_snapshot_in_a_workspace() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        let result = run_named_snapshot(&fx, true, dir.path()).await;
        assert!(result.snapshot_part.is_none());
    }

    #[test]
    fn provider_fallback_and_missing_tools_take_no_snapshot() {
        assert!(ToolTarget::Provider.is_read_only());
        assert_eq!(ToolTarget::Provider.default_approval(), ApprovalNeed::None);
    }

    #[tokio::test]
    async fn conflicting_spellings_reject_before_hooks_and_tool() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y", "newString": "y"}),
        )
        .await;

        assert!(result.is_error);
        let message = text(&result);
        assert!(
            message.contains("new_string") && message.contains("newString"),
            "{message}"
        );
        assert!(fx.hook_inputs().is_empty(), "hook must not run");
        assert!(fx.invoked().is_empty(), "tool must not run");
    }

    #[tokio::test]
    async fn conflict_inside_hook_replacement_rejects_without_invoking_the_tool() {
        let fx = Fixture::new().await;
        let hooks = fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "allow",
            "updated_input": {"file_path": "a.rs", "new_string": "z", "newString": "z"}
        }}));
        let config = fx.config(Some(hooks));

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y"}),
        )
        .await;

        assert!(result.is_error);
        assert!(text(&result).contains("newString"), "{}", text(&result));
        assert_eq!(fx.hook_inputs().len(), 1);
        assert!(fx.invoked().is_empty());
    }

    #[tokio::test]
    async fn validation_failure_runs_the_hook_once_and_never_the_tool() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        let result = run(&fx, &config, json!({"file_path": "a.rs"})).await;

        assert!(result.is_error);
        assert!(
            text(&result).starts_with("Error: invalid tool arguments:"),
            "{}",
            text(&result)
        );
        assert_eq!(fx.hook_inputs().len(), 1);
        assert!(fx.invoked().is_empty());
    }

    /// Run a raw call (possibly provider-marked) through prepare + execute.
    async fn run_raw(
        fx: &Fixture,
        config: &crate::agent::agent_config::AgentConfig,
        raw: &str,
        invalid_input: bool,
    ) -> ToolResult {
        let exec = fx.agent.execution_context().await;
        let mut call = mock_tool_call("call-1", "rec_edit", raw);
        call.invalid_input = invalid_input;
        execute_tool_call(config, &call, &exec).await.unwrap()
    }

    #[tokio::test]
    async fn malformed_outer_json_is_rejected_before_hooks_and_tool() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        let result = run_raw(&fx, &config, r#"{"edits": ["#, false).await;

        assert!(result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&text(&result)).unwrap(),
            json!({"INVALID_JSON": r#"{"edits": ["#})
        );
        assert!(fx.hook_inputs().is_empty(), "hooks must not run");
        assert!(fx.invoked().is_empty(), "tool must not run");
        assert_eq!(
            result.tool_arguments.as_deref(),
            Some(r#"{"edits": ["#),
            "raw input is recorded as emitted"
        );
    }

    #[tokio::test]
    async fn non_object_outer_arguments_are_rejected() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        for raw in ["[1,2]", "\"text\"", "null", "42", ""] {
            let result = run_raw(&fx, &config, raw, false).await;
            assert!(result.is_error, "{raw}");
            assert!(text(&result).contains("INVALID_JSON"), "{raw}");
        }
        assert!(fx.hook_inputs().is_empty());
        assert!(fx.invoked().is_empty());
    }

    #[tokio::test]
    async fn provider_marker_overrides_valid_syntax() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));
        let raw = r#"{"file_path":"a.rs","new_string":"b"}"#;

        let result = run_raw(&fx, &config, raw, true).await;

        assert!(result.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&text(&result)).unwrap(),
            json!({"INVALID_JSON": raw})
        );
        assert!(fx.hook_inputs().is_empty(), "hooks must not run");
        assert!(fx.invoked().is_empty(), "tool must not run");
    }

    #[tokio::test]
    async fn zero_argument_empty_object_is_valid_input() {
        let call = mock_tool_call("c", "rec_edit", "{}");
        assert!(!has_invalid_input(&call));
        // Fails schema validation (required fields), not outer-input validation.
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let result = run_raw(&fx, &config, "{}", false).await;
        assert!(
            text(&result).contains("missing required field"),
            "{}",
            text(&result)
        );
    }

    #[test]
    fn invalid_json_diagnostic_is_bounded_and_unicode_safe() {
        let raw = "é".repeat(2500);
        let message = invalid_input_text(&raw);
        let parsed: Value = serde_json::from_str(&message).unwrap();
        let shown = parsed["INVALID_JSON"].as_str().unwrap();
        assert_eq!(shown.chars().count(), INVALID_JSON_MAX_CHARS);
        assert!(shown.chars().all(|c| c == 'é'), "no split scalar values");

        let exact = "x".repeat(INVALID_JSON_MAX_CHARS);
        let parsed: Value = serde_json::from_str(&invalid_input_text(&exact)).unwrap();
        assert_eq!(parsed["INVALID_JSON"].as_str().unwrap(), exact);
    }

    #[tokio::test]
    async fn a_nested_unrepaired_string_is_validation_not_invalid_input() {
        // Valid outer object whose nested value cannot be repaired: reaches
        // schema validation, never INVALID_JSON.
        let fx = Fixture::new().await;
        let config = fx.config(Some(fx.hooks(json!({}))));

        let result = run_raw(
            &fx,
            &config,
            r#"{"file_path": 5, "new_string": "x"}"#,
            false,
        )
        .await;

        assert!(result.is_error);
        assert!(!text(&result).contains("INVALID_JSON"), "{}", text(&result));
        assert!(text(&result).starts_with("Error: invalid tool arguments:"));
        assert_eq!(fx.hook_inputs().len(), 1, "hooks ran for valid outer input");
    }

    #[tokio::test]
    async fn validation_error_lists_every_violation_with_bounded_echo() {
        let fx = Fixture::new().await;
        let config = fx.config(None);

        let result = run(
            &fx,
            &config,
            json!({"file_path": "x".repeat(2000), "new_string": 7, "bogus": 1}),
        )
        .await;

        assert!(result.is_error);
        let message = text(&result);
        assert!(
            message.contains("2 violations:") || message.contains("3 violations:"),
            "{message}"
        );
        assert!(
            message.contains("/new_string: expected string, got integer: 7"),
            "{message}"
        );
        assert!(
            message.contains("property `bogus` is not allowed"),
            "{message}"
        );
        assert!(
            message.len() < 600,
            "2,000-char value must not be echoed: {message}"
        );
        assert!(fx.invoked().is_empty());
    }

    #[tokio::test]
    async fn validation_error_lists_all_missing_required_fields() {
        let fx = Fixture::new().await;
        let config = fx.config(None);

        let result = run(&fx, &config, json!({})).await;

        let message = text(&result);
        assert!(
            message.contains("missing required field `file_path`"),
            "{message}"
        );
        assert!(
            message.contains("missing required field `new_string`"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn unknown_keys_are_rejected_by_the_closed_schema() {
        let fx = Fixture::new().await;
        let config = fx.config(None);

        let result = run(
            &fx,
            &config,
            json!({"file_path": "a.rs", "new_string": "y", "bogus": 1}),
        )
        .await;

        assert!(result.is_error);
        assert!(text(&result).contains("bogus"), "{}", text(&result));
        assert!(fx.invoked().is_empty());
    }

    #[tokio::test]
    async fn hook_block_rejects_the_call_without_invoking_the_tool() {
        let fx = Fixture::new().await;
        let config = fx.config(Some(
            fx.hooks(json!({"decision": "block", "reason": "nope"})),
        ));

        let result = run(&fx, &config, json!({"filePath": "a.rs", "newString": "y"})).await;

        assert!(result.is_error);
        assert_eq!(text(&result), "Error: nope");
        assert_eq!(fx.hook_inputs().len(), 1);
        assert!(fx.invoked().is_empty());
        assert_eq!(
            result.tool_arguments.as_deref(),
            Some(r#"{"file_path":"a.rs","new_string":"y"}"#),
            "rejected call records the effective arguments"
        );
    }

    #[tokio::test]
    async fn prepare_returns_one_rejection_and_effective_arguments() {
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let exec = fx.agent.execution_context().await;

        let ok = prepare_tool_call(
            &config,
            &mock_tool_call("c1", "rec_edit", r#"{"filePath":"a","newString":"b"}"#),
            &exec,
        )
        .await
        .unwrap();
        assert!(matches!(ok.disposition, Disposition::Ready));
        assert_eq!(ok.args, json!({"file_path": "a", "new_string": "b"}));
        assert_eq!(ok.repairs.len(), 2);
        assert!(fx.invoked().is_empty(), "preparing never invokes the tool");

        let bad = prepare_tool_call(
            &config,
            &mock_tool_call("c2", "rec_edit", r#"{"file_path":"a"}"#),
            &exec,
        )
        .await
        .unwrap();
        assert!(matches!(bad.disposition, Disposition::Rejected(_)));
    }

    // ---- repair telemetry (task 4.5) ----

    use crate::test_utils::helpers::genai_metrics;
    use crate::test_utils::helpers::genai_trace::{attr, capture};
    use opentelemetry::Value as OtelValue;

    fn repaired(rule: &'static str) -> u64 {
        genai_metrics::sum(
            "querymt.tool.args_repaired",
            &[
                ("tool", OtelValue::from("rec_edit")),
                ("rule", OtelValue::from(rule)),
            ],
        )
    }

    fn calls(repaired: bool, outcome: &'static str) -> u64 {
        genai_metrics::sum(
            "querymt.tool.calls",
            &[
                ("tool", OtelValue::from("rec_edit")),
                ("repaired", OtelValue::from(repaired)),
                ("outcome", OtelValue::from(outcome)),
            ],
        )
    }

    #[tokio::test]
    async fn alias_repairs_are_counted_per_mutation_and_listed_on_the_span() {
        genai_metrics::install();
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let exec = fx.agent.execution_context().await;
        let call = mock_tool_call("c1", "rec_edit", r#"{"filePath":"a","newString":"b"}"#);
        let before = repaired("alias");

        let (result, spans) = capture(execute_tool_call(&config, &call, &exec)).await;

        assert!(!result.unwrap().is_error);
        assert_eq!(repaired("alias") - before, 2, "one count per renamed key");
        let span = spans
            .iter()
            .find(|span| {
                attr(span, "gen_ai.operation.name") == Some(&OtelValue::from("execute_tool"))
            })
            .expect("tool span");
        let Some(OtelValue::Array(opentelemetry::Array::String(rules))) =
            attr(span, "querymt.tool.args_repairs")
        else {
            panic!("span lists the applied repair rules");
        };
        let rules: Vec<&str> = rules.iter().map(|rule| rule.as_str()).collect();
        assert_eq!(rules, ["alias", "alias"]);
    }

    #[tokio::test]
    async fn clean_calls_record_no_repair() {
        genai_metrics::install();
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let exec = fx.agent.execution_context().await;
        let call = mock_tool_call("c1", "rec_edit", r#"{"file_path":"a","new_string":"b"}"#);
        let (before, clean_before) = (repaired("alias"), calls(false, "success"));

        let (result, spans) = capture(execute_tool_call(&config, &call, &exec)).await;

        assert!(!result.unwrap().is_error);
        assert_eq!(repaired("alias"), before);
        assert_eq!(calls(false, "success") - clean_before, 1);
        assert!(
            spans
                .iter()
                .all(|span| attr(span, "querymt.tool.args_repairs").is_none())
        );
    }

    #[tokio::test]
    async fn repaired_calls_that_succeed_and_fail_are_distinguishable() {
        genai_metrics::install();
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let exec = fx.agent.execution_context().await;
        let (ok_before, rejected_before) = (calls(true, "success"), calls(true, "rejected"));
        let alias_before = repaired("alias");

        // Repaired and succeeds.
        let ok = execute_tool_call(
            &config,
            &mock_tool_call("c1", "rec_edit", r#"{"filePath":"a","newString":"b"}"#),
            &exec,
        )
        .await
        .unwrap();
        // Repaired (one alias) but still fails validation: `new_string` is missing.
        let bad = execute_tool_call(
            &config,
            &mock_tool_call("c2", "rec_edit", r#"{"filePath":"a"}"#),
            &exec,
        )
        .await
        .unwrap();

        assert!(!ok.is_error);
        assert!(bad.is_error);
        assert_eq!(calls(true, "success") - ok_before, 1);
        assert_eq!(calls(true, "rejected") - rejected_before, 1);
        // Rule counters alone cannot tell the two apart: 2 + 1 renames.
        assert_eq!(repaired("alias") - alias_before, 3);
    }

    #[tokio::test]
    async fn hook_replacement_repairs_count_once_and_canonical_replacement_adds_none() {
        genai_metrics::install();
        let fx = Fixture::new().await;
        let exec = fx.agent.execution_context().await;
        let before = repaired("alias");

        // The model sends canonical names; the hook replaces them with aliases.
        let aliased_hook = fx.config(Some(fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "allow",
            "updated_input": {"filePath": "r.rs", "newString": "z"}
        }}))));
        execute_tool_call(
            &aliased_hook,
            &mock_tool_call("c1", "rec_edit", r#"{"file_path":"a","new_string":"y"}"#),
            &exec,
        )
        .await
        .unwrap();
        assert_eq!(repaired("alias") - before, 2, "hook output normalisation");

        // A canonical replacement is re-normalised unchanged: nothing is counted.
        let canonical_hook = fx.config(Some(fx.hooks(json!({"hook_specific_output": {
            "hook_event_name": "pre_tool_use",
            "permission_decision": "allow",
            "updated_input": {"file_path": "r.rs", "new_string": "z"}
        }}))));
        execute_tool_call(
            &canonical_hook,
            &mock_tool_call("c2", "rec_edit", r#"{"file_path":"a","new_string":"y"}"#),
            &exec,
        )
        .await
        .unwrap();
        assert_eq!(repaired("alias") - before, 2, "unchanged re-normalisation");
    }

    #[tokio::test]
    async fn scheduling_does_not_double_count_repairs() {
        genai_metrics::install();
        let fx = Fixture::new().await;
        let config = fx.config(None);
        let exec = fx.agent.execution_context().await;
        let before = repaired("alias");

        // Preparing reports the repairs once; executing the prepared call (as the
        // scheduler does) must not report them again.
        let call = mock_tool_call("c1", "rec_edit", r#"{"filePath":"a","newString":"b"}"#);
        let prepared = prepare_tool_call(&config, &call, &exec).await.unwrap();
        assert_eq!(repaired("alias") - before, 2);
        execute_prepared_tool_call(&config, &call, prepared, &exec)
            .await
            .unwrap();
        assert_eq!(repaired("alias") - before, 2);
    }

    /// Sends the repair telemetry through the production OTLP metric exporter to a
    /// real collector (task 9.3). Ignored by default; run with
    /// `QMT_TEST_OTLP_ENDPOINT=http://127.0.0.1:4317 cargo nextest run -p querymt-agent
    /// --run-ignored only otlp_export` and then query the backend (see docs/docs/telemetry.md).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs an OTLP collector: set QMT_TEST_OTLP_ENDPOINT"]
    async fn otlp_export_of_repair_telemetry() {
        let endpoint = std::env::var("QMT_TEST_OTLP_ENDPOINT").expect("QMT_TEST_OTLP_ENDPOINT");
        let provider =
            querymt_utils::telemetry::init_meter_provider("qmt-repair-check", "0", &endpoint)
                .expect("OTLP meter provider");
        opentelemetry::global::set_meter_provider(provider.clone());

        let fx = Fixture::new().await;
        let exec = fx.agent.execution_context().await;
        let plain = fx.config(None);
        let blocking = fx.config(Some(
            fx.hooks(json!({"decision": "block", "reason": "nope"})),
        ));
        // repaired + success (two renames), repaired + rejected (one rename, missing
        // field), clean + success, and a hook block that never runs the tool.
        let cases = [
            (&plain, r#"{"filePath":"a","newString":"b"}"#),
            (&plain, r#"{"filePath":"a"}"#),
            (&plain, r#"{"file_path":"a","new_string":"b"}"#),
            (&blocking, r#"{"file_path":"a","new_string":"b"}"#),
        ];
        for (index, (config, arguments)) in cases.into_iter().enumerate() {
            execute_tool_call(
                config,
                &mock_tool_call(&format!("otlp{index}"), "rec_edit", arguments),
                &exec,
            )
            .await
            .unwrap();
        }
        provider
            .force_flush()
            .expect("flush metrics to the collector");
        provider.shutdown().expect("shutdown meter provider");
    }

    // ---- baseline replay (task 9.1) ----

    /// Wraps a real builtin: same name, schema, aliases and wrap rule, but the call
    /// only records the effective arguments it received (no file is touched).
    struct Replay {
        inner: Arc<dyn Tool>,
        invocations: Arc<Mutex<Vec<(String, Value)>>>,
    }

    #[async_trait::async_trait]
    impl Tool for Replay {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn definition(&self) -> querymt::chat::Tool {
            self.inner.definition()
        }
        fn arg_aliases(&self) -> &'static [ArgAlias] {
            self.inner.arg_aliases()
        }
        fn wrap_single_item(&self) -> Option<WrapRule> {
            self.inner.wrap_single_item()
        }
        async fn call(
            &self,
            args: Value,
            _: &dyn ToolContext,
        ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError> {
            self.invocations
                .lock()
                .unwrap()
                .push((self.name().to_string(), args));
            Ok(vec![querymt::chat::ToolResultPart::text("ok")])
        }
    }

    /// Text that must survive preparation byte for byte: escapes, quotes, CRLF,
    /// trailing blanks, and content that looks like JSON.
    const OLD_TEXT: &str = "fn a() {\n\t\"q\" \\n \\\\ \u{e9} ' [1, 2] \r\n  trailing  ";
    const NEW_TEXT: &str = "{\"a\": [1, 2]}\n  x = '\\t'\n";

    fn fill(value: &Value) -> Value {
        match value {
            Value::String(text) => match text.as_str() {
                "<path>" => json!("src/a.rs"),
                "<old>" => json!(OLD_TEXT),
                "<new>" => json!(NEW_TEXT),
                "<symbol>" => json!("foo"),
                "<pattern>" => json!("foo"),
                "<glob>" => json!("target/**"),
                _ => value.clone(),
            },
            Value::Array(items) => Value::Array(items.iter().map(fill).collect()),
            Value::Object(map) => {
                Value::Object(map.iter().map(|(k, v)| (k.clone(), fill(v))).collect())
            }
            other => other.clone(),
        }
    }

    async fn replay_config(
        fx: &Fixture,
        log: &Arc<Mutex<Vec<(String, Value)>>>,
    ) -> crate::agent::agent_config::AgentConfig {
        let mut registry = ToolRegistry::new();
        for tool in crate::tools::builtins::all_builtin_tools() {
            registry.add(Arc::new(Replay {
                inner: tool,
                invocations: log.clone(),
            }));
        }
        AgentConfigBuilder::from_provider(
            fx.agent.storage.clone(),
            fx.agent.config.provider.clone(),
            fx.agent.storage.event_journal(),
        )
        .with_tool_registry(registry)
        .build()
    }

    #[tokio::test]
    async fn baseline_failure_shapes_replay_through_preparation_validation_and_execution() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tools/testdata/baseline_failures.json"))
                .unwrap();
        let classes = fixture["classes"].as_array().unwrap();
        let covered: u64 = classes
            .iter()
            .map(|c| c["baseline_count"].as_u64().unwrap())
            .sum();
        assert_eq!(covered, 111, "fixtures cover every baseline schema failure");

        let fx = Fixture::new().await;
        let exec = fx.agent.execution_context().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = replay_config(&fx, &log).await;

        let mut table = Vec::new();
        for (index, class) in classes.iter().enumerate() {
            let id = class["id"].as_str().unwrap();
            let tool = class["tool"].as_str().unwrap();
            let args = fill(&class["args"]);
            let before = log.lock().unwrap().len();
            let call = mock_tool_call(&format!("replay{index}"), tool, &args.to_string());
            let result = execute_tool_call(&config, &call, &exec).await.unwrap();
            let invoked: Vec<(String, Value)> = log.lock().unwrap()[before..].to_vec();

            match class["expect"].as_str().unwrap() {
                "success" => {
                    assert!(!result.is_error, "{id}: {}", text(&result));
                    assert_eq!(invoked.len(), 1, "{id}: invoked exactly once");
                    assert_eq!(invoked[0].0, tool, "{id}");
                    assert_eq!(
                        invoked[0].1,
                        fill(&class["effective"]),
                        "{id}: effective arguments (edit text must be unchanged)"
                    );
                    // The recorded effective arguments match what actually ran.
                    assert_eq!(
                        serde_json::from_str::<Value>(result.tool_arguments.as_deref().unwrap())
                            .unwrap(),
                        invoked[0].1,
                        "{id}"
                    );
                }
                "fail" => {
                    assert!(result.is_error, "{id}: unexpectedly succeeded");
                    assert!(invoked.is_empty(), "{id}: a rejected call never runs");
                    let message = text(&result);
                    assert!(
                        message.starts_with("Error: invalid tool arguments:"),
                        "{id}: {message}"
                    );
                    for needle in class["message_contains"].as_array().unwrap() {
                        let needle = needle.as_str().unwrap();
                        assert!(
                            message.contains(needle),
                            "{id}: missing `{needle}` in {message}"
                        );
                    }
                }
                other => panic!("{id}: unknown expectation {other}"),
            }
            table.push(format!(
                "{:>3}  {:<14} {:<8} {id}",
                class["baseline_count"],
                tool,
                class["expect"].as_str().unwrap()
            ));
        }
        println!(
            "baseline replay coverage (count, tool, outcome, shape):\n{}",
            table.join("\n")
        );
    }

    #[tokio::test]
    async fn invalid_outer_json_never_executes() {
        let fx = Fixture::new().await;
        let exec = fx.agent.execution_context().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = replay_config(&fx, &log).await;

        // Truncated object, a non-object, and a bare string: none become `{}`.
        for (index, raw) in [
            r#"{"file_path": "a.rs", "edits": ["#,
            "[1, 2]",
            r#""text""#,
            "",
        ]
        .into_iter()
        .enumerate()
        {
            let call = mock_tool_call(&format!("outer{index}"), "multiedit", raw);
            let result = execute_tool_call(&config, &call, &exec).await.unwrap();
            assert!(result.is_error, "{raw}");
            assert!(
                text(&result).contains("INVALID_JSON"),
                "{raw}: {}",
                text(&result)
            );
        }
        assert!(log.lock().unwrap().is_empty(), "no call executed");
    }
}

#[cfg(test)]
mod mcp_tool_tests {
    use super::*;
    use crate::acp::protocol::{SessionUpdate, ToolCallStatus};
    use crate::events::EventEnvelope;
    use crate::test_utils::mcp_harness::{HarnessOptions, McpHarness};
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestAgent, mock_tool_call};
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::broadcast;

    struct Fixture {
        agent: TestAgent,
        harness: McpHarness,
        exec: ExecutionContext,
        events: broadcast::Receiver<EventEnvelope>,
    }

    async fn fixture() -> Fixture {
        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let harness = McpHarness::start(HarnessOptions::default()).await;
        let exec = agent
            .execution_context_with_mcp(harness.tool_state.clone())
            .await;
        let events = agent.config.event_sink.fanout().subscribe();
        Fixture {
            agent,
            harness,
            exec,
            events,
        }
    }

    /// Everything published within `window`, in order.
    async fn collect(
        rx: &mut broadcast::Receiver<EventEnvelope>,
        window: Duration,
    ) -> Vec<EventEnvelope> {
        let deadline = tokio::time::Instant::now() + window;
        let mut out = Vec::new();
        while let Ok(received) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match received {
                Ok(envelope) => out.push(envelope),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        out
    }

    fn progress_values(events: &[EventEnvelope]) -> Vec<Option<f64>> {
        events
            .iter()
            .filter_map(|e| match e.kind() {
                AgentEventKind::ToolCallProgress { progress, .. } => Some(*progress),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn listed_mcp_tool_is_not_called_until_approved() {
        let f = fixture().await;
        let mut config = f.agent.config.as_ref().clone();
        config.permissions = crate::config::Permissions::new()
            .ask(["harness.echo".parse().unwrap()])
            .timeout(Duration::from_millis(50));
        let call = mock_tool_call("c-ask", "echo", r#"{"text":"hi"}"#);

        // Nobody answers: the server must never see the call.
        let result = execute_tool_call(&config, &call, &f.exec).await.unwrap();

        assert!(result.is_error);
        assert_eq!(result.tool_source, "blocked");
        let message: String = result.content.iter().filter_map(|b| b.as_text()).collect();
        assert!(
            message.contains("no answer within") && message.ends_with("; the tool was not run"),
            "{message}"
        );
        assert!(f.harness.log.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn approved_mcp_tool_reaches_the_server() {
        let f = fixture().await;
        let mut config = f.agent.config.as_ref().clone();
        config.permissions =
            crate::config::Permissions::new().ask(["harness.echo".parse().unwrap()]);
        let call = mock_tool_call("c-ok", "echo", r#"{"text":"hi"}"#);

        let run = execute_tool_call(&config, &call, &f.exec);
        let driver = async {
            for _ in 0..200 {
                let id = config
                    .pending_elicitations
                    .lock()
                    .await
                    .iter()
                    .find(|(_, e)| e.tool_call_id.as_deref() == Some("c-ok"))
                    .map(|(id, _)| id.clone());
                if let Some(id) = id {
                    crate::elicitation::resolve_in_map(
                        &config.pending_elicitations,
                        Some(&f.exec.session_id),
                        &id,
                        "conn-1",
                        None,
                        crate::elicitation::ElicitationResponse::permission_selected(
                            OPTION_ALLOW_ONCE,
                        ),
                    )
                    .await
                    .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no permission request became pending");
        };
        let (result, ()) = tokio::join!(run, driver);

        let result = result.unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(f.harness.log.calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn is_error_result_is_a_failed_tool_call_with_server_text() {
        let mut f = fixture().await;
        let call = mock_tool_call("c1", "error", "{}");

        let result = execute_tool_call(&f.agent.config, &call, &f.exec)
            .await
            .unwrap();

        assert!(
            result.is_error && result.execution_is_error,
            "outcome is `error`"
        );
        let texts: Vec<_> = result.content.iter().filter_map(|b| b.as_text()).collect();
        assert_eq!(texts, ["quota exceeded"], "server text is used verbatim");

        let events = collect(&mut f.events, Duration::from_millis(300)).await;
        let end = events
            .iter()
            .find(|e| matches!(e.kind(), AgentEventKind::ToolCallEnd { .. }))
            .expect("ToolCallEnd emitted");
        assert!(matches!(
            end.kind(),
            AgentEventKind::ToolCallEnd { is_error: true, result, .. } if result == "quota exceeded"
        ));
        let Some(SessionUpdate::ToolCallUpdate(update)) =
            crate::acp::shared::translate_replay_event_to_update(end)
        else {
            panic!("expected tool call update");
        };
        assert_eq!(update.fields.status, Some(ToolCallStatus::Failed));
    }

    #[tokio::test]
    async fn transport_errors_keep_the_error_prefix() {
        let f = fixture().await;
        // The harness rejects unknown tools with a protocol error.
        let mut tool = f
            .harness
            .running
            .peer()
            .list_all_tools()
            .await
            .unwrap()
            .remove(0);
        tool.name = "missing".into();
        let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
            tool,
            f.harness.running.peer().clone(),
            "harness".into(),
        )
        .unwrap();
        f.exec
            .runtime
            .mcp_tool_state
            .store(crate::agent::core::McpToolSnapshot {
                tool_defs: vec![],
                tools: [("missing".to_string(), Arc::new(adapter))].into(),
                tools_hash: None,
            });
        let call = mock_tool_call("c1", "missing", "{}");
        let result = execute_tool_call(&f.agent.config, &call, &f.exec)
            .await
            .unwrap();
        assert!(result.is_error);
        let text = result
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<String>();
        assert!(text.starts_with("Error: "), "{text}");
    }

    #[tokio::test]
    async fn progress_burst_is_throttled_and_the_latest_update_is_delivered() {
        let mut f = fixture().await;
        let args = json!({"count": 20, "total": 20, "gap_ms": 5, "hold_ms": 500}).to_string();
        let call = mock_tool_call("c1", "progress", &args);

        execute_tool_call(&f.agent.config, &call, &f.exec)
            .await
            .unwrap();

        let events = collect(&mut f.events, Duration::from_millis(300)).await;
        let progress = progress_values(&events);
        assert!(
            (1..=2).contains(&progress.len()),
            "throttled to at most 2 events, got {progress:?}"
        );
        assert_eq!(
            progress.last(),
            Some(&Some(20.0)),
            "latest update delivered"
        );
    }

    /// Task 9.3 manual smoke test against a real MCP server
    /// (`@modelcontextprotocol/server-everything`). Prints what it observes.
    ///
    /// `QMT_SMOKE_MCP` is `stdio` (default) or the URL of a Streamable HTTP
    /// endpoint, e.g. `http://127.0.0.1:3001/mcp`.
    #[tokio::test]
    #[ignore = "manual smoke test: needs node/npx (and network for npx); see task 9.3"]
    async fn smoke_real_mcp_server() {
        use crate::agent::mcp::McpConnectContext;
        use crate::config::McpServerConfig;
        use querymt::tool_decorator::CallFunctionTool;

        let spec = std::env::var("QMT_SMOKE_MCP").unwrap_or_else(|_| "stdio".into());
        let server = if spec == "stdio" {
            McpServerConfig::stdio("everything", "npx").with_args([
                "-y",
                "@modelcontextprotocol/server-everything",
                "stdio",
            ])
        } else {
            McpServerConfig::http("everything", spec.clone())
        };
        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let tool_state = crate::agent::core::McpToolState::empty();
        let _clients = tokio::time::timeout(
            Duration::from_secs(120),
            crate::agent::protocol::build_mcp_state(
                &[server.to_acp()],
                &McpConnectContext::default(),
                Default::default(),
                agent.config.event_sink.clone(),
                "smoke".into(),
                &crate::agent::mcp::agent_implementation(),
                tool_state.clone(),
                None,
            ),
        )
        .await
        .expect("server starts in time")
        .expect("connects");
        let mut names: Vec<String> = tool_state.load().tools.keys().cloned().collect();
        names.sort();
        println!("SMOKE[{spec}] tools: {names:?}");

        let long = names
            .iter()
            .find(|n| n.contains("long-running"))
            .expect("server exposes a long-running (progress) tool")
            .clone();
        let exec = agent.execution_context_with_mcp(tool_state.clone()).await;
        let mut events = agent.config.event_sink.fanout().subscribe();
        let args = json!({"duration": 2, "steps": 4}).to_string();
        let call = mock_tool_call("smoke1", &long, &args);
        let result = execute_tool_call(&agent.config, &call, &exec)
            .await
            .unwrap();
        let seen = collect(&mut events, Duration::from_millis(500)).await;
        let progress: Vec<_> = seen
            .iter()
            .filter_map(|e| match e.kind() {
                AgentEventKind::ToolCallProgress {
                    progress,
                    total,
                    message,
                    ..
                } => Some((*progress, *total, message.clone())),
                _ => None,
            })
            .collect();
        println!(
            "SMOKE[{spec}] {long}: is_error={} progress updates={progress:?}",
            result.is_error
        );
        println!(
            "SMOKE[{spec}] {long}: result={:?}",
            result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<String>()
        );
        assert!(!result.is_error);
        assert!(!progress.is_empty(), "progress updates were observed");

        // An `isError` result: invalid arguments sent straight to the adapter
        // (the agent would reject them against the schema first).
        let adapter = tool_state.load().tools.get(&long).cloned().unwrap();
        let err = adapter
            .call(json!({"duration": "not-a-number", "steps": "x"}))
            .await
            .expect_err("server reports the invalid input as an error");
        match err.downcast_ref::<querymt::mcp::adapter::McpToolError>() {
            Some(querymt::mcp::adapter::McpToolError::Reported { parts }) => {
                println!("SMOKE[{spec}] isError result -> Reported: {parts:?}");
            }
            other => panic!("expected McpToolError::Reported, got {other:?} ({err})"),
        }
    }

    /// Task 8.8: no access or refresh token string reaches events or results,
    /// including on the path that reports a login requirement.
    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn oauth_tokens_never_reach_events_or_results() {
        use crate::agent::protocol::tests::oauth_connect as oc;
        use crate::mcp_auth::test_support::{MemoryBackend, creds};

        const TOKENS: [&str; 4] = [
            "good-token",
            "refresh-secret",
            "fresh-token",
            "rotated-refresh",
        ];
        let mock = Arc::new(oc::Mock::default());
        let url = format!("{}/mcp", oc::serve(mock.clone()).await);
        let backend = Arc::new(MemoryBackend::default());
        oc::seed(&backend, &url, creds("good-token")).await;
        let server = oc::oauth_server(&url);
        let tool_state = crate::agent::core::McpToolState::empty();
        let _clients = oc::connect(&server, &backend, tool_state.clone())
            .await
            .expect("connects with stored credentials");

        let agent =
            TestAgent::with_mock_provider(SharedLlmProvider::new(MockLlmProvider::new(), vec![]))
                .await;
        let exec = agent.execution_context_with_mcp(tool_state).await;
        let mut config = (*agent.config).clone();
        config.mcp_servers = vec![server];
        let mut events = agent.config.event_sink.fanout().subscribe();

        // The token is revoked and the refresh token rejected mid-session.
        mock.revoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        mock.reject_refresh
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let call = mock_tool_call("c1", "secret", "{}");
        let result = execute_tool_call(&config, &call, &exec).await.unwrap();

        assert!(result.is_error);
        let text: String = result.content.iter().filter_map(|b| b.as_text()).collect();
        assert_eq!(
            text,
            "Error: authorization required for MCP server secured; \
             log in with provider mcp:secured"
        );
        let seen = collect(&mut events, Duration::from_millis(300)).await;
        let kinds: Vec<_> = seen.iter().map(|e| e.kind()).collect();
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, AgentEventKind::ToolCallStart { .. }))
        );
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, AgentEventKind::ToolCallEnd { .. }))
        );
        assert!(kinds.iter().any(|k| matches!(
            k,
            AgentEventKind::McpServerAuthRequired { server_name, .. } if server_name == "secured"
        )));

        let published = serde_json::to_string(&seen).unwrap();
        for token in TOKENS {
            assert!(!published.contains(token), "event stream leaked `{token}`");
            assert!(!text.contains(token), "tool result leaked `{token}`");
        }
    }

    #[tokio::test]
    async fn no_progress_follows_tool_call_end() {
        let mut f = fixture().await;
        // Burst, then the call ends at once: the pending update must be dropped.
        let call = mock_tool_call("c1", "progress", &json!({"count": 20}).to_string());

        execute_tool_call(&f.agent.config, &call, &f.exec)
            .await
            .unwrap();

        // Longer than the throttle window, so a leaked trailing timer would fire.
        let events = collect(&mut f.events, Duration::from_millis(700)).await;
        let end = events
            .iter()
            .position(|e| matches!(e.kind(), AgentEventKind::ToolCallEnd { .. }))
            .expect("ToolCallEnd emitted");
        assert!(
            progress_values(&events[end + 1..]).is_empty(),
            "no ToolCallProgress after ToolCallEnd"
        );
    }
}
