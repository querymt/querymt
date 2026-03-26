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
//!     listen: Some("/ip4/0.0.0.0/tcp/9000".to_string()),
//!     discovery: MeshDiscovery::Mdns,
//!     bootstrap_peers: vec![],
//!     directory: DirectoryMode::default(),
//!     request_timeout: std::time::Duration::from_secs(300),
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

use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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

/// Transport layer for the mesh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MeshTransportMode {
    /// Traditional libp2p TCP + QUIC + Noise + Yamux (LAN-optimised).
    #[default]
    Lan,
    /// iroh-backed QUIC transport with relay and NAT traversal (internet-capable).
    Iroh,
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
    /// Sensible defaults for local development: listen on port 9000 with mDNS.
    fn default() -> Self {
        Self {
            listen: Some("/ip4/0.0.0.0/tcp/9000".to_string()),
            discovery: MeshDiscovery::Mdns,
            bootstrap_peers: vec![],
            directory: DirectoryMode::default(),
            request_timeout: std::time::Duration::from_secs(300),
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
    /// Broadcast channel for peer lifecycle events.
    /// Capacity 32 — more than enough for typical mesh sizes.
    peer_events_tx: broadcast::Sender<PeerEvent>,
    /// Map of currently-alive peers → their last-known multiaddrs.
    ///
    /// Inserted/updated on mDNS Discovered, removed on Expired.  Used as
    /// ground truth to filter stale DHT records when listing remote nodes,
    /// and to distinguish a genuine address change from a periodic mDNS
    /// re-announcement of an already-connected peer (which must not trigger
    /// the re-registration cascade or a PeerEvent).
    known_peers: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
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
        peer_events_tx: broadcast::Sender<PeerEvent>,
        known_peers: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
        local_hostname: String,
        re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
        keypair: libp2p::identity::Keypair,
        invite_store: Option<Arc<RwLock<super::invite::InviteStore>>>,
        membership_store: Option<Arc<RwLock<super::invite::MembershipStore>>>,
        transport_mode: MeshTransportMode,
    ) -> Self {
        Self {
            peer_id,
            peer_events_tx,
            known_peers,
            local_hostname: Arc::new(local_hostname),
            re_register_fns,
            keypair: Arc::new(keypair),
            invite_store,
            membership_store,
            transport_mode,
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
        self.known_peers.read().contains_key(peer_id)
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
        self.known_peers.write().entry(peer_id).or_default();
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

        let peers: Vec<PeerId> = self.known_peers.read().keys().copied().collect();

        for peer_id in peers {
            let per_peer_name =
                crate::agent::remote::dht_name::node_manager_for_peer(&peer_id.to_string());
            let node_manager = match self.lookup_actor::<RemoteNodeManager>(&per_peer_name).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    log::debug!(
                        "resolve_peer_node_id: no RemoteNodeManager under '{}'",
                        per_peer_name
                    );
                    continue;
                }
                Err(e) => {
                    log::debug!(
                        "resolve_peer_node_id: lookup error for '{}': {}",
                        per_peer_name,
                        e
                    );
                    continue;
                }
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
            self.known_peers.read().len()
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
        self.known_peers.read().keys().copied().collect()
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
    pub fn transport_mode(&self) -> MeshTransportMode {
        self.transport_mode.clone()
    }

    /// Return true when the mesh is internet-capable iroh transport.
    pub fn is_iroh_transport(&self) -> bool {
        matches!(self.transport_mode, MeshTransportMode::Iroh)
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
    }
}

// ── Shared event-loop helpers ──────────────────────────────────────────────────

/// Handle a `ConnectionEstablished` swarm event.
///
/// Populates Kademlia's routing table, updates `known_peers`, fires
/// `PeerEvent::Discovered` for genuinely new peers, and triggers the
/// re-registration cascade so the new peer's DHT is populated immediately.
fn handle_connection_established<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    peer_id: PeerId,
    remote_addr: Multiaddr,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    re_register_fns: &RwLock<HashMap<String, ReRegisterFn>>,
) {
    swarm.add_peer_address(peer_id, remote_addr.clone());
    let is_new = {
        let mut peers = known_peers.write();
        let entry = peers.entry(peer_id).or_default();
        let was_empty = entry.is_empty();
        entry.insert(remote_addr.clone());
        was_empty
    };

    if is_new {
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
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    if num_established == 0 {
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

/// Shared pre-bootstrap setup: load identity, validate peers, create channels.
struct MeshBootstrapContext {
    keypair: libp2p::identity::Keypair,
    peer_events_tx: broadcast::Sender<PeerEvent>,
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

    let (peer_events_tx, _) = broadcast::channel::<PeerEvent>(32);
    let known_peers = Arc::new(RwLock::new(HashMap::new()));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let local_hostname = resolve_local_hostname();

    Ok(MeshBootstrapContext {
        keypair,
        peer_events_tx,
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
        ctx.known_peers,
        ctx.local_hostname,
        ctx.re_register_fns,
        ctx.keypair,
        invite_store,
        membership_store,
        transport_mode,
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

    // ── Swarm event loop ──────────────────────────────────────────────────────
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    // A single mDNS event may carry multiple (peer_id, multiaddr)
                    // pairs — one per transport (TCP, QUIC) and one per address.
                    // We always call add_peer_address so libp2p's peer store stays
                    // current, but we only fire PeerEvent::Discovered + the
                    // re-registration cascade when something genuinely changed:
                    //
                    //   • Peer is brand-new (not in known_peers)            → full event
                    //   • Peer is known but gained at least one new address  → full event
                    //     (address change: host got a new IP, VPN reconnect, etc.)
                    //   • Peer is known and ALL addresses are already tracked → skip
                    //     (periodic mDNS re-announcement, ~every 15 s)
                    //
                    // Suppressing the cascade for the third case is the fix for
                    // in-flight LLM stream disruption: the re-registration of all
                    // ephemeral stream_rx::* actors mid-stream caused kameo to
                    // invalidate in-flight request routing, dropping chunks and
                    // triggering the 60 s STREAM_CHUNK_TIMEOUT.
                    //
                    // Collect addresses per peer first so we can check atomically.
                    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
                    for (peer_id, multiaddr) in list {
                        swarm.add_peer_address(peer_id, multiaddr.clone());
                        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
                    }

                    for (peer_id, new_addrs) in addrs_by_peer {
                        // Determine whether this is a new peer or an address change.
                        let (is_new, has_new_addr) = {
                            let peers = known_peers_loop.read();
                            match peers.get(&peer_id) {
                                None => (true, false),
                                Some(known_addrs) => {
                                    let any_new =
                                        new_addrs.iter().any(|a| !known_addrs.contains(a));
                                    (false, any_new)
                                }
                            }
                        };

                        // Always update the stored address set.
                        {
                            let mut peers = known_peers_loop.write();
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
                        } else {
                            // Periodic re-announcement — same peer, same addresses.
                            // Skip the cascade to avoid disrupting in-flight streams.
                            log::debug!(
                                "mDNS re-announced peer {peer_id} (no address change, skipping re-registration)"
                            );
                            continue;
                        }

                        // Genuine new peer or address change: fire event + re-register.
                        let _ = peer_events_tx_loop.send(PeerEvent::Discovered(peer_id));

                        // Phase 1c: re-publish all locally registered actors into
                        // the new/updated peer's Kademlia routing table so that
                        // lookups from the peer succeed immediately rather than
                        // waiting for the next Kademlia republish cycle.
                        let fns: Vec<ReRegisterFn> =
                            re_register_fns_loop.read().values().cloned().collect();
                        if !fns.is_empty() {
                            tokio::spawn(async move {
                                for f in &fns {
                                    f().await;
                                }
                            });
                        }
                    }
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    // mDNS expiry: a peer's TTL lapsed without re-announcement.
                    // Remove the expired addresses from known_peers.  If all
                    // addresses for a peer have expired, the peer is considered
                    // gone: disconnect and fire PeerEvent::Expired.
                    //
                    // We do NOT fire Expired if only some addresses expired —
                    // the peer may still be reachable at a remaining address.
                    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
                    for (peer_id, multiaddr) in list {
                        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
                    }

                    for (peer_id, expired_addrs) in addrs_by_peer {
                        let peer_fully_gone = {
                            let mut peers = known_peers_loop.write();
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

                        if peer_fully_gone {
                            log::info!("mDNS peer expired (went away): {peer_id}");
                            // Close the active connection so kameo stops trying
                            // to route messages to the dead peer.
                            let _ = swarm.disconnect_peer_id(peer_id);
                            let _ = peer_events_tx_loop.send(PeerEvent::Expired(peer_id));
                        } else {
                            log::debug!(
                                "mDNS partial expiry for peer {peer_id}: some addresses expired but peer still reachable"
                            );
                        }
                    }
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
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
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    // Load the membership store now so the event loop can update cached peers
    // whenever new peers are discovered.  Failures are non-fatal.
    let membership_store_loop: Option<Arc<RwLock<super::invite::MembershipStore>>> =
        super::invite::default_membership_store_path()
            .ok()
            .and_then(|p| super::invite::MembershipStore::load_or_create(&p).ok())
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

    // ── Swarm event loop ──────────────────────────────────────────────────────
    // Simpler than LAN: no mDNS events, just connection lifecycle + kameo.
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                    );

                    // Refresh the membership store's cached peer list whenever
                    // a new peer joins.  This keeps the reconnection fallback
                    // list up-to-date even if the original inviter goes offline.
                    if let Some(ref ms) = membership_store_loop {
                        let current_peers: Vec<super::invite::PeerEntry> = known_peers_loop
                            .read()
                            .keys()
                            .map(|pid| super::invite::PeerEntry {
                                peer_id: pid.to_string(),
                                addrs: vec![format!("/p2p/{pid}")],
                            })
                            .collect();
                        let ms = Arc::clone(ms);
                        let peers = current_peers;
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
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } => {
                    handle_connection_closed(
                        peer_id,
                        num_established,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
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
            }
        }
    });

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        "iroh-relay",
        MeshTransportMode::Iroh,
    ))
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
        AdmissionResponse::Admitted { membership_token } => {
            log::info!(
                "Admitted to mesh '{}' (admitted_by={})",
                mesh_id,
                membership_token.admitted_by
            );

            // Snapshot current known peers for the membership record.
            let known_peers: Vec<PeerEntry> = mesh
                .known_peer_ids()
                .into_iter()
                .map(|pid| PeerEntry {
                    peer_id: pid.to_string(),
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
        }
        AdmissionResponse::Readmitted => {
            log::info!("Readmitted to mesh '{}' (token accepted)", mesh_id);
            membership_store
                .touch_last_connected(&mesh_id)
                .map_err(|e| {
                    MeshError::SwarmError(format!("failed to update membership timestamp: {e}"))
                })?;
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
    use crate::agent::remote::dht_name;

    // Give the swarm a moment to complete the connection before querying the DHT.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Try the inviter first.
    let inviter_dht = dht_name::node_manager_for_peer(&inviter_peer_id.to_string());
    if let Ok(Some(nm)) = mesh.lookup_actor::<RemoteNodeManager>(&inviter_dht).await {
        log::debug!("Admission target: inviter ({})", inviter_peer_id);
        return Some(nm);
    }

    // Fall back to cached peers.
    for peer in fallback_peers {
        let dht = dht_name::node_manager_for_peer(&peer.peer_id);
        if let Ok(Some(nm)) = mesh.lookup_actor::<RemoteNodeManager>(&dht).await {
            log::debug!("Admission target: cached peer ({})", peer.peer_id);
            return Some(nm);
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
        let actor = StreamReceiverActor::new(tx, test_name.clone(), None);
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
