use super::super::invite::SignedInviteGrant;

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

    /// No automatic discovery - peers must be added manually via
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
    /// peer discovery. Recommended for small LAN meshes (2-10 nodes) where
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
    /// over the mesh (e.g. compaction, no-tools LLM inference). The default
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
    /// with mDNS discovery. `Iroh` uses `libp2p-iroh` for NAT traversal and
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
    pub invite: Option<SignedInviteGrant>,
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
