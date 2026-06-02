//! Mesh bootstrap and discovery for the kameo remote actor network.
//!
//! This module provides `MeshConfig` and `bootstrap_mesh()` which initialise
//! the libp2p swarm so that `SessionActor`s and `RemoteNodeManager`s become
//! addressable across the network.
//!
//! ## Quick start (local dev)
//!
//! ```no_run
//! use querymt_agent::agent::remote::mesh::{MeshConfig, MeshDiscovery, DirectoryMode, bootstrap_mesh};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = MeshConfig {
//!     listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
//!     discovery: MeshDiscovery::Mdns,
//!     bootstrap_peers: vec![],
//!     directory: DirectoryMode::default(),
//!     request_timeout: std::time::Duration::from_secs(300),
//!     stream_reconnect_grace: std::time::Duration::from_secs(120),
//!     ..Default::default()
//! };
//! let mesh = bootstrap_mesh(&config).await?;
//! println!("Mesh peer ID: {}", mesh.peer_id());
//! # Ok(())
//! # }
//! ```
//!
//! ## Production (cross-subnet)
//!
//! Use `MeshDiscovery::None` with explicit `bootstrap_peers` addresses for
//! deployments where mDNS multicast is not available.

mod bootstrap;
mod config;
mod events;
mod handle;
mod join;
mod routes;
mod runtime;

#[cfg(test)]
mod tests;

pub use config::{DirectoryMode, MeshConfig, MeshDiscovery, MeshError, MeshTransportMode};
pub use handle::MeshHandle;
pub use routes::{MeshRoute, RouteTable};

use bootstrap::{bootstrap_composite_mesh, bootstrap_iroh_mesh, bootstrap_lan_mesh};
use libp2p::PeerId;

use super::scope::{MeshScopeId, MeshTransportKind};

/// Commands sent from `MeshHandle` to the swarm event loop.
///
/// The event loop owns the `Swarm` and is the only place that can mutate it.
/// Higher-level code uses `MeshHandle` methods (e.g. `dial_peer`) which
/// translate intent into a `SwarmCommand` and send it over an `mpsc` channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialReason {
    Admission,
    Reconnect,
    ExistingMeshPeer,
    Manual,
}

#[derive(Debug)]
enum SwarmCommand {
    /// Request the swarm to dial a peer by `PeerId`.
    ///
    /// The event loop converts this to a `/p2p/{peer_id}` multiaddr and calls
    /// `swarm.dial()`. Scoped Iroh dials seed reconnect metadata before dialing
    /// so admission-time peers are not dropped as unknown LAN-only peers.
    DialPeer {
        peer_id: PeerId,
        scope: Option<MeshScopeId>,
        reason: DialReason,
    },
    /// Join or refresh an iroh scope on the existing runtime.
    JoinIrohScope { mesh_id: String, peers: Vec<PeerId> },
    /// Drop scope-specific reconnect targets for a left Iroh scope.
    LeaveIrohScope { mesh_id: String },
}

/// A peer lifecycle event emitted by the swarm event loop.
///
/// Subscribe via [`MeshHandle::subscribe_peer_events`] to receive these.
/// Each WebSocket connection spawns a watcher that reacts to these events
/// and pushes an updated `remote_nodes` list to the client.
#[derive(Debug, Clone)]
pub enum MeshEvent {
    /// A new peer was discovered via mDNS (or added via bootstrap_peers).
    Discovered(PeerId),
    /// A previously discovered peer's mDNS record expired (peer went away).
    Expired(PeerId),
    /// A route was learned or refreshed for this peer.
    RouteAdded { peer_id: PeerId, route: MeshRoute },
    /// A route was removed or expired for this peer.
    RouteRemoved { peer_id: PeerId, route: MeshRoute },
    /// Scope membership changed.
    ScopeJoined(MeshScopeId),
    /// Scope membership changed.
    ScopeLeft(MeshScopeId),
}

pub type PeerEvent = MeshEvent;

/// Bootstrap the kameo mesh swarm.
///
/// Starts the libp2p networking stack according to `config`. After this call
/// `ActorSwarm::get()` returns `Some(...)` and actors can be registered /
/// looked up across the network.
///
/// Dispatches to the LAN (TCP+QUIC+mDNS) or iroh transport based on
/// `config.transport`.
///
/// # One-shot
///
/// Call at most once per process. kameo panics on a second initialisation.
///
/// # Returns
///
/// A [`MeshHandle`] — proof the swarm is up, a capability object for all
/// DHT operations, and a source of [`PeerEvent`] broadcasts.
pub async fn bootstrap_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    match config.transport {
        MeshTransportMode::Lan => bootstrap_lan_mesh(config).await,
        MeshTransportMode::Iroh => bootstrap_iroh_mesh(config).await,
        MeshTransportMode::Composite => bootstrap_composite_mesh(config).await,
    }
}

// ── LAN mesh (TCP + QUIC + mDNS) ──────────────────────────────────────────────

// ── Phase 4: Unified Composite Bootstrap ─────────────────────────────────────
//
// `bootstrap_mesh_runtime` is the new public entry point.  It accepts the
// normalized `MeshRuntimeConfig` (produced by Phase 3's config normalization)
// and builds one swarm with whichever transports are enabled — LAN only,
// Iroh only, or both simultaneously.

/// Bootstrap the process-wide mesh runtime from a normalized runtime config.
///
/// This is the preferred entry point for new code.  It:
/// 1. Loads or generates a persistent ed25519 identity.
/// 2. Builds a single libp2p swarm with whichever transports are enabled.
/// 3. Calls `try_init_global()` exactly once.
/// 4. Starts one unified event loop.
/// 5. Returns a [`MeshRuntimeHandle`] for actor registration and lookup.
///
/// # Errors
///
/// Returns [`MeshError`] if the swarm fails to initialise, the identity
/// keypair cannot be loaded, or (for Iroh) the iroh transport fails to start.
pub async fn bootstrap_mesh_runtime(
    config: &super::mesh_runtime_config::MeshRuntimeConfig,
) -> Result<super::runtime_handle::MeshRuntimeHandle, MeshError> {
    runtime::bootstrap_mesh_runtime(config).await
}

/// Bootstrap the mesh with default settings (mDNS, port 9000).
///
/// Convenience wrapper around `bootstrap_mesh(&MeshConfig::default())`.
pub async fn bootstrap_mesh_default() -> Result<MeshHandle, MeshError> {
    bootstrap_mesh(&MeshConfig::default()).await
}

/// Join an existing mesh using a signed invite grant.
///
/// Steps:
/// 1. Verify the invite grant signature and check expiry.
/// 2. Check `~/.qmt/memberships.json` for an existing token for this mesh —
///    if found, use `AdmissionRequest::Token` (reconnect path); otherwise use
///    `AdmissionRequest::Invite` (first join, consumes one invite use).
/// 3. Bootstrap the iroh swarm and dial the inviter (or cached peers if the
///    inviter is offline).
/// 4. Send the admission request to the target peer's `RemoteNodeManager`.
/// 5. On `Admitted` — persist the returned `MembershipToken` and known peers.
/// 6. On `Rejected` — disconnect and return an error.
///
/// After this call the local node is a full mesh member and can discover other
/// members via Kademlia / iroh relay.
///
/// # Arguments
/// - `invite` — a decoded `SignedInviteGrant` (signature verified offline)
/// - `identity_file` — optional path to the persistent ed25519 identity file
pub async fn join_mesh_via_invite(
    invite: &super::invite::SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
) -> Result<MeshHandle, MeshError> {
    join::join_mesh_via_invite(invite, identity_file).await
}

/// Resolve the local hostname (same logic as `RemoteNodeManager::get_hostname`
/// but available outside the `remote_impl` module).
fn resolve_local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
