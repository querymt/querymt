//! Tool execution, permission checking, and result storage
//!
//! This module handles the complete lifecycle of tool calls: execution, permission
//! checking, snapshotting, and storing results back into conversation history.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SnapshotPolicy;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::snapshots::{SnapshotState, snapshot_metadata};
use crate::agent::utils::genai;
use crate::events::AgentEventKind;
use crate::hooks::{
    PermissionRequestDecision, PostToolUseRequest, PreDelegationRequest, PreToolUseRequest,
    UpdatedDelegation,
};
use crate::middleware::ToolCall as MiddlewareToolCall;
use crate::middleware::{ExecutionState, ToolResult, WaitCondition};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use log::debug;
use querymt::chat::ChatRole;
use std::sync::Arc;
use tracing::{Instrument, Span, info_span, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

/// Execute a single tool call.
///
/// This function:
/// 1. Emits tool call start event
/// 2. Creates a snapshot if configured
/// 3. Records progress
/// 4. Checks permissions (if required)
/// 5. Executes the tool
/// 6. Truncates output if needed
/// 7. Creates snapshot diff/metadata
/// 8. Returns the tool result
#[instrument(
    name = "agent.tool.execute",
    skip(config, call, exec_ctx, bridge),
    fields(
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
    )
)]
pub(super) async fn execute_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ToolResult, anyhow::Error> {
    genai::agent_id(&Span::current(), config.provider.agent_id.as_deref());
    Span::current().set_attribute("querymt.tool.execution", "not_started");
    execute_tool_call_inner(config, call, exec_ctx, bridge)
        .await
        .inspect_err(|_| {
            if !exec_ctx.cancellation_token.is_cancelled() {
                genai::error("tool_pipeline_error");
            }
        })
}

async fn execute_tool_call_inner(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ToolResult, anyhow::Error> {
    debug!(
        "Executing tool: session={}, tool={}",
        exec_ctx.session_id, call.function.name
    );

    let mut args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| serde_json::json!({}));
    let hook_result = config
        .hooks
        .run_pre_tool_use(PreToolUseRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: exec_ctx.turn_id().unwrap_or_default().to_string(),
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
    if let Some(updated_input) = hook_result.updated_input {
        args = updated_input;
    }
    let hook_allows_interactive = matches!(
        hook_result.permission_decision,
        Some(crate::hooks::engine::PreToolPermissionDecision::Allow)
    );
    if hook_result.should_block {
        Span::current().set_attribute("querymt.tool.execution", "hook_blocked");
        genai::error("hook_blocked");
        let reason = hook_result
            .block_reason
            .unwrap_or_else(|| "tool blocked by hook".to_string());
        return Ok(ToolResult::new(
            call.id.clone(),
            vec![querymt::chat::ToolResultPart::Text {
                text: format!("Error: {}", reason),
            }],
            true,
            Some(call.function.name.clone()),
            Some(serde_json::to_string(&args).unwrap_or_else(|_| call.function.arguments.clone())),
        )
        .with_execution(true, "blocked")
        .with_hook_contexts(hook_result.context_contributions));
    }

    if let Err(error) = validate_tool_arguments(config, exec_ctx, &call.function.name, &args).await
    {
        Span::current().set_attribute("querymt.tool.execution", "validation_rejected");
        genai::error("invalid_tool_arguments");
        return Ok(ToolResult::new(
            call.id.clone(),
            vec![querymt::chat::ToolResultPart::Text {
                text: format!("Error: invalid tool arguments: {}", error),
            }],
            true,
            Some(call.function.name.clone()),
            Some(serde_json::to_string(&args).unwrap_or_else(|_| call.function.arguments.clone())),
        )
        .with_execution(true, "validation")
        .with_hook_contexts(hook_result.context_contributions));
    }

    let arguments_json =
        serde_json::to_string(&args).unwrap_or_else(|_| call.function.arguments.clone());

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ToolCallStart {
            tool_call_id: call.id.clone(),
            tool_name: call.function.name.clone(),
            arguments: arguments_json.clone(),
        },
    );

    let snapshot = if config.should_snapshot_tool(&call.function.name) {
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
                            let pre_tree = crate::index::merkle::MerkleTree::scan(root.as_path());
                            SnapshotState::Diff { pre_tree, root }
                        }
                        SnapshotPolicy::Metadata => SnapshotState::Metadata { root },
                        SnapshotPolicy::None => SnapshotState::None,
                    }
                })
                .unwrap_or(SnapshotState::None)
        })
    } else {
        SnapshotState::None
    };

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

    // Set up elicitation channel for this tool call
    let (elicitation_tx, mut elicitation_rx) =
        tokio::sync::mpsc::channel::<crate::tools::ElicitationRequest>(1);

    let event_sink = config.event_sink.clone();
    let session_id_clone = exec_ctx.session_id.clone();
    let run_id_clone = exec_ctx.turn_id().unwrap_or_default().to_string();
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

    let tool_context = exec_ctx
        .tool_context(config.agent_registry.clone(), Some(elicitation_tx))
        .with_task_service(crate::session::TaskService::new(
            exec_ctx.state.store.clone(),
            exec_ctx.session_id.clone(),
            call.id.clone(),
        ));

    // Check tool permission using the per-session tool config so that runtime
    // mutations (SetAllowedTools / SetDeniedTools) are respected. Also look up
    // the MCP server name so that "servername.*" wildcard entries in the
    // allowlist correctly permit MCP tools (e.g. "context7.*" → "resolve-library-id").
    //
    // Load a lock-free snapshot of the MCP tool state. The `Guard` is
    // dropped before any `.await` (same discipline as the old RwLock).
    let (mcp_server_name, mcp_tool) = {
        let snap = exec_ctx.runtime.mcp_tool_state.load();
        let server_name = snap
            .tools
            .get(&call.function.name)
            .map(|a| a.server_name().to_owned());
        let tool = snap.tools.get(&call.function.name).cloned();
        (server_name, tool)
    };

    let (raw_result_blocks, is_error, tool_source, execution) =
        if !crate::agent::tools::is_mcp_tool_allowed_with(
            &exec_ctx.tool_config,
            &call.function.name,
            mcp_server_name.as_deref(),
        ) {
            (
                vec![querymt::chat::ToolResultPart::Text {
                    text: format!("Error: tool '{}' is not allowed", call.function.name),
                }],
                true,
                "blocked",
                "policy_blocked",
            )
        } else if let Some(tool) = config.tool_registry.find(&call.function.name) {
            match genai::tool_scope(
                Span::current(),
                tool.call(args.clone(), &tool_context)
                    .instrument(info_span!(
                        "agent.tool.invoke",
                        source = "builtin",
                        tool_name = %call.function.name,
                        tool_call_id = %call.id,
                    )),
            )
            .await
            {
                Ok(res) => (res, false, "builtin", "executed"),
                Err(e) => (
                    vec![querymt::chat::ToolResultPart::Text {
                        text: format!("Error: {}", e),
                    }],
                    true,
                    "builtin",
                    if matches!(&e, crate::tools::ToolError::PermissionDenied(_)) {
                        "permission_denied"
                    } else {
                        "executed"
                    },
                ),
            }
        } else if let Some(tool) = mcp_tool {
            use querymt::tool_decorator::CallFunctionTool;
            match tool
                .call(args.clone())
                .instrument(info_span!(
                    "agent.tool.invoke",
                    source = "mcp",
                    tool_name = %call.function.name,
                    tool_call_id = %call.id,
                ))
                .await
            {
                Ok(res) => (res, false, "mcp", "executed"),
                Err(e) => (
                    vec![querymt::chat::ToolResultPart::Text {
                        text: format!("Error: {}", e),
                    }],
                    true,
                    "mcp",
                    "executed",
                ),
            }
        } else if !ensure_tool_permission(
            config,
            exec_ctx,
            &call.id,
            &call.function.name,
            &args,
            bridge,
            PermissionContext {
                turn_id: exec_ctx.turn_id().unwrap_or_default(),
                bypass_interactive_prompt: hook_allows_interactive,
            },
        )
        .instrument(info_span!(
            "agent.tool.permission_wait",
            tool_name = %call.function.name,
            tool_call_id = %call.id,
            session_id = %exec_ctx.session_id,
        ))
        .await
        .map_err(|e| anyhow::anyhow!("Permission check failed: {}", e))?
        {
            (
                vec![querymt::chat::ToolResultPart::Text {
                    text: "Error: permission denied".to_string(),
                }],
                true,
                "provider",
                "permission_denied",
            )
        } else {
            match exec_ctx
                .session_handle
                .call_tool(&call.function.name, args.clone())
                .instrument(info_span!(
                    "agent.tool.invoke",
                    source = "provider",
                    tool_name = %call.function.name,
                    tool_call_id = %call.id,
                ))
                .await
            {
                Ok(res) => (res, false, "provider", "executed"),
                Err(e) => (
                    vec![querymt::chat::ToolResultPart::Text {
                        text: format!("Error: {}", e),
                    }],
                    true,
                    "provider",
                    "executed",
                ),
            }
        };

    let span = Span::current();
    span.record("tool_source", tool_source);
    span.record("is_error", is_error);
    span.set_attribute("querymt.tool.execution", execution);
    if is_error && !exec_ctx.cancellation_token.is_cancelled() {
        genai::error(match execution {
            "policy_blocked" => "policy_blocked",
            "permission_denied" => "permission_denied",
            _ => "tool_error",
        });
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
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: exec_ctx.turn_id().unwrap_or_default().to_string(),
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
            tool_source: tool_source.to_string(),
            tool_use_id: call.id.clone(),
        })
        .await?;
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
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::SnapshotEnd {
                    summary: Some(changed_paths.summary()),
                },
            );
            Some(MessagePart::Snapshot {
                root_hash: post_tree.root_hash,
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

    let mut hook_contexts = hook_result.context_contributions;
    hook_contexts.extend(post_hook.context_contributions);
    let mut tool_result = ToolResult::new(
        call.id.clone(),
        result_blocks,
        is_error,
        Some(call.function.name.clone()),
        Some(arguments_json.clone()),
    )
    .with_execution(execution_is_error, tool_source)
    .with_hook_contexts(hook_contexts);
    if let Some(part) = snapshot_part {
        tool_result = tool_result.with_snapshot(part);
    }

    Span::current().record("has_snapshot", tool_result.snapshot_part.is_some());

    Ok(tool_result)
}

async fn validate_tool_arguments(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    tool_name: &str,
    arguments: &serde_json::Value,
) -> anyhow::Result<()> {
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
    let schema = match schema {
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
    };
    let Some(schema) = schema else {
        return Ok(());
    };
    let schema_key = serde_json::to_string(&schema)?;
    let validator = {
        let cache = exec_ctx.runtime.tool_validator_cache.lock();
        cache.get(&schema_key).cloned()
    };
    let validator = match validator {
        Some(validator) => validator,
        None => {
            let validator = Arc::new(
                jsonschema::validator_for(&schema)
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
    validator
        .validate(arguments)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
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

pub(super) struct PermissionContext<'a> {
    turn_id: &'a str,
    bypass_interactive_prompt: bool,
}

/// Check if a tool call requires permission and request it if needed.
///
/// Returns `true` if permission is granted (or not required), `false` if denied.
#[instrument(
    name = "agent.tool.permission",
    skip(config, exec_ctx, args, bridge, permission),
    fields(
        session_id = %exec_ctx.session_id,
        tool_name = %tool_name,
        tool_call_id = %tool_call_id,
        requires_permission = tracing::field::Empty,
        cache_hit = tracing::field::Empty,
        granted = tracing::field::Empty
    )
)]
pub(super) async fn ensure_tool_permission(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    bridge: Option<&ClientBridgeSender>,
    permission: PermissionContext<'_>,
) -> Result<bool, agent_client_protocol::Error> {
    use crate::acp::protocol::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
        RequestPermissionRequest, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };
    use crate::agent::utils::{extract_locations, tool_kind_for_tool};

    let requires_permission = config.requires_permission_for_tool(tool_name);
    Span::current().record("requires_permission", requires_permission);
    if !requires_permission {
        Span::current().record("cache_hit", false);
        Span::current().record("granted", true);
        return Ok(true);
    }

    if let Some(task) = &exec_ctx.state.active_task
        && task.status != TaskStatus::Active
    {
        return Ok(false);
    }

    {
        let cache = exec_ctx.runtime.permission_cache.lock();
        if let Some(cached) = cache.get(tool_name) {
            Span::current().record("cache_hit", true);
            Span::current().record("granted", *cached);
            return Ok(*cached);
        }
    }
    Span::current().record("cache_hit", false);

    if permission.bypass_interactive_prompt {
        Span::current().record("granted", true);
        return Ok(true);
    }

    let hook_decision = config
        .hooks
        .run_permission_request(crate::hooks::PermissionRequestRequest {
            session_id: exec_ctx.session_id.clone(),
            mcp_tool_state: Some(exec_ctx.runtime.mcp_tool_state.clone()),
            turn_id: permission.turn_id.to_string(),
            cwd: exec_ctx.cwd().map(|path| path.to_path_buf()),
            model: exec_ctx
                .llm_config()
                .map(|cfg| cfg.model.clone())
                .unwrap_or_default(),
            permission_mode: exec_ctx.permission_mode().to_string(),
            tool_name: tool_name.to_string(),
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
        Some(PermissionRequestDecision::Allow) => {
            Span::current().record("granted", true);
            return Ok(true);
        }
        Some(PermissionRequestDecision::Deny { message }) => {
            log::debug!(
                "Session {}: permission hook denied {}: {}",
                exec_ctx.session_id,
                tool_name,
                message
            );
            Span::current().record("granted", false);
            return Ok(false);
        }
        None => {}
    }

    let permission_id = Uuid::new_v4().to_string();
    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::PermissionRequested {
            permission_id: permission_id.clone(),
            task_id: exec_ctx
                .state
                .active_task
                .as_ref()
                .map(|task| task.public_id.clone()),
            tool_name: tool_name.to_string(),
            reason: format!("Tool {} requires explicit permission", tool_name),
        },
    );

    // In the actor model, only the bridge is available (no client)
    let Some(bridge) = bridge else {
        // No bridge available — auto-grant permission
        config.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::PermissionGranted {
                permission_id,
                granted: true,
            },
        );
        Span::current().record("granted", true);
        return Ok(true);
    };

    let locations = extract_locations(args);
    let tool_update_fields = ToolCallUpdateFields::new()
        .title(format!("Run {}", tool_name))
        .kind(tool_kind_for_tool(tool_name))
        .status(ToolCallStatus::Pending)
        .locations(if locations.is_empty() {
            None
        } else {
            Some(locations)
        })
        .raw_input(args.clone());

    let request = RequestPermissionRequest::new(
        exec_ctx.session_id.clone(),
        ToolCallUpdate::new(
            ToolCallId::from(tool_call_id.to_string()),
            tool_update_fields,
        ),
        vec![
            PermissionOption::new(
                PermissionOptionId::from("allow_once"),
                "Allow once",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::from("allow_always"),
                "Always allow",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(
                PermissionOptionId::from("reject_once"),
                "Reject once",
                PermissionOptionKind::RejectOnce,
            ),
            PermissionOption::new(
                PermissionOptionId::from("reject_always"),
                "Always reject",
                PermissionOptionKind::RejectAlways,
            ),
        ],
    );

    let response = bridge.request_permission(request).await?;
    let granted = match response.outcome {
        RequestPermissionOutcome::Selected(selected) => {
            let option_id = selected.option_id.0.as_ref();
            let allow = option_id == "allow_once" || option_id == "allow_always";
            {
                let mut cache = exec_ctx.runtime.permission_cache.lock();
                if option_id == "allow_always" {
                    cache.insert(tool_name.to_string(), true);
                } else if option_id == "reject_always" {
                    cache.insert(tool_name.to_string(), false);
                }
            }
            allow
        }
        _ => false,
    };

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::PermissionGranted {
            permission_id,
            granted,
        },
    );

    Span::current().record("granted", granted);

    Ok(granted)
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

        let pre_hook = config
            .hooks
            .run_pre_delegation(PreDelegationRequest {
                session_id: exec_ctx.session_id.clone(),
                turn_id: exec_ctx.turn_id().unwrap_or_default().to_string(),
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
        } = pre_hook.updated_delegation.unwrap_or_default();

        let target_agent_id = updated_target_agent_id.unwrap_or(target_agent_id);
        let objective = updated_objective.unwrap_or(objective);
        let mut context_val = updated_context.or(context_val);
        let constraints = updated_constraints.or(constraints);
        let expected_output = updated_expected_output.or(expected_output);

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

        if let Ok(serialized_args) = serde_json::to_string(&serde_json::json!({
            "target_agent_id": target_agent_id,
            "objective": objective,
            "context": context_val,
            "constraints": constraints,
            "expected_output": expected_output,
        })) {
            result.tool_arguments = Some(serialized_args);
        }

        if let Ok(delegation) = exec_ctx
            .state
            .record_delegation(
                target_agent_id.clone(),
                objective.clone(),
                context_val.clone(),
                constraints,
                expected_output,
            )
            .await
        {
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::DelegationRequested {
                    delegation: delegation.clone(),
                    tool_call_id: Some(result.call_id.clone()),
                },
            );
            return Ok(Some(WaitCondition::delegation(
                delegation.public_id.clone(),
            )));
        }
    }

    Ok(None)
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

    // Record per-result side effects (delegations, artifacts, etc.) before
    // persisting the combined tool_result message so hook-driven rewrites are
    // visible to the model in the stored transcript.
    let mut adjusted_results = Vec::with_capacity(results.len());
    for mut result in results.iter().cloned() {
        if let Some(wait_condition) =
            record_tool_side_effects(config, &mut result, exec_ctx).await?
        {
            wait_conditions.push(wait_condition);
        }
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

    messages.push(
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
        .with_session_mode(context.session_mode),
    );

    // Aggregate changed file paths from tool results for dedup check
    let mut combined = crate::index::DiffPaths::default();
    for result in results.iter() {
        if let Some(ref snapshot) = result.snapshot_part
            && let Some(paths) = snapshot.changed_paths()
        {
            combined.added.extend(paths.added.iter().cloned());
            combined.modified.extend(paths.modified.iter().cloned());
            combined.removed.extend(paths.removed.iter().cloned());
        }
    }

    combined.added.sort();
    combined.added.dedup();
    combined.modified.sort();
    combined.modified.dedup();
    combined.removed.sort();
    combined.removed.dedup();

    if !combined.is_empty() {
        let mut diffs = exec_ctx.runtime.turn_diffs.lock();
        diffs.added.extend(combined.added);
        diffs.modified.extend(combined.modified);
        diffs.removed.extend(combined.removed);
        diffs.added.sort();
        diffs.added.dedup();
        diffs.modified.sort();
        diffs.modified.dedup();
        diffs.removed.sort();
        diffs.removed.dedup();
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
            let (result, spans) =
                capture_info(execute_tool_call(&config, &call, &exec, None)).await;
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
            for span in &spans {
                let path = dir.path().display().to_string();
                let keys: Vec<_> = span
                    .attributes
                    .iter()
                    .filter(|attr| attr.value.to_string().contains(&path))
                    .map(|attr| attr.key.as_str())
                    .collect();
                let event_keys: Vec<_> = span
                    .events
                    .iter()
                    .flat_map(|event| event.attributes.iter())
                    .filter(|attr| attr.value.to_string().contains(&path))
                    .map(|attr| attr.key.as_str())
                    .collect();
                assert!(
                    keys.is_empty() && event_keys.is_empty(),
                    "mode={mode} span={} path attribute keys={keys:?} event keys={event_keys:?}",
                    span.name
                );
            }
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
            exec.tool_config.denylist.clear();
            if mode == "policy_blocked" {
                exec.tool_config.denylist.insert("shell".into());
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
            let (result, spans) =
                capture_info(execute_tool_call(&config, &call, &exec, None)).await;
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
                execute_tool_call(&config, &call, &exec, None)
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
        let (result, spans) = capture(execute_tool_call(&config, &call, &exec, None)).await;
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
            exec.runtime
                .permission_cache
                .lock()
                .insert("telemetry_tool".into(), false);
            let mut builder = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                fixture.config.provider.clone(),
                fixture.storage.event_journal(),
            )
            .with_mutating_tools(vec!["telemetry_tool".to_string()]);
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
            let (result, spans) = capture(execute_tool_call(&config, &call, &exec, None)).await;
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
