//! Client bridge communication helper
//!
//! This module provides utilities for sending session updates to the client via the bridge.

use crate::acp::client_bridge::ClientBridgeSender;
use agent_client_protocol::SessionUpdate;
use tokio_util::sync::CancellationToken;

/// Send a session update notification to the client via the bridge.
///
/// If no bridge is available, this is a no-op. The send is awaited so bridge
/// channel backpressure is applied to prompt execution instead of creating
/// one spawned task per notification.
pub(super) async fn send_session_update(
    bridge: Option<&ClientBridgeSender>,
    session_id: &str,
    update: SessionUpdate,
    cancel_token: Option<&CancellationToken>,
) {
    if let Some(bridge) = bridge {
        let notification = agent_client_protocol::SessionNotification::new(
            agent_client_protocol::SessionId::from(session_id.to_string()),
            update,
        );

        if let Some(token) = cancel_token {
            if token.is_cancelled() {
                return;
            }

            tokio::select! {
                biased;
                _ = token.cancelled() => {}
                res = bridge.notify(notification) => {
                    if let Err(e) = res {
                        log::debug!("Failed to send session update: {}", e);
                    }
                }
            }
        } else if let Err(e) = bridge.notify(notification).await {
            log::debug!("Failed to send session update: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client_bridge::{ClientBridgeMessage, ClientBridgeSender};
    use agent_client_protocol::{ContentBlock, SessionUpdate, TextContent};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn send_session_update_enqueues_notification_without_cancellation() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = ClientBridgeSender::new(tx);

        send_session_update(
            Some(&sender),
            "sess-1",
            SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                ContentBlock::Text(TextContent::new("hi".to_string())),
            )),
            None,
        )
        .await;

        let msg = rx.try_recv().expect("notification queued");
        assert!(matches!(msg, ClientBridgeMessage::Notification(_)));
    }

    #[tokio::test]
    async fn send_session_update_skips_when_cancelled() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = ClientBridgeSender::new(tx);
        let token = CancellationToken::new();
        token.cancel();

        send_session_update(
            Some(&sender),
            "sess-1",
            SessionUpdate::AgentMessageChunk(agent_client_protocol::ContentChunk::new(
                ContentBlock::Text(TextContent::new("hi".to_string())),
            )),
            Some(&token),
        )
        .await;

        assert!(rx.try_recv().is_err(), "cancelled send should not enqueue");
    }
}
