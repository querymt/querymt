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

/// Shorthand access modes for mesh defaults.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MeshAccessMode {
    /// No access beyond identity discovery.
    None,
    /// Sessions, filesystem, unsandboxed shell, and every provider and model.
    Full,
}

/// Default access for peers without a saved grant.
///
/// Either `"none"`, `"full"`, or a custom grant table:
/// ```toml
/// lan_access = { mode = "custom", sessions = false, filesystem = false, providers = [
///   { provider_id = "llama_cpp", models = { mode = "all" } },
/// ] }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(untagged)]
pub enum MeshAccessToml {
    Mode(MeshAccessMode),
    Grant(#[schemars(with = "serde_json::Value")] querymt_remote::peer_access::PeerGrant),
}

impl MeshAccessToml {
    pub fn into_grant(self) -> querymt_remote::peer_access::PeerGrant {
        use querymt_remote::peer_access::PeerGrant;
        match self {
            Self::Mode(MeshAccessMode::None) => PeerGrant::None,
            Self::Mode(MeshAccessMode::Full) => PeerGrant::Full,
            Self::Grant(grant) => grant,
        }
    }
}

/// A trust scope: peers holding a token signed by `authority` for `name`
/// receive `access` on this host, over any transport and without internet access.
///
/// ```toml
/// [[mesh.trust]]
/// name = "acme-eng"
/// authority = "12D3KooW..."   # peer ID of the signing key
/// access = "full"              # what this host gives members (default: "none")
/// token = "${QMT_TRUST_TOKEN}" # this node's own membership token, if any
/// ```
///
/// Issue a token offline with `qmtcode --mesh-trust-issue=<peer_id> --mesh-trust-scope=<name>`
/// on the authority machine.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MeshTrustTomlConfig {
    /// Scope name. Tokens are bound to it.
    pub name: String,
    /// Peer ID of the authority key that signs membership tokens.
    pub authority: String,
    /// Access this host gives scope members. Default: `"none"`.
    #[serde(default)]
    pub access: Option<MeshAccessToml>,
    /// This node's membership token, presented to every peer it connects to.
    /// Supports `${VAR}` interpolation.
    #[serde(default)]
    pub token: Option<String>,
}

impl MeshTrustTomlConfig {
    pub fn into_runtime(self) -> querymt_remote::peer_access::TrustScopeConfig {
        querymt_remote::peer_access::TrustScopeConfig {
            name: self.name,
            authority: self.authority,
            access: self
                .access
                .map(MeshAccessToml::into_grant)
                .unwrap_or_default(),
            token: self.token.filter(|token| !token.trim().is_empty()),
        }
    }
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

    /// Enable Generic Segmentation Offload (GSO) on the shared iroh actor endpoint.
    /// Default: true. Set to false as a UDP offload compatibility workaround.
    /// Applies to all Iroh scopes; does not affect LAN or separate payload endpoints.
    #[serde(default = "default_true")]
    pub iroh_gso: bool,

    /// DNS policy for the shared iroh endpoint; absent preserves upstream defaults.
    /// Disabling public DNS fallback does not disable N0's public pkarr services.
    #[serde(default)]
    pub iroh_dns: Option<querymt_remote::IrohDnsConfig>,

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

    /// Access for peers that have a live connection from a private-network
    /// address (RFC 1918, loopback, link-local, IPv6 unique-local) and no saved
    /// grant. Default: `"none"`. A saved per-peer grant always wins.
    #[serde(default)]
    pub lan_access: Option<MeshAccessToml>,

    /// Trust scopes that grant access to token holders without per-peer grants.
    #[serde(default)]
    pub trust: Vec<MeshTrustTomlConfig>,
}

impl MeshTomlConfig {
    /// Host defaults for peers without a saved grant.
    pub fn access_config(&self) -> querymt_remote::peer_access::MeshAccessConfig {
        querymt_remote::peer_access::MeshAccessConfig {
            lan: self
                .lan_access
                .clone()
                .map(MeshAccessToml::into_grant)
                .unwrap_or_default(),
            trust: self
                .trust
                .iter()
                .cloned()
                .map(MeshTrustTomlConfig::into_runtime)
                .collect(),
        }
    }
}

impl Default for MeshTomlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_mesh_listen(),
            discovery: MeshDiscoveryConfig::default(),
            transport: MeshTransportConfig::default(),
            iroh_gso: true,
            iroh_dns: None,
            auto_fallback: false,
            peers: Vec::new(),
            request_timeout_secs: default_mesh_request_timeout_secs(),
            stream_reconnect_grace_secs: default_mesh_stream_reconnect_grace_secs(),
            identity_file: None,
            invite: None,
            node_name: None,
            lan: None,
            iroh: Vec::new(),
            lan_access: None,
            trust: Vec::new(),
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
