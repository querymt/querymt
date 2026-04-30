//! Shared types and functions for ACP server implementations.
//!
//! This module provides common types and utilities used by both stdio and WebSocket
//! ACP server implementations, including JSON-RPC types, event translation, and
//! RPC message handling.

use crate::agent::LocalAgentHandle as AgentHandle;
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EventEnvelope};
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use agent_client_protocol::schema::{
    Content, ContentBlock, ContentChunk, Error, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, RequestPermissionOutcome, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol_schema::AGENT_METHOD_NAMES;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

/// Type alias for session ownership mapping (session_id -> connection_id)
pub type SessionOwnerMap = Arc<Mutex<HashMap<String, String>>>;

/// Type alias for pending permission requests (tool_call_id -> response sender)
pub type PermissionMap = Arc<Mutex<HashMap<String, oneshot::Sender<RequestPermissionOutcome>>>>;

/// Type alias for pending elicitation requests (elicitation_id -> response sender)
pub type PendingElicitationMap = crate::elicitation::PendingElicitationMap;

/// JSON-RPC 2.0 request structure
#[derive(Deserialize)]
pub struct RpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
    pub id: serde_json::Value,
}

/// JSON-RPC 2.0 response structure
#[derive(Serialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    pub id: serde_json::Value,
}

/// Translate an internal agent event to a JSON-RPC notification.
///
/// Returns `None` if the event should not be sent to the client.
pub fn translate_event_to_notification(event: &EventEnvelope) -> Option<serde_json::Value> {
    // Handle ElicitationRequested specially - it's a custom notification, not a session/update
    if let AgentEventKind::ElicitationRequested {
        elicitation_id,
        session_id,
        message,
        requested_schema,
        source,
    } = event.kind()
    {
        return Some(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "elicitation/requested",
            "params": {
                "elicitationId": elicitation_id,
                "sessionId": session_id,
                "message": message,
                "requestedSchema": requested_schema,
                "source": source,
            }
        }));
    }

    let session_id = event.session_id().to_owned();
    let update = translate_event_to_update(event)?;

    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update
        }
    }))
}

/// Translate an agent event to a SessionUpdate.
///
/// Returns `None` if the event should not be sent to the client.
pub fn translate_event_to_update(event: &EventEnvelope) -> Option<SessionUpdate> {
    match event.kind() {
        AgentEventKind::PromptReceived { content, .. } => Some(SessionUpdate::UserMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(content.clone()))),
        )),
        AgentEventKind::AssistantMessageStored { content, .. } => {
            if content.is_empty() {
                return None;
            }
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(content.clone())),
            )))
        }
        // Streaming text deltas: forward to ACP clients so they also benefit from streaming.
        AgentEventKind::AssistantContentDelta { content, .. } => {
            if content.is_empty() {
                return None;
            }
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(content.clone())),
            )))
        }
        // Thinking/reasoning deltas: ACP has no thinking content type yet — drop.
        AgentEventKind::AssistantThinkingDelta { .. } => None,
        AgentEventKind::ToolCallStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            if is_todo_write_tool(tool_name) {
                return todo_plan_from_arguments(arguments).map(SessionUpdate::Plan);
            }

            let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
            Some(SessionUpdate::ToolCall(
                ToolCall::new(
                    ToolCallId::from(tool_call_id.clone()),
                    format!("Run {}", tool_name),
                )
                .kind(tool_kind_for_tool(tool_name))
                .status(ToolCallStatus::InProgress)
                .raw_input(args),
            ))
        }
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            if is_todo_write_tool(tool_name) {
                return None;
            }

            let status = if *is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let raw_output = serde_json::from_str(result).ok();
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(tool_call_id.clone()),
                ToolCallUpdateFields::new()
                    .kind(tool_kind_for_tool(tool_name))
                    .status(status)
                    .title(format!("Run {}", tool_name))
                    .content(vec![ToolCallContent::Content(Content::new(
                        ContentBlock::Text(TextContent::new(result.clone())),
                    ))])
                    .raw_output(raw_output),
            )))
        }
        _ => None,
    }
}

fn is_todo_write_tool(tool_name: &str) -> bool {
    matches!(tool_name, "todowrite" | "mcp_todowrite")
}

fn todo_plan_from_arguments(arguments: &str) -> Option<Plan> {
    let parsed: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let todos = parsed.get("todos")?.as_array()?;
    let mut entries = Vec::with_capacity(todos.len());

    for todo in todos {
        let content = todo.get("content")?.as_str()?.to_string();
        let Some(status) = todo
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_status_to_plan_status)
        else {
            continue;
        };

        let priority = todo
            .get("priority")
            .and_then(serde_json::Value::as_str)
            .and_then(todo_priority_to_plan_priority)
            .unwrap_or(PlanEntryPriority::Medium);

        entries.push(PlanEntry::new(content, priority, status));
    }

    Some(Plan::new(entries))
}

fn todo_priority_to_plan_priority(priority: &str) -> Option<PlanEntryPriority> {
    match priority {
        "high" => Some(PlanEntryPriority::High),
        "medium" => Some(PlanEntryPriority::Medium),
        "low" => Some(PlanEntryPriority::Low),
        _ => None,
    }
}

fn todo_status_to_plan_status(status: &str) -> Option<PlanEntryStatus> {
    match status {
        "pending" => Some(PlanEntryStatus::Pending),
        "in_progress" => Some(PlanEntryStatus::InProgress),
        "completed" => Some(PlanEntryStatus::Completed),
        // ACP plans do not support a cancelled state; omit these entries.
        "cancelled" => None,
        _ => None,
    }
}

/// Map tool names to ToolKind enum.
pub fn tool_kind_for_tool(name: &str) -> ToolKind {
    match name {
        "search_text" => ToolKind::Search,
        "write_file" | "edit" | "multiedit" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" | "browse" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Check if an event belongs to a specific connection.
///
/// Also handles session forking (delegation) by propagating ownership to child sessions.
pub async fn is_event_owned(
    session_owners: &SessionOwnerMap,
    conn_id: &str,
    event: &EventEnvelope,
) -> bool {
    // Handle session forking - propagate ownership to child sessions
    if let AgentEventKind::SessionForked {
        parent_session_id,
        child_session_id,
        origin,
        ..
    } = event.kind()
        && matches!(origin, ForkOrigin::Delegation)
    {
        let mut owners = session_owners.lock().await;
        if let Some(owner) = owners.get(parent_session_id).cloned() {
            owners.insert(child_session_id.clone(), owner);
        }
    }

    // Check if this connection owns the session
    let owners = session_owners.lock().await;
    owners
        .get(event.session_id())
        .map(|owner| owner == conn_id)
        .unwrap_or(false)
}

/// Collect EventFanout sources from agent and all delegate agents.
///
/// This function collects the EventFanout from the main agent and recursively
/// collects EventFanouts from all registered delegate agents. Each EventFanout is
/// deduplicated by pointer address to avoid subscribing multiple times.
///
/// # Arguments
/// * `agent` - The main agent to collect EventFanout sources from
///
/// # Returns
/// A vector of unique EventFanout instances
pub fn collect_event_sources(agent: &Arc<AgentHandle>) -> Vec<Arc<EventFanout>> {
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let primary = agent.config.event_sink.fanout().clone();
    if seen.insert(Arc::as_ptr(&primary) as usize) {
        sources.push(primary);
    }

    let registry = agent.agent_registry();
    for info in registry.list_agents() {
        if let Some(handle) = registry.get_handle(&info.id) {
            let fanout = handle.event_fanout().clone();
            if seen.insert(Arc::as_ptr(&fanout) as usize) {
                sources.push(fanout);
            }
        }
    }

    sources
}

/// Re-export from session_registry — the single source of truth for config option shape.
/// Used by tests in this module and by the config_option_update notification handler.
#[cfg(test)]
use crate::agent::session_registry::config_options as session_config_options;

/// Handle an RPC request and return a response.
///
/// This function routes JSON-RPC methods to the appropriate `SendAgent` trait methods.
pub async fn handle_rpc_message<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    pending_permissions: &PermissionMap,
    pending_elicitations: &PendingElicitationMap,
    conn_id: &str,
    req: RpcRequest,
) -> RpcResponse {
    let result = match req.method.as_str() {
        m if m == AGENT_METHOD_NAMES.initialize => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .initialize(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.authenticate => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .authenticate(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },

        m if m == AGENT_METHOD_NAMES.session_new => match serde_json::from_value(req.params) {
            Ok(params) => {
                let response = agent.new_session(params).await;
                match response {
                    Ok(r) => {
                        let session_id = r.session_id.to_string();
                        let mut owners = session_owners.lock().await;
                        owners.insert(session_id, conn_id.to_string());
                        Ok(serde_json::to_value(r).unwrap())
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(Error::invalid_params().data(serde_json::json!({
                "error": e.to_string()
            }))),
        },
        m if m == AGENT_METHOD_NAMES.session_prompt => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .prompt(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },

        m if m == AGENT_METHOD_NAMES.session_cancel => match serde_json::from_value(req.params) {
            Ok(params) => agent.cancel(params).await.map(|_| serde_json::Value::Null),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_fork => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .fork_session(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_list => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .list_sessions(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_load => match serde_json::from_value(req.params) {
            Ok(params) => {
                let response = agent.load_session(params).await;
                match response {
                    Ok(r) => Ok(serde_json::to_value(r).unwrap()),
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_resume => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .resume_session(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_set_config_option => {
            match serde_json::from_value(req.params) {
                Ok(params) => agent
                    .set_session_config_option(params)
                    .await
                    .map(|r| serde_json::to_value(r).unwrap()),
                Err(e) => {
                    Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
                }
            }
        }
        m if m == AGENT_METHOD_NAMES.session_set_mode => match serde_json::from_value(req.params) {
            Ok(params) => agent
                .set_session_mode(params)
                .await
                .map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => {
                Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
            }
        },
        m if m == AGENT_METHOD_NAMES.session_set_model => {
            match serde_json::from_value(req.params) {
                Ok(params) => agent
                    .set_session_model(params)
                    .await
                    .map(|r| serde_json::to_value(r).unwrap()),
                Err(e) => {
                    Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
                }
            }
        }

        "permission_result" => {
            #[derive(Deserialize)]
            struct PermissionResultParams {
                tool_call_id: String,
                outcome: RequestPermissionOutcome,
            }
            match serde_json::from_value::<PermissionResultParams>(req.params) {
                Ok(params) => {
                    let mut pending = pending_permissions.lock().await;
                    if let Some(tx) = pending.remove(&params.tool_call_id) {
                        let _ = tx.send(params.outcome);
                        Ok(serde_json::Value::Null)
                    } else {
                        Err(Error::internal_error()
                            .data("No pending permission for this tool_call_id"))
                    }
                }
                Err(e) => {
                    Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
                }
            }
        }

        "elicitation_result" => {
            #[derive(Deserialize)]
            struct ElicitationResultParams {
                elicitation_id: String,
                action: String,
                content: Option<serde_json::Value>,
            }
            match serde_json::from_value::<ElicitationResultParams>(req.params) {
                Ok(params) => {
                    // Parse action string to enum
                    let action_result = match params.action.as_str() {
                        "accept" => Ok(crate::elicitation::ElicitationAction::Accept),
                        "decline" => Ok(crate::elicitation::ElicitationAction::Decline),
                        "cancel" => Ok(crate::elicitation::ElicitationAction::Cancel),
                        _ => Err(Error::invalid_params().data(serde_json::json!({
                            "error": format!("Invalid action: {}", params.action)
                        }))),
                    };

                    match action_result {
                        Ok(action) => {
                            let response = crate::elicitation::ElicitationResponse {
                                action,
                                content: params.content,
                            };

                            let mut tx = {
                                let mut pending = pending_elicitations.lock().await;
                                pending.remove(&params.elicitation_id)
                            };

                            if tx.is_none()
                                && let Some(query_agent) =
                                    agent.as_any().downcast_ref::<AgentHandle>()
                            {
                                tx = crate::elicitation::take_pending_elicitation_sender(
                                    query_agent,
                                    &params.elicitation_id,
                                )
                                .await;
                            }

                            if let Some(tx) = tx {
                                let _ = tx.send(response);
                                Ok(serde_json::Value::Null)
                            } else {
                                Err(Error::internal_error()
                                    .data("No pending elicitation for this elicitation_id"))
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => {
                    Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
                }
            }
        }

        // Forward _querymt/* extension methods to the agent's ext_method handler.
        m if m.starts_with("_querymt/") || m.starts_with("querymt/") => {
            let raw_params = serde_json::value::RawValue::from_string(
                serde_json::to_string(&req.params).unwrap_or_else(|_| "null".to_string()),
            )
            .unwrap_or_else(|_| {
                serde_json::value::RawValue::from_string("null".to_string()).unwrap()
            });
            let ext_req =
                agent_client_protocol::schema::ExtRequest::new(m, std::sync::Arc::from(raw_params));
            agent
                .ext_method(ext_req)
                .await
                .map(|r| serde_json::from_str(r.0.get()).unwrap_or(serde_json::Value::Null))
        }

        _ => Err(Error::method_not_found()),
    };

    match result {
        Ok(res) => RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: Some(res),
            error: None,
            id: req.id,
        },
        Err(e) => RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(serde_json::to_value(e).unwrap()),
            id: req.id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::core::AgentMode;
    use crate::elicitation::ElicitationAction;
    use crate::events::{AgentEventKind, DurableEvent, EventEnvelope, EventOrigin};
    use crate::test_utils::DelegateTestFixture;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::sync::oneshot;

    fn tool_start_event(tool_name: &str, arguments: serde_json::Value) -> EventEnvelope {
        EventEnvelope::Durable(DurableEvent {
            event_id: "evt-1".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: tool_name.to_string(),
                arguments: arguments.to_string(),
            },
        })
    }

    fn tool_end_event(tool_name: &str) -> EventEnvelope {
        EventEnvelope::Durable(DurableEvent {
            event_id: "evt-2".into(),
            stream_seq: 2,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallEnd {
                tool_call_id: "tc-1".to_string(),
                tool_name: tool_name.to_string(),
                result: "{}".to_string(),
                is_error: false,
            },
        })
    }

    #[test]
    fn todo_tool_start_translates_to_plan_update() {
        let event = tool_start_event(
            "todowrite",
            serde_json::json!({
                "todos": [
                    {"id": "a", "content": "task a", "status": "pending", "priority": "high"},
                    {"id": "b", "content": "task b", "status": "in_progress", "priority": "medium"},
                    {"id": "c", "content": "task c", "status": "completed", "priority": "low"}
                ]
            }),
        );

        let update = translate_event_to_update(&event);
        let Some(SessionUpdate::Plan(plan)) = update else {
            panic!("expected plan update");
        };

        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].content, "task a");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::Pending);
        assert_eq!(plan.entries[0].priority, PlanEntryPriority::High);
    }

    #[test]
    fn cancelled_todos_are_omitted_from_plan() {
        let event = tool_start_event(
            "mcp_todowrite",
            serde_json::json!({
                "todos": [
                    {"id": "a", "content": "task a", "status": "cancelled", "priority": "high"},
                    {"id": "b", "content": "task b", "status": "pending", "priority": "low"}
                ]
            }),
        );

        let update = translate_event_to_update(&event);
        let Some(SessionUpdate::Plan(plan)) = update else {
            panic!("expected plan update");
        };

        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].content, "task b");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::Pending);
    }

    #[test]
    fn malformed_todowrite_arguments_do_not_emit_update() {
        let event = EventEnvelope::Durable(DurableEvent {
            event_id: "evt-3".into(),
            stream_seq: 1,
            timestamp: 0,
            session_id: "s-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::ToolCallStart {
                tool_call_id: "tc-1".to_string(),
                tool_name: "todowrite".to_string(),
                arguments: "{ not-json".to_string(),
            },
        });

        assert!(translate_event_to_update(&event).is_none());
    }

    #[test]
    fn todo_tools_do_not_emit_tool_call_end_updates() {
        assert!(translate_event_to_update(&tool_end_event("todowrite")).is_none());
        assert!(translate_event_to_update(&tool_end_event("mcp_todowrite")).is_none());
    }

    #[test]
    fn non_todo_tools_still_emit_tool_call_updates() {
        let start = tool_start_event("read_tool", serde_json::json!({"path": "src/main.rs"}));
        let end = tool_end_event("read_tool");

        assert!(matches!(
            translate_event_to_update(&start),
            Some(SessionUpdate::ToolCall(_))
        ));
        assert!(matches!(
            translate_event_to_update(&end),
            Some(SessionUpdate::ToolCallUpdate(_))
        ));
    }

    #[test]
    fn mode_config_option_contains_expected_shape() {
        use agent_client_protocol::schema::SessionConfigOptionCategory;
        let options = session_config_options(AgentMode::Plan, None);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id.0.as_ref(), "mode");
        assert_eq!(options[0].category, Some(SessionConfigOptionCategory::Mode));

        let select = match &options[0].kind {
            agent_client_protocol::schema::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "plan");
    }

    #[test]
    fn reasoning_effort_config_option_contains_expected_shape() {
        use agent_client_protocol::schema::SessionConfigOptionCategory;
        let options =
            session_config_options(AgentMode::Build, Some(querymt::chat::ReasoningEffort::High));
        assert_eq!(options.len(), 2);
        assert_eq!(options[1].id.0.as_ref(), "reasoning_effort");
        assert_eq!(
            options[1].category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );

        let select = match &options[1].kind {
            agent_client_protocol::schema::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "high");
    }

    #[test]
    fn reasoning_effort_config_option_auto_when_none() {
        let options = session_config_options(AgentMode::Build, None);
        let select = match &options[1].kind {
            agent_client_protocol::schema::SessionConfigKind::Select(select) => select,
            _ => panic!("expected select config option"),
        };
        assert_eq!(select.current_value.0.as_ref(), "auto");
    }

    /// Verify that the RPC dispatcher correctly forwards set_session_config_option
    /// to the SendAgent trait method (default impl returns method_not_found).
    #[tokio::test]
    async fn set_config_option_rpc_forwards_to_send_agent() {
        let session_owners: SessionOwnerMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));

        // Minimal SendAgent that returns method_not_found for set_session_config_option
        // (the default impl). This proves the dispatcher forwards correctly.
        struct Dummy;

        #[async_trait::async_trait]
        impl SendAgent for Dummy {
            async fn initialize(
                &self,
                _: agent_client_protocol::schema::InitializeRequest,
            ) -> Result<agent_client_protocol::schema::InitializeResponse, Error> {
                unreachable!()
            }
            async fn authenticate(
                &self,
                _: agent_client_protocol::schema::AuthenticateRequest,
            ) -> Result<agent_client_protocol::schema::AuthenticateResponse, Error> {
                unreachable!()
            }
            async fn new_session(
                &self,
                _: agent_client_protocol::schema::NewSessionRequest,
            ) -> Result<agent_client_protocol::schema::NewSessionResponse, Error> {
                unreachable!()
            }
            async fn prompt(
                &self,
                _: agent_client_protocol::schema::PromptRequest,
            ) -> Result<agent_client_protocol::schema::PromptResponse, Error> {
                unreachable!()
            }
            async fn cancel(
                &self,
                _: agent_client_protocol::schema::CancelNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            async fn load_session(
                &self,
                _: agent_client_protocol::schema::LoadSessionRequest,
            ) -> Result<agent_client_protocol::schema::LoadSessionResponse, Error> {
                unreachable!()
            }
            async fn list_sessions(
                &self,
                _: agent_client_protocol::schema::ListSessionsRequest,
            ) -> Result<agent_client_protocol::schema::ListSessionsResponse, Error> {
                unreachable!()
            }
            async fn fork_session(
                &self,
                _: agent_client_protocol::schema::ForkSessionRequest,
            ) -> Result<agent_client_protocol::schema::ForkSessionResponse, Error> {
                unreachable!()
            }
            async fn resume_session(
                &self,
                _: agent_client_protocol::schema::ResumeSessionRequest,
            ) -> Result<agent_client_protocol::schema::ResumeSessionResponse, Error> {
                unreachable!()
            }
            async fn set_session_model(
                &self,
                _: agent_client_protocol::schema::SetSessionModelRequest,
            ) -> Result<agent_client_protocol::schema::SetSessionModelResponse, Error> {
                unreachable!()
            }
            async fn ext_method(
                &self,
                _: agent_client_protocol::schema::ExtRequest,
            ) -> Result<agent_client_protocol::schema::ExtResponse, Error> {
                unreachable!()
            }
            async fn ext_notification(
                &self,
                _: agent_client_protocol::schema::ExtNotification,
            ) -> Result<(), Error> {
                unreachable!()
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let response = handle_rpc_message(
            &Dummy,
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                method: "session/set_config_option".to_string(),
                params: serde_json::json!({
                    "sessionId": "s-1",
                    "configId": "mode",
                    "value": "plan"
                }),
                id: serde_json::json!(1),
            },
        )
        .await;

        // Default SendAgent impl returns method_not_found
        assert!(response.error.is_some(), "expected error from default impl");
        let err: agent_client_protocol::Error =
            serde_json::from_value(response.error.unwrap()).unwrap();
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }

    #[tokio::test]
    async fn elicitation_result_routes_to_delegate_pending_map() {
        let fixture = DelegateTestFixture::new().await.unwrap();

        let elicitation_id = "delegate-elicitation-rpc".to_string();
        let (tx, rx) = oneshot::channel();
        fixture
            .delegate
            .pending_elicitations()
            .lock()
            .await
            .insert(elicitation_id.clone(), tx);

        let session_owners: SessionOwnerMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_permissions: PermissionMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations = fixture.planner.pending_elicitations();

        let response = handle_rpc_message(
            fixture.planner.as_ref(),
            &session_owners,
            &pending_permissions,
            &pending_elicitations,
            "conn-1",
            RpcRequest {
                jsonrpc: "2.0".to_string(),
                method: "elicitation_result".to_string(),
                params: serde_json::json!({
                    "elicitation_id": elicitation_id,
                    "action": "accept",
                    "content": {"selection": "allow_once"}
                }),
                id: serde_json::json!(1),
            },
        )
        .await;

        assert!(
            response.error.is_none(),
            "rpc should succeed: {:?}",
            response.error
        );
        let delivered = rx
            .await
            .expect("delegate elicitation should receive response");
        assert_eq!(delivered.action, ElicitationAction::Accept);
        assert_eq!(
            delivered.content,
            Some(serde_json::json!({"selection": "allow_once"}))
        );
    }
}
