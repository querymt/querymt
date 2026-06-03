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
