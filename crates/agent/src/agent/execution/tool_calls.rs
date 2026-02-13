//! Tool execution, permission checking, and result storage
//!
//! This module handles the complete lifecycle of tool calls: execution, permission
//! checking, snapshotting, and storing results back into conversation history.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::SnapshotPolicy;
use crate::agent::execution_context::ExecutionContext;
use crate::agent::snapshots::{SnapshotState, snapshot_metadata};
use crate::events::AgentEventKind;
use crate::middleware::ToolCall as MiddlewareToolCall;
use crate::middleware::{ExecutionState, ToolResult, WaitCondition};
use crate::model::{AgentMessage, MessagePart};
use crate::session::domain::TaskStatus;
use log::debug;
use querymt::chat::ChatRole;
use std::sync::Arc;
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
pub(super) async fn execute_tool_call(
    config: &AgentConfig,
    call: &MiddlewareToolCall,
    exec_ctx: &ExecutionContext,
    bridge: Option<&ClientBridgeSender>,
) -> Result<ToolResult, anyhow::Error> {
    debug!(
        "Executing tool: session={}, tool={}",
        exec_ctx.session_id, call.function.name
    );

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ToolCallStart {
            tool_call_id: call.id.clone(),
            tool_name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        },
    );

    let snapshot = if config.should_snapshot_tool(&call.function.name) {
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
    } else {
        SnapshotState::None
    };

    let progress_entry = exec_ctx
        .state
        .record_progress(
            crate::session::domain::ProgressKind::ToolCall,
            format!("Calling tool: {}", call.function.name),
            Some(serde_json::from_str(&call.function.arguments).unwrap_or_default()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to record progress: {}", e))?;

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ProgressRecorded { progress_entry },
    );

    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).unwrap_or_else(|_| serde_json::json!({}));

    // Set up elicitation channel for this tool call
    let (elicitation_tx, mut elicitation_rx) =
        tokio::sync::mpsc::channel::<crate::tools::ElicitationRequest>(1);

    let event_bus = config.event_bus.clone();
    let session_id_clone = exec_ctx.session_id.clone();
    let pending_elicitations = config.pending_elicitations.clone();
    tokio::spawn(async move {
        while let Some(request) = elicitation_rx.recv().await {
            let elicitation_id = request.elicitation_id.clone();
            {
                let mut pending = pending_elicitations.lock().await;
                pending.insert(elicitation_id.clone(), request.response_tx);
            }
            event_bus.publish(
                &session_id_clone,
                crate::events::AgentEventKind::ElicitationRequested {
                    elicitation_id,
                    session_id: session_id_clone.clone(),
                    message: request.message,
                    requested_schema: request.requested_schema,
                    source: request.source,
                },
            );
        }
    });

    let tool_context = exec_ctx.tool_context(config.agent_registry.clone(), Some(elicitation_tx));

    let (raw_result_json, is_error) = if !config.is_tool_allowed(&call.function.name) {
        (
            format!("Error: tool '{}' is not allowed", call.function.name),
            true,
        )
    } else if let Some(tool) = config.tool_registry.find(&call.function.name) {
        match tool.call(args.clone(), &tool_context).await {
            Ok(res) => (res, false),
            Err(e) => (format!("Error: {}", e), true),
        }
    } else if let Some(tool) = exec_ctx.runtime.mcp_tools.get(&call.function.name) {
        use querymt::tool_decorator::CallFunctionTool;
        match tool.call(args.clone()).await {
            Ok(res) => (res, false),
            Err(e) => (format!("Error: {}", e), true),
        }
    } else if !ensure_tool_permission(
        config,
        exec_ctx,
        &call.id,
        &call.function.name,
        &args,
        bridge,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Permission check failed: {}", e))?
    {
        ("Error: permission denied".to_string(), true)
    } else {
        match exec_ctx
            .session_handle
            .call_tool(&call.function.name, args.clone())
            .await
        {
            Ok(res) => (res, false),
            Err(e) => (format!("Error: {}", e), true),
        }
    };

    // Apply Layer 1 truncation
    let result_json = if !is_error {
        use crate::tools::builtins::helpers::{
            TruncationDirection, format_truncation_message_with_overflow, save_overflow_output,
            truncate_output,
        };
        let tc = &config.tool_output_config;
        let truncation = truncate_output(
            &raw_result_json,
            tc.max_lines,
            tc.max_bytes,
            TruncationDirection::Head,
        );
        if truncation.was_truncated {
            let overflow = save_overflow_output(
                &raw_result_json,
                &tc.overflow_storage,
                &exec_ctx.session_id,
                &call.id,
                None,
            );

            let tool_hint = config
                .tool_registry
                .find(&call.function.name)
                .and_then(|t| t.truncation_hint());

            let suffix = format_truncation_message_with_overflow(
                &truncation,
                TruncationDirection::Head,
                Some(&overflow),
                tool_hint,
            );
            format!("{}{}", truncation.content, suffix)
        } else {
            raw_result_json
        }
    } else {
        raw_result_json
    };

    config.emit_event(
        &exec_ctx.session_id,
        AgentEventKind::ToolCallEnd {
            tool_call_id: call.id.clone(),
            tool_name: call.function.name.clone(),
            is_error,
            result: result_json.clone(),
        },
    );

    let snapshot_part = match snapshot {
        SnapshotState::Diff { pre_tree, root } => {
            let post_tree = crate::index::merkle::MerkleTree::scan(root.as_path());
            let changed_paths = post_tree.diff_paths(&pre_tree);
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
            let (part, summary) = snapshot_metadata(root.as_path());
            config.emit_event(
                &exec_ctx.session_id,
                AgentEventKind::SnapshotEnd { summary },
            );
            Some(part)
        }
        SnapshotState::None => None,
    };

    let mut tool_result = ToolResult::new(
        call.id.clone(),
        result_json,
        is_error,
        Some(call.function.name.clone()),
        Some(call.function.arguments.clone()),
    );
    if let Some(part) = snapshot_part {
        tool_result = tool_result.with_snapshot(part);
    }

    Ok(tool_result)
}

/// Check if a tool call requires permission and request it if needed.
///
/// Returns `true` if permission is granted (or not required), `false` if denied.
pub(super) async fn ensure_tool_permission(
    config: &AgentConfig,
    exec_ctx: &ExecutionContext,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    bridge: Option<&ClientBridgeSender>,
) -> Result<bool, agent_client_protocol::Error> {
    use crate::agent::utils::{extract_locations, tool_kind_for_tool};
    use agent_client_protocol::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
        RequestPermissionRequest, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };

    let requires_permission = config.requires_permission_for_tool(tool_name);
    if !requires_permission {
        return Ok(true);
    }

    if let Some(task) = &exec_ctx.state.active_task
        && task.status != TaskStatus::Active
    {
        return Ok(false);
    }

    if let Ok(cache) = exec_ctx.runtime.permission_cache.lock()
        && let Some(cached) = cache.get(tool_name)
    {
        return Ok(*cached);
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
            if let Ok(mut cache) = exec_ctx.runtime.permission_cache.lock() {
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

    Ok(granted)
}

/// Record side effects of a tool execution (artifacts, delegations).
///
/// Returns a wait condition if the tool initiated an action that requires waiting
/// (e.g., delegation).
pub(super) async fn record_tool_side_effects(
    config: &AgentConfig,
    result: &ToolResult,
    exec_ctx: &ExecutionContext,
) -> Option<WaitCondition> {
    if result.is_error {
        return None;
    }

    let tool_name = result.tool_name.as_ref()?;

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
                },
            );
            return Some(WaitCondition::delegation(delegation.public_id.clone()));
        }
    }

    None
}

/// Store all completed tool results back into conversation history.
///
/// This function:
/// 1. Creates user messages with tool results
/// 2. Stores them in the database
/// 3. Records side effects (artifacts, delegations)
/// 4. Aggregates file changes for deduplication
/// 5. Returns either WaitingForEvent (if delegation) or BeforeLlmCall
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

    for result in results.iter() {
        let mut parts = vec![MessagePart::ToolResult {
            call_id: result.call_id.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
            tool_name: result.tool_name.clone(),
            tool_arguments: result.tool_arguments.clone(),
            compacted_at: None,
        }];
        if let Some(ref snapshot) = result.snapshot_part {
            parts.push(snapshot.clone());
        }

        let result_msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: exec_ctx.session_id.clone(),
            role: ChatRole::User,
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };

        exec_ctx
            .add_message(result_msg.clone())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store tool result: {}", e))?;

        messages.push(result_msg.to_chat_message());

        if let Some(wait_condition) = record_tool_side_effects(config, result, exec_ctx).await {
            wait_conditions.push(wait_condition);
        }
    }

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

    if !combined.is_empty()
        && let Ok(mut diffs) = exec_ctx.runtime.turn_diffs.lock()
    {
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
