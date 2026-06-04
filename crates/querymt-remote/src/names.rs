//! Canonical DHT name constructors for the remote mesh.

use std::fmt;

/// The well-known DHT name for the global node manager singleton.
pub const NODE_MANAGER: &str = "node_manager";

pub fn node_manager_for_peer(peer_id: &impl fmt::Display) -> String {
    format!("node_manager::peer::{}", peer_id)
}

pub fn provider_host(peer_id: &impl fmt::Display) -> String {
    format!("provider_host::peer::{}", peer_id)
}

pub fn session(session_id: &str) -> String {
    format!("session::{}", session_id)
}

pub fn event_relay(session_id: &str, peer_id: &impl fmt::Display) -> String {
    format!("event_relay::{}::{}", session_id, peer_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_builders_use_canonical_formats() {
        assert_eq!(NODE_MANAGER, "node_manager");
        assert_eq!(
            node_manager_for_peer(&"peer-1"),
            "node_manager::peer::peer-1"
        );
        assert_eq!(provider_host(&"peer-1"), "provider_host::peer::peer-1");
        assert_eq!(session("session-1"), "session::session-1");
        assert_eq!(
            event_relay("session-1", &"peer-1"),
            "event_relay::session-1::peer-1"
        );
    }
}
