//! Agent-side TOML normalization into the generic `querymt-remote` runtime config.

use anyhow::Result;
pub use querymt_remote::{IrohMeshConfig, LanDiscovery, LanMeshConfig, MeshRuntimeConfig};
use std::path::PathBuf;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
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
    lan: Option<crate::config::LanMeshTomlConfig>,
    iroh: Vec<crate::config::IrohMeshTomlConfig>,
) -> Result<MeshRuntimeConfig> {
    let identity_file = identity_file.map(PathBuf::from);
    let request_timeout = Duration::from_secs(request_timeout_secs);
    let stream_reconnect_grace = Duration::from_secs(stream_reconnect_grace_secs);

    let (lan_config, iroh_enabled, iroh_configs) = if !enabled {
        (None, false, Vec::new())
    } else if lan.is_some() || !iroh.is_empty() {
        let lan_config = match lan {
            Some(l) if l.enabled => {
                let disc = match l.discovery {
                    crate::config::MeshDiscoveryConfig::Mdns => LanDiscovery::Mdns,
                    crate::config::MeshDiscoveryConfig::Kademlia => LanDiscovery::Mdns,
                    crate::config::MeshDiscoveryConfig::None => LanDiscovery::None,
                };
                Some(LanMeshConfig {
                    listen: l.listen.or(listen),
                    discovery: disc,
                    directory: querymt_remote::mesh_runtime_config::DirectoryMode::default(),
                })
            }
            _ => None,
        };

        let mut iroh_configs = Vec::new();
        for iroh_entry in iroh {
            if !iroh_entry.enabled {
                continue;
            }
            let mesh_id = if let Some(name) = iroh_entry.name.clone() {
                name
            } else if let Some(invite) = iroh_entry.invite.as_ref() {
                let invite_grant = crate::agent::remote::invite::SignedInviteGrant::decode(invite)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "invalid Iroh invite for enabled scope without a name: {e}"
                        )
                    })?;
                crate::agent::remote::invite::mesh_id_for(
                    &invite_grant.grant.inviter_peer_id,
                    invite_grant.grant.mesh_name.as_deref(),
                )
            } else {
                anyhow::bail!(
                    "enabled Iroh scope requires either 'name' or a valid 'invite' so mesh_id is stable"
                );
            };

            iroh_configs.push(IrohMeshConfig {
                mesh_id,
                invite: iroh_entry.invite,
                name: iroh_entry.name,
            });
        }

        (
            lan_config,
            transport == crate::config::MeshTransportConfig::Iroh
                || invite.is_some()
                || !iroh_configs.is_empty(),
            iroh_configs,
        )
    } else {
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
                    directory: querymt_remote::mesh_runtime_config::DirectoryMode::default(),
                };
                (Some(lan_config), false, Vec::new())
            }
            crate::config::MeshTransportConfig::Iroh => {
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
                (None, true, vec![iroh_config])
            }
        }
    };

    let config = MeshRuntimeConfig {
        enabled,
        lan: lan_config,
        iroh_enabled,
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
