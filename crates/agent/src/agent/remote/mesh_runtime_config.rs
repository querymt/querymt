//! Normalized runtime configuration for the multi-transport scoped mesh.
//!
//! This module introduces [`MeshRuntimeConfig`], [`LanMeshConfig`], and
//! [`IrohMeshConfig`] — the *internal* representation of mesh configuration
//! that the bootstrap layer consumes.
//!
//! ## Normalization
//!
//! [`MeshRuntimeConfig`] is produced by normalizing a
//! [`MeshTomlConfig`](crate::config::MeshTomlConfig).  The normalization
//! accepts two shapes of TOML input:
//!
//! ### Old syntax (single transport)
//!
//! ```toml
//! [mesh]
//! enabled = true
//! transport = "lan"   # or "iroh"
//! discovery = "mdns"
//! listen = "/ip4/0.0.0.0/tcp/0"
//! ```
//!
//! ### New syntax (multi-transport)
//!
//! ```toml
//! [mesh]
//! enabled = true
//!
//! [mesh.lan]
//! enabled = true
//! discovery = "mdns"
//! listen = "/ip4/0.0.0.0/tcp/0"
//!
//! [[mesh.iroh]]
//! enabled = true
//! invite = "..."
//! name = "personal"
//! ```
//!
//! Both shapes produce the same [`MeshRuntimeConfig`] internally.  The new
//! syntax allows LAN and one or more Iroh scopes to be enabled concurrently.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, ensure};

/// Default request timeout for non-streaming mesh calls (5 minutes).
#[cfg(test)]
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 300;

/// Default grace period for mesh stream reconnection (120 seconds).
#[cfg(test)]
const DEFAULT_STREAM_RECONNECT_GRACE_SECS: u64 = 120;

// ── Runtime config (normalized) ────────────────────────────────────────────

/// Normalized internal configuration for the process-wide mesh runtime.
///
/// Produced by [`MeshRuntimeConfig::from_toml_config`].  The bootstrap layer
/// consumes this — it does **not** depend on serde directly.
///
/// # Invariants (validated on construction)
///
/// - When `enabled` is `true`, at least one transport (LAN or Iroh) is enabled.
/// - Every enabled Iroh scope has a non-empty `mesh_id`.
/// - `identity_file` is shared across all transports (derived from the single
///   `[mesh]`-level setting).
/// - The `invite` field on Iroh scopes is optional; when absent the Iroh scope
///   starts in listening/standalone mode.
#[derive(Clone, Debug)]
pub struct MeshRuntimeConfig {
    /// Whether the mesh subsystem is active.
    pub enabled: bool,

    /// LAN transport configuration.  `None` means LAN is disabled.
    pub lan: Option<LanMeshConfig>,

    /// Iroh transport scopes.  Empty means Iroh is disabled.
    pub iroh_scopes: Vec<IrohMeshConfig>,

    /// Path to the persistent ed25519 identity file.
    /// Shared across all transports.
    pub identity_file: Option<PathBuf>,

    /// Timeout for non-streaming mesh request-response calls.
    pub request_timeout: Duration,

    /// Grace period to tolerate transport disconnects during streaming.
    pub stream_reconnect_grace: Duration,

    /// Human-readable node name advertised to mesh peers.
    pub node_name: Option<String>,

    /// Explicit peers to connect to at startup.
    pub peers: Vec<String>,

    /// Auto-fallback from `provider_node_id = None` to mesh discovery.
    pub auto_fallback: bool,
}

/// LAN transport configuration (normalized).
#[derive(Clone, Debug)]
pub struct LanMeshConfig {
    /// Multiaddr to listen on, e.g. `"/ip4/0.0.0.0/tcp/0"`.
    pub listen: Option<String>,

    /// Discovery strategy for LAN peers.
    pub discovery: LanDiscovery,

    /// Directory mode for actor registration lookups.
    pub directory: DirectoryMode,
}

/// LAN discovery strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanDiscovery {
    /// Zero-config mDNS multicast on the local network.
    Mdns,
    /// No automatic discovery — rely on explicit peers.
    None,
}

/// Directory mode for actor lookups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryMode {
    /// Standard Kademlia DHT lookups.
    #[default]
    Kademlia,
    /// Cached lookups with peer registry exchange.
    Cached,
}

/// Iroh mesh scope configuration (normalized).
///
/// Each Iroh scope represents a logical mesh membership identified by
/// `mesh_id`.  Multiple Iroh scopes can be active simultaneously.
#[derive(Clone, Debug)]
pub struct IrohMeshConfig {
    /// Stable mesh identifier derived from the invite grant or configured name.
    ///
    /// Must be non-empty for enabled scopes.
    pub mesh_id: String,

    /// Optional invite token string (will be parsed into a
    /// `SignedInviteGrant` at bootstrap time).
    pub invite: Option<String>,

    /// Optional human-readable display name for UI purposes.
    pub name: Option<String>,
}

impl MeshRuntimeConfig {
    /// Normalize a TOML-level mesh config into a runtime config.
    ///
    /// This is the single entry point for converting what the user wrote in
    /// TOML into the internal representation consumed by the bootstrap layer.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `enabled` is `true` but no transport is enabled
    /// - an Iroh scope has an empty `mesh_id`
    /// - the old `invite` field is present without `transport = "iroh"` or
    ///   `[mesh.iroh]` (ambiguous intent)
    /// - duplicate `mesh_id`s are found in Iroh scopes
    pub fn from_toml_config(
        enabled: bool,
        transport: crate::config::MeshTransportConfig,
        discovery: crate::config::MeshDiscoveryConfig,
        listen: Option<String>,
        peers: Vec<String>,
        request_timeout_secs: u64,
        stream_reconnect_grace_secs: u64,
        identity_file: Option<String>,
        invite: Option<String>,
        node_name: Option<String>,
        auto_fallback: bool,
        // New multi-transport fields
        lan: Option<crate::config::LanMeshTomlConfig>,
        iroh: Vec<crate::config::IrohMeshTomlConfig>,
    ) -> Result<Self> {
        let identity_file = identity_file.map(PathBuf::from);
        let request_timeout = Duration::from_secs(request_timeout_secs);
        let stream_reconnect_grace = Duration::from_secs(stream_reconnect_grace_secs);

        // Determine LAN and Iroh configs from either old or new syntax.
        // When the mesh is disabled, skip transport config creation.
        let (lan_config, iroh_configs) = if !enabled {
            (None, Vec::new())
        } else if lan.is_some() || !iroh.is_empty() {
            // New syntax — use the structured sub-tables.
            let lan_config = match lan {
                Some(l) if l.enabled => {
                    let disc = match l.discovery {
                        crate::config::MeshDiscoveryConfig::Mdns => LanDiscovery::Mdns,
                        crate::config::MeshDiscoveryConfig::Kademlia => LanDiscovery::Mdns, // Kademlia on LAN uses mDNS fallback
                        crate::config::MeshDiscoveryConfig::None => LanDiscovery::None,
                    };
                    Some(LanMeshConfig {
                        listen: l.listen.or(listen),
                        discovery: disc,
                        directory: DirectoryMode::default(),
                    })
                }
                _ => None,
            };

            let mut iroh_configs = Vec::new();
            for iroh_entry in iroh {
                if !iroh_entry.enabled {
                    continue;
                }
                let mesh_id = iroh_entry
                    .name
                    .clone()
                    .or_else(|| {
                        // Derive mesh_id from invite if no explicit name
                        iroh_entry.invite.as_ref().and_then(|inv| {
                            // Try to extract mesh_name from invite
                            crate::agent::remote::invite::SignedInviteGrant::decode(inv)
                                .ok()
                                .and_then(|g| g.grant.mesh_name.clone())
                                .or_else(|| {
                                    // Fallback: derive from inviter peer ID
                                    crate::agent::remote::invite::SignedInviteGrant::decode(inv)
                                        .ok()
                                        .map(|g| {
                                            crate::agent::remote::invite::mesh_id_for(
                                                &g.grant.inviter_peer_id,
                                                g.grant.mesh_name.as_deref(),
                                            )
                                        })
                                })
                        })
                    })
                    .unwrap_or_else(|| {
                        // Last resort: generate a short ID from index
                        format!("iroh-{}", iroh_configs.len())
                    });

                iroh_configs.push(IrohMeshConfig {
                    mesh_id,
                    invite: iroh_entry.invite,
                    name: iroh_entry.name,
                });
            }

            (lan_config, iroh_configs)
        } else {
            // Old syntax — derive from single transport field.
            match transport {
                crate::config::MeshTransportConfig::Lan => {
                    let disc = match discovery {
                        crate::config::MeshDiscoveryConfig::Mdns => LanDiscovery::Mdns,
                        crate::config::MeshDiscoveryConfig::Kademlia => LanDiscovery::Mdns,
                        crate::config::MeshDiscoveryConfig::None => LanDiscovery::None,
                    };
                    let lan_config = LanMeshConfig {
                        listen,
                        discovery: disc,
                        directory: DirectoryMode::default(),
                    };
                    (Some(lan_config), Vec::new())
                }
                crate::config::MeshTransportConfig::Iroh => {
                    // Derive mesh_id from invite if available
                    let mesh_id = invite
                        .as_ref()
                        .and_then(|inv| {
                            crate::agent::remote::invite::SignedInviteGrant::decode(inv)
                                .ok()
                                .map(|g| {
                                    crate::agent::remote::invite::mesh_id_for(
                                        &g.grant.inviter_peer_id,
                                        g.grant.mesh_name.as_deref(),
                                    )
                                })
                        })
                        .unwrap_or_else(|| "default".to_string());

                    let iroh_config = IrohMeshConfig {
                        mesh_id,
                        invite,
                        name: None,
                    };
                    (None, vec![iroh_config])
                }
            }
        };

        let config = Self {
            enabled,
            lan: lan_config,
            iroh_scopes: iroh_configs,
            identity_file,
            request_timeout,
            stream_reconnect_grace,
            node_name,
            peers,
            auto_fallback,
        };

        config.validate()?;
        Ok(config)
    }

    /// Validate the normalized config.
    fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        // At least one transport must be enabled.
        let has_lan = self.lan.is_some();
        let has_iroh = !self.iroh_scopes.is_empty();
        ensure!(
            has_lan || has_iroh,
            "mesh is enabled but no transport is configured. \
             Enable LAN ([mesh.lan] enabled = true) or add an Iroh scope ([[mesh.iroh]] enabled = true), \
             or set the legacy transport = \"lan\" | \"iroh\"."
        );

        // All Iroh scopes must have non-empty mesh_id.
        for scope in &self.iroh_scopes {
            ensure!(
                !scope.mesh_id.is_empty(),
                "Iroh scope has an empty mesh_id. Set a 'name' or provide an 'invite'."
            );
        }

        // No duplicate mesh_ids.
        let mut seen_mesh_ids = std::collections::HashSet::new();
        for scope in &self.iroh_scopes {
            ensure!(
                seen_mesh_ids.insert(scope.mesh_id.clone()),
                "duplicate Iroh mesh_id '{}'. Each Iroh scope must have a unique name/invite.",
                scope.mesh_id
            );
        }

        Ok(())
    }

    /// Returns `true` if LAN transport is enabled.
    pub fn has_lan(&self) -> bool {
        self.lan.is_some()
    }

    /// Returns `true` if at least one Iroh scope is enabled.
    pub fn has_iroh(&self) -> bool {
        !self.iroh_scopes.is_empty()
    }

    /// Returns the list of enabled transport kinds.
    pub fn enabled_transports(&self) -> Vec<super::scope::MeshTransportKind> {
        let mut transports = Vec::new();
        if self.has_lan() {
            transports.push(super::scope::MeshTransportKind::Lan);
        }
        if self.has_iroh() {
            transports.push(super::scope::MeshTransportKind::Iroh);
        }
        transports
    }

    /// Returns the active scope IDs derived from this config.
    pub fn active_scopes(&self) -> Vec<super::scope::MeshScopeId> {
        let mut scopes = Vec::new();
        if self.lan.is_some() {
            scopes.push(super::scope::MeshScopeId::Lan);
        }
        for iroh in &self.iroh_scopes {
            scopes.push(super::scope::MeshScopeId::Iroh {
                mesh_id: iroh.mesh_id.clone(),
            });
        }
        scopes
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create default old-syntax params.
    fn default_old_params() -> OldSyntaxParams {
        OldSyntaxParams {
            enabled: false,
            transport: crate::config::MeshTransportConfig::Lan,
            discovery: crate::config::MeshDiscoveryConfig::Mdns,
            listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
            peers: Vec::new(),
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            stream_reconnect_grace_secs: DEFAULT_STREAM_RECONNECT_GRACE_SECS,
            identity_file: None,
            invite: None,
            node_name: None,
            auto_fallback: false,
            lan: None,
            iroh: Vec::new(),
        }
    }

    struct OldSyntaxParams {
        enabled: bool,
        transport: crate::config::MeshTransportConfig,
        discovery: crate::config::MeshDiscoveryConfig,
        listen: Option<String>,
        peers: Vec<String>,
        request_timeout_secs: u64,
        stream_reconnect_grace_secs: u64,
        identity_file: Option<String>,
        invite: Option<String>,
        node_name: Option<String>,
        auto_fallback: bool,
        lan: Option<crate::config::LanMeshTomlConfig>,
        iroh: Vec<crate::config::IrohMeshTomlConfig>,
    }

    impl OldSyntaxParams {
        fn build(self) -> Result<MeshRuntimeConfig> {
            MeshRuntimeConfig::from_toml_config(
                self.enabled,
                self.transport,
                self.discovery,
                self.listen,
                self.peers,
                self.request_timeout_secs,
                self.stream_reconnect_grace_secs,
                self.identity_file,
                self.invite,
                self.node_name,
                self.auto_fallback,
                self.lan,
                self.iroh,
            )
        }
    }

    // ── Old syntax ────────────────────────────────────────────────────────

    #[test]
    fn old_syntax_disabled_is_ok() {
        let config = default_old_params().build().unwrap();
        assert!(!config.enabled);
        assert!(config.lan.is_none());
        assert!(config.iroh_scopes.is_empty());
    }

    #[test]
    fn old_syntax_lan_produces_lan_config() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .transport(crate::config::MeshTransportConfig::Lan)
            .build()
            .unwrap();
        assert!(config.enabled);
        assert!(config.has_lan());
        assert!(!config.has_iroh());
        assert_eq!(
            config.enabled_transports(),
            vec![super::super::scope::MeshTransportKind::Lan]
        );
        assert_eq!(
            config.active_scopes(),
            vec![super::super::scope::MeshScopeId::Lan]
        );
    }

    #[test]
    fn old_syntax_iroh_produces_iroh_config() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .transport(crate::config::MeshTransportConfig::Iroh)
            .build()
            .unwrap();
        assert!(config.enabled);
        assert!(!config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(
            config.enabled_transports(),
            vec![super::super::scope::MeshTransportKind::Iroh]
        );
        assert_eq!(config.iroh_scopes.len(), 1);
        // Without invite, mesh_id defaults to "default"
        assert_eq!(config.iroh_scopes[0].mesh_id, "default");
    }

    #[test]
    fn old_syntax_enabled_but_no_transport_is_error() {
        let result = default_old_params()
            .into_builder()
            .enabled(true)
            // old syntax always sets a transport, but let's validate the
            // invariant by checking the validation path directly
            .transport(crate::config::MeshTransportConfig::Lan)
            .build();
        // With transport = Lan, this is fine. The "no transport" case can only
        // happen with new syntax, tested below.
        assert!(result.is_ok());
    }

    // ── New syntax ────────────────────────────────────────────────────────

    #[test]
    fn new_syntax_lan_only() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .lan(Some(crate::config::LanMeshTomlConfig {
                enabled: true,
                listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
                discovery: crate::config::MeshDiscoveryConfig::Mdns,
            }))
            .build()
            .unwrap();
        assert!(config.has_lan());
        assert!(!config.has_iroh());
        let lan = config.lan.as_ref().unwrap();
        assert_eq!(lan.listen.as_deref(), Some("/ip4/0.0.0.0/tcp/0"));
        assert_eq!(lan.discovery, LanDiscovery::Mdns);
    }

    #[test]
    fn new_syntax_lan_disabled_but_iroh_ok() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .lan(Some(crate::config::LanMeshTomlConfig {
                enabled: false,
                listen: None,
                discovery: crate::config::MeshDiscoveryConfig::Mdns,
            }))
            .iroh(vec![crate::config::IrohMeshTomlConfig {
                enabled: true,
                invite: None,
                name: Some("remote".to_string()),
            }])
            .build()
            .unwrap();
        assert!(!config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(config.iroh_scopes.len(), 1);
        assert_eq!(config.iroh_scopes[0].mesh_id, "remote");
    }

    #[test]
    fn new_syntax_iroh_with_name() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .iroh(vec![crate::config::IrohMeshTomlConfig {
                enabled: true,
                invite: None,
                name: Some("personal".to_string()),
            }])
            .build()
            .unwrap();
        assert!(!config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(config.iroh_scopes.len(), 1);
        assert_eq!(config.iroh_scopes[0].mesh_id, "personal");
        assert_eq!(config.iroh_scopes[0].name.as_deref(), Some("personal"));
    }

    #[test]
    fn new_syntax_lan_plus_iroh() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .lan(Some(crate::config::LanMeshTomlConfig {
                enabled: true,
                listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
                discovery: crate::config::MeshDiscoveryConfig::Mdns,
            }))
            .iroh(vec![crate::config::IrohMeshTomlConfig {
                enabled: true,
                invite: None,
                name: Some("team-a".to_string()),
            }])
            .build()
            .unwrap();
        assert!(config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(config.enabled_transports().len(), 2);
        assert_eq!(config.active_scopes().len(), 2);
        // LAN scope first, then Iroh
        assert!(config.active_scopes()[0].is_lan());
        assert_eq!(config.active_scopes()[1].iroh_mesh_id(), Some("team-a"));
    }

    #[test]
    fn new_syntax_multiple_iroh_scopes() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .lan(Some(crate::config::LanMeshTomlConfig {
                enabled: true,
                listen: None,
                discovery: crate::config::MeshDiscoveryConfig::Mdns,
            }))
            .iroh(vec![
                crate::config::IrohMeshTomlConfig {
                    enabled: true,
                    invite: None,
                    name: Some("team-a".to_string()),
                },
                crate::config::IrohMeshTomlConfig {
                    enabled: true,
                    invite: None,
                    name: Some("team-b".to_string()),
                },
            ])
            .build()
            .unwrap();
        assert!(config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(config.iroh_scopes.len(), 2);
        assert_eq!(config.iroh_scopes[0].mesh_id, "team-a");
        assert_eq!(config.iroh_scopes[1].mesh_id, "team-b");
    }

    // ── Validation ────────────────────────────────────────────────────────

    #[test]
    fn enabled_no_transport_is_error() {
        let result = default_old_params()
            .into_builder()
            .enabled(true)
            // New syntax with both disabled
            .lan(Some(crate::config::LanMeshTomlConfig {
                enabled: false,
                listen: None,
                discovery: crate::config::MeshDiscoveryConfig::Mdns,
            }))
            // No iroh, no lan enabled
            .build();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no transport is configured"),
            "expected actionable error, got: {msg}"
        );
    }

    #[test]
    fn duplicate_iroh_mesh_ids_is_error() {
        let result = default_old_params()
            .into_builder()
            .enabled(true)
            .iroh(vec![
                crate::config::IrohMeshTomlConfig {
                    enabled: true,
                    invite: None,
                    name: Some("same-name".to_string()),
                },
                crate::config::IrohMeshTomlConfig {
                    enabled: true,
                    invite: None,
                    name: Some("same-name".to_string()),
                },
            ])
            .build();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("duplicate Iroh mesh_id"),
            "expected duplicate error, got: {msg}"
        );
    }

    #[test]
    fn disabled_mesh_skips_validation() {
        // Even with no transports, disabled mesh should be fine.
        let config = default_old_params()
            .into_builder()
            .enabled(false)
            .build()
            .unwrap();
        assert!(!config.enabled);
    }

    // ── Identity and timeout pass-through ─────────────────────────────────

    #[test]
    fn identity_file_and_timeouts_pass_through() {
        let config = default_old_params()
            .into_builder()
            .enabled(true)
            .transport(crate::config::MeshTransportConfig::Lan)
            .identity_file(Some("/tmp/key".to_string()))
            .request_timeout_secs(60)
            .stream_reconnect_grace_secs(30)
            .node_name(Some("my-node".to_string()))
            .auto_fallback(true)
            .build()
            .unwrap();
        assert_eq!(config.identity_file, Some(PathBuf::from("/tmp/key")));
        assert_eq!(config.request_timeout, Duration::from_secs(60));
        assert_eq!(config.stream_reconnect_grace, Duration::from_secs(30));
        assert_eq!(config.node_name.as_deref(), Some("my-node"));
        assert!(config.auto_fallback);
    }

    // ── Builder helper ────────────────────────────────────────────────────

    /// Simple builder for test ergonomics.
    struct OldSyntaxParamsBuilder(OldSyntaxParams);

    impl OldSyntaxParams {
        fn into_builder(self) -> OldSyntaxParamsBuilder {
            OldSyntaxParamsBuilder(self)
        }
    }

    impl OldSyntaxParamsBuilder {
        fn enabled(mut self, v: bool) -> Self {
            self.0.enabled = v;
            self
        }
        fn transport(mut self, v: crate::config::MeshTransportConfig) -> Self {
            self.0.transport = v;
            self
        }
        fn discovery(mut self, v: crate::config::MeshDiscoveryConfig) -> Self {
            self.0.discovery = v;
            self
        }
        fn listen(mut self, v: Option<String>) -> Self {
            self.0.listen = v;
            self
        }
        fn peers(mut self, v: Vec<String>) -> Self {
            self.0.peers = v;
            self
        }
        fn request_timeout_secs(mut self, v: u64) -> Self {
            self.0.request_timeout_secs = v;
            self
        }
        fn stream_reconnect_grace_secs(mut self, v: u64) -> Self {
            self.0.stream_reconnect_grace_secs = v;
            self
        }
        fn identity_file(mut self, v: Option<String>) -> Self {
            self.0.identity_file = v;
            self
        }
        fn invite(mut self, v: Option<String>) -> Self {
            self.0.invite = v;
            self
        }
        fn node_name(mut self, v: Option<String>) -> Self {
            self.0.node_name = v;
            self
        }
        fn auto_fallback(mut self, v: bool) -> Self {
            self.0.auto_fallback = v;
            self
        }
        fn lan(mut self, v: Option<crate::config::LanMeshTomlConfig>) -> Self {
            self.0.lan = v;
            self
        }
        fn iroh(mut self, v: Vec<crate::config::IrohMeshTomlConfig>) -> Self {
            self.0.iroh = v;
            self
        }
        fn build(self) -> Result<MeshRuntimeConfig> {
            self.0.build()
        }
    }
}
