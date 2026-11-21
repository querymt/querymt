//! Public mesh-facing types and lightweight constructors.

#[cfg(feature = "remote")]
use anyhow::Result;
#[cfg(feature = "remote")]
use std::sync::Arc;

#[cfg(feature = "remote")]
use crate::agent::LocalAgentHandle;
#[cfg(feature = "remote")]
use crate::config::{LanMeshTomlConfig, MeshTomlConfig, MeshTransportConfig};

#[cfg(feature = "remote")]
use crate::agent::remote::MeshRuntimeHandle;

#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
pub enum MeshSpec {
    Disabled,
    Lan,
    Iroh,
    Hybrid,
    Toml(Box<MeshTomlConfig>),
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug)]
pub enum MeshSpec {
    Disabled,
}

#[cfg(feature = "remote")]
#[derive(Clone)]
pub struct AgentMesh {
    runtime: MeshRuntimeHandle,
    agent: Arc<LocalAgentHandle>,
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug, Default)]
pub struct AgentMesh;

#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
pub struct MeshJoinOutcome {
    pub mesh_id: String,
    pub mesh_name: Option<String>,
    pub inviter_peer_id: String,
    pub already_joined: bool,
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug, Default)]
pub struct MeshJoinOutcome {
    pub mesh_id: String,
    pub mesh_name: Option<String>,
    pub inviter_peer_id: String,
    pub already_joined: bool,
}

#[cfg(feature = "remote")]
impl MeshSpec {
    pub(crate) fn into_toml(self) -> MeshTomlConfig {
        match self {
            MeshSpec::Disabled => MeshTomlConfig::default(),
            MeshSpec::Lan => {
                let mut cfg = MeshTomlConfig {
                    enabled: true,
                    ..MeshTomlConfig::default()
                };
                cfg.lan = Some(LanMeshTomlConfig {
                    enabled: true,
                    listen: cfg.listen.clone(),
                    discovery: cfg.discovery.clone(),
                });
                cfg
            }
            MeshSpec::Iroh => MeshTomlConfig {
                enabled: true,
                transport: MeshTransportConfig::Iroh,
                ..MeshTomlConfig::default()
            },
            MeshSpec::Hybrid => {
                let mut cfg = MeshTomlConfig {
                    enabled: true,
                    transport: MeshTransportConfig::Iroh,
                    ..MeshTomlConfig::default()
                };
                cfg.lan = Some(LanMeshTomlConfig {
                    enabled: true,
                    listen: cfg.listen.clone(),
                    discovery: cfg.discovery.clone(),
                });
                cfg.invite = None;
                cfg
            }
            MeshSpec::Toml(cfg) => *cfg,
        }
    }
}

#[cfg(feature = "remote")]
impl AgentMesh {
    #[doc(hidden)]
    pub fn new(runtime: MeshRuntimeHandle, agent: Arc<LocalAgentHandle>) -> Self {
        Self { runtime, agent }
    }

    pub fn runtime(&self) -> &MeshRuntimeHandle {
        &self.runtime
    }

    pub async fn ensure_published(&self) -> Result<()> {
        self.agent.ensure_mesh_published(None).await
    }

    pub async fn join(&self, invite: impl AsRef<str>) -> Result<MeshJoinOutcome> {
        self.ensure_published().await?;
        let invite = crate::agent::remote::invite::SignedInviteGrant::decode(invite.as_ref())?;
        self.agent.join_mesh_invite(invite).await
    }
}
