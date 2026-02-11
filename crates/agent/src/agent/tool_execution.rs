//! Tool execution logic for the agent
//!
//! This module contains the core tool execution logic, including:
//! - Executing individual tool calls
//! - Recording side effects (artifacts, delegations)
//! - Storing batch tool results

use crate::agent::core::{QueryMTAgent, SnapshotPolicy};
use crate::agent::execution_context::ExecutionContext;
use crate::agent::snapshots::{SnapshotState, snapshot_metadata};
use crate::events::AgentEventKind;
use crate::middleware::{
    ConversationContext, ExecutionState, ToolCall as MiddlewareToolCall, ToolResult, WaitCondition,
};
use crate::model::MessagePart;
use log::debug;
use querymt::chat::ChatRole;
use std::sync::Arc;
use tracing::instrument;
use uuid::Uuid;

impl QueryMTAgent {
    #[instrument(
        name = "agent.tool_call",
        skip(self, call, exec_ctx),
        fields(
            session_id = %exec_ctx.session_id,
            tool_name = %call.function.name,
            tool_call_id = %call.id,
            is_error = tracing::field::Empty
        )
    )]
    pub(crate) async fn execute_tool_call(
        &self,
        call: &MiddlewareToolCall,
        exec_ctx: &ExecutionContext,
    ) -> Result<ToolResult, anyhow::Error> {
        debug!(
            "Executing tool: session={}, tool={}",
            exec_ctx.session_id, call.function.name
        );

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::ToolCallStart {
                tool_call_id: call.id.clone(),
                tool_name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            },
        );

        let snapshot = if self.should_snapshot_tool(&call.function.name) {
            self.prepare_snapshot(exec_ctx.cwd())
                .map(|(root, policy)| {
                    self.emit_event(
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

        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::ProgressRecorded { progress_entry },
        );

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .unwrap_or_else(|_| serde_json::json!({}));

        let provider_context = self
            .provider
            .with_session(&exec_ctx.session_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        // Set up elicitation channel for this tool call
        let (elicitation_tx, mut elicitation_rx) =
            tokio::sync::mpsc::channel::<crate::tools::ElicitationRequest>(1);

        // Spawn task to handle elicitation requests from the tool
        let event_bus = self.event_bus.clone();
        let session_id_clone = exec_ctx.session_id.clone();
        let pending_elicitations = self.pending_elicitations.clone();
        tokio::spawn(async move {
            while let Some(request) = elicitation_rx.recv().await {
                let elicitation_id = request.elicitation_id.clone();
                // Store the oneshot sender in the pending map
                {
                    let mut pending = pending_elicitations.lock().await;
                    pending.insert(elicitation_id.clone(), request.response_tx);
                }
                // Emit event so clients learn about the elicitation request
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

        let tool_context = exec_ctx.tool_context(self.agent_registry.clone(), Some(elicitation_tx));

        let (raw_result_json, is_error) = if !self.is_tool_allowed(&call.function.name) {
            (
                format!("Error: tool '{}' is not allowed", call.function.name),
                true,
            )
        } else if let Some(tool) = self
            .tool_registry
            .lock()
            .ok()
            .and_then(|registry| registry.find(&call.function.name))
        {
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
        } else if !self
            .ensure_tool_permission(exec_ctx, &call.id, &call.function.name, &args)
            .await
            .map_err(|e| anyhow::anyhow!("Permission check failed: {}", e))?
        {
            ("Error: permission denied".to_string(), true)
        } else {
            match provider_context
                .call_tool(&call.function.name, args.clone())
                .await
            {
                Ok(res) => (res, false),
                Err(e) => (format!("Error: {}", e), true),
            }
        };

        // Apply Layer 1 truncation: cap tool output by line count and byte size.
        // This is a safety net that prevents any single tool result from consuming
        // excessive context, regardless of whether the tool itself limits its output.
        let result_json = if !is_error {
            use crate::tools::builtins::helpers::{
                TruncationDirection, format_truncation_message_with_overflow, save_overflow_output,
                truncate_output,
            };
            let config = &self.tool_output_config;
            let truncation = truncate_output(
                &raw_result_json,
                config.max_lines,
                config.max_bytes,
                TruncationDirection::Head,
            );
            if truncation.was_truncated {
                let overflow = save_overflow_output(
                    &raw_result_json,
                    &config.overflow_storage,
                    &exec_ctx.session_id,
                    &call.id,
                    None, // TODO: pass data_dir when available
                );

                // Get tool-specific truncation hint (if available)
                let tool_hint = self
                    .tool_registry
                    .lock()
                    .ok()
                    .and_then(|r| r.find(&call.function.name))
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

        // Note: We emit ToolCallEnd event which gets translated to SessionUpdate::ToolCallUpdate
        // via the EventBus path. The direct send_session_update() was removed to prevent
        // race conditions where the update could arrive before the initial ToolCall.
        self.emit_event(
            &exec_ctx.session_id,
            AgentEventKind::ToolCallEnd {
                tool_call_id: call.id.clone(),
                tool_name: call.function.name.clone(),
                is_error,
                result: result_json.clone(),
            },
        );

        // Record is_error in the tracing span
        tracing::Span::current().record("is_error", is_error);

        let snapshot_part = match snapshot {
            SnapshotState::Diff { pre_tree, root } => {
                let post_tree = crate::index::merkle::MerkleTree::scan(root.as_path());
                let changed_paths = post_tree.diff_paths(&pre_tree);
                self.emit_event(
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
                self.emit_event(
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

    /// Records artifacts and delegations produced by a tool execution.
    pub(crate) async fn record_tool_side_effects(
        &self,
        result: &ToolResult,
        exec_ctx: &ExecutionContext,
    ) -> Option<WaitCondition> {
        if result.is_error {
            return None;
        }

        let tool_name = result.tool_name.as_ref()?;

        // Record artifact for file-producing tools
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
                self.emit_event(
                    &exec_ctx.session_id,
                    AgentEventKind::ArtifactRecorded { artifact },
                );
            }
        }

        // Record delegation
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
                self.emit_event(
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

    #[instrument(
        name = "agent.store_tool_results",
        skip(self, results, context, exec_ctx),
        fields(
            session_id = %exec_ctx.session_id,
            result_count = %results.len()
        )
    )]
    pub(crate) async fn store_all_tool_results(
        &self,
        results: &Arc<[ToolResult]>,
        context: &Arc<ConversationContext>,
        exec_ctx: &mut ExecutionContext,
    ) -> Result<ExecutionState, anyhow::Error> {
        debug!(
            "Storing all tool results: session={}, count={}",
            exec_ctx.session_id,
            results.len()
        );

        let provider_context = self
            .provider
            .with_session(&exec_ctx.session_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get provider context: {}", e))?;

        // Store each tool result as a separate message
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

            let result_msg = crate::model::AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: exec_ctx.session_id.clone(),
                role: ChatRole::User,
                parts,
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            };

            provider_context
                .add_message(result_msg.clone())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to store tool result: {}", e))?;

            messages.push(result_msg.to_chat_message());

            if let Some(wait_condition) = self.record_tool_side_effects(result, exec_ctx).await {
                wait_conditions.push(wait_condition);
            }
        }

        let new_context = Arc::new(ConversationContext::new(
            context.session_id.clone(),
            Arc::from(messages.into_boxed_slice()),
            context.stats.clone(),
            context.provider.clone(),
            context.model.clone(),
        ));

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

        // Deduplicate paths
        combined.added.sort();
        combined.added.dedup();
        combined.modified.sort();
        combined.modified.dedup();
        combined.removed.sort();
        combined.removed.dedup();

        // Accumulate for end-of-turn dedup check
        if !combined.is_empty()
            && let Ok(mut diffs) = exec_ctx.runtime.turn_diffs.lock()
        {
            diffs.added.extend(combined.added);
            diffs.modified.extend(combined.modified);
            diffs.removed.extend(combined.removed);
            // Deduplicate
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
}
