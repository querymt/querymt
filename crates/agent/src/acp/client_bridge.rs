//! Transport-owned outbound queue for one ACP connection.
//!
//! A `ClientBridgeSender` is the send side of a per-connection mpsc queue that
//! lets `Send + Sync` code hand messages to the task that owns the (possibly
//! `!Send`) ACP connection. It belongs to the transport, not to sessions.
//!
//! ## Role
//!
//! Sessions hold **no** bridge. Prompts carry no bridge, session actors store
//! none, and remote sessions are reached through `SessionActorRef` only. User
//! interactions (tool permission, form elicitation) are session-owned pending
//! interactions announced by durable events and answered through
//! `SessionActorRef` messages; see `crate::elicitation`. Session code never
//! produces `RequestPermission`, `Elicit` or `WorkspaceQuery` messages.
//!
//! What flows through the queue:
//!
//! - `Notification` / `Flush`: ordered `session/update` delivery written by
//!   the connection's event forwarders and replay.
//! - `ExtNotification`: agent-wide extension notifications (schedules,
//!   control, input state) via [`ClientBridgeSender::notify_ext`].
//! - `RequestPermission` / `Elicit`: outbound ACP requests created by the
//!   transport's own interaction forwarder when it delivers a pending
//!   interaction to this connection. The answer is resolved through the
//!   owning session, never by the caller of these methods.
//! - `WorkspaceQuery`: editor language queries, available only to locally
//!   hosted sessions whose attached connection supports them. Sessions on a
//!   remote node have no route and report the capability as unavailable.
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
//!                         │  Transport (forwarders, replay)    │
//!                         │  - Holds ClientBridgeSender        │
//!                         └────────────────────────────────────┘
//! ```

use crate::acp::protocol::{
    Error, ExtNotification, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Messages sent from transport tasks to the active ACP bridge task.
///
/// The mpsc channel keeps the transport's producers decoupled from the ACP
/// connection's request and notification lifecycle. Session code does not
/// produce `RequestPermission`, `Elicit` or `WorkspaceQuery`.
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

    /// Request-response permission request emitted by a transport's
    /// interaction forwarder to deliver a pending permission interaction.
    ///
    /// The bridge task will call `connection.request_permission(...)` and
    /// send the response back through the oneshot channel.
    RequestPermission {
        request: RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<RequestPermissionResponse, Error>>,
    },

    /// Request-response elicitation request emitted by a transport's
    /// interaction forwarder to deliver a pending elicitation.
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

    /// Workspace query request (agent → client → VS Code LSP) for a locally
    /// hosted session. Remote-hosted sessions never reach this message.
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

/// Per-connection state shared by every clone of a connection's sender.
pub(crate) struct ClientBridgeConnectionState {
    active: AtomicBool,
    attachment_lock: Mutex<()>,
}

impl ClientBridgeConnectionState {
    pub(crate) async fn lock_attachment(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.attachment_lock.lock().await
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Send-side handle for a connection's outbound queue.
///
/// This type is `Send + Sync` and cheap to clone. It is owned by the transport
/// and by agent-wide notifiers (`set_bridge` on the handle, used for
/// `notify_ext`); sessions never hold one.
///
/// ```rust,ignore
/// let (tx, rx) = mpsc::channel::<ClientBridgeMessage>(100);
/// let sender = ClientBridgeSender::new(tx);
/// sender.notify(notification).await?;
/// ```
#[derive(Clone)]
pub struct ClientBridgeSender {
    tx: mpsc::Sender<ClientBridgeMessage>,
    connection_id: Option<Arc<str>>,
    connection_state: Option<Arc<ClientBridgeConnectionState>>,
}

impl ClientBridgeSender {
    /// Create a new bridge sender wrapping the channel.
    ///
    /// This is typically called by the ACP server when setting up the bridge.
    pub fn new(tx: mpsc::Sender<ClientBridgeMessage>) -> Self {
        Self {
            tx,
            connection_id: None,
            connection_state: None,
        }
    }

    pub(crate) fn for_connection(
        tx: mpsc::Sender<ClientBridgeMessage>,
        connection_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            tx,
            connection_id: Some(connection_id.into()),
            connection_state: Some(Arc::new(ClientBridgeConnectionState {
                active: AtomicBool::new(true),
                attachment_lock: Mutex::new(()),
            })),
        }
    }

    pub(crate) fn connection_id(&self) -> Option<&str> {
        self.connection_id.as_deref()
    }

    pub(crate) fn connection_state(&self) -> Option<Arc<ClientBridgeConnectionState>> {
        self.connection_state.clone()
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
