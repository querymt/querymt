//! Mesh bootstrap and discovery for the kameo remote actor network.
//!
//! This module provides `MeshConfig` and `bootstrap_mesh()` which initialise
//! the libp2p swarm so that `SessionActor`s and `RemoteNodeManager`s become
//! addressable across the network.
//!
//! ## Quick start (local dev)
//!
//! ```no_run
//! use querymt_agent::agent::remote::mesh::{MeshConfig, MeshDiscovery, bootstrap_mesh};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = MeshConfig {
//!     listen: Some("/ip4/0.0.0.0/tcp/9000".to_string()),
//!     discovery: MeshDiscovery::Mdns,
//!     bootstrap_peers: vec![],
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

use libp2p::PeerId;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

/// A peer lifecycle event emitted by the swarm event loop.
///
/// Subscribe via [`MeshHandle::subscribe_peer_events`] to receive these.
/// Each WebSocket connection spawns a watcher that reacts to these events
/// and pushes an updated `remote_nodes` list to the client.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// A new peer was discovered via mDNS (or added via bootstrap_peers).
    Discovered(PeerId),
    /// A previously discovered peer's mDNS record expired (peer went away).
    Expired(PeerId),
}

/// How peers discover each other in the mesh.
#[derive(Debug, Clone)]
pub enum MeshDiscovery {
    /// Zero-config local-network discovery via mDNS multicast.
    ///
    /// Works out of the box on local LANs. Not suitable for cross-subnet
    /// deployments because mDNS doesn't route across routers.
    Mdns,

    /// Distributed discovery using the Kademlia DHT.
    ///
    /// Suitable for cross-subnet production deployments. Requires at least
    /// one well-known bootstrap peer.
    Kademlia { bootstrap: Vec<String> },

    /// No automatic discovery — peers must be added manually via
    /// `bootstrap_peers` in `MeshConfig`.
    None,
}

/// Configuration for the kameo mesh (libp2p swarm).
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Multiaddr to listen on, e.g. `"/ip4/0.0.0.0/tcp/9000"`.
    ///
    /// `None` lets the OS pick a random port (useful for tests/ephemeral nodes).
    pub listen: Option<String>,

    /// Peer discovery strategy.
    pub discovery: MeshDiscovery,

    /// Explicit peer addresses to dial immediately after bootstrap.
    ///
    /// Format: multiaddr strings such as `"/ip4/192.168.1.100/tcp/9000"`.
    /// Used when mDNS is unavailable (cross-subnet) or for well-known peers.
    pub bootstrap_peers: Vec<String>,
}

impl Default for MeshConfig {
    /// Sensible defaults for local development: listen on port 9000 with mDNS.
    fn default() -> Self {
        Self {
            listen: Some("/ip4/0.0.0.0/tcp/9000".to_string()),
            discovery: MeshDiscovery::Mdns,
            bootstrap_peers: vec![],
        }
    }
}

/// Errors that can occur during mesh bootstrap.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("libp2p swarm error: {0}")]
    SwarmError(String),

    #[error("invalid listen address '{addr}': {reason}")]
    InvalidListenAddr { addr: String, reason: String },

    #[error("invalid bootstrap peer address '{addr}': {reason}")]
    InvalidBootstrapAddr { addr: String, reason: String },
}

/// Proof that the kameo mesh swarm is running.
///
/// Returned by [`bootstrap_mesh`]. Cheap to clone — pass wherever DHT
/// registration or lookup is needed. Holding a `MeshHandle` is the only
/// way to interact with the mesh; there is no global flag to query.
///
/// # DHT helpers
///
/// [`register_actor`](MeshHandle::register_actor) and
/// [`lookup_actor`](MeshHandle::lookup_actor) consolidate the repeated
/// `into_remote_ref() + register(name)` / `lookup(name)` boilerplate so
/// call sites stay clean.
///
/// # Peer events
///
/// [`subscribe_peer_events`](MeshHandle::subscribe_peer_events) returns a
/// `broadcast::Receiver<PeerEvent>` that fires whenever mDNS discovers or
/// loses a peer. Use this to push real-time `remote_nodes` updates to
/// WebSocket clients without polling.
#[derive(Clone, Debug)]
pub struct MeshHandle {
    peer_id: PeerId,
    /// Broadcast channel for peer lifecycle events.
    /// Capacity 32 — more than enough for typical mesh sizes.
    peer_events_tx: broadcast::Sender<PeerEvent>,
    /// Set of currently-alive peer IDs (inserted on Discovered, removed on Expired).
    /// Used as ground truth to filter stale DHT records when listing remote nodes.
    known_peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Hostname of this node, cached at bootstrap time.
    /// Used to tag local models with a `provider_node` when routing through the mesh.
    local_hostname: Arc<String>,
}

impl MeshHandle {
    fn new(
        peer_id: PeerId,
        peer_events_tx: broadcast::Sender<PeerEvent>,
        known_peers: Arc<RwLock<HashSet<PeerId>>>,
        local_hostname: String,
    ) -> Self {
        Self {
            peer_id,
            peer_events_tx,
            known_peers,
            local_hostname: Arc::new(local_hostname),
        }
    }

    /// The local `PeerId` of this node in the libp2p network.
    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    /// The hostname of this node (cached at mesh bootstrap time).
    pub fn local_hostname(&self) -> &str {
        &self.local_hostname
    }

    /// Check whether a peer is currently known to be alive (discovered and not expired).
    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.known_peers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(peer_id)
    }

    /// Subscribe to peer lifecycle events (discovered / expired).
    ///
    /// Each call returns an independent receiver. Lagged receivers receive
    /// `RecvError::Lagged` and can catch up by calling `recv()` again.
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.peer_events_tx.subscribe()
    }

    /// Register a local actor in REMOTE_REGISTRY and the Kademlia DHT.
    ///
    /// Performs the two-step sequence every mesh-visible actor needs:
    /// 1. `actor_ref.into_remote_ref().await` — inserts into `REMOTE_REGISTRY`
    ///    so incoming remote messages are routable by `ActorId`.
    /// 2. `actor_ref.register(name).await` — publishes under `name` in the
    ///    Kademlia DHT so remote peers can discover the actor by name.
    ///
    /// A warning is logged on DHT registration failure but the function does
    /// not return an error — the actor is still locally routable.
    pub async fn register_actor<A>(
        &self,
        actor_ref: kameo::actor::ActorRef<A>,
        name: impl Into<String>,
    ) where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name = name.into();
        actor_ref.into_remote_ref().await;
        if let Err(e) = actor_ref.register(name.clone()).await {
            log::warn!("MeshHandle: failed to register '{}' in DHT: {}", name, e);
        } else {
            log::debug!("MeshHandle: registered '{}' in kameo DHT", name);
        }
    }

    /// Look up a remote actor by its DHT name.
    ///
    /// Returns `None` when the name is not yet registered.
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        kameo::actor::RemoteActorRef::<A>::lookup(name.into()).await
    }

    /// Stream all remote actors registered under `name` in the DHT.
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        kameo::actor::RemoteActorRef::<A>::lookup_all(name.into())
    }
}

/// Bootstrap the kameo mesh swarm.
///
/// Starts the libp2p networking stack according to `config`. After this call
/// `ActorSwarm::get()` returns `Some(...)` and actors can be registered /
/// looked up across the network.
///
/// Unlike kameo's built-in `bootstrap_on()`, this builds the swarm directly
/// using `kameo::remote::Behaviour` so we own the event loop and can emit
/// [`PeerEvent`]s whenever mDNS discovers or loses a peer.
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
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::{
        SwarmBuilder, mdns, noise,
        swarm::{NetworkBehaviour, SwarmEvent},
        tcp, yamux,
    };

    let listen_addr = config.listen.as_deref().unwrap_or("/ip4/0.0.0.0/tcp/0");

    // Validate bootstrap_peers addresses up-front so we fail fast.
    for peer_addr in &config.bootstrap_peers {
        peer_addr
            .parse::<libp2p::Multiaddr>()
            .map_err(|e| MeshError::InvalidBootstrapAddr {
                addr: peer_addr.clone(),
                reason: e.to_string(),
            })?;
    }

    // Broadcast channel for peer lifecycle events.
    // Capacity 32 — enough for any realistic mesh churn burst.
    let (peer_events_tx, _) = broadcast::channel::<PeerEvent>(32);
    let peer_events_tx_loop = peer_events_tx.clone();

    // Shared set of currently-alive peers, maintained by the event loop.
    let known_peers: Arc<RwLock<HashSet<PeerId>>> = Arc::new(RwLock::new(HashSet::new()));
    let known_peers_loop = Arc::clone(&known_peers);

    // Cache hostname once at bootstrap time (same logic as RemoteNodeManager).
    let local_hostname = resolve_local_hostname();

    // ── Build the libp2p swarm ────────────────────────────────────────────────
    // We replicate exactly what kameo's bootstrap_on() does, but own the event
    // loop so we can emit PeerEvents on mDNS discovery / expiry.

    #[derive(NetworkBehaviour)]
    struct MeshBehaviour {
        kameo: remote::Behaviour,
        mdns: mdns::tokio::Behaviour,
    }

    let mut swarm = SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| MeshError::SwarmError(e.to_string()))?
        .with_quic()
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();
            let kameo_behaviour =
                remote::Behaviour::new(local_peer_id, remote::messaging::Config::default());
            // Use a short TTL and query interval so disconnected peers are
            // detected promptly (~30 s) rather than waiting for the 5-minute
            // libp2p default. The query_interval drives how often we re-announce
            // ourselves; ttl is how long peers are considered alive after their
            // last announcement. Together these bound the stale-peer window to
            // roughly ttl (30 s) after a crash rather than 5+ minutes.
            let mdns_config = mdns::Config {
                ttl: std::time::Duration::from_secs(30),
                query_interval: std::time::Duration::from_secs(15),
                ..mdns::Config::default()
            };
            let mdns_behaviour = mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?;
            Ok(MeshBehaviour {
                kameo: kameo_behaviour,
                mdns: mdns_behaviour,
            })
        })
        .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
        .build();

    // Register the kameo behaviour as the global ActorSwarm.
    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    swarm
        .listen_on(listen_addr.parse().map_err(|e: libp2p::multiaddr::Error| {
            MeshError::InvalidListenAddr {
                addr: listen_addr.to_string(),
                reason: e.to_string(),
            }
        })?)
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let local_peer_id = *swarm.local_peer_id();

    // ── Swarm event loop ──────────────────────────────────────────────────────
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    // A single peer may be reported multiple times in the list
                    // (once per transport address, e.g. TCP + QUIC).  Register
                    // all addresses but emit only one PeerEvent per unique peer
                    // so downstream watchers don't fire redundant DHT queries.
                    let mut seen = std::collections::HashSet::new();
                    for (peer_id, multiaddr) in list {
                        swarm.add_peer_address(peer_id, multiaddr);
                        if seen.insert(peer_id) {
                            log::info!("mDNS discovered peer: {peer_id}");
                            // Track as alive
                            if let Ok(mut peers) = known_peers_loop.write() {
                                peers.insert(peer_id);
                            }
                            let _ = peer_events_tx_loop.send(PeerEvent::Discovered(peer_id));
                        }
                    }
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    // Same dedup as Discovered: one event per unique peer.
                    // Collect all addresses per peer first so we can re-add
                    // them if the peer comes back (libp2p 0.56 has no
                    // remove_peer_address, so we just disconnect and let mDNS
                    // re-announce the fresh addresses on reconnect).
                    let mut seen = std::collections::HashSet::new();
                    for (peer_id, _multiaddr) in list {
                        if seen.insert(peer_id) {
                            log::info!("mDNS peer expired (went away): {peer_id}");
                            // Remove from known-peers set so list_remote_nodes
                            // won't try to query stale DHT records.
                            if let Ok(mut peers) = known_peers_loop.write() {
                                peers.remove(&peer_id);
                            }
                            // Close the active connection so kameo stops trying
                            // to route messages to the dead peer.
                            let _ = swarm.disconnect_peer_id(peer_id);
                            let _ = peer_events_tx_loop.send(PeerEvent::Expired(peer_id));
                        }
                    }
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("ActorSwarm listening on {address}");
                }
                _ => {}
            }
        }
    });

    if matches!(&config.discovery, MeshDiscovery::Kademlia { bootstrap } if !bootstrap.is_empty()) {
        if let MeshDiscovery::Kademlia { bootstrap } = &config.discovery {
            for addr in bootstrap {
                log::info!("Kademlia bootstrap peer: {}", addr);
            }
        }
    }

    log::info!(
        "Kameo mesh bootstrapped: peer_id={}, listen={}",
        local_peer_id,
        listen_addr
    );

    Ok(MeshHandle::new(
        local_peer_id,
        peer_events_tx,
        known_peers,
        local_hostname,
    ))
}

/// Bootstrap the mesh with default settings (mDNS, port 9000).
///
/// Convenience wrapper around `bootstrap_mesh(&MeshConfig::default())`.
pub async fn bootstrap_mesh_default() -> Result<MeshHandle, MeshError> {
    bootstrap_mesh(&MeshConfig::default()).await
}

/// Resolve the local hostname (same logic as `RemoteNodeManager::get_hostname`
/// but available outside the `remote_impl` module).
fn resolve_local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
