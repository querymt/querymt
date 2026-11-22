//! High-level mesh startup with the agent's persistent policy store.

#[cfg(feature = "remote")]
use anyhow::Result;
#[cfg(feature = "remote")]
use querymt_remote::MeshRuntimeHandle;

#[cfg(feature = "remote")]
use crate::config::{MeshTomlConfig, RemoteAgentConfig};

pub use querymt_agent_runtime::api::mesh::{AgentMesh, MeshJoinOutcome, MeshSpec};
#[cfg(feature = "remote")]
pub use querymt_agent_runtime::api::mesh::{invite, runtime, runtime_config};

pub mod types {
    pub use super::{AgentMesh, Mesh, MeshJoinOutcome, MeshRuntime, MeshSpec};
}

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
pub struct MeshRuntime {
    runtime: MeshRuntimeHandle,
}

#[cfg(not(feature = "remote"))]
#[derive(Clone, Debug, Default)]
pub struct MeshRuntime;

#[cfg(feature = "remote")]
impl Mesh {
    fn from_spec(spec: MeshSpec) -> Self {
        Self {
            spec,
            remote_agents: Vec::new(),
        }
    }

    pub fn disabled() -> Self {
        Self::from_spec(MeshSpec::Disabled)
    }

    pub fn lan() -> Self {
        Self::from_spec(MeshSpec::Lan)
    }

    pub fn iroh() -> Self {
        Self::from_spec(MeshSpec::Iroh)
    }

    pub fn hybrid() -> Self {
        Self::from_spec(MeshSpec::Hybrid)
    }

    pub fn from_toml(config: MeshTomlConfig) -> Self {
        Self::from_spec(MeshSpec::Toml(Box::new(config)))
    }

    /// Builds the default shared mesh configuration used by the high-level API.
    pub fn shared() -> Self {
        Self::hybrid()
    }

    pub fn with_remote_agents(mut self, remote_agents: Vec<RemoteAgentConfig>) -> Self {
        self.remote_agents = remote_agents;
        self
    }

    #[doc(hidden)]
    pub fn remote_agents(&self) -> &[RemoteAgentConfig] {
        &self.remote_agents
    }

    #[doc(hidden)]
    pub fn spec_for_internal_use(&self) -> &MeshSpec {
        &self.spec
    }

    #[doc(hidden)]
    pub fn node_name(&self) -> Option<String> {
        match &self.spec {
            MeshSpec::Toml(cfg) => cfg.node_name.clone(),
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn is_disabled(&self) -> bool {
        matches!(self.spec, MeshSpec::Disabled)
    }

    pub async fn start(&self) -> Result<MeshRuntime> {
        MeshRuntime::shared(self.spec.clone()).await
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
impl MeshRuntime {
    /// Starts or reuses the process-wide shared mesh runtime.
    pub async fn shared(spec: MeshSpec) -> Result<Self> {
        let runtime = runtime::start_shared_runtime(spec).await?;
        Ok(Self { runtime })
    }

    pub fn handle(&self) -> MeshRuntimeHandle {
        self.runtime.clone()
    }
}
