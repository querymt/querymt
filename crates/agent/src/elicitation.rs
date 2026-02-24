//! MCP elicitation handler and types
//!
//! This module provides the unified elicitation system that handles both:
//! - MCP server elicitation requests (via ClientHandler trait)
//! - Built-in QuestionTool requests (converted to MCP format)
//!
//! All elicitation requests flow through the same event system and pending map.

use rmcp::RoleClient;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientInfo, CreateElicitationRequestParam, CreateElicitationResult};
use rmcp::service::RequestContext;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

/// Action taken in response to an elicitation request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl From<ElicitationAction> for rmcp::model::ElicitationAction {
    fn from(action: ElicitationAction) -> Self {
        match action {
            ElicitationAction::Accept => rmcp::model::ElicitationAction::Accept,
            ElicitationAction::Decline => rmcp::model::ElicitationAction::Decline,
            ElicitationAction::Cancel => rmcp::model::ElicitationAction::Cancel,
        }
    }
}

impl From<rmcp::model::ElicitationAction> for ElicitationAction {
    fn from(action: rmcp::model::ElicitationAction) -> Self {
        match action {
            rmcp::model::ElicitationAction::Accept => ElicitationAction::Accept,
            rmcp::model::ElicitationAction::Decline => ElicitationAction::Decline,
            rmcp::model::ElicitationAction::Cancel => ElicitationAction::Cancel,
        }
    }
}

/// Response to an elicitation request from the UI/ACP client
#[derive(Debug, Clone)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

/// Type alias for the pending elicitation map (elicitation_id -> response sender)
pub type PendingElicitationMap = Arc<Mutex<HashMap<String, oneshot::Sender<ElicitationResponse>>>>;

/// Removes and returns a pending elicitation sender by ID.
///
/// Searches the primary agent first, then all registered delegate agents.
/// This allows UI/ACP responders to resolve delegate-originated elicitations
/// while holding only a reference to the primary agent.
pub async fn take_pending_elicitation_sender(
    agent: &crate::agent::AgentHandle,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    if let Some(sender) = take_from_pending_map(&agent.pending_elicitations(), elicitation_id).await
    {
        return Some(sender);
    }

    let registry = agent.agent_registry();
    let mut seen_agents = HashSet::new();

    for info in registry.list_agents() {
        let Some(instance) = registry.get_agent_instance(&info.id) else {
            continue;
        };

        let Some(delegate) = instance
            .as_any()
            .downcast_ref::<crate::agent::AgentHandle>()
        else {
            continue;
        };

        let ptr = delegate as *const _ as usize;
        if !seen_agents.insert(ptr) {
            continue;
        }

        if let Some(sender) =
            take_from_pending_map(&delegate.pending_elicitations(), elicitation_id).await
        {
            return Some(sender);
        }
    }

    None
}

async fn take_from_pending_map(
    pending_map: &PendingElicitationMap,
    elicitation_id: &str,
) -> Option<oneshot::Sender<ElicitationResponse>> {
    let mut pending = pending_map.lock().await;
    pending.remove(elicitation_id)
}

/// MCP client handler that routes elicitation requests through the agent's event system.
/// This replaces `()` as the handler in `serve_client()`.
pub struct ElicitationHandler {
    pending: PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    server_name: String,
    session_id: String,
}

impl ElicitationHandler {
    pub fn new(
        pending: PendingElicitationMap,
        event_sink: Arc<crate::event_sink::EventSink>,
        server_name: String,
        session_id: String,
    ) -> Self {
        Self {
            pending,
            event_sink,
            server_name,
            session_id,
        }
    }
}

impl ClientHandler for ElicitationHandler {
    #[allow(clippy::manual_async_fn)]
    fn create_elicitation(
        &self,
        request: CreateElicitationRequestParam,
        _context: RequestContext<RoleClient>,
    ) -> impl std::future::Future<Output = Result<CreateElicitationResult, rmcp::ErrorData>> + Send + '_
    {
        async move {
            let elicitation_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = oneshot::channel();

            // Store the response channel
            {
                let mut pending = self.pending.lock().await;
                pending.insert(elicitation_id.clone(), tx);
            }

            // Convert ElicitationSchema to a JSON value for the event
            let schema_json =
                serde_json::to_value(&request.requested_schema).unwrap_or(serde_json::Value::Null);

            // Durable: elicitation must be visible in UI replay.
            if let Err(err) = self
                .event_sink
                .emit_durable(
                    &self.session_id,
                    crate::events::AgentEventKind::ElicitationRequested {
                        elicitation_id: elicitation_id.clone(),
                        session_id: self.session_id.clone(),
                        message: request.message,
                        requested_schema: schema_json,
                        source: format!("mcp:{}", self.server_name),
                    },
                )
                .await
            {
                log::warn!("failed to emit ElicitationRequested: {}", err);
            }

            // Wait for response from UI/ACP
            match rx.await {
                Ok(response) => Ok(CreateElicitationResult {
                    action: response.action.into(),
                    content: response.content,
                }),
                Err(_) => Ok(CreateElicitationResult {
                    action: rmcp::model::ElicitationAction::Cancel,
                    content: None,
                }),
            }
        }
    }

    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::DelegateTestFixture;

    // ── ElicitationAction -> rmcp::model::ElicitationAction ───────────────

    #[test]
    fn elicitation_action_accept_converts_to_rmcp_accept() {
        let action = ElicitationAction::Accept;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Accept);
    }

    #[test]
    fn elicitation_action_decline_converts_to_rmcp_decline() {
        let action = ElicitationAction::Decline;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Decline);
    }

    #[test]
    fn elicitation_action_cancel_converts_to_rmcp_cancel() {
        let action = ElicitationAction::Cancel;
        let rmcp_action: rmcp::model::ElicitationAction = action.into();
        assert_eq!(rmcp_action, rmcp::model::ElicitationAction::Cancel);
    }

    // ── rmcp::model::ElicitationAction -> ElicitationAction ───────────────

    #[test]
    fn rmcp_accept_converts_to_elicitation_action_accept() {
        let rmcp_action = rmcp::model::ElicitationAction::Accept;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Accept);
    }

    #[test]
    fn rmcp_decline_converts_to_elicitation_action_decline() {
        let rmcp_action = rmcp::model::ElicitationAction::Decline;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Decline);
    }

    #[test]
    fn rmcp_cancel_converts_to_elicitation_action_cancel() {
        let rmcp_action = rmcp::model::ElicitationAction::Cancel;
        let action: ElicitationAction = rmcp_action.into();
        assert_eq!(action, ElicitationAction::Cancel);
    }

    // ── ElicitationAction serde round-trip ─────────────────────────────────

    #[test]
    fn elicitation_action_accept_serializes_as_lowercase() {
        let action = ElicitationAction::Accept;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""accept""#);
    }

    #[test]
    fn elicitation_action_decline_serializes_as_lowercase() {
        let action = ElicitationAction::Decline;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""decline""#);
    }

    #[test]
    fn elicitation_action_cancel_serializes_as_lowercase() {
        let action = ElicitationAction::Cancel;
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#""cancel""#);
    }

    #[test]
    fn elicitation_action_deserializes_from_lowercase() {
        let json = r#""accept""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Accept);

        let json = r#""decline""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Decline);

        let json = r#""cancel""#;
        let action: ElicitationAction = serde_json::from_str(json).unwrap();
        assert_eq!(action, ElicitationAction::Cancel);
    }

    #[test]
    fn all_elicitation_actions_round_trip() {
        let actions = vec![
            ElicitationAction::Accept,
            ElicitationAction::Decline,
            ElicitationAction::Cancel,
        ];

        for original in actions {
            let json = serde_json::to_string(&original).unwrap();
            let restored: ElicitationAction = serde_json::from_str(&json).unwrap();
            assert_eq!(original, restored);
        }
    }

    // ── ElicitationResponse construction ───────────────────────────────────

    #[test]
    fn elicitation_response_with_content() {
        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({"answer": "yes"})),
        };
        assert_eq!(response.action, ElicitationAction::Accept);
        assert!(response.content.is_some());
        assert_eq!(response.content.unwrap()["answer"], "yes");
    }

    #[test]
    fn elicitation_response_without_content() {
        let response = ElicitationResponse {
            action: ElicitationAction::Cancel,
            content: None,
        };
        assert_eq!(response.action, ElicitationAction::Cancel);
        assert!(response.content.is_none());
    }

    // ── PendingElicitationMap insert/remove lifecycle ──────────────────────

    #[tokio::test]
    async fn pending_elicitation_map_insert_and_retrieve() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();

        {
            let mut pending = map.lock().await;
            pending.insert("elicit-1".to_string(), tx);
        }

        let has_entry = {
            let pending = map.lock().await;
            pending.contains_key("elicit-1")
        };

        assert!(has_entry);
    }

    #[tokio::test]
    async fn pending_elicitation_map_remove_on_response() {
        let map: PendingElicitationMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = map.lock().await;
            pending.insert("elicit-2".to_string(), tx);
        }

        // Simulate response
        let tx = {
            let mut pending = map.lock().await;
            pending.remove("elicit-2")
        };

        assert!(tx.is_some());

        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(serde_json::json!({"data": "test"})),
        };

        tx.unwrap().send(response).unwrap();

        let received = rx.await.unwrap();
        assert_eq!(received.action, ElicitationAction::Accept);
    }

    #[tokio::test]
    async fn take_sender_resolves_delegate_pending_elicitation() {
        let fixture = DelegateTestFixture::new().await.unwrap();

        let elicitation_id = "delegate-elicitation-1".to_string();
        let (tx, rx) = oneshot::channel();
        fixture
            .delegate
            .pending_elicitations()
            .lock()
            .await
            .insert(elicitation_id.clone(), tx);

        let sender = take_pending_elicitation_sender(fixture.planner.as_ref(), &elicitation_id)
            .await
            .expect("delegate pending elicitation should be resolved");

        sender
            .send(ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(serde_json::json!({"selection": "allow_once"})),
            })
            .unwrap();

        let response = rx.await.unwrap();
        assert_eq!(response.action, ElicitationAction::Accept);
        assert_eq!(
            response.content,
            Some(serde_json::json!({"selection": "allow_once"}))
        );
    }
}
