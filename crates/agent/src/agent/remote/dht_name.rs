//! Canonical DHT name constructors for the kameo mesh.
//!
//! Every actor registered in the distributed hash table uses a well-known naming
//! convention.  This module centralises those conventions so that **registration
//! and lookup always agree** — eliminating the class of bugs where a producer
//! registers under one string and a consumer looks up a different one.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use querymt_agent::agent::remote::dht_name;
//!
//! // Registration side
//! let name = dht_name::provider_host(mesh.peer_id());
//! mesh.register_actor(actor_ref, name).await;
//!
//! // Lookup side  (uses the same function → guaranteed match)
//! let name = dht_name::provider_host(target_peer_id);
//! mesh.lookup_actor::<ProviderHostActor>(&name).await;
//! ```
//!
//! ## Naming conventions
//!
//! | Actor                  | DHT name                            |
//! |------------------------|-------------------------------------|
//! | `ProviderHostActor`    | `provider_host::peer::{peer_id}`    |
//! | `SessionActor`         | `session::{session_id}`             |
//! | `EventRelayActor`      | `event_relay::{session_id}`         |
//! | `StreamReceiverActor`  | `stream_rx::{request_id}`           |
//! | `RemoteNodeManager`    | `node_manager`                      |

use std::fmt;

/// The well-known DHT name for the `RemoteNodeManager` singleton.
///
/// All nodes register under this shared name so that `lookup_all_actors`
/// can discover every node in the mesh. Peers also register under the
/// per-peer name (see [`node_manager_for_peer`]) to enable fast direct
/// lookups by `find_node_manager`.
pub const NODE_MANAGER: &str = "node_manager";

/// Per-peer DHT name for a `RemoteNodeManager`, keyed by the owning node's
/// peer id.
///
/// Every node registers its `RemoteNodeManager` under **both** the global
/// [`NODE_MANAGER`] name (for mesh-wide discovery) and this per-peer name
/// (for direct O(1) lookup by [`AgentHandle::find_node_manager`]).
///
/// The direct-lookup path in `find_node_manager` bypasses the
/// `is_peer_alive` filter that guards the `lookup_all_actors` scan, so a
/// node that lost its mDNS heartbeat momentarily is still reachable as long
/// as the DHT record is fresh and the TCP connection is alive.
pub fn node_manager_for_peer(peer_id: &impl fmt::Display) -> String {
    format!("node_manager::peer::{}", peer_id)
}

/// DHT name for a `ProviderHostActor` keyed by the owning node's peer id.
///
/// The peer id is the stable libp2p identity of the node that owns the
/// provider credentials.  Consumers resolve this name to proxy LLM calls
/// to that node via `MeshChatProvider`.
pub fn provider_host(peer_id: &impl fmt::Display) -> String {
    format!("provider_host::peer::{}", peer_id)
}

/// DHT name for a remote `SessionActor`.
///
/// `RemoteNodeManager` registers every session it creates under this name
/// so that remote peers can send `Prompt`, `Cancel`, etc. messages to it.
pub fn session(session_id: &str) -> String {
    format!("session::{}", session_id)
}

/// DHT name for an `EventRelayActor` associated with a session.
///
/// The local node registers this actor so that the remote `SessionActor`
/// can look it up and install an `EventForwarder` that streams events back.
pub fn event_relay(session_id: &str) -> String {
    format!("event_relay::{}", session_id)
}

/// DHT name for an ephemeral `StreamReceiverActor`.
///
/// Registered by `MeshChatProvider` for the duration of a single streaming
/// LLM request.  The remote `ProviderHostActor` sends `StreamChunkRelay`
/// messages to this actor.
pub fn stream_receiver(request_id: &str) -> String {
    format!("stream_rx::{}", request_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_host_uses_peer_prefix() {
        let name = provider_host(&"12D3KooWABC");
        assert_eq!(name, "provider_host::peer::12D3KooWABC");
    }

    #[test]
    fn provider_host_never_uses_hostname() {
        // The bug that motivated this module: using hostname instead of peer id.
        let name = provider_host(&"12D3KooWABC");
        assert!(
            name.starts_with("provider_host::peer::"),
            "must use 'peer::' prefix, got: {}",
            name
        );
        assert!(
            !name.contains("hostname"),
            "must not contain hostname, got: {}",
            name
        );
    }

    #[test]
    fn session_format() {
        assert_eq!(session("abc-123"), "session::abc-123");
    }

    #[test]
    fn event_relay_format() {
        assert_eq!(event_relay("abc-123"), "event_relay::abc-123");
    }

    #[test]
    fn stream_receiver_format() {
        assert_eq!(stream_receiver("req-42"), "stream_rx::req-42");
    }

    #[test]
    fn node_manager_is_bare_string() {
        assert_eq!(NODE_MANAGER, "node_manager");
    }

    #[test]
    fn node_manager_for_peer_format() {
        let name = node_manager_for_peer(&"12D3KooWABC");
        assert_eq!(name, "node_manager::peer::12D3KooWABC");
    }

    #[test]
    fn node_manager_for_peer_never_collides_with_global() {
        let name = node_manager_for_peer(&"12D3KooWABC");
        assert_ne!(name, NODE_MANAGER);
        assert!(
            name.starts_with("node_manager::peer::"),
            "must use 'peer::' prefix, got: {}",
            name
        );
    }

    #[test]
    fn node_manager_for_peer_registration_and_lookup_agree() {
        let peer_id = "12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH";
        let reg_name = node_manager_for_peer(&peer_id);
        let lookup_name = node_manager_for_peer(&peer_id);
        assert_eq!(reg_name, lookup_name);
    }

    /// The DHT name functions used for registration and lookup must produce
    /// identical strings — this is the whole point of the module.
    #[test]
    fn registration_and_lookup_agree() {
        let peer_id = "12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH";

        // Simulate registration side
        let reg_name = provider_host(&peer_id);
        // Simulate lookup side (same function, same input)
        let lookup_name = provider_host(&peer_id);

        assert_eq!(reg_name, lookup_name);
    }
}
