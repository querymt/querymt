//! Shared types and functions for ACP server implementations.
//!
//! This module provides common types and utilities used by both stdio and WebSocket
//! ACP server implementations, including JSON-RPC types, event translation, and
//! RPC message handling.

use crate::agent::QueryMTAgent;
use crate::event_bus::EventBus;
use crate::events::{AgentEvent, AgentEventKind};
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use agent_client_protocol::{
    Content, ContentBlock, ContentChunk, Error, RequestPermissionOutcome, SessionUpdate,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
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
pub fn translate_event_to_notification(event: &AgentEvent) -> Option<serde_json::Value> {
    let session_id = event.session_id.clone();
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
pub fn translate_event_to_update(event: &AgentEvent) -> Option<SessionUpdate> {
    match &event.kind {
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
        AgentEventKind::ToolCallStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
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

/// Map tool names to ToolKind enum.
pub fn tool_kind_for_tool(name: &str) -> ToolKind {
    match name {
        "search_text" => ToolKind::Search,
        "write_file" | "apply_patch" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Check if an event belongs to a specific connection.
///
/// Also handles session forking (delegation) by propagating ownership to child sessions.
pub async fn is_event_owned(
    session_owners: &SessionOwnerMap,
    conn_id: &str,
    event: &AgentEvent,
) -> bool {
    // Handle session forking - propagate ownership to child sessions
    if let AgentEventKind::SessionForked {
        parent_session_id,
        child_session_id,
        origin,
        ..
    } = &event.kind
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
        .get(&event.session_id)
        .map(|owner| owner == conn_id)
        .unwrap_or(false)
}

/// Collect EventBus sources from agent and all delegate agents.
///
/// This function collects the EventBus from the main agent and recursively
/// collects EventBuses from all registered delegate agents. Each EventBus is
/// deduplicated by pointer address to avoid subscribing multiple times.
///
/// # Arguments
/// * `agent` - The main agent to collect EventBuses from
///
/// # Returns
/// A vector of unique EventBus instances
pub fn collect_event_sources(agent: &Arc<QueryMTAgent>) -> Vec<Arc<EventBus>> {
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let primary = agent.event_bus();
    if seen.insert(Arc::as_ptr(&primary) as usize) {
        sources.push(primary);
    }

    let registry = agent.agent_registry();
    for info in registry.list_agents() {
        if let Some(instance) = registry.get_agent_instance(&info.id)
            && let Some(bus) = instance
                .as_any()
                .downcast_ref::<QueryMTAgent>()
                .map(|agent| agent.event_bus())
            && seen.insert(Arc::as_ptr(&bus) as usize)
        {
            sources.push(bus);
        }
    }

    sources
}

/// Handle an RPC request and return a response.
///
/// This function routes JSON-RPC methods to the appropriate `SendAgent` trait methods.
pub async fn handle_rpc_message<S: SendAgent>(
    agent: &S,
    session_owners: &SessionOwnerMap,
    pending_permissions: &PermissionMap,
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
        m if m == AGENT_METHOD_NAMES.session_fork => {
            log::warn!("session/fork not yet implemented");
            Err(Error::new(-32601, "session/fork not implemented yet"))
        }
        m if m == AGENT_METHOD_NAMES.session_list => {
            log::warn!("session/list not yet implemented");
            Err(Error::new(-32601, "session/list not implemented yet"))
        }
        m if m == AGENT_METHOD_NAMES.session_load => {
            log::warn!("session/load not yet implemented");
            Err(Error::new(-32601, "session/load not implemented yet"))
        }
        m if m == AGENT_METHOD_NAMES.session_resume => {
            log::warn!("session/resume not yet implemented");
            Err(Error::new(-32601, "session/resume not implemented yet"))
        }
        m if m == AGENT_METHOD_NAMES.session_set_config_option => {
            log::warn!("session/set_config_option not implemented yet");
            Err(Error::new(
                -32601,
                "session/set_config_option not implemented yet",
            ))
        }
        m if m == AGENT_METHOD_NAMES.session_set_mode => {
            log::warn!("session/set_mode not yet implemented");
            Err(Error::new(-32601, "session/set_mode not implemented yet"))
        }
        m if m == AGENT_METHOD_NAMES.session_set_model => {
            log::warn!("session/set_model not yet implemented");
            Err(Error::new(-32601, "session/set_model not implemented yet"))
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
                        Err(Error::new(
                            -32000,
                            "No pending permission for this tool_call_id",
                        ))
                    }
                }
                Err(e) => {
                    Err(Error::invalid_params().data(serde_json::json!({"error": e.to_string()})))
                }
            }
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
