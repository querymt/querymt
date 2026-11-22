//! Public mesh API plus remote-only support helpers split into focused submodules.

#[cfg(feature = "remote")]
pub mod invite;
#[cfg(feature = "remote")]
pub mod runtime;
#[cfg(feature = "remote")]
pub mod runtime_config;
pub mod types {
    pub use super::{AgentMesh, MeshJoinOutcome, MeshSpec};
}

#[cfg(feature = "remote")]
pub(crate) use invite::admit_via_invite_on_runtime;

#[cfg(feature = "remote")]
use crate::{
    agent::{LocalAgentHandle, remote::MeshRuntimeHandle},
    config::{LanMeshTomlConfig, MeshTomlConfig, MeshTransportConfig},
};
#[cfg(feature = "remote")]
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum MeshSpec {
    Disabled,
    #[cfg(feature = "remote")]
    Lan,
    #[cfg(feature = "remote")]
    Iroh,
    #[cfg(feature = "remote")]
    Hybrid,
    #[cfg(feature = "remote")]
    Toml(Box<MeshTomlConfig>),
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

#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "remote"), derive(Default))]
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
            MeshSpec::Lan | MeshSpec::Hybrid => {
                let mut cfg = MeshTomlConfig {
                    enabled: true,
                    ..MeshTomlConfig::default()
                };
                cfg.lan = Some(LanMeshTomlConfig {
                    enabled: true,
                    listen: cfg.listen.clone(),
                    discovery: cfg.discovery.clone(),
                });
                if matches!(self, MeshSpec::Hybrid) {
                    cfg.transport = MeshTransportConfig::Iroh;
                    cfg.invite = None;
                }
                cfg
            }
            MeshSpec::Iroh => MeshTomlConfig {
                enabled: true,
                transport: MeshTransportConfig::Iroh,
                ..MeshTomlConfig::default()
            },
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

    pub async fn ensure_published(&self) -> anyhow::Result<()> {
        self.agent.ensure_mesh_published(None).await
    }

    pub async fn join(&self, invite: impl AsRef<str>) -> anyhow::Result<MeshJoinOutcome> {
        self.ensure_published().await?;
        let invite = crate::agent::remote::invite::SignedInviteGrant::decode(invite.as_ref())?;
        self.agent.join_mesh_invite(invite).await
    }
}
