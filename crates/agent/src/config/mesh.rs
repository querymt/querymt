use super::*;

/// A single peer that this node should connect to in the mesh.
///
/// In TOML:
/// ```toml
/// [[mesh.peers]]
/// name = "dev-gpu"
/// addr = "/ip4/192.168.1.100/tcp/9000"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshPeerConfig {
    /// Human-readable label (referenced by `[[remote_agents]]`).
    pub name: String,
    /// libp2p multiaddr of the peer, e.g. `"/ip4/192.168.1.100/tcp/9000"`.
    pub addr: String,
}

/// Discovery strategy for the libp2p swarm.
///
/// In TOML: `discovery = "mdns"` | `"kademlia"` | `"none"`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MeshDiscoveryConfig {
    /// Zero-config local-network discovery (mDNS multicast).
    #[default]
    Mdns,
    /// Distributed discovery using the Kademlia DHT (for cross-subnet).
    Kademlia,
    /// No automatic discovery — peers are added only via `[[mesh.peers]]`.
    None,
}

/// Transport layer for the libp2p swarm.
///
/// In TOML: `transport = "lan"` | `"iroh"`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MeshTransportConfig {
    /// Traditional TCP + QUIC + Noise + Yamux (LAN-optimised, default).
    #[default]
    Lan,
    /// iroh-backed QUIC transport with relay and NAT traversal (internet-capable).
    /// Requires the `remote` feature.
    Iroh,
}

/// LAN mesh sub-configuration for multi-transport setups.
///
/// In TOML:
/// ```toml
/// [mesh.lan]
/// enabled = true
/// discovery = "mdns"
/// listen = "/ip4/0.0.0.0/tcp/0"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct LanMeshTomlConfig {
    /// Whether the LAN transport is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Multiaddr to listen on for LAN connections.
    ///
    /// Falls back to `[mesh] listen` when absent.
    #[serde(default)]
    pub listen: Option<String>,

    /// Discovery strategy for LAN peers.
    #[serde(default)]
    pub discovery: MeshDiscoveryConfig,
}

/// Iroh mesh scope sub-configuration for multi-transport setups.
///
/// In TOML (array of tables):
/// ```toml
/// [[mesh.iroh]]
/// enabled = true
/// invite = "..."
/// name = "personal"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrohMeshTomlConfig {
    /// Whether this Iroh scope is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Invite token string to join an existing Iroh mesh.
    ///
    /// When set, the `mesh_id` is derived from the invite grant.
    /// Supports `${VAR}` interpolation.
    #[serde(default)]
    pub invite: Option<String>,

    /// Human-readable name for this Iroh scope.
    ///
    /// Used as the `mesh_id` when `invite` is absent, or as a display
    /// label alongside the invite-derived mesh_id.
    #[serde(default)]
    pub name: Option<String>,
}

fn default_mesh_listen() -> Option<String> {
    Some("/ip4/0.0.0.0/tcp/0".to_string())
}

fn default_mesh_request_timeout_secs() -> u64 {
    300
}

fn default_mesh_stream_reconnect_grace_secs() -> u64 {
    120
}

/// Configuration for the kameo libp2p mesh.
///
/// In TOML:
/// ```toml
/// [mesh]
/// enabled = true
/// listen = "/ip4/0.0.0.0/tcp/0"
/// discovery = "mdns"
///
/// [[mesh.peers]]
/// name = "dev-gpu"
/// addr = "/ip4/192.168.1.100/tcp/9000"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshTomlConfig {
    /// Whether to start the mesh swarm at startup.  Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Multiaddr to listen on.  Default: `"/ip4/0.0.0.0/tcp/0"` (OS-assigned random port).
    #[serde(default = "default_mesh_listen")]
    pub listen: Option<String>,

    /// Peer discovery strategy.  Default: `"mdns"`.
    #[serde(default)]
    pub discovery: MeshDiscoveryConfig,

    /// Transport layer.  Default: `"lan"`.
    ///
    /// Set to `"iroh"` for internet-capable mesh with NAT traversal and relay.
    /// Requires the `remote` feature.
    #[serde(default)]
    pub transport: MeshTransportConfig,

    /// Whether `provider_node_id = None` may fall back to mesh provider discovery.
    ///
    /// Default: `false` (local-only unless an explicit `provider_node_id` is set).
    #[serde(default)]
    pub auto_fallback: bool,

    /// Explicit peers to connect to at startup.
    #[serde(default)]
    pub peers: Vec<MeshPeerConfig>,

    /// Timeout in seconds for non-streaming mesh request-response calls
    /// (e.g. compaction, no-tools LLM calls).  Default: 300 (5 minutes).
    /// Increase for very slow models or large context windows.
    #[serde(default = "default_mesh_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Grace period (seconds) to wait for mesh reconnection before failing an
    /// in-flight remote streaming request.
    #[serde(default = "default_mesh_stream_reconnect_grace_secs")]
    pub stream_reconnect_grace_secs: u64,

    /// Path to the persistent ed25519 identity file.
    ///
    /// When absent, defaults to `~/.qmt/mesh_identity.key`.  The node's
    /// `PeerId` is derived from this keypair and stays stable across restarts.
    #[serde(default)]
    pub identity_file: Option<String>,

    /// Invite token to join an existing mesh.
    ///
    /// When set, the node bootstraps in "join" mode: it dials the inviter
    /// from the token using the iroh transport.  Overrides `transport` to
    /// `"iroh"` automatically.
    ///
    /// Supports `${VAR}` interpolation, e.g. `"${QMT_MESH_INVITE}"`.
    ///
    /// Example:
    /// ```toml
    /// [mesh]
    /// enabled = true
    /// invite = "${QMT_MESH_INVITE}"
    /// ```
    #[serde(default)]
    pub invite: Option<String>,

    /// Human-readable name advertised to mesh peers.
    ///
    /// When set, overrides the OS hostname in `GetNodeInfo` responses.
    /// Useful on mobile where the OS hostname is often meaningless ("unknown").
    #[serde(default)]
    pub node_name: Option<String>,

    /// LAN transport sub-configuration (new multi-transport syntax).
    ///
    /// When present and `enabled = true`, the LAN transport is activated
    /// alongside any Iroh scopes.  Takes precedence over the legacy
    /// `transport` and `discovery` fields for LAN configuration.
    #[serde(default)]
    pub lan: Option<LanMeshTomlConfig>,

    /// Iroh mesh scopes (new multi-transport syntax).
    ///
    /// Each entry represents a logical Iroh mesh scope.  When present and
    /// at least one entry has `enabled = true`, the Iroh transport is
    /// activated alongside LAN (if also enabled).
    #[serde(default)]
    pub iroh: Vec<IrohMeshTomlConfig>,
}

impl Default for MeshTomlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_mesh_listen(),
            discovery: MeshDiscoveryConfig::default(),
            transport: MeshTransportConfig::default(),
            auto_fallback: false,
            peers: Vec::new(),
            request_timeout_secs: default_mesh_request_timeout_secs(),
            stream_reconnect_grace_secs: default_mesh_stream_reconnect_grace_secs(),
            identity_file: None,
            invite: None,
            node_name: None,
            lan: None,
            iroh: Vec::new(),
        }
    }
}

/// A remote agent running on another mesh node, used for task delegation.
///
/// Remote agents require `[mesh] enabled = true` and a matching peer in
/// `[[mesh.peers]]`. This is for multi-machine agent delegation — it is
/// NOT for MCP tool servers (use `[[mcp]]` for those).
///
/// ```toml
/// [mesh]
/// enabled = true
///
/// [[mesh.peers]]
/// name = "dev-gpu"
/// addr = "/ip4/192.168.1.100/tcp/9000"
///
/// [[remote_agents]]
/// id = "gpu-coder"
/// name = "GPU Coder"
/// description = "Coder running on GPU server with fast model"
/// peer = "dev-gpu"
/// capabilities = ["gpu", "fast-model"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteAgentConfig {
    /// Unique agent identifier used when targeting this agent for delegation.
    pub id: String,
    /// Human-readable display name shown in delegation context.
    pub name: String,
    /// Short description of the agent's purpose or specialisation,
    /// shown to the planner when choosing which agent to delegate to.
    #[serde(default)]
    pub description: String,
    /// Name of the peer in `[[mesh.peers]]` that hosts this agent.
    /// Must match the `name` field of a `[[mesh.peers]]` entry.
    pub peer: String,
    /// Capability tags used by the planner to select suitable agents
    /// (e.g. `["gpu", "shell", "filesystem"]`).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

// ============================================================================
// End Mesh & Remote Agent Configuration
