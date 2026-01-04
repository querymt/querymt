//! Client bridge for Send/!Send boundary crossing.
//!
//! This module provides types that allow a `Send + Sync` agent (like `QueryMTAgent`)
//! to communicate with a `!Send` client connection (like `AgentSideConnection` from the SDK).
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        LocalSet                              │
//! │  ┌──────────────────┐      ┌──────────────────────────────┐ │
//! │  │AgentSideConnection│◄─────│    Bridge Task               │ │
//! │  │  (!Send)         │      │  - Receives from mpsc        │ │
//! │  │                  │      │  - Calls connection methods  │ │
//! │  └──────────────────┘      └──────────────▲───────────────┘ │
//! │                                            │                 │
//! │                                            │                 │
//! └────────────────────────────────────────────┼─────────────────┘
//!                                              │
//!                                              │ ClientBridgeMessage
//!                                              │ (Send types only)
//!                                              │
//!                          ┌───────────────────┴────────────────┐
//!                          │  QueryMTAgent (Send + Sync)        │
//!                          │  - Holds ClientBridgeSender        │
//!                          │  - Sends messages via mpsc channel │
//!                          └────────────────────────────────────┘
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

use agent_client_protocol::{
    Error, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
};
use tokio::sync::{mpsc, oneshot};

/// Messages sent from agent (Send context) to bridge task (LocalSet context).
///
/// These messages cross the `Send`/`!Send` boundary via an mpsc channel.
/// The bridge task running in a `LocalSet` receives these messages and
/// forwards them to the `!Send` client connection.
pub enum ClientBridgeMessage {
    /// Fire-and-forget session notification.
    ///
    /// The bridge task will call `connection.session_notification(...)`.
    /// No response is expected.
    Notification(SessionNotification),

    /// Request-response permission request.
    ///
    /// The bridge task will call `connection.request_permission(...)` and
    /// send the response back through the oneshot channel.
    RequestPermission {
        request: RequestPermissionRequest,
        response_tx: oneshot::Sender<Result<RequestPermissionResponse, Error>>,
    },
}

/// Send-side handle for the client bridge.
///
/// This type is `Send + Sync` and can be cloned and used from multi-threaded contexts.
/// It allows a `Send + Sync` agent to communicate with a `!Send` client connection
/// by sending messages through an mpsc channel to a bridge task running in `LocalSet`.
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
            .map_err(|_| Error::new(-32000, "Client bridge closed"))
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
            .map_err(|_| Error::new(-32000, "Client bridge closed"))?;

        response_rx
            .await
            .map_err(|_| Error::new(-32000, "Permission response channel dropped"))?
    }
}
