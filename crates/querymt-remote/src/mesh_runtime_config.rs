use crate::scope::{MeshScopeId, MeshTransportKind};
use anyhow::{Result, ensure};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshRuntimeConfig {
    pub enabled: bool,
    pub lan: Option<LanMeshConfig>,
    pub iroh_enabled: bool,
    pub iroh_scopes: Vec<IrohMeshConfig>,
    pub identity_file: Option<PathBuf>,
    pub request_timeout: Duration,
    pub stream_reconnect_grace: Duration,
    pub node_name: Option<String>,
    pub peers: Vec<String>,
    pub auto_fallback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanMeshConfig {
    pub listen: Option<String>,
    pub discovery: LanDiscovery,
    pub directory: DirectoryMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanDiscovery {
    Mdns,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryMode {
    #[default]
    Kademlia,
    Cached,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrohMeshConfig {
    pub mesh_id: String,
    pub invite: Option<String>,
    pub name: Option<String>,
}

impl MeshRuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let has_lan = self.lan.is_some();
        let has_iroh = self.iroh_enabled;
        ensure!(
            has_lan || has_iroh,
            "mesh is enabled but no transport is configured. Enable LAN ([mesh.lan] enabled = true) or add an Iroh scope ([[mesh.iroh]] enabled = true), or set the legacy transport = \"lan\" | \"iroh\"."
        );

        let mut seen_mesh_ids = std::collections::HashSet::new();
        for scope in &self.iroh_scopes {
            ensure!(
                !scope.mesh_id.is_empty(),
                "Iroh scope has an empty mesh_id. Set a 'name' or provide an 'invite'."
            );
            ensure!(
                seen_mesh_ids.insert(scope.mesh_id.clone()),
                "duplicate Iroh mesh_id '{}'. Each Iroh scope must have a unique name/invite.",
                scope.mesh_id
            );
        }

        Ok(())
    }

    pub fn has_lan(&self) -> bool {
        self.lan.is_some()
    }

    pub fn has_iroh(&self) -> bool {
        self.iroh_enabled
    }

    pub fn enabled_transports(&self) -> Vec<MeshTransportKind> {
        let mut transports = Vec::new();
        if self.has_lan() {
            transports.push(MeshTransportKind::Lan);
        }
        if self.has_iroh() {
            transports.push(MeshTransportKind::Iroh);
        }
        transports
    }

    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        let mut scopes = Vec::new();
        if self.lan.is_some() {
            scopes.push(MeshScopeId::lan_default());
        }
        for iroh in &self.iroh_scopes {
            scopes.push(MeshScopeId::Iroh {
                mesh_id: iroh.mesh_id.clone(),
            });
        }
        scopes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> MeshRuntimeConfig {
        MeshRuntimeConfig {
            enabled: true,
            lan: None,
            iroh_enabled: false,
            iroh_scopes: Vec::new(),
            identity_file: None,
            request_timeout: Duration::from_secs(300),
            stream_reconnect_grace: Duration::from_secs(120),
            node_name: None,
            peers: Vec::new(),
            auto_fallback: false,
        }
    }

    #[test]
    fn disabled_config_skips_transport_validation() {
        let mut config = base_config();
        config.enabled = false;

        assert!(config.validate().is_ok());
    }

    #[test]
    fn enabled_config_requires_at_least_one_transport() {
        let err = base_config()
            .validate()
            .expect_err("missing transport should fail");
        assert!(err.to_string().contains("no transport is configured"));
    }

    #[test]
    fn duplicate_iroh_mesh_ids_are_rejected() {
        let mut config = base_config();
        config.iroh_enabled = true;
        config.iroh_scopes = vec![
            IrohMeshConfig {
                mesh_id: "mesh-a".to_string(),
                invite: None,
                name: Some("A".to_string()),
            },
            IrohMeshConfig {
                mesh_id: "mesh-a".to_string(),
                invite: None,
                name: Some("B".to_string()),
            },
        ];

        let err = config
            .validate()
            .expect_err("duplicate mesh ids should fail");
        assert!(err.to_string().contains("duplicate Iroh mesh_id 'mesh-a'"));
    }

    #[test]
    fn empty_iroh_mesh_id_is_rejected() {
        let mut config = base_config();
        config.iroh_enabled = true;
        config.iroh_scopes = vec![IrohMeshConfig {
            mesh_id: String::new(),
            invite: None,
            name: None,
        }];

        let err = config.validate().expect_err("empty mesh id should fail");
        assert!(err.to_string().contains("empty mesh_id"));
    }

    #[test]
    fn enabled_transport_queries_reflect_config() {
        let mut config = base_config();
        config.lan = Some(LanMeshConfig {
            listen: Some("/ip4/0.0.0.0/tcp/0".to_string()),
            discovery: LanDiscovery::Mdns,
            directory: DirectoryMode::Cached,
        });
        config.iroh_enabled = true;

        assert!(config.has_lan());
        assert!(config.has_iroh());
        assert_eq!(
            config.enabled_transports(),
            vec![MeshTransportKind::Lan, MeshTransportKind::Iroh]
        );
    }

    #[test]
    fn active_scopes_include_lan_then_each_iroh_scope() {
        let mut config = base_config();
        config.lan = Some(LanMeshConfig {
            listen: None,
            discovery: LanDiscovery::None,
            directory: DirectoryMode::Kademlia,
        });
        config.iroh_enabled = true;
        config.iroh_scopes = vec![
            IrohMeshConfig {
                mesh_id: "mesh-a".to_string(),
                invite: None,
                name: Some("Mesh A".to_string()),
            },
            IrohMeshConfig {
                mesh_id: "mesh-b".to_string(),
                invite: None,
                name: Some("Mesh B".to_string()),
            },
        ];

        assert_eq!(
            config.active_scopes(),
            vec![
                MeshScopeId::lan_default(),
                MeshScopeId::Iroh {
                    mesh_id: "mesh-a".to_string()
                },
                MeshScopeId::Iroh {
                    mesh_id: "mesh-b".to_string()
                },
            ]
        );
    }
}
