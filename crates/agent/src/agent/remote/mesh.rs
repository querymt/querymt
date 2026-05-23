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

use kameo::remote;
use libp2p::{Multiaddr, PeerId};
use moka::sync::Cache;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

use super::scope::{MeshScopeId, MeshTransportKind};

/// Commands sent from `MeshHandle` to the swarm event loop.
///
/// The event loop owns the `Swarm` and is the only place that can mutate it.
/// Higher-level code uses `MeshHandle` methods (e.g. `dial_peer`) which
/// translate intent into a `SwarmCommand` and send it over an `mpsc` channel.
#[derive(Debug)]
#[cfg_attr(not(feature = "remote-internet"), allow(dead_code))]
enum SwarmCommand {
    /// Request the swarm to dial a peer by `PeerId`.
    ///
    /// The event loop converts this to a `/p2p/{peer_id}` multiaddr and calls
    /// `swarm.dial()`.  For iroh transport the relay network resolves the
    /// address; for LAN the peer must already have a known address (mDNS).
    DialPeer(PeerId),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    peer_id: PeerId,
    transport: MeshTransportKind,
    scope: MeshScopeId,
}

#[derive(Debug, Clone)]
pub struct MeshRoute {
    pub peer_id: PeerId,
    pub transport: MeshTransportKind,
    pub scope: MeshScopeId,
    pub addrs: HashSet<Multiaddr>,
    pub last_seen: Instant,
    pub priority: u32,
}

#[derive(Clone, Debug)]
pub struct RouteTable {
    routes: Cache<RouteKey, MeshRoute>,
}

impl RouteTable {
    fn route_sort_key(route: &MeshRoute) -> (u32, Instant) {
        (route.priority, route.last_seen)
    }
    pub fn new(ttl: Duration) -> Self {
        Self {
            routes: Cache::builder().time_to_idle(ttl).build(),
        }
    }

    pub fn upsert_addrs<I>(
        &self,
        peer_id: PeerId,
        transport: MeshTransportKind,
        scope: MeshScopeId,
        addrs: I,
        priority: u32,
    ) -> MeshRoute
    where
        I: IntoIterator<Item = Multiaddr>,
    {
        let key = RouteKey {
            peer_id,
            transport,
            scope,
        };
        let mut route = self.routes.get(&key).unwrap_or(MeshRoute {
            peer_id,
            transport,
            scope: key.scope.clone(),
            addrs: HashSet::new(),
            last_seen: Instant::now(),
            priority,
        });
        for addr in addrs {
            route.addrs.insert(addr);
        }
        route.last_seen = Instant::now();
        route.priority = priority;
        self.routes.insert(key, route.clone());
        route
    }

    pub fn remove_addrs(
        &self,
        peer_id: PeerId,
        transport: MeshTransportKind,
        scope: MeshScopeId,
        expired: &HashSet<Multiaddr>,
    ) -> Option<MeshRoute> {
        let key = RouteKey {
            peer_id,
            transport,
            scope,
        };
        let mut route = self.routes.get(&key)?;
        for addr in expired {
            route.addrs.remove(addr);
        }
        if route.addrs.is_empty() {
            self.routes.remove(&key);
            None
        } else {
            route.last_seen = Instant::now();
            self.routes.insert(key, route.clone());
            Some(route)
        }
    }

    pub fn routes_for_peer(&self, peer_id: &PeerId) -> Vec<MeshRoute> {
        self.routes
            .iter()
            .filter_map(|(k, v)| if &k.peer_id == peer_id { Some(v) } else { None })
            .collect()
    }

    pub fn remove_peer(&self, peer_id: &PeerId) {
        let keys: Vec<std::sync::Arc<RouteKey>> = self
            .routes
            .iter()
            .filter_map(|(k, _)| if &k.peer_id == peer_id { Some(k) } else { None })
            .collect();
        for k in keys {
            self.routes.remove(k.as_ref());
        }
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.routes.iter().any(|(k, _)| &k.peer_id == peer_id)
    }

    pub fn peer_ids(&self) -> Vec<PeerId> {
        let mut out = HashSet::new();
        for (k, _) in self.routes.iter() {
            out.insert(k.peer_id);
        }
        out.into_iter().collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peer_ids().len()
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes
            .iter()
            .filter_map(|(k, v)| if &k.peer_id == peer_id { Some(v) } else { None })
            .max_by_key(Self::route_sort_key)
    }
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

/// Transport layer for the mesh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MeshTransportMode {
    /// Traditional libp2p TCP + QUIC + Noise + Yamux (LAN-optimised).
    #[default]
    Lan,
    /// iroh-backed QUIC transport with relay and NAT traversal (internet-capable).
    Iroh,
    /// Composite: LAN + Iroh transports active concurrently.
    Composite,
}

impl MeshTransportMode {
    /// Returns `true` when the LAN transport is active (Lan or Composite).
    pub fn has_lan(&self) -> bool {
        matches!(self, Self::Lan | Self::Composite)
    }
}

/// How actor lookups are performed in the mesh.
///
/// Set via `MeshConfig::directory`.
#[derive(Debug, Clone, Default)]
pub enum DirectoryMode {
    /// Standard Kademlia DHT lookups with Phase 1b retry backoff.
    ///
    /// This is the default and works well for all mesh sizes.
    #[default]
    Kademlia,

    /// Cached lookups with peer registry exchange (Phase 3).
    ///
    /// Maintains a local `HashMap<name, RemoteActorRef>` cache pre-warmed on
    /// peer discovery. Recommended for small LAN meshes (2–10 nodes) where
    /// Kademlia propagation latency is noticeable. Falls back to Kademlia on
    /// cache miss.
    Cached,
}

/// Configuration for the kameo mesh (libp2p swarm).
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Multiaddr to listen on, e.g. `"/ip4/0.0.0.0/tcp/0"` (OS-assigned random port).
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

    /// How actors are discovered in the mesh.
    ///
    /// Defaults to [`DirectoryMode::Kademlia`]. Switch to
    /// [`DirectoryMode::Cached`] for small LAN meshes to eliminate first-call
    /// Kademlia latency.
    pub directory: DirectoryMode,

    /// Timeout for non-streaming mesh request-response calls.
    ///
    /// This controls how long a caller waits for a response to `ask()` calls
    /// over the mesh (e.g. compaction, no-tools LLM inference).  The default
    /// libp2p request-response timeout is only 10 s, which is far too short
    /// for LLM inference on large contexts.
    ///
    /// Defaults to 300 seconds (5 minutes).
    pub request_timeout: std::time::Duration,

    /// Grace period to tolerate transport disconnects while waiting for stream
    /// delivery to resume.
    pub stream_reconnect_grace: std::time::Duration,

    /// Transport layer to use.
    ///
    /// `Lan` (default) uses the traditional TCP + QUIC + Noise + Yamux stack
    /// with mDNS discovery.  `Iroh` uses `libp2p-iroh` for NAT traversal and
    /// relay-based connectivity across the internet.
    pub transport: MeshTransportMode,

    /// Path to the persistent ed25519 identity file.
    ///
    /// When `None`, defaults to `~/.qmt/mesh_identity.key`.
    /// The keypair is generated on first use and reused on subsequent starts
    /// so the node's `PeerId` is stable across restarts.
    pub identity_file: Option<std::path::PathBuf>,

    /// Pre-parsed signed invite grant (v2.5).
    ///
    /// When set, the mesh bootstraps in "join" mode: it dials the inviter
    /// (from the grant) instead of waiting for mDNS or explicit peers.
    /// Takes priority over `bootstrap_peers`.
    pub invite: Option<super::invite::SignedInviteGrant>,
}

impl Default for MeshConfig {
    /// Sensible defaults: listen on a random port with mDNS.
    fn default() -> Self {
        Self {
            listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
            discovery: MeshDiscovery::Mdns,
            bootstrap_peers: vec![],
            directory: DirectoryMode::default(),
            request_timeout: std::time::Duration::from_secs(300),
            stream_reconnect_grace: std::time::Duration::from_secs(120),
            transport: MeshTransportMode::default(),
            identity_file: None,
            invite: None,
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
/// Type alias for a boxed re-registration closure.
///
/// Each closure captures one (`ActorRef`, `name`) pair and re-runs the full
/// `into_remote_ref() + register(name)` sequence when called.  Stored so that
/// the event loop can re-publish all local actors whenever a new peer is
/// discovered (Phase 1c).
type ReRegisterFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct MeshHandle {
    peer_id: PeerId,
    /// Broadcast channel for mesh lifecycle events.
    /// Capacity 64 to absorb bursty route updates.
    peer_events_tx: broadcast::Sender<MeshEvent>,
    /// Transport/scope-aware peer reachability table with TTL-based aging.
    routes: Arc<RouteTable>,
    /// Hostname of this node, cached at bootstrap time for display-only metadata.
    local_hostname: Arc<String>,
    /// Re-registration closures for all locally-registered actors.
    ///
    /// Populated by `register_actor`; invoked by the event loop whenever mDNS
    /// discovers a new peer so the new peer's Kademlia routing table is
    /// populated immediately rather than waiting for the next republish cycle.
    re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    /// The node's ed25519 identity keypair, cloned from bootstrap context.
    ///
    /// Used to sign invite grants.  The keypair is the same one used by
    /// the libp2p swarm — its public key derives this node's `PeerId`.
    keypair: Arc<libp2p::identity::Keypair>,
    /// Host-side invite store for tracking issued invites.
    ///
    /// `None` when the node is a joiner (not a host).  Wrapped in
    /// `Arc<RwLock<..>>` for shared access from the mesh handle and
    /// the admission handler in `RemoteNodeManager`.
    invite_store: Option<Arc<RwLock<super::invite::InviteStore>>>,
    /// Joiner-side membership store: persists tokens + cached peer addresses.
    ///
    /// `None` when the node never joined via an invite, or when the store
    /// could not be loaded from disk.
    membership_store: Option<Arc<RwLock<super::invite::MembershipStore>>>,
    /// Active transport mode used by this mesh handle.
    transport_mode: MeshTransportMode,
    /// Channel for sending commands to the swarm event loop.
    ///
    /// Used by `dial_peer()` to form a full mesh in iroh mode, where peers
    /// only connect to the inviter by default (star topology).  The event
    /// loop polls the receiver and executes each `SwarmCommand`.
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    /// Grace period to tolerate temporary disconnections during streaming.
    stream_reconnect_grace: std::time::Duration,
    /// Cached union of config-derived scopes (e.g. from MeshRuntimeConfig)
    /// and currently joined Iroh scopes persisted in the membership store.
    config_scopes: Vec<MeshScopeId>,
}

impl std::fmt::Debug for MeshHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshHandle")
            .field("peer_id", &self.peer_id)
            .field("local_hostname", &self.local_hostname)
            .field("re_register_fns_count", &self.re_register_fns.read().len())
            .field("has_invite_store", &self.invite_store.is_some())
            .field("has_membership_store", &self.membership_store.is_some())
            .field("transport_mode", &self.transport_mode)
            .finish_non_exhaustive()
    }
}

impl MeshHandle {
    #[allow(clippy::too_many_arguments)]
    fn new(
        peer_id: PeerId,
        peer_events_tx: broadcast::Sender<MeshEvent>,
        routes: Arc<RouteTable>,
        local_hostname: String,
        re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
        keypair: libp2p::identity::Keypair,
        invite_store: Option<Arc<RwLock<super::invite::InviteStore>>>,
        membership_store: Option<Arc<RwLock<super::invite::MembershipStore>>>,
        transport_mode: MeshTransportMode,
        swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
        stream_reconnect_grace: std::time::Duration,
    ) -> Self {
        Self {
            peer_id,
            peer_events_tx,
            routes,
            local_hostname: Arc::new(local_hostname),
            re_register_fns,
            keypair: Arc::new(keypair),
            invite_store,
            membership_store,
            transport_mode,
            swarm_cmd_tx,
            stream_reconnect_grace,
            config_scopes: Vec::new(),
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
        self.routes.is_peer_alive(peer_id)
    }

    /// Grace period used while waiting for a disconnected stream to reconnect.
    pub fn stream_reconnect_grace(&self) -> std::time::Duration {
        self.stream_reconnect_grace
    }

    /// Inject a peer directly into the `known_peers` map, bypassing mDNS.
    ///
    /// **Test-only.** In production `known_peers` is populated exclusively by
    /// mDNS `Discovered` events (which require real network time).  This helper
    /// lets integration tests simulate "mDNS has fired" so that
    /// `resolve_peer_node_id` will iterate the injected peer without waiting
    /// for the actual mDNS timer.
    #[cfg(test)]
    pub fn inject_known_peer_for_test(&self, peer_id: PeerId) {
        self.routes.upsert_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [Multiaddr::empty()],
            100,
        );
    }

    /// Subscribe to peer lifecycle events (discovered / expired).
    ///
    /// Each call returns an independent receiver. Lagged receivers receive
    /// `RecvError::Lagged` and can catch up by calling `recv()` again.
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<MeshEvent> {
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
    ///
    /// Additionally stores a re-registration closure so that when a new peer
    /// is discovered via mDNS, all locally registered actors are immediately
    /// re-published into the new peer's Kademlia routing table (Phase 1c).
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

        // Store a closure for re-registration on peer discovery (Phase 1c).
        // `into_remote_ref()` is idempotent for already-registered actors;
        // `register()` re-publishes the provider record and DHT entry.
        let name_clone = name.clone();
        let actor_ref_clone = actor_ref.clone();
        let re_register: ReRegisterFn = Arc::new(move || {
            let name = name_clone.clone();
            let actor_ref = actor_ref_clone.clone();
            Box::pin(async move {
                actor_ref.into_remote_ref().await;
                if let Err(e) = actor_ref.register(name.clone()).await {
                    log::warn!(
                        "MeshHandle: re-registration of '{}' after peer discovery failed: {}",
                        name,
                        e
                    );
                } else {
                    log::debug!(
                        "MeshHandle: re-registered '{}' after new peer discovery",
                        name
                    );
                }
            })
        });
        self.re_register_fns
            .write()
            .insert(name.clone(), re_register);
    }

    /// Look up a remote actor by its DHT name.
    ///
    /// Performs up to 4 attempts with exponential backoff (250 ms, 500 ms,
    /// 1 000 ms between retries) so that transient Kademlia propagation gaps
    /// — e.g. immediately after mDNS re-discovery — are masked transparently.
    /// Total worst-case latency: ~1.75 s, acceptable for an LLM call.
    ///
    /// Returns `None` only after all attempts have missed.
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        const BACKOFF_MS: &[u64] = &[250, 500, 1_000];
        let name: String = name.into();

        // First attempt — no delay.
        if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
            return Ok(Some(r));
        }

        // Retry with backoff.
        for (attempt, &delay_ms) in BACKOFF_MS.iter().enumerate() {
            tracing::debug!(
                attempt = attempt + 1,
                delay_ms,
                dht_name = %name,
                "DHT lookup miss, retrying after backoff"
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

            if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
                tracing::info!(
                    attempt = attempt + 1,
                    dht_name = %name,
                    "DHT lookup succeeded on retry"
                );
                return Ok(Some(r));
            }
        }

        Ok(None)
    }

    /// Look up a single remote actor by its DHT name **without** retries.
    ///
    /// This is a cheaper variant of [`lookup_actor`] intended for bulk / background
    /// operations (e.g. bookmark reattach during session listing) where the
    /// caller prefers a fast `None` over spending ~1.75 s on retry backoff.
    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name: String = name.into();
        kameo::actor::RemoteActorRef::<A>::lookup(name).await
    }

    /// Stream all remote actors registered under `name` in the DHT.
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        kameo::actor::RemoteActorRef::<A>::lookup_all(name.into())
    }

    /// Resolve a human-readable peer name (from `[[mesh.peers]]`) to a `NodeId`.
    ///
    /// Iterates known (alive) peers, looks up each peer's `RemoteNodeManager`
    /// via the per-peer DHT name (`node_manager::peer::{peer_id}`), and calls
    /// `GetNodeInfo` to check whether the hostname matches `peer_name`.
    ///
    /// Returns `None` if no matching peer is found after checking all known peers.
    ///
    /// # Why per-peer DHT names?
    ///
    /// The per-peer name (`node_manager::peer::{peer_id}`) bypasses the global
    /// Kademlia `GET_PROVIDERS` lookup that fails in 2-node meshes. It's the same
    /// mechanism used by `MeshChatProvider` and the Model Picker, which is why
    /// this path is reliable even immediately after `mDNS` discovery.
    pub async fn resolve_peer_node_id(
        &self,
        peer_name: &str,
    ) -> Option<crate::agent::remote::NodeId> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};

        let peers: Vec<PeerId> = self.routes.peer_ids();

        for peer_id in peers {
            let mut node_manager = None;
            for scope in self.active_scopes() {
                let per_peer_name =
                    crate::agent::remote::scope::scoped_node_manager_for_peer(&scope, &peer_id);
                match self.lookup_actor::<RemoteNodeManager>(&per_peer_name).await {
                    Ok(Some(r)) => {
                        node_manager = Some(r);
                        break;
                    }
                    Ok(None) => {
                        log::debug!(
                            "resolve_peer_node_id: no RemoteNodeManager under '{}'",
                            per_peer_name
                        );
                    }
                    Err(e) => {
                        log::debug!(
                            "resolve_peer_node_id: lookup error for '{}': {}",
                            per_peer_name,
                            e
                        );
                    }
                }
            }
            let Some(node_manager) = node_manager else {
                continue;
            };

            let node_info = match node_manager.ask::<GetNodeInfo>(&GetNodeInfo).await {
                Ok(info) => info,
                Err(e) => {
                    log::debug!(
                        "resolve_peer_node_id: GetNodeInfo failed for peer {}: {}",
                        peer_id,
                        e
                    );
                    continue;
                }
            };

            if node_info.hostname == peer_name {
                log::info!(
                    "resolve_peer_node_id: resolved '{}' → node_id={}",
                    peer_name,
                    node_info.node_id
                );
                return Some(node_info.node_id);
            }
        }

        log::debug!(
            "resolve_peer_node_id: no peer with hostname '{}' found in {} known peers",
            peer_name,
            self.routes.peer_count()
        );
        None
    }

    /// Remove the re-registration closure for a named actor.
    ///
    /// Call this when a mesh-registered actor is stopped so that its closure
    /// does not accumulate in the re-registration map and fire uselessly on
    /// every future peer discovery event.
    ///
    /// No-op if `name` is not present.
    pub fn deregister_actor(&self, name: &str) {
        self.re_register_fns.write().remove(name);
    }

    /// Return the number of currently registered re-registration closures.
    ///
    /// Useful for tests and debug logging to verify that dead actors are
    /// cleaned up properly.
    pub fn re_register_fns_count(&self) -> usize {
        self.re_register_fns.read().len()
    }

    /// Check whether a re-registration closure exists for the given name.
    ///
    /// Useful in tests to verify register/deregister without relying on
    /// absolute counts (the test mesh is shared across concurrent tests).
    pub fn has_re_register_fn(&self, name: &str) -> bool {
        self.re_register_fns.read().contains_key(name)
    }

    /// Return all currently-known peer IDs (alive, not expired).
    pub fn known_peer_ids(&self) -> Vec<PeerId> {
        self.routes.peer_ids()
    }

    /// Return currently active logical scopes for this runtime.
    ///
    /// Scopes are the union of config-derived scopes and persisted joined Iroh
    /// memberships. If neither source has entries (legacy LAN bootstrap), we
    /// fall back to `Lan` for backward compatibility.
    /// Set the config-derived scopes for this handle.
    ///
    /// Carrying config scopes in the handle ensures that iroh scopes are
    /// authoritative for DHT registration/lookup even before persistable
    /// membership stores have been populated.
    pub fn set_config_scopes(&mut self, scopes: Vec<MeshScopeId>) {
        self.config_scopes = scopes;
    }

    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        let mut scopes = Vec::new();

        // Always include the config-derived scopes first — these are
        // authoritative and guarantee scoped DHT names exist from startup.
        scopes.extend(self.config_scopes.iter().cloned());

        // Union with persisted Iroh memberships (joins from invites).
        if let Some(store) = &self.membership_store {
            for mesh_id in store.read().mesh_ids() {
                let scope = MeshScopeId::Iroh { mesh_id };
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }

        // Include LAN scope whenever the LAN transport is active.
        // This is NOT gated on emptiness — a node with both LAN + Iroh
        // transports must register/lookup under the LAN scope too, since
        // mDNS-discovered peers are reachable via LAN routes.
        if self.transport_mode.has_lan() {
            let lan = MeshScopeId::lan_default();
            if !scopes.contains(&lan) {
                scopes.push(lan);
            }
        }

        // Legacy backward compat: when no scopes are configured at all AND
        // transport_mode doesn't advertise LAN (shouldn't happen in practice),
        // fall back to Lan.
        if scopes.is_empty() {
            scopes.push(MeshScopeId::lan_default());
        }

        scopes.sort_by_key(|s| s.to_string());
        scopes.dedup();
        scopes
    }

    /// Return joined Iroh scopes only (deterministic order).
    pub fn joined_iroh_scopes(&self) -> Vec<MeshScopeId> {
        let Some(store) = &self.membership_store else {
            return Vec::new();
        };
        store
            .read()
            .mesh_ids()
            .into_iter()
            .map(|mesh_id| MeshScopeId::Iroh { mesh_id })
            .collect()
    }

    /// Leave an Iroh scope while keeping LAN runtime alive.
    ///
    /// This removes scope membership and asks the swarm loop to stop
    /// scope-associated reconnect attempts. It does not tear down the swarm.
    pub fn leave_iroh_scope(&self, mesh_id: &str) -> Result<bool, super::invite::InviteError> {
        let Some(store) = &self.membership_store else {
            return Ok(false);
        };
        let removed = store.write().remove_membership(mesh_id)?;
        if !removed {
            return Ok(false);
        }

        let _ = self
            .peer_events_tx
            .send(MeshEvent::ScopeLeft(MeshScopeId::Iroh {
                mesh_id: mesh_id.to_string(),
            }));

        if self
            .swarm_cmd_tx
            .send(SwarmCommand::LeaveIrohScope {
                mesh_id: mesh_id.to_string(),
            })
            .is_err()
        {
            log::warn!("leave_iroh_scope: swarm event loop has shut down");
        }
        Ok(true)
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes.best_route_for_peer(peer_id)
    }

    /// Create a signed invite grant for this mesh (v2.5).
    ///
    /// The invite contains this node's `PeerId` (as the entry point) and is
    /// signed with this node's ed25519 identity keypair.  Share the resulting
    /// token (via QR code, URL, or clipboard) with the joining node.
    ///
    /// If an `InviteStore` is attached, the invite is recorded for tracking.
    ///
    /// # Arguments
    /// - `mesh_name` — optional human-readable label (e.g. "Dev Mesh")
    /// - `ttl_secs` — optional time-to-live in seconds; `None` = no expiry
    /// - `max_uses` — max number of times this invite can be used; `None` = 1
    /// - `can_invite` — whether the joiner can create their own invites
    pub fn create_invite(
        &self,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: Option<u32>,
        can_invite: bool,
    ) -> Result<super::invite::SignedInviteGrant, super::invite::InviteError> {
        let max_uses = max_uses.unwrap_or(1);
        let permissions = super::invite::InvitePermissions {
            can_invite,
            role: "member".to_string(),
        };

        if let Some(ref store) = self.invite_store {
            store.write().create_invite(
                &self.keypair,
                &self.peer_id.to_string(),
                mesh_name,
                ttl_secs,
                max_uses,
                permissions,
            )
        } else {
            // No store — just sign and return without tracking.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expires_at = ttl_secs.map(|ttl| now + ttl).unwrap_or(0);

            let grant = super::invite::InviteGrant {
                version: 3,
                invite_id: uuid::Uuid::now_v7().to_string(),
                inviter_peer_id: self.peer_id.to_string(),
                mesh_name,
                expires_at,
                max_uses,
                permissions,
            };
            grant.sign(&self.keypair)
        }
    }

    /// Return a reference to the node's identity keypair.
    ///
    /// Useful for callers that need to sign additional data (e.g. custom
    /// auth handshakes in future versions).
    pub fn keypair(&self) -> &libp2p::identity::Keypair {
        &self.keypair
    }

    /// Return a reference to the invite store (if any).
    pub fn invite_store(&self) -> Option<&Arc<RwLock<super::invite::InviteStore>>> {
        self.invite_store.as_ref()
    }

    /// Return a reference to the membership store (if any).
    pub fn membership_store(&self) -> Option<&Arc<RwLock<super::invite::MembershipStore>>> {
        self.membership_store.as_ref()
    }

    /// Update the cached peer list for a mesh membership entry.
    ///
    /// Called from the swarm event loop whenever peers are discovered, so that
    /// the joiner always has a fresh list for reconnection without the original
    /// inviter.
    pub fn update_membership_peers(&self, mesh_id: &str, peers: Vec<super::invite::PeerEntry>) {
        if let Some(ref store) = self.membership_store
            && let Err(e) = store.write().update_known_peers(mesh_id, peers)
        {
            log::warn!(
                "Failed to update membership peer cache for {}: {}",
                mesh_id,
                e
            );
        }
    }

    /// Return the active transport mode for this mesh handle.
    ///
    /// # Deprecation
    ///
    /// This method assumes a single mutually-exclusive transport.  In the
    /// multi-transport architecture (Phases 4+), a runtime may have multiple
    /// transports active concurrently.  Use
    /// [`MeshRuntimeHandle::has_transport`] or
    /// [`MeshRuntimeHandle::enabled_transports`] instead.
    #[deprecated(
        since = "0.1.0",
        note = "use MeshRuntimeHandle::has_transport() or MeshRuntimeHandle::enabled_transports()"
    )]
    pub fn transport_mode(&self) -> MeshTransportMode {
        self.transport_mode.clone()
    }

    /// Return true when the mesh is internet-capable iroh transport.
    ///
    /// # Deprecation
    ///
    /// This method assumes a single mutually-exclusive transport.  In the
    /// multi-transport architecture (Phases 4+), a runtime may have multiple
    /// transports active concurrently.  Use
    /// [`MeshRuntimeHandle::has_transport`] instead.
    #[deprecated(
        since = "0.1.0",
        note = "use MeshRuntimeHandle::has_transport(MeshTransportKind::Iroh)"
    )]
    pub fn is_iroh_transport(&self) -> bool {
        matches!(self.transport_mode, MeshTransportMode::Iroh)
    }

    /// Request the swarm event loop to dial a peer by `PeerId`.
    ///
    /// Used for iroh mesh fan-out after invite admission. On LAN, peer discovery
    /// is mDNS-driven and explicit dialing is intentionally a no-op.
    pub fn dial_peer(&self, peer_id: &PeerId) {
        if peer_id == &self.peer_id {
            return; // Don't dial ourselves.
        }
        if matches!(self.transport_mode, MeshTransportMode::Lan) {
            log::debug!("dial_peer ignored on LAN-only transport (mDNS handles discovery)");
            return;
        }
        // Both Iroh-only and Composite modes send dial commands.

        if self
            .swarm_cmd_tx
            .send(SwarmCommand::DialPeer(*peer_id))
            .is_err()
        {
            log::warn!("dial_peer: swarm event loop has shut down");
        }
    }

    /// Internal: check if transport mode is Iroh without triggering deprecation.
    ///
    /// For use within this crate only while call sites are being migrated to
    /// [`MeshRuntimeHandle`].  External/new code should use
    /// `MeshRuntimeHandle::has_transport(MeshTransportKind::Iroh)`.
    pub(crate) fn is_iroh_transport_internal(&self) -> bool {
        matches!(
            self.transport_mode,
            MeshTransportMode::Iroh | MeshTransportMode::Composite
        )
    }
}

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
        MeshTransportMode::Iroh => {
            #[cfg(feature = "remote-internet")]
            {
                bootstrap_iroh_mesh(config).await
            }
            #[cfg(not(feature = "remote-internet"))]
            {
                Err(MeshError::SwarmError(
                    "iroh transport requires the 'remote-internet' cargo feature".to_string(),
                ))
            }
        }
        MeshTransportMode::Composite => {
            #[cfg(feature = "remote-internet")]
            {
                bootstrap_composite_mesh(config).await
            }
            #[cfg(not(feature = "remote-internet"))]
            {
                Err(MeshError::SwarmError(
                    "composite transport requires the 'remote-internet' cargo feature".to_string(),
                ))
            }
        }
    }
}

// ── Shared event-loop helpers ──────────────────────────────────────────────────

/// Handle a `ConnectionEstablished` swarm event.
///
/// Populates Kademlia's routing table, updates `known_peers`, fires
/// `PeerEvent::Discovered` for genuinely new peers, and triggers the
/// re-registration cascade so the new peer's DHT is populated immediately.
fn handle_mdns_discovered<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    list: Vec<(PeerId, Multiaddr)>,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    routes: &RouteTable,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    re_register_fns: &RwLock<HashMap<String, ReRegisterFn>>,
) {
    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    for (peer_id, multiaddr) in list {
        swarm.add_peer_address(peer_id, multiaddr.clone());
        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
    }

    for (peer_id, new_addrs) in addrs_by_peer {
        let (is_new, has_new_addr) = {
            let peers = known_peers.read();
            match peers.get(&peer_id) {
                None => (true, false),
                Some(known_addrs) => {
                    let any_new = new_addrs.iter().any(|a| !known_addrs.contains(a));
                    (false, any_new)
                }
            }
        };

        {
            let mut peers = known_peers.write();
            let entry = peers.entry(peer_id).or_default();
            for addr in &new_addrs {
                entry.insert(addr.clone());
            }
        }

        if is_new {
            log::info!("mDNS discovered peer: {peer_id}");
        } else if has_new_addr {
            log::info!(
                "mDNS re-discovered peer {peer_id} with new address(es): {:?}",
                new_addrs
            );
            // Fall through to route refresh.
        } else {
            log::debug!("mDNS re-announced peer {peer_id} (refreshing route TTL)");
            // Continue to route refresh; do NOT skip.  The RouteTable uses a
            // 90-second TTL cache — skipping the refresh here would allow the
            // route to expire even though the peer is still live, causing
            // is_peer_alive / provider-discovery divergence from known_peers.
        }

        let route = routes.upsert_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            new_addrs.clone(),
            100,
        );
        let _ = peer_events_tx.send(PeerEvent::RouteAdded { peer_id, route });
        let _ = peer_events_tx.send(PeerEvent::Discovered(peer_id));

        let fns: Vec<ReRegisterFn> = re_register_fns.read().values().cloned().collect();
        if !fns.is_empty() {
            tokio::spawn(async move {
                for f in &fns {
                    f().await;
                }
            });
        }
    }
}

fn handle_mdns_expired<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    list: Vec<(PeerId, Multiaddr)>,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    routes: &RouteTable,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    for (peer_id, multiaddr) in list {
        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
    }

    for (peer_id, expired_addrs) in addrs_by_peer {
        let peer_fully_gone = {
            let mut peers = known_peers.write();
            if let Some(known_addrs) = peers.get_mut(&peer_id) {
                for addr in &expired_addrs {
                    known_addrs.remove(addr);
                }
                if known_addrs.is_empty() {
                    peers.remove(&peer_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        let expired_set: HashSet<Multiaddr> = expired_addrs.into_iter().collect();
        if let Some(route) = routes.remove_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            &expired_set,
        ) {
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        if peer_fully_gone {
            log::info!("mDNS peer expired (went away): {peer_id}");
            let _ = swarm.disconnect_peer_id(peer_id);
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        } else {
            log::debug!(
                "mDNS partial expiry for peer {peer_id}: some addresses expired but peer still reachable"
            );
        }
    }
}

/// Determine which routes should be refreshed on a connection-established event.
///
/// In composite mode we always keep LAN routing warm (for unscoped DHT names and
/// mDNS-style discoverability) and additionally add Iroh-scoped routing when the
/// peer is known in an Iroh scope.
fn connection_route_plan(
    has_lan: bool,
    has_iroh: bool,
    iroh_scope: Option<&MeshScopeId>,
) -> Vec<(MeshTransportKind, MeshScopeId, u32)> {
    let mut plan = Vec::new();
    if has_lan {
        plan.push((MeshTransportKind::Lan, MeshScopeId::lan_default(), 100));
    }
    if has_iroh && let Some(scope) = iroh_scope {
        plan.push((MeshTransportKind::Iroh, scope.clone(), 70));
    }
    plan
}

fn refresh_membership_known_peers(
    membership_store: &Option<Arc<RwLock<super::invite::MembershipStore>>>,
    routes: &RouteTable,
) {
    let Some(ms) = membership_store.as_ref() else {
        return;
    };

    let peers: Vec<super::invite::PeerEntry> = routes
        .peer_ids()
        .into_iter()
        .map(|pid| super::invite::PeerEntry {
            peer_id: pid.to_string(),
            addrs: vec![format!("/p2p/{pid}")],
        })
        .collect();

    let ms = Arc::clone(ms);
    tokio::spawn(async move {
        let mut store = ms.write();
        for (mid, _) in store
            .all()
            .map(|(k, _)| (k.to_string(), ()))
            .collect::<Vec<_>>()
        {
            let _ = store.update_known_peers(&mid, peers.clone());
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_connection_established<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    peer_id: PeerId,
    remote_addr: Multiaddr,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    re_register_fns: &RwLock<HashMap<String, ReRegisterFn>>,
    transport: MeshTransportKind,
    scope: MeshScopeId,
    priority: u32,
) {
    swarm.add_peer_address(peer_id, remote_addr.clone());

    let was_alive = routes.is_peer_alive(&peer_id);
    let route = routes.upsert_addrs(peer_id, transport, scope, [remote_addr.clone()], priority);
    let _ = peer_events_tx.send(PeerEvent::RouteAdded { peer_id, route });

    let is_new = {
        let mut peers = known_peers.write();
        let entry = peers.entry(peer_id).or_default();
        let was_empty = entry.is_empty();
        entry.insert(remote_addr.clone());
        was_empty
    };

    if !was_alive || is_new {
        log::info!("Connected to peer: {peer_id} at {remote_addr}");
        let _ = peer_events_tx.send(PeerEvent::Discovered(peer_id));

        let fns: Vec<ReRegisterFn> = re_register_fns.read().values().cloned().collect();
        if !fns.is_empty() {
            tokio::spawn(async move {
                for f in &fns {
                    f().await;
                }
            });
        }
    } else {
        log::debug!("Additional connection to already-known peer {peer_id} at {remote_addr}");
    }
}

/// Handle a `ConnectionClosed` swarm event.
///
/// When the last connection to a peer closes, removes it from `known_peers`
/// and fires `PeerEvent::Expired`.
fn handle_connection_closed(
    peer_id: PeerId,
    num_established: u32,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    if num_established == 0 {
        let removed_routes = routes.routes_for_peer(&peer_id);
        routes.remove_peer(&peer_id);
        for route in removed_routes {
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        let was_known = {
            let mut peers = known_peers.write();
            peers.remove(&peer_id).is_some()
        };
        if was_known {
            log::info!("Peer disconnected (no remaining connections): {peer_id}");
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        }
    }
}

#[cfg(feature = "remote-internet")]
fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    use libp2p::multiaddr::Protocol;

    addr.iter().find_map(|p| match p {
        Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

#[cfg(feature = "remote-internet")]
fn admitted_peer_ids_for_local_mesh(
    store: &super::invite::InviteStore,
    local_peer_id: &PeerId,
    local_mesh_id: Option<&str>,
) -> Vec<PeerId> {
    let local_peer = local_peer_id.to_string();
    let local_mesh_prefix = format!("{}:", local_peer);

    store
        .admitted_memberships()
        .filter_map(|(_peer_id, token)| {
            if token.admitted_by != local_peer {
                return None;
            }
            if let Some(mesh_id) = local_mesh_id {
                if token.mesh_id != mesh_id {
                    return None;
                }
            } else if !token.mesh_id.starts_with(&local_mesh_prefix) {
                return None;
            }
            token.peer_id.parse::<PeerId>().ok()
        })
        .collect()
}

#[cfg(feature = "remote-internet")]
fn reconnect_backoff_duration(attempt: u32) -> std::time::Duration {
    let secs = (1u64 << attempt.min(5)).min(30);
    std::time::Duration::from_secs(secs)
}

fn log_kameo_messaging_event(event: &remote::messaging::Event) {
    match event {
        remote::messaging::Event::AskResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(_) => tracing::debug!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                "kameo ask completed"
            ),
            Err(error) => tracing::warn!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                error = ?error,
                "kameo ask failed"
            ),
        },
        remote::messaging::Event::TellResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(()) => tracing::debug!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                "kameo tell acknowledged"
            ),
            Err(error) => tracing::warn!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                error = %error,
                "kameo tell failed"
            ),
        },
        remote::messaging::Event::LinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo link result"
        ),
        remote::messaging::Event::UnlinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo unlink result"
        ),
        remote::messaging::Event::SignalLinkDiedResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo signal_link_died result"
        ),
        remote::messaging::Event::OutboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => tracing::warn!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            error = %error,
            "kameo outbound failure"
        ),
        remote::messaging::Event::InboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => tracing::warn!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            error = ?error,
            "kameo inbound failure"
        ),
        remote::messaging::Event::ResponseSent {
            peer,
            connection_id,
            request_id,
        } => tracing::trace!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            "kameo response sent"
        ),
    }
}

/// Shared pre-bootstrap setup: load identity, validate peers, create channels.
struct MeshBootstrapContext {
    keypair: libp2p::identity::Keypair,
    peer_events_tx: broadcast::Sender<MeshEvent>,
    routes: Arc<RouteTable>,
    known_peers: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
    re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    local_hostname: String,
}

fn prepare_bootstrap(config: &MeshConfig) -> Result<MeshBootstrapContext, MeshError> {
    let keypair = super::identity::load_or_generate_keypair(config.identity_file.as_deref())
        .map_err(|e| MeshError::SwarmError(format!("failed to load mesh identity: {e}")))?;

    for peer_addr in &config.bootstrap_peers {
        peer_addr
            .parse::<libp2p::Multiaddr>()
            .map_err(|e| MeshError::InvalidBootstrapAddr {
                addr: peer_addr.clone(),
                reason: e.to_string(),
            })?;
    }

    let (peer_events_tx, _) = broadcast::channel::<MeshEvent>(64);
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let known_peers = Arc::new(RwLock::new(HashMap::new()));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let local_hostname = resolve_local_hostname();

    Ok(MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    })
}

fn finalize_bootstrap(
    local_peer_id: PeerId,
    ctx: MeshBootstrapContext,
    listen_label: &str,
    transport_mode: MeshTransportMode,
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    stream_reconnect_grace: std::time::Duration,
) -> MeshHandle {
    log::info!(
        "Kameo mesh bootstrapped: peer_id={}, listen={}",
        local_peer_id,
        listen_label,
    );

    // Try to load or create the invite store at the default path.
    let invite_store = match super::invite::default_invite_store_path() {
        Ok(path) => match super::invite::InviteStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!("Failed to load invite store: {e}; invites will not be persisted");
                None
            }
        },
        Err(e) => {
            log::warn!("Cannot determine invite store path: {e}; invites will not be persisted");
            None
        }
    };

    // Try to load or create the membership store at the default path.
    let membership_store = match super::invite::default_membership_store_path() {
        Ok(path) => match super::invite::MembershipStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!(
                    "Failed to load membership store: {e}; reconnection tokens will not be persisted"
                );
                None
            }
        },
        Err(e) => {
            log::warn!(
                "Cannot determine membership store path: {e}; reconnection tokens will not be persisted"
            );
            None
        }
    };

    MeshHandle::new(
        local_peer_id,
        ctx.peer_events_tx,
        ctx.routes,
        ctx.local_hostname,
        ctx.re_register_fns,
        ctx.keypair,
        invite_store,
        membership_store,
        transport_mode,
        swarm_cmd_tx,
        stream_reconnect_grace,
    )
}

// ── LAN mesh (TCP + QUIC + mDNS) ──────────────────────────────────────────────

async fn bootstrap_lan_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::{
        SwarmBuilder, mdns, noise,
        swarm::{NetworkBehaviour, SwarmEvent},
        tcp, yamux,
    };

    let ctx = prepare_bootstrap(config)?;
    let listen_addr = config.listen.as_deref().unwrap_or("/ip4/0.0.0.0/tcp/0");

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    // ── Build the libp2p swarm ────────────────────────────────────────────────
    // We replicate exactly what kameo's bootstrap_on() does, but own the event
    // loop so we can emit PeerEvents on mDNS discovery / expiry.

    #[derive(NetworkBehaviour)]
    struct MeshBehaviour {
        kameo: remote::Behaviour,
        mdns: mdns::tokio::Behaviour,
    }

    let mut swarm = SwarmBuilder::with_existing_identity(ctx.keypair.clone())
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
            let kameo_behaviour = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            );
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
        .with_swarm_config(|c| {
            // Keep connections alive for 5 minutes between prompts.
            // libp2p's default is 60 s which causes the TCP connection to drop
            // between prompts, leaving Kademlia with no route and returning
            // NotFound on the next lookup.
            c.with_idle_connection_timeout(std::time::Duration::from_secs(300))
        })
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

    // ── Dial explicit bootstrap peers ─────────────────────────────────────────
    // These are already validated above so the parse() cannot fail.
    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    let local_peer_id = *swarm.local_peer_id();

    let (swarm_cmd_tx_lan, _swarm_cmd_rx_lan) = mpsc::unbounded_channel::<SwarmCommand>();

    // ── Swarm event loop ──────────────────────────────────────────────────────
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(MeshBehaviourEvent::Kameo(remote::Event::Messaging(
                    event,
                ))) => {
                    log_kameo_messaging_event(&event);
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    handle_mdns_discovered(
                        &mut swarm,
                        list,
                        &known_peers_loop,
                        &routes_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                    );
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    handle_mdns_expired(
                        &mut swarm,
                        list,
                        &known_peers_loop,
                        &routes_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                        MeshTransportKind::Lan,
                        MeshScopeId::lan_default(),
                        100,
                    );
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } => {
                    handle_connection_closed(
                        peer_id,
                        num_established,
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    log::warn!("Outgoing connection error (peer={:?}): {}", peer_id, error);
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("ActorSwarm listening on {address}");
                }
                _ => {}
            }
        }
    });

    if matches!(&config.discovery, MeshDiscovery::Kademlia { bootstrap } if !bootstrap.is_empty())
        && let MeshDiscovery::Kademlia { bootstrap } = &config.discovery
    {
        for addr in bootstrap {
            log::info!("Kademlia bootstrap peer: {}", addr);
        }
    }

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        listen_addr,
        MeshTransportMode::Lan,
        swarm_cmd_tx_lan,
        config.stream_reconnect_grace,
    ))
}

// ── Iroh mesh (QUIC + relay, NAT traversal) ────────────────────────────────────

#[cfg(feature = "remote-internet")]
async fn bootstrap_iroh_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::swarm::{NetworkBehaviour, Swarm, SwarmEvent};

    let ctx = prepare_bootstrap(config)?;

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    let local_mesh_id = config.invite.as_ref().map(|invite| {
        super::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        )
    });

    // Load the membership store now so the event loop can update cached peers
    // whenever new peers are discovered.  Failures are non-fatal.
    let membership_store_loop: Option<Arc<RwLock<super::invite::MembershipStore>>> =
        super::invite::default_membership_store_path()
            .ok()
            .and_then(|p| super::invite::MembershipStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));
    let invite_store_loop: Option<Arc<RwLock<super::invite::InviteStore>>> =
        super::invite::default_invite_store_path()
            .ok()
            .and_then(|p| super::invite::InviteStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));

    // ── Build the iroh-backed libp2p transport ─────────────────────────────────

    // v2.5: no transport-level peer_filter.
    //
    // Signed invite grants (v2.5) verify the inviter's identity client-side
    // via ed25519 signature.  iroh already provides TLS-authenticated,
    // encrypted connections — each peer's identity is verified via its
    // ed25519 key.
    //
    // Post-connect grant verification (challenge-response handshake) is a
    // future v3 enhancement.
    let iroh_config = libp2p_iroh::TransportConfig {
        timeout: config.request_timeout,
        ..Default::default()
    };

    let transport = libp2p_iroh::Transport::with_config(Some(&ctx.keypair), iroh_config)
        .await
        .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

    let local_peer_id = transport.peer_id;

    // ── Build behaviour (kameo only — no mDNS, iroh handles connectivity) ──────

    #[derive(NetworkBehaviour)]
    struct IrohMeshBehaviour {
        kameo: remote::Behaviour,
    }

    let kameo_behaviour = remote::Behaviour::new(
        local_peer_id,
        remote::messaging::Config::default()
            .with_request_timeout(config.request_timeout)
            .with_response_size_maximum(50 * 1024 * 1024),
    );

    let behaviour = IrohMeshBehaviour {
        kameo: kameo_behaviour,
    };

    let mut swarm = Swarm::new(
        libp2p::Transport::boxed(transport),
        behaviour,
        local_peer_id,
        libp2p::swarm::Config::with_executor(Box::new(|fut| {
            tokio::spawn(fut);
        }))
        .with_idle_connection_timeout(std::time::Duration::from_secs(300)),
    );

    // Register the kameo behaviour as the global ActorSwarm.
    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    // For iroh, listen on Multiaddr::empty() — iroh manages its own listener.
    swarm
        .listen_on(Multiaddr::empty())
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    // ── Dial the inviter or explicit bootstrap peers ──────────────────────────

    // If we have an invite, dial the inviter first — this is the entry point
    // into the mesh.  The inviter's PeerId is resolved by iroh's relay.
    if let Some(ref invite) = config.invite {
        let inviter_addr: Multiaddr = format!("/p2p/{}", invite.grant.inviter_peer_id)
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| {
                MeshError::SwarmError(format!(
                    "invalid inviter PeerId '{}': {}",
                    invite.grant.inviter_peer_id, e
                ))
            })?;
        match swarm.dial(inviter_addr.clone()) {
            Ok(_) => log::info!(
                "Dialing inviter via iroh relay: {} (mesh: {:?})",
                inviter_addr,
                invite.grant.mesh_name
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    // Also dial any explicitly configured bootstrap peers.
    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated in prepare_bootstrap");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer (iroh): {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();

    if let Some(ref invite) = config.invite
        && let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>()
    {
        reconnect_targets.insert(inviter_pid);
    }

    for peer_addr in &config.bootstrap_peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = membership_store_loop {
        let store = ms.read();
        if let Some(mesh_id) = local_mesh_id.as_deref() {
            if let Some(membership) = store.get_membership(mesh_id) {
                for peer in &membership.known_peers {
                    if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                        reconnect_targets.insert(pid);
                    }
                }
            }
        } else {
            for (_, membership) in store.all() {
                for peer in &membership.known_peers {
                    if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                        reconnect_targets.insert(pid);
                    }
                }
            }
        }
    }

    if let Some(ref is) = invite_store_loop {
        let store = is.read();
        for pid in
            admitted_peer_ids_for_local_mesh(&store, &local_peer_id, local_mesh_id.as_deref())
        {
            reconnect_targets.insert(pid);
        }
    }

    reconnect_targets.remove(&local_peer_id);

    let (swarm_cmd_tx_iroh, mut swarm_cmd_rx_iroh) = mpsc::unbounded_channel::<SwarmCommand>();

    // ── Swarm event loop ──────────────────────────────────────────────────────
    // Simpler than LAN: no mDNS events, just connection lifecycle + kameo.
    tokio::spawn(async move {
        // Track pending dials to avoid redundant connection attempts.
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                _ = reconnect_tick.tick() => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if routes_loop.is_peer_alive(&peer_id) || pending_dials.contains(&peer_id) {
                            continue;
                        }
                        if reconnect_next_due
                            .get(&peer_id)
                            .is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}")
                            .parse()
                            .expect("PeerId always produces a valid /p2p/ multiaddr");
                        match swarm.dial(addr) {
                            Ok(_) => {
                                log::debug!("Reconnect dial (iroh): {}", peer_id);
                                pending_dials.insert(peer_id);
                            }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!(
                                    "Reconnect dial failed (iroh, peer={}, attempt={}): {}",
                                    peer_id,
                                    *attempt,
                                    e
                                );
                            }
                        }
                    }
                }
                Some(cmd) = swarm_cmd_rx_iroh.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer(peer_id) => {
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                log::debug!("Skipping dial for {} (already connected or pending)", peer_id);
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}")
                                .parse()
                                .expect("PeerId always produces a valid /p2p/ multiaddr");
                            match swarm.dial(addr) {
                                Ok(_) => {
                                    log::info!("Dialing peer (iroh): {}", peer_id);
                                    pending_dials.insert(peer_id);
                                }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                                    );
                                    log::warn!("Failed to dial peer {} (iroh): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::LeaveIrohScope { .. } => {
                            // Single-scope iroh bootstrap does not track per-scope reconnect sets.
                        }
                    }
                }
                event = swarm.select_next_some() => {
                match event {
                SwarmEvent::Behaviour(IrohMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                    log_kameo_messaging_event(&event);
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    pending_dials.remove(&peer_id);
                    reconnect_targets.insert(peer_id);
                    reconnect_attempts.remove(&peer_id);
                    reconnect_next_due.remove(&peer_id);

                    let (scope, priority) = if let Some(mesh_id) = local_mesh_id.clone() {
                        (MeshScopeId::Iroh { mesh_id }, 70)
                    } else {
                        // No local invite-derived mesh_id available (legacy path).
                        // Avoid emitting malformed `Iroh { mesh_id: "" }` scopes.
                        (MeshScopeId::lan_default(), 100)
                    };
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                        MeshTransportKind::Iroh,
                        scope,
                        priority,
                    );

                    refresh_membership_known_peers(&membership_store_loop, &routes_loop);
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } => {
                    reconnect_targets.insert(peer_id);
                    reconnect_next_due.remove(&peer_id);
                    handle_connection_closed(
                        peer_id,
                        num_established,
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    if let Some(pid) = peer_id {
                        pending_dials.remove(&pid);
                        reconnect_targets.insert(pid);
                        let attempt = reconnect_attempts.entry(pid).or_insert(0);
                        *attempt = attempt.saturating_add(1);
                        reconnect_next_due.insert(
                            pid,
                            tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                        );
                    }
                    log::warn!(
                        "Outgoing connection error (iroh, peer={:?}): {}",
                        peer_id,
                        error
                    );
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("ActorSwarm listening on {} (iroh)", address);
                }
                _ => {}
            } // match event
            } // select: event arm
            } // tokio::select!
        }
    });

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        "iroh-relay",
        MeshTransportMode::Iroh,
        swarm_cmd_tx_iroh,
        config.stream_reconnect_grace,
    ))
}

// ── Composite mesh (LAN TCP/QUIC/mDNS + Iroh in one swarm) ─────────────────────
//
// Phase 0 spike: proves that LAN and Iroh transports can coexist in a single
// libp2p Swarm with a single kameo `try_init_global()`.
//
// Architecture:
//   SwarmBuilder
//     .with_tcp()          ← LAN TCP + Noise + Yamux
//     .with_quic()         ← LAN QUIC
//     .with_other_transport(iroh)  ← Iroh QUIC/relay
//     .with_behaviour(CompositeMeshBehaviour { kameo, mdns })
//
// The mDNS behaviour is wrapped in `Toggle` so it can be enabled/disabled
// based on config without requiring separate struct definitions.

#[cfg(feature = "remote-internet")]
async fn bootstrap_composite_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::{
        SwarmBuilder, mdns, noise,
        swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle},
        tcp, yamux,
    };

    let ctx = prepare_bootstrap(config)?;
    let listen_addr = config.listen.as_deref().unwrap_or("/ip4/0.0.0.0/tcp/0");

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    // Create the iroh transport *before* SwarmBuilder (it's async).
    // We borrow the keypair; ownership moves to SwarmBuilder below.
    let iroh_config = libp2p_iroh::TransportConfig {
        timeout: config.request_timeout,
        ..Default::default()
    };
    let iroh_transport = libp2p_iroh::Transport::with_config(Some(&ctx.keypair), iroh_config)
        .await
        .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

    let enable_mdns = matches!(config.discovery, MeshDiscovery::Mdns);

    // Composite behaviour: kameo registry + optional mDNS.
    #[derive(NetworkBehaviour)]
    struct CompositeMeshBehaviour {
        kameo: remote::Behaviour,
        mdns: Toggle<mdns::tokio::Behaviour>,
    }

    let mut swarm = SwarmBuilder::with_existing_identity(ctx.keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| MeshError::SwarmError(e.to_string()))?
        .with_quic()
        .with_other_transport(move |_| iroh_transport)
        .map_err(|e| -> MeshError { match e {} })?
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();
            let kameo_behaviour = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            );

            let mdns_behaviour = if enable_mdns {
                let mdns_config = mdns::Config {
                    ttl: std::time::Duration::from_secs(30),
                    query_interval: std::time::Duration::from_secs(15),
                    ..mdns::Config::default()
                };
                Some(mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?)
            } else {
                None
            };

            Ok(CompositeMeshBehaviour {
                kameo: kameo_behaviour,
                mdns: mdns_behaviour.into(),
            })
        })
        .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(300)))
        .build();

    // Register the kameo behaviour as the global ActorSwarm — exactly once.
    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    // Listen on LAN address (TCP/QUIC).
    swarm
        .listen_on(listen_addr.parse().map_err(|e: libp2p::multiaddr::Error| {
            MeshError::InvalidListenAddr {
                addr: listen_addr.to_string(),
                reason: e.to_string(),
            }
        })?)
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    // Listen on empty multiaddr for iroh (iroh manages its own listener).
    swarm
        .listen_on(Multiaddr::empty())
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    // ── Dial explicit bootstrap peers (LAN full multiaddrs) ──────────────────
    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    // ── Dial the iroh inviter ────────────────────────────────────────────────
    if let Some(ref invite) = config.invite {
        let inviter_addr: Multiaddr = format!("/p2p/{}", invite.grant.inviter_peer_id)
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| {
                MeshError::SwarmError(format!(
                    "invalid inviter PeerId '{}': {}",
                    invite.grant.inviter_peer_id, e
                ))
            })?;
        match swarm.dial(inviter_addr.clone()) {
            Ok(_) => log::info!(
                "Dialing inviter via iroh relay: {} (mesh: {:?})",
                inviter_addr,
                invite.grant.mesh_name
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    let local_peer_id = *swarm.local_peer_id();

    // ── Load membership / invite stores for iroh reconnect ───────────────────
    let local_mesh_id = config.invite.as_ref().map(|invite| {
        super::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        )
    });

    let membership_store_loop: Option<Arc<RwLock<super::invite::MembershipStore>>> =
        super::invite::default_membership_store_path()
            .ok()
            .and_then(|p| super::invite::MembershipStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));

    let invite_store_loop: Option<Arc<RwLock<super::invite::InviteStore>>> =
        super::invite::default_invite_store_path()
            .ok()
            .and_then(|p| super::invite::InviteStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));

    // Build reconnect targets from invite, bootstrap peers, membership, and admitted peers.
    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();

    if let Some(ref invite) = config.invite
        && let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>()
    {
        reconnect_targets.insert(inviter_pid);
    }

    for peer_addr in &config.bootstrap_peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = membership_store_loop {
        let store = ms.read();
        if let Some(mesh_id) = local_mesh_id.as_deref() {
            if let Some(membership) = store.get_membership(mesh_id) {
                for peer in &membership.known_peers {
                    if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                        reconnect_targets.insert(pid);
                    }
                }
            }
        } else {
            for (_, membership) in store.all() {
                for peer in &membership.known_peers {
                    if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                        reconnect_targets.insert(pid);
                    }
                }
            }
        }
    }

    if let Some(ref is) = invite_store_loop {
        let store = is.read();
        for pid in
            admitted_peer_ids_for_local_mesh(&store, &local_peer_id, local_mesh_id.as_deref())
        {
            reconnect_targets.insert(pid);
        }
    }

    reconnect_targets.remove(&local_peer_id);

    let (swarm_cmd_tx, mut swarm_cmd_rx) = mpsc::unbounded_channel::<SwarmCommand>();

    // ── Unified swarm event loop ─────────────────────────────────────────────
    // Handles mDNS events (when LAN enabled), connection lifecycle, kameo
    // messaging, and iroh reconnect logic — all in one loop.
    tokio::spawn(async move {
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                _ = reconnect_tick.tick() => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if routes_loop.is_peer_alive(&peer_id) || pending_dials.contains(&peer_id) {
                            continue;
                        }
                        if reconnect_next_due
                            .get(&peer_id)
                            .is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}")
                            .parse()
                            .expect("PeerId always produces a valid /p2p/ multiaddr");
                        match swarm.dial(addr) {
                            Ok(_) => {
                                log::debug!("Reconnect dial (composite): {}", peer_id);
                                pending_dials.insert(peer_id);
                            }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!(
                                    "Reconnect dial failed (composite, peer={}, attempt={}): {}",
                                    peer_id,
                                    *attempt,
                                    e
                                );
                            }
                        }
                    }
                }
                Some(cmd) = swarm_cmd_rx.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer(peer_id) => {
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                log::debug!("Skipping dial for {} (already connected or pending)", peer_id);
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}")
                                .parse()
                                .expect("PeerId always produces a valid /p2p/ multiaddr");
                            match swarm.dial(addr) {
                                Ok(_) => {
                                    log::info!("Dialing peer (composite): {}", peer_id);
                                    pending_dials.insert(peer_id);
                                }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                                    );
                                    log::warn!("Failed to dial peer {} (composite): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::LeaveIrohScope { .. } => {
                            // Legacy composite bootstrap keeps a flat reconnect set.
                        }
                    }
                }
                event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                        log_kameo_messaging_event(&event);
                    }
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        handle_mdns_discovered(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                            &re_register_fns_loop,
                        );
                    }
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        handle_mdns_expired(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        pending_dials.remove(&peer_id);
                        reconnect_targets.insert(peer_id);
                        reconnect_attempts.remove(&peer_id);
                        reconnect_next_due.remove(&peer_id);

                        handle_connection_established(
                            &mut swarm,
                            peer_id,
                            endpoint.get_remote_address().clone(),
                            &routes_loop,
                            &known_peers_loop,
                            &peer_events_tx_loop,
                            &re_register_fns_loop,
                            MeshTransportKind::Lan,
                            MeshScopeId::lan_default(),
                            100,
                        );

                        refresh_membership_known_peers(&membership_store_loop, &routes_loop);
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        ..
                    } => {
                        reconnect_targets.insert(peer_id);
                        reconnect_next_due.remove(&peer_id);
                        handle_connection_closed(
                            peer_id,
                            num_established,
                            &routes_loop,
                            &known_peers_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(pid) = peer_id {
                            pending_dials.remove(&pid);
                            reconnect_targets.insert(pid);
                            let attempt = reconnect_attempts.entry(pid).or_insert(0);
                            *attempt = attempt.saturating_add(1);
                            reconnect_next_due.insert(
                                pid,
                                tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                            );
                        }
                        log::warn!(
                            "Outgoing connection error (composite, peer={:?}): {}",
                            peer_id,
                            error
                        );
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        log::info!("ActorSwarm listening on {address} (composite)");
                    }
                    _ => {}
                } // match event
                } // select: event arm
            } // tokio::select!
        }
    });

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        listen_addr,
        MeshTransportMode::Lan, // TODO: Phase 2 introduces composite mode
        swarm_cmd_tx,
        config.stream_reconnect_grace,
    ))
}

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
    use super::mesh_runtime_config::LanDiscovery;
    use futures_util::StreamExt as _;
    use libp2p::swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle};

    // ── Determine enabled transports ────────────────────────────────────────
    let has_lan = config.has_lan();
    let has_iroh = config.has_iroh();

    if !has_lan && !has_iroh {
        return Err(MeshError::SwarmError(
            "no transport enabled in MeshRuntimeConfig".to_string(),
        ));
    }

    if has_iroh {
        #[cfg(not(feature = "remote-internet"))]
        {
            return Err(MeshError::SwarmError(
                "iroh transport requires the 'remote-internet' cargo feature".to_string(),
            ));
        }
    }

    let transport_mode = match (has_lan, has_iroh) {
        (true, false) => MeshTransportMode::Lan,
        (false, true) => MeshTransportMode::Iroh,
        (true, true) => MeshTransportMode::Composite,
        _ => unreachable!(),
    };

    // ── Load identity ───────────────────────────────────────────────────────
    let keypair = super::identity::load_or_generate_keypair(config.identity_file.as_deref())
        .map_err(|e| MeshError::SwarmError(format!("failed to load mesh identity: {e}")))?;

    for peer_addr in &config.peers {
        peer_addr
            .parse::<libp2p::Multiaddr>()
            .map_err(|e| MeshError::InvalidBootstrapAddr {
                addr: peer_addr.clone(),
                reason: e.to_string(),
            })?;
    }

    let (peer_events_tx, _) = broadcast::channel::<PeerEvent>(32);
    let known_peers = Arc::new(RwLock::new(HashMap::<PeerId, HashSet<Multiaddr>>::new()));
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let local_hostname = resolve_local_hostname();

    let peer_events_tx_loop = peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&known_peers);
    let routes_loop = Arc::clone(&routes);
    let re_register_fns_loop = Arc::clone(&re_register_fns);

    let enable_mdns = has_lan
        && config
            .lan
            .as_ref()
            .is_some_and(|l| matches!(l.discovery, LanDiscovery::Mdns));

    let lan_listen_addr = config
        .lan
        .as_ref()
        .and_then(|l| l.listen.as_deref())
        .unwrap_or("/ip4/0.0.0.0/tcp/0");

    // ── Parse iroh invites ──────────────────────────────────────────────────
    let iroh_invites: Vec<(
        super::invite::SignedInviteGrant,
        String, // mesh_id
    )> = if has_iroh {
        let mut invites = Vec::new();
        for scope in &config.iroh_scopes {
            if let Some(ref invite_str) = scope.invite {
                let grant = super::invite::SignedInviteGrant::decode(invite_str).map_err(|e| {
                    MeshError::SwarmError(format!(
                        "invalid invite for scope '{}': {e}",
                        scope.mesh_id
                    ))
                })?;
                invites.push((grant, scope.mesh_id.clone()));
            }
        }
        invites
    } else {
        Vec::new()
    };

    // ── Load membership / invite stores for iroh reconnect ──────────────────
    let membership_store_loop: Option<Arc<RwLock<super::invite::MembershipStore>>> = if has_iroh {
        super::invite::default_membership_store_path()
            .ok()
            .and_then(|p| super::invite::MembershipStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)))
    } else {
        None
    };

    let invite_store_loop: Option<Arc<RwLock<super::invite::InviteStore>>> = if has_iroh {
        super::invite::default_invite_store_path()
            .ok()
            .and_then(|p| super::invite::InviteStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)))
    } else {
        None
    };

    // ── Build composite behaviour (used by all transport configs) ────────────
    #[derive(NetworkBehaviour)]
    struct UnifiedMeshBehaviour {
        kameo: remote::Behaviour,
        mdns: Toggle<libp2p::mdns::tokio::Behaviour>,
    }

    // ── Build the swarm ─────────────────────────────────────────────────────
    // Three builder paths produce the same Swarm<UnifiedMeshBehaviour> type.

    let mut swarm: libp2p::Swarm<UnifiedMeshBehaviour> = if has_lan && has_iroh {
        // ── Full composite: TCP + QUIC + Iroh ───────────────────────────────
        #[cfg(feature = "remote-internet")]
        {
            let iroh_config = libp2p_iroh::TransportConfig {
                timeout: config.request_timeout,
                ..Default::default()
            };
            let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
                .await
                .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

            libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
                .with_tokio()
                .with_tcp(
                    libp2p::tcp::Config::default(),
                    libp2p::noise::Config::new,
                    libp2p::yamux::Config::default,
                )
                .map_err(|e| MeshError::SwarmError(e.to_string()))?
                .with_quic()
                .with_other_transport(move |_| iroh_transport)
                .map_err(|e: std::convert::Infallible| -> MeshError { match e {} })?
                .with_behaviour(|key| {
                    let local_peer_id = key.public().to_peer_id();
                    let kameo_behaviour = remote::Behaviour::new(
                        local_peer_id,
                        remote::messaging::Config::default()
                            .with_request_timeout(config.request_timeout)
                            .with_response_size_maximum(50 * 1024 * 1024),
                    );
                    let mdns_behaviour = if enable_mdns {
                        let mdns_config = libp2p::mdns::Config {
                            ttl: std::time::Duration::from_secs(30),
                            query_interval: std::time::Duration::from_secs(15),
                            ..libp2p::mdns::Config::default()
                        };
                        Some(libp2p::mdns::tokio::Behaviour::new(
                            mdns_config,
                            local_peer_id,
                        )?)
                    } else {
                        None
                    };
                    Ok(UnifiedMeshBehaviour {
                        kameo: kameo_behaviour,
                        mdns: mdns_behaviour.into(),
                    })
                })
                .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
                .with_swarm_config(|c| {
                    c.with_idle_connection_timeout(std::time::Duration::from_secs(300))
                })
                .build()
        }
        #[cfg(not(feature = "remote-internet"))]
        {
            unreachable!("guarded above")
        }
    } else if has_lan {
        // ── LAN only: TCP + QUIC + optional mDNS ────────────────────────────
        libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .map_err(|e| MeshError::SwarmError(e.to_string()))?
            .with_quic()
            .with_behaviour(|key| {
                let local_peer_id = key.public().to_peer_id();
                let kameo_behaviour = remote::Behaviour::new(
                    local_peer_id,
                    remote::messaging::Config::default()
                        .with_request_timeout(config.request_timeout)
                        .with_response_size_maximum(50 * 1024 * 1024),
                );
                let mdns_behaviour = if enable_mdns {
                    let mdns_config = libp2p::mdns::Config {
                        ttl: std::time::Duration::from_secs(30),
                        query_interval: std::time::Duration::from_secs(15),
                        ..libp2p::mdns::Config::default()
                    };
                    Some(libp2p::mdns::tokio::Behaviour::new(
                        mdns_config,
                        local_peer_id,
                    )?)
                } else {
                    None
                };
                Ok(UnifiedMeshBehaviour {
                    kameo: kameo_behaviour,
                    mdns: mdns_behaviour.into(),
                })
            })
            .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(std::time::Duration::from_secs(300))
            })
            .build()
    } else {
        // ── Iroh only ───────────────────────────────────────────────────────
        #[cfg(feature = "remote-internet")]
        {
            let iroh_config = libp2p_iroh::TransportConfig {
                timeout: config.request_timeout,
                ..Default::default()
            };
            let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
                .await
                .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

            let local_peer_id = iroh_transport.peer_id;

            let kameo_behaviour = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            );

            let behaviour = UnifiedMeshBehaviour {
                kameo: kameo_behaviour,
                mdns: None.into(),
            };

            libp2p::Swarm::new(
                libp2p::Transport::boxed(iroh_transport),
                behaviour,
                local_peer_id,
                libp2p::swarm::Config::with_executor(Box::new(|fut| {
                    tokio::spawn(fut);
                }))
                .with_idle_connection_timeout(std::time::Duration::from_secs(300)),
            )
        }
        #[cfg(not(feature = "remote-internet"))]
        {
            unreachable!("guarded above")
        }
    };

    // ── Register the kameo behaviour as the global ActorSwarm — once ────────
    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let local_peer_id = *swarm.local_peer_id();

    // ── Listen on enabled transports ────────────────────────────────────────
    if has_lan {
        swarm
            .listen_on(
                lan_listen_addr
                    .parse()
                    .map_err(|e: libp2p::multiaddr::Error| MeshError::InvalidListenAddr {
                        addr: lan_listen_addr.to_string(),
                        reason: e.to_string(),
                    })?,
            )
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;
    }

    if has_iroh {
        swarm
            .listen_on(Multiaddr::empty())
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;
    }

    // ── Dial explicit peers (LAN full multiaddrs) ──────────────────────────
    for peer_addr in &config.peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    // ── Dial iroh inviters ──────────────────────────────────────────────────
    for (invite, mesh_id) in &iroh_invites {
        let inviter_addr: Multiaddr = format!("/p2p/{}", invite.grant.inviter_peer_id)
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| {
                MeshError::SwarmError(format!(
                    "invalid inviter PeerId '{}': {}",
                    invite.grant.inviter_peer_id, e
                ))
            })?;
        match swarm.dial(inviter_addr.clone()) {
            Ok(_) => log::info!(
                "Dialing inviter via iroh relay: {} (mesh: {})",
                inviter_addr,
                mesh_id,
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    // ── Build reconnect targets for iroh peers ─────────────────────────────
    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();
    let mut reconnect_targets_by_scope: HashMap<String, HashSet<PeerId>> = HashMap::new();

    for (invite, mesh_id) in &iroh_invites {
        if let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>() {
            reconnect_targets.insert(inviter_pid);
            reconnect_targets_by_scope
                .entry(mesh_id.clone())
                .or_default()
                .insert(inviter_pid);
        }
    }

    for peer_addr in &config.peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = membership_store_loop {
        let store = ms.read();
        for (mesh_id, membership) in store.all() {
            for peer in &membership.known_peers {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                    reconnect_targets_by_scope
                        .entry(mesh_id.to_string())
                        .or_default()
                        .insert(pid);
                }
            }
        }
    }

    if let Some(ref is) = invite_store_loop {
        let store = is.read();
        // Collect admitted peer IDs across all iroh scope mesh_ids
        let mesh_ids: Vec<String> = config
            .iroh_scopes
            .iter()
            .map(|s| s.mesh_id.clone())
            .collect();
        for mesh_id in &mesh_ids {
            for pid in admitted_peer_ids_for_local_mesh(&store, &local_peer_id, Some(mesh_id)) {
                reconnect_targets.insert(pid);
                reconnect_targets_by_scope
                    .entry(mesh_id.clone())
                    .or_default()
                    .insert(pid);
            }
        }
    }

    reconnect_targets.remove(&local_peer_id);

    // Build a reverse lookup: peer_id → MeshScopeId so the connection
    // established handler can tag routes with the correct scope.
    let peer_iroh_scope: HashMap<PeerId, MeshScopeId> = reconnect_targets_by_scope
        .iter()
        .flat_map(|(mesh_id, pids)| {
            pids.iter().map(move |pid| {
                (
                    *pid,
                    MeshScopeId::Iroh {
                        mesh_id: mesh_id.clone(),
                    },
                )
            })
        })
        .collect();

    let (swarm_cmd_tx, mut swarm_cmd_rx) = mpsc::unbounded_channel::<SwarmCommand>();

    // ── Unified swarm event loop ────────────────────────────────────────────
    // Handles mDNS events (when LAN enabled), connection lifecycle, kameo
    // messaging, and iroh reconnect logic — all in one loop.
    let has_lan_loop = has_lan;
    let has_iroh_loop = has_iroh;
    let peer_iroh_scope_loop = peer_iroh_scope;
    tokio::spawn(async move {
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                // ── Reconnect tick (iroh only) ────────────────────────────────
                _ = reconnect_tick.tick(), if has_iroh_loop => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if routes_loop.is_peer_alive(&peer_id) || pending_dials.contains(&peer_id) {
                            continue;
                        }
                        if reconnect_next_due
                            .get(&peer_id)
                            .is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}")
                            .parse()
                            .expect("PeerId always produces a valid /p2p/ multiaddr");
                        match swarm.dial(addr) {
                            Ok(_) => {
                                log::debug!("Reconnect dial (unified): {}", peer_id);
                                pending_dials.insert(peer_id);
                            }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!(
                                    "Reconnect dial failed (unified, peer={}, attempt={}): {}",
                                    peer_id,
                                    *attempt,
                                    e
                                );
                            }
                        }
                    }
                }
                // ── Swarm command ─────────────────────────────────────────────
                Some(cmd) = swarm_cmd_rx.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer(peer_id) => {
                            if !has_iroh_loop {
                                log::debug!("dial_peer command ignored (no iroh transport)");
                                continue;
                            }
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                log::debug!("Skipping dial for {} (already connected or pending)", peer_id);
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}")
                                .parse()
                                .expect("PeerId always produces a valid /p2p/ multiaddr");
                            match swarm.dial(addr) {
                                Ok(_) => {
                                    log::info!("Dialing peer (unified): {}", peer_id);
                                    pending_dials.insert(peer_id);
                                }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                                    );
                                    log::warn!("Failed to dial peer {} (unified): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::LeaveIrohScope { mesh_id } => {
                            if let Some(peers) = reconnect_targets_by_scope.remove(&mesh_id) {
                                for pid in peers {
                                    reconnect_targets.remove(&pid);
                                    pending_dials.remove(&pid);
                                    reconnect_attempts.remove(&pid);
                                    reconnect_next_due.remove(&pid);
                                }
                            }
                        }
                    }
                }
                // ── Swarm events ──────────────────────────────────────────────
                event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                        log_kameo_messaging_event(&event);
                    }
                    SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Mdns(libp2p::mdns::Event::Discovered(list))) => {
                        handle_mdns_discovered(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                            &re_register_fns_loop,
                        );
                    }
                    SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Mdns(libp2p::mdns::Event::Expired(list))) => {
                        handle_mdns_expired(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        pending_dials.remove(&peer_id);
                        reconnect_targets.insert(peer_id);
                        reconnect_attempts.remove(&peer_id);
                        reconnect_next_due.remove(&peer_id);

                        let remote_addr = endpoint.get_remote_address().clone();
                        let plan = connection_route_plan(
                            has_lan_loop,
                            has_iroh_loop,
                            peer_iroh_scope_loop.get(&peer_id),
                        );
                        for (transport, scope, priority) in plan {
                            handle_connection_established(
                                &mut swarm,
                                peer_id,
                                remote_addr.clone(),
                                &routes_loop,
                                &known_peers_loop,
                                &peer_events_tx_loop,
                                &re_register_fns_loop,
                                transport,
                                scope,
                                priority,
                            );
                        }

                        refresh_membership_known_peers(&membership_store_loop, &routes_loop);
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        ..
                    } => {
                        reconnect_targets.insert(peer_id);
                        reconnect_next_due.remove(&peer_id);
                        handle_connection_closed(
                            peer_id,
                            num_established,
                            &routes_loop,
                            &known_peers_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(pid) = peer_id {
                            pending_dials.remove(&pid);
                            reconnect_targets.insert(pid);
                            let attempt = reconnect_attempts.entry(pid).or_insert(0);
                            *attempt = attempt.saturating_add(1);
                            reconnect_next_due.insert(
                                pid,
                                tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                            );
                        }
                        log::warn!(
                            "Outgoing connection error (unified, peer={:?}): {}",
                            peer_id,
                            error
                        );
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        log::info!("ActorSwarm listening on {address} ({})", if has_iroh_loop && has_lan { "composite" } else if has_iroh_loop { "iroh" } else { "lan" });
                    }
                    _ => {}
                } // match event
                } // select: event arm
            } // tokio::select!
        }
    });

    // ── Build and return handle ─────────────────────────────────────────────
    let ctx = MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    };

    let listen_label = match transport_mode {
        MeshTransportMode::Lan => lan_listen_addr.to_string(),
        MeshTransportMode::Iroh => "iroh-relay".to_string(),
        MeshTransportMode::Composite => format!("{}+iroh", lan_listen_addr),
    };

    let mut handle = finalize_bootstrap(
        local_peer_id,
        ctx,
        &listen_label,
        transport_mode,
        swarm_cmd_tx,
        config.stream_reconnect_grace,
    );

    // Seed the handle with config-derived scopes so DHT registrations
    // (remote_setup) use the authoritative scope list, not just persisted
    // memberships.
    handle.set_config_scopes(config.active_scopes());

    Ok(super::runtime_handle::MeshRuntimeHandle::new(handle))
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
#[cfg(feature = "remote-internet")]
pub async fn join_mesh_via_invite(
    invite: &super::invite::SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
) -> Result<MeshHandle, MeshError> {
    use super::invite::{
        MembershipStore, MeshMembership, PeerEntry, default_membership_store_path, mesh_id_for,
    };
    use crate::agent::remote::node_manager::{AdmissionRequest, AdmissionResponse};

    // ── 1. Verify invite offline ──────────────────────────────────────────────
    invite
        .verify()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let mesh_id = mesh_id_for(
        &invite.grant.inviter_peer_id,
        invite.grant.mesh_name.as_deref(),
    );

    // ── 2. Check for existing membership (reconnect vs first join) ────────────
    let membership_path =
        default_membership_store_path().map_err(|e| MeshError::SwarmError(e.to_string()))?;
    let mut membership_store = MembershipStore::load_or_create(&membership_path)
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let (existing_token, fallback_peers) = match membership_store.get_membership(&mesh_id) {
        Some(m) if !m.token.is_expired() => {
            log::info!(
                "Found existing membership for mesh '{}', attempting reconnect",
                mesh_id
            );
            (Some(m.token.clone()), m.known_peers.clone())
        }
        _ => (None, vec![]),
    };

    // ── 3. Bootstrap swarm ────────────────────────────────────────────────────
    // Primary: dial the inviter.  Fallback: cached peers (inviter may be offline).
    let mut bootstrap_peers: Vec<String> = vec![];
    if existing_token.is_some() && !fallback_peers.is_empty() {
        // On reconnect, prefer cached peers in case the inviter is offline.
        for p in &fallback_peers {
            bootstrap_peers.push(format!("/p2p/{}", p.peer_id));
        }
        log::info!(
            "Reconnect: will dial {} cached peer(s) as fallback",
            bootstrap_peers.len()
        );
    }

    let config = MeshConfig {
        listen: None,
        discovery: MeshDiscovery::None,
        bootstrap_peers,
        directory: DirectoryMode::default(),
        request_timeout: std::time::Duration::from_secs(300),
        stream_reconnect_grace: std::time::Duration::from_secs(120),
        transport: MeshTransportMode::Iroh,
        identity_file,
        // Always include the invite so the swarm dials the inviter first.
        invite: Some(invite.clone()),
    };

    log::info!(
        "Joining mesh via invite (inviter={}, name={:?})",
        invite.grant.inviter_peer_id,
        invite.grant.mesh_name
    );

    let mesh = bootstrap_mesh(&config).await?;
    let my_peer_id = mesh.peer_id().to_string();

    // ── 4. Admission handshake ────────────────────────────────────────────────
    // Build the request: reconnect (Token) or first join (Invite).
    let request = match existing_token {
        Some(token) => AdmissionRequest::Token {
            membership_token: token,
            peer_id: my_peer_id.clone(),
        },
        None => AdmissionRequest::Invite {
            invite_id: invite.grant.invite_id.clone(),
            peer_id: my_peer_id.clone(),
        },
    };

    // Look up the admission target — prefer the original inviter, fall back to
    // any cached peer that is reachable.
    let target_nm = find_admission_target(&mesh, &invite.grant.inviter_peer_id, &fallback_peers)
        .await
        .ok_or_else(|| {
            MeshError::SwarmError("no reachable peer found for admission handshake".to_string())
        })?;

    let response = target_nm
        .ask::<AdmissionRequest>(&request)
        .await
        .map_err(|e| MeshError::SwarmError(format!("admission handshake failed: {e}")))?;

    // ── 5. Handle response ────────────────────────────────────────────────────
    match response {
        AdmissionResponse::Admitted {
            membership_token,
            existing_peers,
        } => {
            log::info!(
                "Admitted to mesh '{}' (admitted_by={}, {} existing peers)",
                mesh_id,
                membership_token.admitted_by,
                existing_peers.len(),
            );

            // Dial all existing mesh peers to form a full mesh.
            // Without this, iroh nodes only connect to the inviter (star
            // topology) and cannot route actor messages to each other.
            for peer_str in &existing_peers {
                if let Ok(pid) = peer_str.parse::<PeerId>() {
                    log::info!("Dialing existing mesh peer: {}", pid);
                    mesh.dial_peer(&pid);
                } else {
                    log::warn!("Ignoring invalid PeerId in existing_peers: {}", peer_str);
                }
            }

            // Snapshot current known peers for the membership record.
            // Include both directly connected peers and the peers we just
            // started dialing (they'll connect shortly).
            let mut all_peer_strs: Vec<String> = mesh
                .known_peer_ids()
                .into_iter()
                .map(|pid| pid.to_string())
                .collect();
            for p in &existing_peers {
                if !all_peer_strs.contains(p) {
                    all_peer_strs.push(p.clone());
                }
            }
            let known_peers: Vec<PeerEntry> = all_peer_strs
                .into_iter()
                .map(|pid| PeerEntry {
                    peer_id: pid.clone(),
                    addrs: vec![format!("/p2p/{pid}")],
                })
                .collect();

            membership_store
                .store_membership(
                    mesh_id.clone(),
                    MeshMembership {
                        token: membership_token,
                        known_peers,
                        last_connected: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    },
                )
                .map_err(|e| MeshError::SwarmError(format!("failed to persist membership: {e}")))?;

            let _ = mesh
                .peer_events_tx
                .send(MeshEvent::ScopeJoined(MeshScopeId::Iroh {
                    mesh_id: mesh_id.clone(),
                }));
        }
        AdmissionResponse::Readmitted { existing_peers } => {
            log::info!(
                "Readmitted to mesh '{}' (token accepted, {} existing peers)",
                mesh_id,
                existing_peers.len(),
            );

            // Dial all existing mesh peers to form a full mesh.
            for peer_str in &existing_peers {
                if let Ok(pid) = peer_str.parse::<PeerId>() {
                    log::info!("Dialing existing mesh peer (readmit): {}", pid);
                    mesh.dial_peer(&pid);
                } else {
                    log::warn!("Ignoring invalid PeerId in existing_peers: {}", peer_str);
                }
            }

            membership_store
                .touch_last_connected(&mesh_id)
                .map_err(|e| {
                    MeshError::SwarmError(format!("failed to update membership timestamp: {e}"))
                })?;

            let _ = mesh
                .peer_events_tx
                .send(MeshEvent::ScopeJoined(MeshScopeId::Iroh {
                    mesh_id: mesh_id.clone(),
                }));
        }
        AdmissionResponse::Rejected { reason } => {
            log::warn!("Mesh admission rejected: {}", reason);
            return Err(MeshError::SwarmError(format!(
                "admission rejected: {reason}"
            )));
        }
    }

    // ── 6. Seed the membership store into the mesh handle ─────────────────────
    // The handle's membership_store was loaded from disk during finalize_bootstrap.
    // Overwrite it with the now-updated in-memory store so future
    // update_membership_peers() calls see the right mesh_id entry.
    if let Some(ref store_arc) = mesh.membership_store {
        let fresh = MembershipStore::load_or_create(&membership_path)
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;
        *store_arc.write() = fresh;
    }

    Ok(mesh)
}

/// Find a reachable `RemoteNodeManager` for the admission handshake.
///
/// Tries the original inviter first (per-peer DHT name), then falls back to
/// cached peers in order.  Returns the first one that responds.
#[cfg(feature = "remote-internet")]
async fn find_admission_target(
    mesh: &MeshHandle,
    inviter_peer_id: &str,
    fallback_peers: &[super::invite::PeerEntry],
) -> Option<kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>> {
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::scope::scoped_node_manager_for_peer;

    // Give the swarm a moment to complete the connection before querying the DHT.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Try the inviter first.
    for scope in mesh.active_scopes() {
        let inviter_dht = scoped_node_manager_for_peer(&scope, &inviter_peer_id.to_string());
        if let Ok(Some(nm)) = mesh.lookup_actor::<RemoteNodeManager>(&inviter_dht).await {
            log::debug!("Admission target: inviter ({})", inviter_peer_id);
            return Some(nm);
        }
    }

    // Fall back to cached peers.
    for peer in fallback_peers {
        for scope in mesh.active_scopes() {
            let dht = scoped_node_manager_for_peer(&scope, &peer.peer_id);
            if let Ok(Some(nm)) = mesh.lookup_actor::<RemoteNodeManager>(&dht).await {
                log::debug!("Admission target: cached peer ({})", peer.peer_id);
                return Some(nm);
            }
        }
    }

    None
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

#[cfg(test)]
mod tests {
    use super::{
        MeshEvent, MeshHandle, MeshScopeId, MeshTransportKind, MeshTransportMode, RouteTable,
        SwarmCommand, connection_route_plan,
    };
    #[cfg(feature = "remote-internet")]
    use super::{admitted_peer_ids_for_local_mesh, peer_id_from_multiaddr};
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    #[cfg(feature = "remote-internet")]
    #[test]
    fn admitted_peer_ids_filters_to_specific_local_mesh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invites.json");
        let mut store = crate::agent::remote::invite::InviteStore::load_or_create(&path).unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();

        let mesh_a = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                Some("mesh-a".to_string()),
                None,
                1,
                crate::agent::remote::invite::InvitePermissions::default(),
            )
            .unwrap();
        let peer_a_kp = libp2p::identity::Keypair::generate_ed25519();
        let peer_a_id = peer_a_kp.public().to_peer_id().to_string();
        store
            .admit_peer(
                &mesh_a.grant.invite_id,
                &peer_a_id,
                &host_kp,
                Some("mesh-a"),
            )
            .unwrap();

        let mesh_b = store
            .create_invite(
                &host_kp,
                &host_peer_id,
                Some("mesh-b".to_string()),
                None,
                1,
                crate::agent::remote::invite::InvitePermissions::default(),
            )
            .unwrap();
        let peer_b_kp = libp2p::identity::Keypair::generate_ed25519();
        let peer_b_id = peer_b_kp.public().to_peer_id().to_string();
        store
            .admit_peer(
                &mesh_b.grant.invite_id,
                &peer_b_id,
                &host_kp,
                Some("mesh-b"),
            )
            .unwrap();

        let host_pid: libp2p::PeerId = host_peer_id.parse().unwrap();
        let local_mesh_id =
            crate::agent::remote::invite::mesh_id_for(&host_peer_id, Some("mesh-a"));
        let local = admitted_peer_ids_for_local_mesh(&store, &host_pid, Some(&local_mesh_id));

        assert_eq!(
            local.len(),
            1,
            "should include only local mesh admitted peers"
        );
        assert_eq!(local[0].to_string(), peer_a_id);
    }

    #[cfg(feature = "remote-internet")]
    #[test]
    fn peer_id_from_multiaddr_extracts_p2p_component() {
        let kp = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = kp.public().to_peer_id();
        let addr: libp2p::Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{peer_id}")
            .parse()
            .unwrap();
        assert_eq!(peer_id_from_multiaddr(&addr), Some(peer_id));
    }

    #[cfg(feature = "remote-internet")]
    fn test_mesh_with_memberships(
        mesh_ids: &[&str],
    ) -> (
        tempfile::TempDir,
        MeshHandle,
        tokio::sync::broadcast::Receiver<MeshEvent>,
        tokio::sync::mpsc::UnboundedReceiver<SwarmCommand>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let membership_path = dir.path().join("memberships.json");
        let mut membership_store =
            crate::agent::remote::invite::MembershipStore::load_or_create(&membership_path)
                .unwrap();

        let host_kp = libp2p::identity::Keypair::generate_ed25519();
        let host_peer_id = host_kp.public().to_peer_id().to_string();

        for mesh_id in mesh_ids {
            let token = crate::agent::remote::invite::MembershipToken::issue(
                (*mesh_id).to_string(),
                &host_peer_id,
                &host_kp,
                format!("invite-{mesh_id}"),
                crate::agent::remote::invite::InvitePermissions::default(),
                u64::MAX,
            )
            .unwrap();
            membership_store
                .store_membership(
                    (*mesh_id).to_string(),
                    crate::agent::remote::invite::MeshMembership {
                        token,
                        known_peers: Vec::new(),
                        last_connected: 0,
                    },
                )
                .unwrap();
        }

        let (peer_events_tx, peer_events_rx) = tokio::sync::broadcast::channel::<MeshEvent>(16);
        let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
        let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
        let (swarm_cmd_tx, swarm_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

        let mesh = MeshHandle::new(
            host_kp.public().to_peer_id(),
            peer_events_tx,
            routes,
            "test-host".to_string(),
            re_register_fns,
            host_kp,
            None,
            Some(Arc::new(RwLock::new(membership_store))),
            MeshTransportMode::Composite,
            swarm_cmd_tx,
            Duration::from_secs(30),
        );

        (dir, mesh, peer_events_rx, swarm_cmd_rx)
    }

    #[cfg(feature = "remote-internet")]
    #[test]
    fn leave_iroh_scope_removes_membership_emits_event_and_notifies_swarm() {
        let mesh_id = "inviter:mesh-a";
        let (_dir, mesh, mut events_rx, mut swarm_cmd_rx) = test_mesh_with_memberships(&[mesh_id]);

        let removed = mesh.leave_iroh_scope(mesh_id).unwrap();
        assert!(removed, "existing scope should be removed");
        assert!(mesh.joined_iroh_scopes().is_empty());
        assert_eq!(mesh.active_scopes(), vec![MeshScopeId::lan_default()]);

        match events_rx.try_recv().unwrap() {
            MeshEvent::ScopeLeft(MeshScopeId::Iroh { mesh_id: left }) => {
                assert_eq!(left, mesh_id);
            }
            other => panic!("expected ScopeLeft event, got {other:?}"),
        }

        match swarm_cmd_rx.try_recv().unwrap() {
            SwarmCommand::LeaveIrohScope { mesh_id: left } => assert_eq!(left, mesh_id),
            other => panic!("expected LeaveIrohScope command, got {other:?}"),
        }
    }

    #[cfg(feature = "remote-internet")]
    #[test]
    fn leave_iroh_scope_missing_scope_returns_false_and_emits_nothing() {
        let (_dir, mesh, mut events_rx, mut swarm_cmd_rx) =
            test_mesh_with_memberships(&["inviter:mesh-a"]);

        let removed = mesh.leave_iroh_scope("inviter:mesh-b").unwrap();
        assert!(!removed, "missing scope should report false");
        assert_eq!(
            mesh.joined_iroh_scopes(),
            vec![MeshScopeId::Iroh {
                mesh_id: "inviter:mesh-a".to_string()
            }]
        );

        assert!(matches!(
            events_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(swarm_cmd_rx.try_recv().is_err());
    }

    #[test]
    fn connection_route_plan_composite_with_iroh_peer_includes_lan_and_iroh() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let plan = connection_route_plan(true, true, Some(&scope));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0], (MeshTransportKind::Lan, MeshScopeId::lan_default(), 100));
        assert_eq!(plan[1], (MeshTransportKind::Iroh, scope, 70));
    }

    #[test]
    fn connection_route_plan_iroh_only_without_scope_adds_no_lan() {
        let plan = connection_route_plan(false, true, None);
        assert!(plan.is_empty(), "iroh-only without known scope should not synthesize lan route");
    }

    #[test]
    fn route_table_prefers_lan_over_iroh_for_same_peer() {
        use libp2p::Multiaddr;

        let routes = RouteTable::new(Duration::from_secs(90));
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();

        let iroh_scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };

        let iroh_addr: Multiaddr = format!("/p2p/{peer}").parse().unwrap();
        let lan_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/12345/p2p/{peer}").parse().unwrap();

        routes.upsert_addrs(peer, MeshTransportKind::Iroh, iroh_scope.clone(), [iroh_addr], 70);
        routes.upsert_addrs(peer, MeshTransportKind::Lan, MeshScopeId::lan_default(), [lan_addr], 100);

        let best = routes.best_route_for_peer(&peer).expect("best route exists");
        assert_eq!(best.transport, MeshTransportKind::Lan);
        assert_eq!(best.scope, MeshScopeId::lan_default());
        assert_eq!(best.priority, 100);
    }

    #[cfg(feature = "remote-internet")]
    #[test]
    fn scope_joined_and_left_events_roundtrip_through_broadcast_channel() {
        let (_dir, mesh, mut events_rx, _swarm_cmd_rx) =
            test_mesh_with_memberships(&["inviter:mesh-a"]);
        let joined = MeshScopeId::Iroh {
            mesh_id: "inviter:mesh-b".to_string(),
        };
        let left = MeshScopeId::Iroh {
            mesh_id: "inviter:mesh-a".to_string(),
        };

        let _ = mesh
            .peer_events_tx
            .send(MeshEvent::ScopeJoined(joined.clone()));
        let _ = mesh.peer_events_tx.send(MeshEvent::ScopeLeft(left.clone()));

        assert!(
            matches!(events_rx.try_recv().unwrap(), MeshEvent::ScopeJoined(scope) if scope == joined)
        );
        assert!(
            matches!(events_rx.try_recv().unwrap(), MeshEvent::ScopeLeft(scope) if scope == left)
        );
    }

    /// `resolve_peer_node_id` with no known peers returns `None`.
    ///
    /// This is a pure unit test: no DHT or network required. The `known_peers`
    /// set is empty so the iteration body never executes and the method must
    /// return `None` without panicking.
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn resolve_peer_node_id_no_known_peers_returns_none() {
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

        let mesh = get_test_mesh().await.clone();
        // known_peers is empty right after bootstrap (no peers discovered yet)
        let result = mesh.resolve_peer_node_id("gpu-node").await;
        assert!(
            result.is_none(),
            "expected None when no peers are known, got {:?}",
            result
        );
    }

    /// `resolve_peer_node_id` returns `None` for an unknown peer name even
    /// when the mesh has known peers. This test uses the test mesh (single node)
    /// which has no remote peers with any hostname.
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn resolve_peer_node_id_unknown_name_returns_none() {
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

        let mesh = get_test_mesh().await.clone();
        let result = mesh.resolve_peer_node_id("nonexistent-peer-xyz").await;
        assert!(result.is_none());
    }

    /// `re_register_fns_count` returns a valid count (test mesh is shared,
    /// so the count may be non-zero if other tests have registered actors).
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn re_register_fns_count_is_accessible() {
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

        let mesh = get_test_mesh().await.clone();
        // Just verify the method works and returns a sane value.
        let count = mesh.re_register_fns_count();
        assert!(
            count < 10_000,
            "re_register_fns_count should be finite, got {}",
            count
        );
    }

    /// `deregister_actor` removes the re-registration closure for a given name.
    ///
    /// The test mesh is shared across all tests so we cannot rely on absolute
    /// counts. Instead we verify the specific key is present after register
    /// and absent after deregister.
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn deregister_actor_removes_re_register_fn() {
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

        let mesh = get_test_mesh().await.clone();

        // Register a dummy actor under a unique name
        let test_name = format!("test_deregister_{}", uuid::Uuid::now_v7());
        use crate::agent::remote::provider_host::StreamReceiverActor;
        use kameo::actor::Spawn;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);
        mesh.register_actor(actor_ref, test_name.clone()).await;

        // Verify the key is present
        assert!(
            mesh.has_re_register_fn(&test_name),
            "register_actor should insert a re-register fn under the given name"
        );

        // Deregister
        mesh.deregister_actor(&test_name);

        assert!(
            !mesh.has_re_register_fn(&test_name),
            "deregister_actor should remove the re-register fn"
        );
    }

    /// `deregister_actor` is a no-op for unknown names (no panic).
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn deregister_actor_unknown_name_is_noop() {
        use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

        let mesh = get_test_mesh().await.clone();
        let before = mesh.re_register_fns_count();
        mesh.deregister_actor("nonexistent_actor_name");
        assert_eq!(mesh.re_register_fns_count(), before);
    }
}
