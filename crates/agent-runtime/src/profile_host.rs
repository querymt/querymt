//! Runtime-side profile contracts.
//!
//! The profile manager lives in the top crate because it builds `api::Agent`.
//! Runtime code only needs the data types, the catalog trait and the
//! [`ProfileRuntimeHost`] abstraction defined here.

use crate::agent::LocalAgentHandle;
use crate::config::Config;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Existing config shape that a profile resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileConfigKind {
    Single,
    Quorum,
}

/// Location of a profile config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSource {
    Embedded { key: String },
    EmbeddedToml { key: String },
    LocalPath { path: PathBuf },
}

/// Metadata exposed by the profile catalog without requiring full config loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileMetadata {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub source: ProfileSource,
    pub config_kind: Option<ProfileConfigKind>,
    pub fingerprint: Option<String>,
}

/// Optional top-level `[profile]` TOML metadata used only by local catalogs.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileProviderMode {
    #[default]
    Legacy,
    Overlay,
    Locked,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileProviderRequirement {
    pub artifact: String,
    pub factory: String,
    #[serde(default = "default_native_provider_kind")]
    pub kind: String,
    #[serde(default)]
    pub plugin_api: Option<u32>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub signer: Option<String>,
}

fn default_native_provider_kind() -> String {
    "native".to_string()
}

/// Loaded profile document backed by the existing config model.
#[derive(Debug)]
pub struct ProfileDocument {
    pub metadata: ProfileMetadata,
    pub provider_mode: ProfileProviderMode,
    pub providers: BTreeMap<String, ProfileProviderRequirement>,
    pub config: Config,
}

/// Binding between a session id and the profile runtime that owns it.
///
/// This matters because resume/routing code must know which profile runtime and
/// concrete agent own a session. Root sessions leave `agent_id` unset; delegated
/// sessions name the sub-agent that must materialize them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProfileBinding {
    pub profile_id: String,
    pub agent_id: Option<String>,
    pub profile_fingerprint: Option<String>,
    pub profile_source: Option<String>,
    pub profile_config_kind: Option<String>,
    pub provider_lock_digest: Option<String>,
    pub provider_locks_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedProviderLockEntry {
    pub logical_name: String,
    pub factory: String,
    pub artifact: String,
    pub kind: String,
    pub plugin_api: Option<u32>,
    pub features: Vec<String>,
    pub signer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedProfileProviderLock {
    pub digest: String,
    pub providers: BTreeMap<String, ResolvedProviderLockEntry>,
}

impl ProfileMetadata {
    pub fn session_binding(&self) -> SessionProfileBinding {
        SessionProfileBinding {
            profile_id: self.id.clone(),
            agent_id: None,
            profile_fingerprint: self.fingerprint.clone(),
            profile_source: Some(self.source.storage_label()),
            profile_config_kind: self
                .config_kind
                .map(|kind| kind.storage_label().to_string()),
            provider_lock_digest: None,
            provider_locks_json: None,
        }
    }
}

impl ProfileConfigKind {
    pub fn storage_label(self) -> &'static str {
        match self {
            ProfileConfigKind::Single => "single",
            ProfileConfigKind::Quorum => "quorum",
        }
    }
}

impl ProfileSource {
    pub fn storage_label(&self) -> String {
        match self {
            ProfileSource::Embedded { key } | ProfileSource::EmbeddedToml { key } => {
                format!("embedded:{key}")
            }
            ProfileSource::LocalPath { path } => format!("local:{}", path.display()),
        }
    }
}

#[async_trait]
pub trait ProfileCatalog: Send + Sync {
    async fn list_profiles(&self) -> Result<Vec<ProfileMetadata>>;
    async fn load_profile(&self, id: &str) -> Result<ProfileDocument>;

    fn watch_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[async_trait]
impl<T> ProfileCatalog for Arc<T>
where
    T: ProfileCatalog + ?Sized,
{
    async fn list_profiles(&self) -> Result<Vec<ProfileMetadata>> {
        (**self).list_profiles().await
    }

    async fn load_profile(&self, id: &str) -> Result<ProfileDocument> {
        (**self).load_profile(id).await
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        (**self).watch_roots()
    }
}

/// Runtime-side view of a materialized profile runtime.
///
/// Runtime code only reads the root agent handle and delegate handles of a
/// profile runtime; the top crate keeps the full `ProfileRuntime` (which owns an
/// `api::Agent`).
#[derive(Clone)]
pub struct ProfileRuntimeView {
    pub metadata: ProfileMetadata,
    pub handle: Arc<LocalAgentHandle>,
    pub provider_lock: Option<ResolvedProfileProviderLock>,
}

impl ProfileRuntimeView {
    pub fn profile_id(&self) -> &str {
        &self.metadata.id
    }

    pub fn handle(&self) -> Arc<LocalAgentHandle> {
        self.handle.clone()
    }

    /// Local member by id, built on first use (`None` for unknown ids, remote
    /// members and members that cannot be built).
    pub fn delegate(&self, id: &str) -> Option<Arc<LocalAgentHandle>> {
        self.handle.config.roster()?.local_member(id)
    }

    pub fn session_binding(&self) -> SessionProfileBinding {
        let mut binding = self.metadata.session_binding();
        if let Some(lock) = &self.provider_lock {
            binding.provider_lock_digest = Some(lock.digest.clone());
            binding.provider_locks_json = serde_json::to_string(lock).ok();
        }
        binding
    }

    pub fn session_handle(&self, agent_id: Option<&str>) -> Option<Arc<LocalAgentHandle>> {
        match agent_id {
            Some(agent_id) => self.delegate(agent_id),
            None => Some(self.handle.clone()),
        }
    }
}

/// Object-safe interface to the profile runtime manager, implemented by the
/// top crate's `ProfileRuntimeManager`.
#[async_trait]
pub trait ProfileRuntimeHost: Send + Sync {
    async fn list_profiles(&self) -> Result<Vec<ProfileMetadata>>;
    async fn active_profile_id(&self) -> String;
    async fn set_active_profile(&self, profile_id: &str) -> Result<()>;
    async fn active_runtime(&self) -> Result<Arc<ProfileRuntimeView>>;
    async fn runtime_for_profile(&self, profile_id: &str) -> Result<Arc<ProfileRuntimeView>>;
    async fn bind_session_to_runtime(
        &self,
        session_id: &str,
        runtime: &ProfileRuntimeView,
    ) -> Result<SessionProfileBinding>;
    async fn set_session_binding(
        &self,
        session_id: &str,
        binding: SessionProfileBinding,
    ) -> Result<()>;
    async fn session_binding(&self, session_id: &str) -> Option<SessionProfileBinding>;
    async fn adopt_unbound_session(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionProfileBinding>>;
    async fn materialized_runtimes(&self) -> Vec<Arc<ProfileRuntimeView>>;
    async fn mcp_servers(&self) -> Vec<(String, crate::config::McpServerConfig)>;
    #[cfg(feature = "remote")]
    fn set_mesh_handle(&self, mesh: crate::agent::remote::MeshHandle);
    #[cfg(feature = "remote")]
    async fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle);
    async fn forget_session_binding(&self, session_id: &str) -> Option<SessionProfileBinding>;
    async fn shutdown(&self);
}

/// Shared handle to the profile runtime manager.
pub type ProfileRuntimeHandle = Arc<dyn ProfileRuntimeHost>;
