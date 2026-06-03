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
