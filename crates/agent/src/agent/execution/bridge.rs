//! Client bridge communication helper
//!
//! This module provides utilities for sending session updates to the client via the bridge.

use crate::acp::client_bridge::ClientBridgeSender;
use agent_client_protocol::SessionUpdate;

/// Send a session update notification to the client via the bridge.
///
/// If no bridge is available, this is a no-op. The notification is sent asynchronously
/// in a spawned task to avoid blocking the state machine.
pub(super) fn send_session_update(
    bridge: Option<&ClientBridgeSender>,
    session_id: &str,
    update: SessionUpdate,
) {
    if let Some(bridge) = bridge {
        let notification = agent_client_protocol::SessionNotification::new(
            agent_client_protocol::SessionId::from(session_id.to_string()),
            update,
        );
        let bridge = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge.notify(notification).await {
                log::debug!("Failed to send session update: {}", e);
            }
        });
    }
}
