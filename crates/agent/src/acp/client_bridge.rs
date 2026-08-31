//! Client bridge for Send/!Send boundary crossing.
//!
//! This module provides types that allow a `Send + Sync` agent (like `AgentHandle`)
//! to communicate with the active ACP client connection.
//!
//! ## Architecture
//!
//! ```text
//! ┌───────────────────┐      ┌──────────────────────────────┐
//! │  ACP Connection   │◄─────│         Bridge Task          │
//! │                   │      │  - Receives from mpsc        │
//! │                   │      │  - Calls connection methods  │
//! └───────────────────┘      └──────────────▲───────────────┘
//!                                           │ ClientBridgeMessage
//!                         ┌─────────────────┴──────────────────┐
//!                         │  QueryMTAgent (Send + Sync)        │
//!                         │  - Holds ClientBridgeSender        │
//!                         └────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! The bridge is set up automatically by the ACP stdio server (`serve_stdio`).
//! The agent uses `ClientBridgeSender` to communicate with the client:
//!
//! ```rust,ignore
//! // In QueryMTAgent methods
//! if let Some(bridge) = self.bridge() {
//!     // Send a notification (fire-and-forget)
//!     bridge.notify(notification).await?;
//!     
//!     // Request permission (wait for response)
//!     let response = bridge.request_permission(request).await?;
//! }
//! ```

use crate::acp::protocol::{
    Error, ExtNotification, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification,
};
use tokio::sync::{mpsc, oneshot};

/// Messages sent from agent tasks to the active ACP bridge task.
///
/// The mpsc channel keeps agent execution decoupled from the ACP connection's
/// request and notification lifecycle.
#[derive(Debug)]
pub enum ClientBridgeMessage {
    /// Fire-and-forget session notification.
    ///
    /// The bridge task will call `connection.session_notification(...)`.
    /// No response is expected.
    Notification(SessionNotification),

    /// Barrier acknowledged after every preceding bridge message has been forwarded.
    Flush {
        response_tx: oneshot::Sender<Result<(), String>>,
    },

    /// Fire-and-forget ACP extension notification payload.
    ExtNotification(ExtNotification),

    /// Request-response permission request.
    ///
    /// The bridge task will call `connection.request_permission(...)` and
    /// send the response back through the oneshot channel.
    RequestPermission {
        request: RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<RequestPermissionResponse, Error>>,
    },

    /// Request-response elicitation request.
    ///
    /// The bridge task will handle elicitation and return the response.
    Elicit {
        elicitation_id: String,
        session_id: String,
        message: String,
        requested_schema: serde_json::Value,
        source: String,
        response_tx: oneshot::Sender<Result<crate::elicitation::ElicitationResponse, Error>>,
    },

    /// Workspace query request (agent → client → VS Code LSP).
    ///
    /// The bridge task sends this as an ACP SDK `AgentRequest::ExtMethodRequest`
    /// using the wire-form method `"_workspace/query"`.
    /// The client (VS Code extension) handles it by calling VS Code language APIs
    /// and returns the result.
    WorkspaceQuery {
        query: crate::workspace_query::WorkspaceQueryRequest,
        response_tx: oneshot::Sender<Result<crate::workspace_query::WorkspaceQueryResponse, Error>>,
    },
}

#[derive(Default)]
pub(crate) struct NotificationForwardingState {
    pending_error: Option<String>,
}

impl NotificationForwardingState {
    pub(crate) fn record_result<E: std::fmt::Debug>(&mut self, result: Result<(), E>) {
        if let Err(error) = result
            && self.pending_error.is_none()
        {
            self.pending_error = Some(format!("session notification forwarding failed: {error:?}"));
        }
    }

    pub(crate) fn take_flush_result(&mut self) -> Result<(), String> {
        match self.pending_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Send-side handle for the client bridge.
///
/// This type is `Send + Sync` and can be cloned and used from multi-threaded contexts.
/// It allows agent tasks to communicate with the ACP client by sending messages
/// through an mpsc channel to the connection's bridge task.
///
/// ## Examples
///
/// ```rust,ignore
/// // Create bridge channel
/// let (tx, rx) = mpsc::channel::<ClientBridgeMessage>(100);
/// let sender = ClientBridgeSender::new(tx);
///
/// // Set on agent
/// agent.set_bridge(sender);
///
/// // In agent methods:
/// agent.bridge().unwrap().notify(notification).await?;
/// ```
#[derive(Clone)]
pub struct ClientBridgeSender {
    tx: mpsc::Sender<ClientBridgeMessage>,
}

impl ClientBridgeSender {
    /// Create a new bridge sender wrapping the channel.
    ///
    /// This is typically called by the ACP server when setting up the bridge.
    pub fn new(tx: mpsc::Sender<ClientBridgeMessage>) -> Self {
        Self { tx }
    }

    /// Send a session notification (fire-and-forget).
    ///
    /// The notification is queued and sent asynchronously to the client.
    /// This method does not wait for the client to receive or process the notification.
    ///
    /// # Errors
    ///
    /// Returns an error if the bridge channel is closed (client disconnected).
    pub async fn notify(&self, notification: SessionNotification) -> Result<(), Error> {
        self.tx
            .send(ClientBridgeMessage::Notification(notification))
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))
    }

    pub async fn flush(&self) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ClientBridgeMessage::Flush { response_tx })
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))?;
        response_rx
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))?
            .map_err(|message| Error::internal_error().data(message))
    }

    pub async fn notify_ext(&self, notification: ExtNotification) -> Result<(), Error> {
        self.tx
            .send(ClientBridgeMessage::ExtNotification(notification))
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))
    }

    /// Request permission from the client and wait for response.
    ///
    /// This method blocks until the client responds to the permission request.
    /// The response flows back through a oneshot channel embedded in the message.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bridge channel is closed
    /// - The client disconnects before responding
    /// - The client rejects the permission request
    pub async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ClientBridgeMessage::RequestPermission {
                request,
                response_tx,
            })
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))?;

        response_rx
            .await
            .map_err(|_| Error::from(crate::error::AgentError::PermissionChannelDropped))?
    }

    /// Send a workspace query to the client (VS Code) and wait for response.
    ///
    /// This enables the agent to access language intelligence (LSP data) from
    /// the editor: diagnostics, references, definitions, symbols, hover info.
    ///
    /// Only available when connected to a client that supports workspace queries
    /// (currently the VS Code extension). In CLI mode, this is not available.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bridge channel is closed
    /// - The client disconnects before responding
    /// - The client does not support workspace queries
    pub async fn workspace_query(
        &self,
        query: crate::workspace_query::WorkspaceQueryRequest,
    ) -> Result<crate::workspace_query::WorkspaceQueryResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ClientBridgeMessage::WorkspaceQuery { query, response_tx })
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))?;

        response_rx
            .await
            .map_err(|_| Error::from(crate::error::AgentError::WorkspaceQueryChannelDropped))?
    }

    /// Request elicitation from the user and wait for response.
    ///
    /// This method blocks until the user responds to the elicitation request.
    /// The response flows back through a oneshot channel embedded in the message.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The bridge channel is closed
    /// - The client disconnects before responding
    pub async fn elicit(
        &self,
        elicitation_id: String,
        session_id: String,
        message: String,
        requested_schema: serde_json::Value,
        source: String,
    ) -> Result<crate::elicitation::ElicitationResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ClientBridgeMessage::Elicit {
                elicitation_id,
                session_id,
                message,
                requested_schema,
                source,
                response_tx,
            })
            .await
            .map_err(|_| Error::from(crate::error::AgentError::ClientBridgeClosed))?;

        response_rx
            .await
            .map_err(|_| Error::internal_error().data("Elicitation response channel dropped"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::{ContentBlock, ContentChunk, SessionId, SessionUpdate, TextContent};

    #[tokio::test]
    async fn flush_is_ordered_after_preceding_notifications() {
        let (tx, mut rx) = mpsc::channel(4);
        let sender = ClientBridgeSender::new(tx);
        let notification = SessionNotification::new(
            SessionId::from("session-1"),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("history"),
            ))),
        );

        sender
            .notify(notification)
            .await
            .expect("enqueue notification");
        let flush = tokio::spawn({
            let sender = sender.clone();
            async move { sender.flush().await }
        });

        assert!(matches!(
            rx.recv().await,
            Some(ClientBridgeMessage::Notification(_))
        ));
        let Some(ClientBridgeMessage::Flush { response_tx }) = rx.recv().await else {
            panic!("flush must follow the notification");
        };
        assert!(!flush.is_finished());
        response_tx.send(Ok(())).expect("acknowledge flush");
        flush.await.expect("flush task").expect("flush succeeds");
    }

    #[tokio::test]
    async fn forwarding_failure_is_returned_by_next_flush() {
        let (tx, mut rx) = mpsc::channel(2);
        let sender = ClientBridgeSender::new(tx);
        let flush = tokio::spawn({
            let sender = sender.clone();
            async move { sender.flush().await }
        });
        let Some(ClientBridgeMessage::Flush { response_tx }) = rx.recv().await else {
            panic!("expected flush message");
        };
        let mut forwarding = NotificationForwardingState::default();
        forwarding.record_result::<&str>(Err("connection closed"));
        response_tx
            .send(forwarding.take_flush_result())
            .expect("acknowledge failed flush");

        let error = flush
            .await
            .expect("flush task")
            .expect_err("flush must report forwarding failure");
        assert!(error.to_string().contains("connection closed"));
        assert!(forwarding.take_flush_result().is_ok());
    }

    #[test]
    fn fully_forwarded_notifications_keep_flush_successful() {
        let mut forwarding = NotificationForwardingState::default();
        forwarding.record_result::<&str>(Ok(()));
        forwarding.record_result::<&str>(Ok(()));
        assert!(forwarding.take_flush_result().is_ok());
    }
}
