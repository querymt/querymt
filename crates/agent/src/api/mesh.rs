use anyhow::{Result, anyhow};
use std::sync::{Arc, Mutex, OnceLock};

use crate::agent::LocalAgentHandle;
#[cfg(feature = "remote")]
use crate::config::RemoteAgentConfig;

#[cfg(feature = "remote")]
use crate::agent::remote::mesh_runtime_config::MeshRuntimeConfig;
#[cfg(feature = "remote")]
use crate::agent::remote::{MeshRuntimeHandle, MeshScopeId, bootstrap_mesh_runtime};
#[cfg(feature = "remote")]
use crate::config::{LanMeshTomlConfig, MeshTomlConfig, MeshTransportConfig};

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
#[derive(Clone, Debug)]
struct SharedMeshState {
    runtime: MeshRuntimeHandle,
    spec: MeshSpec,
}

#[cfg(feature = "remote")]
static SHARED_MESH: OnceLock<Mutex<Option<SharedMeshState>>> = OnceLock::new();

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
impl MeshSpec {
    fn node_name(&self) -> Option<String> {
        match self {
            MeshSpec::Toml(cfg) => cfg.node_name.clone(),
            _ => None,
        }
    }

    pub fn into_toml(self) -> MeshTomlConfig {
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

    fn compatibility_label(&self) -> &'static str {
        match self {
            MeshSpec::Disabled => "disabled",
            MeshSpec::Lan => "lan",
            MeshSpec::Iroh => "iroh",
            MeshSpec::Hybrid => "hybrid",
            MeshSpec::Toml(cfg) => {
                if !cfg.enabled {
                    "disabled"
                } else if cfg.lan.as_ref().is_some_and(|lan| lan.enabled)
                    && (cfg.transport == MeshTransportConfig::Iroh
                        || cfg.invite.is_some()
                        || cfg.iroh.iter().any(|iroh| iroh.enabled))
                {
                    "hybrid"
                } else if cfg.transport == MeshTransportConfig::Iroh
                    || cfg.invite.is_some()
                    || cfg.iroh.iter().any(|iroh| iroh.enabled)
                {
                    "iroh"
                } else {
                    "lan"
                }
            }
        }
    }
}

#[cfg(feature = "remote")]
impl MeshRuntime {
    pub async fn shared(spec: MeshSpec) -> Result<Self> {
        let state = SHARED_MESH.get_or_init(|| Mutex::new(None));

        {
            let guard = state.lock().unwrap();
            if let Some(existing) = guard.as_ref() {
                if !spec_compatible(&existing.spec, &spec) {
                    return Err(anyhow!(
                        "shared mesh runtime already started as {}; requested {}",
                        existing.spec.compatibility_label(),
                        spec.compatibility_label()
                    ));
                }
                return Ok(Self {
                    runtime: existing.runtime.clone(),
                });
            }
        }

        if matches!(spec, MeshSpec::Disabled) {
            return Err(anyhow!(
                "cannot start a shared mesh runtime from Mesh::disabled()"
            ));
        }

        let runtime_cfg = runtime_config_from_spec(spec.clone())?;
        let runtime = bootstrap_mesh_runtime(&runtime_cfg).await?;
        let cloned = runtime.clone();
        *state.lock().unwrap() = Some(SharedMeshState { runtime, spec });
        Ok(Self { runtime: cloned })
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
        let inviter_peer_id = invite.grant.inviter_peer_id.clone();
        let mesh_name = invite.grant.mesh_name.clone();
        let mesh_id = crate::agent::remote::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        );

        let already_joined = self
            .runtime
            .joined_iroh_scopes()
            .into_iter()
            .any(|scope| matches!(scope, MeshScopeId::Iroh { mesh_id: ref existing } if existing == &mesh_id));

        if !already_joined {
            self.agent.join_mesh_invite(invite.clone()).await?;
        }

        Ok(MeshJoinOutcome {
            mesh_id,
            mesh_name,
            inviter_peer_id,
            already_joined,
        })
    }
}

#[cfg(feature = "remote")]
fn spec_compatible(existing: &MeshSpec, requested: &MeshSpec) -> bool {
    match (
        existing.compatibility_label(),
        requested.compatibility_label(),
    ) {
        ("hybrid", _) => true,
        (a, b) => a == b,
    }
}

#[cfg(feature = "remote")]
fn runtime_config_from_spec(spec: MeshSpec) -> Result<MeshRuntimeConfig> {
    let cfg = spec.into_toml();
    MeshRuntimeConfig::from_toml_config(
        cfg.enabled,
        cfg.transport,
        cfg.discovery,
        cfg.listen,
        cfg.peers.into_iter().map(|p| p.addr).collect(),
        cfg.request_timeout_secs,
        cfg.stream_reconnect_grace_secs,
        cfg.identity_file,
        cfg.invite,
        cfg.node_name,
        cfg.auto_fallback,
        cfg.lan,
        cfg.iroh,
    )
}

#[cfg(feature = "remote")]
pub async fn admit_via_invite_on_runtime(
    mesh: &mut crate::agent::remote::MeshHandle,
    invite: &crate::agent::remote::invite::SignedInviteGrant,
) -> Result<()> {
    use crate::agent::remote::node_manager::{AdmissionRequest, AdmissionResponse};

    invite.verify().map_err(|e| anyhow!(e.to_string()))?;
    let mesh_id = crate::agent::remote::invite::mesh_id_for(
        &invite.grant.inviter_peer_id,
        invite.grant.mesh_name.as_deref(),
    );

    let mesh_state_path = crate::agent::remote::mesh_state::default_mesh_state_path()?;
    let mut mesh_state =
        crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&mesh_state_path)
            .map_err(|e| anyhow!(e.to_string()))?;

    let (existing_token, fallback_peers) = match mesh_state.get(&mesh_id) {
        Some(entry)
            if entry.status == crate::agent::remote::mesh_state::MeshStatus::Active
                && entry
                    .membership_token
                    .as_ref()
                    .is_some_and(|token| !token.is_expired()) =>
        {
            (
                entry.membership_token.clone(),
                entry.known_peers.values().cloned().collect(),
            )
        }
        _ => (None, vec![]),
    };

    let request = match existing_token {
        Some(token) => AdmissionRequest::Token {
            membership_token: token,
            peer_id: mesh.peer_id().to_string(),
        },
        None => AdmissionRequest::Invite {
            invite_id: invite.grant.invite_id.clone(),
            mesh_name: invite.grant.mesh_name.clone(),
            peer_id: mesh.peer_id().to_string(),
        },
    };

    let target_nm = crate::agent::remote::mesh::find_admission_target(
        mesh,
        &invite.grant.inviter_peer_id,
        &fallback_peers,
    )
    .await
    .ok_or_else(|| anyhow!("no reachable peer found for admission handshake"))?;

    let response = target_nm
        .ask::<AdmissionRequest>(&request)
        .await
        .map_err(|e| anyhow!("admission handshake failed: {e}"))?;

    match response {
        AdmissionResponse::Admitted {
            membership_token,
            existing_peers,
        } => {
            let known_peers = known_peers_from_strings(mesh, &existing_peers);
            mesh_state
                .upsert_joined_mesh(membership_token, known_peers)
                .map_err(|e| anyhow!("failed to persist mesh state: {e}"))?;
        }
        AdmissionResponse::Readmitted { existing_peers } => {
            let known_peers = known_peers_from_strings(mesh, &existing_peers);
            mesh_state
                .update_known_peers(&mesh_id, known_peers)
                .map_err(|e| anyhow!("failed to update mesh state: {e}"))?;
        }
        AdmissionResponse::Rejected { reason } => {
            return Err(anyhow!("admission rejected: {reason}"));
        }
    }

    if let Some(store_arc) = mesh.mesh_state_store() {
        let fresh =
            crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&mesh_state_path)
                .map_err(|e| anyhow!(e.to_string()))?;
        *store_arc.write() = fresh;
    }
    mesh.ensure_scope(MeshScopeId::Iroh {
        mesh_id: mesh_id.clone(),
    });
    let _ = mesh.subscribe_peer_events().resubscribe().try_recv();
    Ok(())
}

#[cfg(feature = "remote")]
fn known_peers_from_strings(
    mesh: &crate::agent::remote::MeshHandle,
    existing_peers: &[String],
) -> Vec<crate::agent::remote::invite::PeerEntry> {
    let mut all_peer_strs: Vec<String> = mesh
        .known_peer_ids()
        .into_iter()
        .map(|pid| pid.to_string())
        .collect();
    for peer_str in existing_peers {
        if let Ok(pid) = peer_str.parse() {
            mesh.dial_peer(&pid);
        }
        if !all_peer_strs.contains(peer_str) {
            all_peer_strs.push(peer_str.clone());
        }
    }
    all_peer_strs
        .into_iter()
        .map(|pid| crate::agent::remote::invite::PeerEntry {
            peer_id: pid.clone(),
            addrs: vec![format!("/p2p/{pid}")],
        })
        .collect()
}
