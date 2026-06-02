//! Public mesh-facing types and lightweight constructors.

#[cfg(feature = "remote")]
use anyhow::Result;
#[cfg(feature = "remote")]
use std::sync::Arc;

#[cfg(feature = "remote")]
use crate::agent::LocalAgentHandle;
#[cfg(feature = "remote")]
use crate::config::RemoteAgentConfig;
#[cfg(feature = "remote")]
use crate::config::{LanMeshTomlConfig, MeshTomlConfig, MeshTransportConfig};

#[cfg(feature = "remote")]
use crate::agent::remote::MeshRuntimeHandle;

#[cfg(feature = "remote")]
use super::runtime::start_shared_runtime;

#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
pub struct Mesh {
    spec: MeshSpec,
    remote_agents: Vec<RemoteAgentConfig>,
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug, Default)]
pub struct Mesh;

#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
pub enum MeshSpec {
    Disabled,
    Lan,
    Iroh,
    Hybrid,
    Toml(MeshTomlConfig),
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug)]
pub enum MeshSpec {
    Disabled,
}

#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
pub struct MeshRuntime {
    runtime: MeshRuntimeHandle,
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug, Default)]
pub struct MeshRuntime;

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
impl Mesh {
    pub fn disabled() -> Self {
        Self {
            spec: MeshSpec::Disabled,
            remote_agents: Vec::new(),
        }
    }

    pub fn lan() -> Self {
        Self {
            spec: MeshSpec::Lan,
            remote_agents: Vec::new(),
        }
    }

    pub fn iroh() -> Self {
        Self {
            spec: MeshSpec::Iroh,
            remote_agents: Vec::new(),
        }
    }

    pub fn hybrid() -> Self {
        Self {
            spec: MeshSpec::Hybrid,
            remote_agents: Vec::new(),
        }
    }

    pub fn from_toml(config: MeshTomlConfig) -> Self {
        Self {
            spec: MeshSpec::Toml(config),
            remote_agents: Vec::new(),
        }
    }

    /// Builds the default shared mesh configuration used by the high-level API.
    pub fn shared() -> Self {
        Self::hybrid()
    }

    pub fn with_remote_agents(mut self, remote_agents: Vec<RemoteAgentConfig>) -> Self {
        self.remote_agents = remote_agents;
        self
    }

    pub(crate) fn remote_agents(&self) -> &[RemoteAgentConfig] {
        &self.remote_agents
    }

    pub(crate) fn spec_for_internal_use(&self) -> &MeshSpec {
        &self.spec
    }

    pub(crate) fn node_name(&self) -> Option<String> {
        self.spec.node_name()
    }

    pub(crate) fn is_disabled(&self) -> bool {
        matches!(self.spec, MeshSpec::Disabled)
    }

    pub async fn start(&self) -> Result<MeshRuntime> {
        start_shared_runtime(self.spec.clone()).await
    }
}

#[cfg(not(feature = "remote"))]
impl Mesh {
    pub fn disabled() -> Self {
        Self
    }

    pub fn lan() -> Self {
        Self
    }

    pub fn iroh() -> Self {
        Self
    }

    pub fn hybrid() -> Self {
        Self
    }

    pub fn shared() -> Self {
        Self
    }
}

#[cfg(feature = "remote")]
impl MeshSpec {
    pub(crate) fn node_name(&self) -> Option<String> {
        match self {
            MeshSpec::Toml(cfg) => cfg.node_name.clone(),
            _ => None,
        }
    }

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
            MeshSpec::Toml(cfg) => cfg,
        }
    }
}

#[cfg(feature = "remote")]
impl MeshRuntime {
    /// Starts or reuses the process-wide shared mesh runtime.
    pub async fn shared(spec: MeshSpec) -> Result<Self> {
        start_shared_runtime(spec).await
    }

    pub(crate) fn from_handle(runtime: MeshRuntimeHandle) -> Self {
        Self { runtime }
    }

    pub fn handle(&self) -> MeshRuntimeHandle {
        self.runtime.clone()
    }
}

#[cfg(feature = "remote")]
impl AgentMesh {
    pub(crate) fn new(runtime: MeshRuntimeHandle, agent: Arc<LocalAgentHandle>) -> Self {
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
