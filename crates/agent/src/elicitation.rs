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
use std::collections::HashMap;
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

/// MCP client handler that routes elicitation requests through the agent's event system.
/// This replaces `()` as the handler in `serve_client()`.
pub struct ElicitationHandler {
    pending: PendingElicitationMap,
    event_bus: Arc<crate::event_bus::EventBus>,
    server_name: String,
    session_id: String,
}

impl ElicitationHandler {
    pub fn new(
        pending: PendingElicitationMap,
        event_bus: Arc<crate::event_bus::EventBus>,
        server_name: String,
        session_id: String,
    ) -> Self {
        Self {
            pending,
            event_bus,
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

            // Emit event for UI/ACP clients
            self.event_bus.publish(
                &self.session_id,
                crate::events::AgentEventKind::ElicitationRequested {
                    elicitation_id: elicitation_id.clone(),
                    session_id: self.session_id.clone(),
                    message: request.message,
                    requested_schema: schema_json,
                    source: format!("mcp:{}", self.server_name),
                },
            );

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
