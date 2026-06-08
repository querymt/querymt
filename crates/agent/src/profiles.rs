//! Profile domain types and a local catalog MVP.
//!
//! Profiles are catalog entries that resolve through the existing config loader.
//! Remote/service catalogs are intentionally out of scope for this MVP.

use crate::api::{Agent, AgentInfra};
use crate::config::{Config, load_config};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tracing::warn;

/// Embedded profile key used for the qmtcode default single-agent profile.
pub const DEFAULT_EMBEDDED_PROFILE_KEY: &str = "default";
const DEFAULT_EMBEDDED_PROFILE_NAME: &str = "Default";
const DEFAULT_EMBEDDED_PROFILE_DESCRIPTION: &str = "Default single coder profile";
const DEFAULT_EMBEDDED_PROFILE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/confs/single_coder.toml"
);
const STANDARD_SINGLE_CODER_CONFIG: &str = include_str!("../examples/confs/single_coder.toml");
const STANDARD_CODER_DELEGATE_CONFIG: &str = include_str!("../examples/confs/coder_delegate.toml");

#[derive(RustEmbed)]
#[folder = "examples/"]
#[include = "prompts/*"]
struct EmbeddedExampleAssets;

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
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ProfileFileMetadata {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
struct ProfileMetadataEnvelope {
    profile: Option<ProfileFileMetadata>,
}

/// Loaded profile document backed by the existing config model.
#[derive(Debug)]
pub struct ProfileDocument {
    pub metadata: ProfileMetadata,
    pub config: Config,
}

/// Binding between a session id and the profile runtime that owns it.
///
/// This matters because resume/routing code must know which profile runtime owns a
/// session; otherwise a session could be resumed under the wrong profile after the
/// user switches profiles. Bindings are cached in memory and backed by a
/// best-effort `profile_bindings` side table that maps session id to profile id.
///
/// The binding is advisory: missing or unavailable persisted profile ids are
/// ignored so callers can fall back to the active profile behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProfileBinding {
    pub profile_id: String,
    pub profile_fingerprint: Option<String>,
    pub profile_source: Option<String>,
    pub profile_config_kind: Option<String>,
}

/// Materialized runtime for a selected profile.
pub struct ProfileRuntime {
    pub metadata: ProfileMetadata,
    pub agent: Agent,
}

impl ProfileRuntime {
    pub fn profile_id(&self) -> &str {
        &self.metadata.id
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn session_binding(&self) -> SessionProfileBinding {
        self.metadata.session_binding()
    }
}

impl ProfileMetadata {
    pub fn session_binding(&self) -> SessionProfileBinding {
        SessionProfileBinding {
            profile_id: self.id.clone(),
            profile_fingerprint: self.fingerprint.clone(),
            profile_source: Some(self.source.storage_label()),
            profile_config_kind: self
                .config_kind
                .map(|kind| kind.storage_label().to_string()),
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
}

/// Builder for a local-only profile catalog.
#[derive(Debug, Clone)]
pub struct ProfileCatalogBuilder {
    include_embedded_default: bool,
    embedded_toml_profiles: BTreeMap<String, EmbeddedTomlProfile>,
    include_default_user_dir: bool,
    default_user_dir_override: Option<PathBuf>,
    local_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct EmbeddedTomlProfile {
    name: String,
    description: Option<String>,
    tags: Vec<String>,
    toml: String,
    config_kind: Option<ProfileConfigKind>,
}

impl Default for ProfileCatalogBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the standard qmtcode/mobile embedded profile catalog.
///
/// The returned builder contains the inlined `default` and `coder-delegate`
/// TOML profiles without adding any user profile directories.
pub fn standard_embedded_profile_catalog_builder() -> Result<ProfileCatalogBuilder> {
    let single_coder = embedded_profile_config("single_coder.toml", STANDARD_SINGLE_CODER_CONFIG)?;
    let coder_delegate =
        embedded_profile_config("coder_delegate.toml", STANDARD_CODER_DELEGATE_CONFIG)?;

    LocalProfileCatalog::builder()
        .include_embedded_default(false)
        .embedded_profile_toml(single_coder)?
        .embedded_profile_toml(coder_delegate)
}

fn embedded_profile_config(config_name: &str, config: &str) -> Result<String> {
    let mut value: toml::Value = toml::from_str(config)
        .with_context(|| format!("Failed to parse embedded {config_name}"))?;
    inline_embedded_system_prompts(&mut value, config_name)?;
    toml::to_string(&value).with_context(|| format!("Failed to serialize embedded {config_name}"))
}

fn inline_embedded_system_prompts(value: &mut toml::Value, config_name: &str) -> Result<()> {
    let root = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("Embedded {config_name} must be a TOML table"))?;

    if let Some(agent) = root.get_mut("agent").and_then(toml::Value::as_table_mut)
        && let Some(system) = agent.get_mut("system")
    {
        inline_system_prompt_value(system, config_name, "[agent].system")?;
    }

    if let Some(planner) = root.get_mut("planner").and_then(toml::Value::as_table_mut)
        && let Some(system) = planner.get_mut("system")
    {
        inline_system_prompt_value(system, config_name, "[planner].system")?;
    }

    if let Some(delegates) = root
        .get_mut("delegates")
        .and_then(toml::Value::as_array_mut)
    {
        for (index, delegate) in delegates.iter_mut().enumerate() {
            if let Some(system) = delegate
                .as_table_mut()
                .and_then(|delegate| delegate.get_mut("system"))
            {
                inline_system_prompt_value(
                    system,
                    config_name,
                    &format!("[[delegates]][{index}].system"),
                )?;
            }
        }
    }

    Ok(())
}

fn inline_system_prompt_value(
    system: &mut toml::Value,
    config_name: &str,
    path: &str,
) -> Result<()> {
    match system {
        toml::Value::String(_) => Ok(()),
        toml::Value::Array(parts) => {
            for part in parts {
                if let toml::Value::Table(table) = part
                    && let Some(file_ref) = table.get("file").and_then(toml::Value::as_str)
                {
                    let asset_key = embedded_prompt_asset_key(file_ref).ok_or_else(|| {
                        anyhow!("Unsupported embedded prompt path '{file_ref}' in {config_name}")
                    })?;

                    let embedded = EmbeddedExampleAssets::get(&asset_key).ok_or_else(|| {
                        anyhow!("Embedded prompt '{file_ref}' not found under examples")
                    })?;

                    let prompt =
                        String::from_utf8(embedded.data.into_owned()).with_context(|| {
                            format!("Embedded prompt '{file_ref}' is not valid UTF-8")
                        })?;

                    *part = toml::Value::String(prompt);
                }
            }
            Ok(())
        }
        _ => Err(anyhow!(
            "Embedded {config_name} has unsupported {path} format"
        )),
    }
}

fn embedded_prompt_asset_key(file_ref: &str) -> Option<String> {
    let joined = Path::new("confs").join(file_ref);
    let mut normalized_parts: Vec<String> = Vec::new();

    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized_parts.pop()?;
            }
            Component::Normal(part) => {
                normalized_parts.push(part.to_string_lossy().into_owned());
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    let normalized = normalized_parts.join("/");
    normalized.starts_with("prompts/").then_some(normalized)
}

impl ProfileCatalogBuilder {
    pub fn new() -> Self {
        Self {
            include_embedded_default: true,
            embedded_toml_profiles: BTreeMap::new(),
            include_default_user_dir: false,
            default_user_dir_override: None,
            local_dirs: Vec::new(),
        }
    }

    pub fn include_embedded_default(mut self, include: bool) -> Self {
        self.include_embedded_default = include;
        self
    }

    pub fn include_default_user_dir(mut self, include: bool) -> Self {
        self.include_default_user_dir = include;
        self
    }

    pub fn default_user_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.default_user_dir_override = Some(path.into());
        self.include_default_user_dir = true;
        self
    }

    pub fn local_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.local_dirs.push(path.into());
        self
    }

    pub fn embedded_config_toml(
        mut self,
        key: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
        toml: impl Into<String>,
    ) -> Self {
        let toml = toml.into();
        let config_kind = infer_config_kind_from_toml(&toml).ok().flatten();
        self.embedded_toml_profiles.insert(
            key.into(),
            EmbeddedTomlProfile {
                name: name.into(),
                description,
                tags: Vec::new(),
                toml,
                config_kind,
            },
        );
        self
    }

    pub fn embedded_profile_toml(mut self, toml: impl Into<String>) -> Result<Self> {
        let toml = toml.into();
        let metadata = parse_profile_file_metadata(&toml)?.ok_or_else(|| {
            anyhow!("Embedded TOML profile is missing required [profile] metadata")
        })?;
        let id = metadata
            .id
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow!("Embedded TOML profile [profile] metadata requires id"))?;
        let name = metadata
            .name
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| anyhow!("Embedded TOML profile [profile] metadata requires name"))?;
        let config_kind = infer_config_kind_from_toml(&toml)?;

        self.embedded_toml_profiles.insert(
            id,
            EmbeddedTomlProfile {
                name,
                description: metadata.description,
                tags: metadata.tags,
                toml,
                config_kind,
            },
        );
        Ok(self)
    }

    pub fn build(self) -> LocalProfileCatalog {
        LocalProfileCatalog {
            include_embedded_default: self.include_embedded_default,
            embedded_toml_profiles: self.embedded_toml_profiles,
            include_default_user_dir: self.include_default_user_dir,
            default_user_dir_override: self.default_user_dir_override,
            local_dirs: self.local_dirs,
        }
    }
}

/// Catalog over the embedded default profile plus optional local TOML directories.
#[derive(Debug, Clone)]
pub struct LocalProfileCatalog {
    include_embedded_default: bool,
    embedded_toml_profiles: BTreeMap<String, EmbeddedTomlProfile>,
    include_default_user_dir: bool,
    default_user_dir_override: Option<PathBuf>,
    local_dirs: Vec<PathBuf>,
}

impl Default for LocalProfileCatalog {
    fn default() -> Self {
        ProfileCatalogBuilder::new().build()
    }
}

impl LocalProfileCatalog {
    pub fn builder() -> ProfileCatalogBuilder {
        ProfileCatalogBuilder::new()
    }

    pub fn new(local_dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        ProfileCatalogBuilder::new()
            .include_embedded_default(true)
            .with_local_dirs(local_dirs)
            .build()
    }

    fn embedded_default_metadata() -> Result<ProfileMetadata> {
        Ok(ProfileMetadata {
            id: DEFAULT_EMBEDDED_PROFILE_KEY.to_string(),
            name: DEFAULT_EMBEDDED_PROFILE_NAME.to_string(),
            description: Some(DEFAULT_EMBEDDED_PROFILE_DESCRIPTION.to_string()),
            tags: Vec::new(),
            source: ProfileSource::Embedded {
                key: DEFAULT_EMBEDDED_PROFILE_KEY.to_string(),
            },
            config_kind: Some(ProfileConfigKind::Single),
            fingerprint: None,
        })
    }

    async fn load_metadata_source(&self, metadata: &ProfileMetadata) -> Result<Config> {
        match &metadata.source {
            ProfileSource::Embedded { key } if key == DEFAULT_EMBEDDED_PROFILE_KEY => {
                // Use the checked-in file path so existing prompt file refs resolve normally.
                load_config(PathBuf::from(DEFAULT_EMBEDDED_PROFILE_PATH))
                    .await
                    .with_context(|| format!("Failed to load embedded profile '{key}'"))
            }
            ProfileSource::Embedded { key } => Err(anyhow!("Unknown embedded profile '{key}'")),
            ProfileSource::EmbeddedToml { key } => {
                let profile = self
                    .embedded_toml_profiles
                    .get(key)
                    .ok_or_else(|| anyhow!("Unknown embedded TOML profile '{key}'"))?;
                load_config(crate::config::ConfigSource::Toml(profile.toml.clone()))
                    .await
                    .with_context(|| format!("Failed to load embedded TOML profile '{key}'"))
            }
            ProfileSource::LocalPath { path } => load_config(path)
                .await
                .with_context(|| format!("Failed to load local profile '{}'", path.display())),
        }
    }
}

impl ProfileCatalogBuilder {
    fn with_local_dirs(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.local_dirs.extend(dirs);
        self
    }
}

#[async_trait]
impl ProfileCatalog for LocalProfileCatalog {
    async fn list_profiles(&self) -> Result<Vec<ProfileMetadata>> {
        let mut profiles = Vec::new();
        if self.include_embedded_default {
            profiles.push(Self::embedded_default_metadata()?);
        }

        for (key, profile) in &self.embedded_toml_profiles {
            profiles.push(ProfileMetadata {
                id: key.clone(),
                name: profile.name.clone(),
                description: profile.description.clone(),
                tags: profile.tags.clone(),
                source: ProfileSource::EmbeddedToml { key: key.clone() },
                config_kind: profile.config_kind,
                fingerprint: None,
            });
        }

        if self.include_default_user_dir {
            let default_user_dir = self
                .default_user_dir_override
                .clone()
                .or_else(default_user_profiles_dir);
            if let Some(dir) = default_user_dir.as_ref() {
                let mut local = list_local_profiles(dir).with_context(|| {
                    format!("Failed to list local profiles in '{}'", dir.display())
                })?;
                profiles.append(&mut local);
            }
        }

        for dir in &self.local_dirs {
            let mut local = list_local_profiles(dir)
                .with_context(|| format!("Failed to list local profiles in '{}'", dir.display()))?;
            profiles.append(&mut local);
        }

        profiles.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then_with(|| source_sort_key(&a.source).cmp(&source_sort_key(&b.source)))
        });
        ensure_unique_profile_ids(&profiles)?;
        Ok(profiles)
    }

    async fn load_profile(&self, id: &str) -> Result<ProfileDocument> {
        let matches: Vec<_> = self
            .list_profiles()
            .await?
            .into_iter()
            .filter(|metadata| metadata.id == id)
            .collect();

        match matches.as_slice() {
            [] => Err(anyhow!("Profile '{id}' was not found")),
            [metadata] => {
                let config = self.load_metadata_source(metadata).await?;
                let mut metadata = metadata.clone();
                metadata.config_kind = Some(ProfileConfigKind::from_config(&config));
                Ok(ProfileDocument { metadata, config })
            }
            _ => Err(anyhow!(
                "Duplicate profile id '{id}' found; rename one [profile].id or local TOML filename"
            )),
        }
    }
}

impl ProfileConfigKind {
    fn from_config(config: &Config) -> Self {
        match config {
            Config::Single(_) => ProfileConfigKind::Single,
            Config::Multi(_) => ProfileConfigKind::Quorum,
        }
    }
}

/// Returns the conventional user-local profiles directory at `~/.qmt/profiles`.
///
/// Callers that enumerate this directory treat a missing path as an empty profile set.
pub fn default_user_profiles_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".qmt").join("profiles"))
}

/// Rejects duplicate profile ids so profile selection never silently chooses a source.
pub fn ensure_unique_profile_ids(profiles: &[ProfileMetadata]) -> Result<()> {
    let mut by_id: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for profile in profiles {
        by_id
            .entry(&profile.id)
            .or_default()
            .push(profile.source.storage_label());
    }

    let duplicates = by_id
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .map(|(id, sources)| format!("'{id}' ({})", sources.join(", ")))
        .collect::<Vec<_>>();

    if duplicates.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "Duplicate profile id(s) found: {}; rename one [profile].id or local TOML filename",
            duplicates.join("; ")
        ))
    }
}

/// Lazily builds and caches profile runtimes keyed by profile id.
///
/// Session/profile bindings are advisory: they are kept in memory and, when
/// shared storage is available, persisted to the sessions DB `profile_bindings`
/// side table. Missing or unavailable persisted profiles fall back to active.
pub struct ProfileRuntimeManager<C = Arc<dyn ProfileCatalog>> {
    catalog: C,
    shared_infra: AgentInfra,
    // NOTE: This is a server-wide default profile shared by all UI connections.
    // Per-connection profile selection would need to live in ConnectionState and be threaded into
    // profile resolution for new sessions; existing session bindings should remain authoritative.
    active_profile_id: Mutex<String>,
    runtimes: Mutex<HashMap<String, Arc<ProfileRuntime>>>,
    session_bindings: Mutex<HashMap<String, SessionProfileBinding>>,
    #[cfg(feature = "remote")]
    mesh: StdMutex<Option<crate::agent::remote::MeshHandle>>,
}

impl<C> ProfileRuntimeManager<C>
where
    C: ProfileCatalog,
{
    pub async fn with_default_infra(
        catalog: C,
        active_profile_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::with_infra(
            catalog,
            active_profile_id,
            AgentInfra::default_shared().await?,
        ))
    }

    pub fn with_infra(
        catalog: C,
        active_profile_id: impl Into<String>,
        shared_infra: AgentInfra,
    ) -> Self {
        Self {
            catalog,
            shared_infra,
            active_profile_id: Mutex::new(active_profile_id.into()),
            runtimes: Mutex::new(HashMap::new()),
            session_bindings: Mutex::new(HashMap::new()),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
        }
    }

    pub async fn list_profiles(&self) -> Result<Vec<ProfileMetadata>> {
        self.catalog.list_profiles().await
    }

    pub async fn active_profile_id(&self) -> String {
        self.active_profile_id.lock().await.clone()
    }

    pub async fn set_active_profile(&self, profile_id: impl Into<String>) -> Result<()> {
        let profile_id = profile_id.into();
        self.catalog.load_profile(&profile_id).await?;
        *self.active_profile_id.lock().await = profile_id;
        Ok(())
    }

    pub async fn active_runtime(&self) -> Result<Arc<ProfileRuntime>> {
        let profile_id = self.active_profile_id().await;
        self.runtime_for_profile(&profile_id).await
    }

    pub async fn runtime_for_profile(&self, profile_id: &str) -> Result<Arc<ProfileRuntime>> {
        if let Some(runtime) = self.runtimes.lock().await.get(profile_id).cloned() {
            return Ok(runtime);
        }

        // Do profile I/O/runtime startup outside the cache lock so slow profile startup does not
        // block unrelated cached runtime reads or event-forwarder polling. Re-check before insert
        // in case another task won the race.
        let document = self.catalog.load_profile(profile_id).await?;
        let runtime = Arc::new(build_profile_runtime(document, self.shared_infra.clone()).await?);

        let mut runtimes = self.runtimes.lock().await;
        if let Some(existing) = runtimes.get(profile_id) {
            return Ok(existing.clone());
        }
        #[cfg(feature = "remote")]
        if let Some(mesh) = self.mesh_handle() {
            runtime.agent().handle().set_mesh(mesh);
        }
        runtimes.insert(profile_id.to_string(), runtime.clone());
        Ok(runtime)
    }

    pub async fn bind_session_to_profile(
        &self,
        session_id: impl Into<String>,
        profile_id: &str,
    ) -> Result<SessionProfileBinding> {
        let runtime = self.runtime_for_profile(profile_id).await?;
        self.bind_session_to_runtime(session_id, &runtime).await
    }

    pub async fn bind_session_to_runtime(
        &self,
        session_id: impl Into<String>,
        runtime: &ProfileRuntime,
    ) -> Result<SessionProfileBinding> {
        let binding = runtime.session_binding();
        self.set_session_binding(session_id, binding.clone()).await;
        Ok(binding)
    }

    pub async fn set_session_binding(
        &self,
        session_id: impl Into<String>,
        binding: SessionProfileBinding,
    ) {
        let session_id = session_id.into();
        self.session_bindings
            .lock()
            .await
            .insert(session_id.clone(), binding.clone());
        let Some(storage) = &self.shared_infra.storage else {
            warn!(session_id, profile_id = %binding.profile_id, "session profile binding is memory-only because shared storage is unavailable");
            return;
        };
        if let Err(err) = storage
            .session_store()
            .set_profile_binding(&session_id, &binding.profile_id)
            .await
        {
            warn!(session_id, profile_id = %binding.profile_id, error = %err, "failed to persist session profile binding");
        }
    }

    pub async fn session_binding(&self, session_id: &str) -> Option<SessionProfileBinding> {
        if let Some(binding) = self.session_bindings.lock().await.get(session_id).cloned() {
            return Some(binding);
        }

        let Some(storage) = &self.shared_infra.storage else {
            return None;
        };
        let profile_id = match storage
            .session_store()
            .get_profile_binding(session_id)
            .await
        {
            Ok(Some(profile_id)) => profile_id,
            Ok(None) => return None,
            Err(err) => {
                warn!(session_id, error = %err, "failed to read session profile binding");
                return None;
            }
        };
        let runtime = match self.runtime_for_profile(&profile_id).await {
            Ok(runtime) => runtime,
            Err(err) => {
                warn!(session_id, profile_id, error = %err, "ignoring unavailable persisted session profile binding");
                return None;
            }
        };
        let binding = runtime.session_binding();
        self.session_bindings
            .lock()
            .await
            .insert(session_id.to_string(), binding.clone());
        Some(binding)
    }

    pub async fn materialized_runtimes(&self) -> Vec<Arc<ProfileRuntime>> {
        self.runtimes.lock().await.values().cloned().collect()
    }

    #[cfg(feature = "remote")]
    pub fn set_mesh_handle(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.lock().expect("profile mesh mutex poisoned") = Some(mesh);
    }

    #[cfg(feature = "remote")]
    fn mesh_handle(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh
            .lock()
            .expect("profile mesh mutex poisoned")
            .clone()
    }

    #[cfg(feature = "remote")]
    pub async fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        self.set_mesh_handle(mesh.clone());
        for runtime in self.runtimes.lock().await.values() {
            runtime.agent().handle().set_mesh(mesh.clone());
        }
    }

    pub async fn forget_session_binding(&self, session_id: &str) -> Option<SessionProfileBinding> {
        let removed = self.session_bindings.lock().await.remove(session_id);
        if let Some(storage) = &self.shared_infra.storage
            && let Err(err) = storage
                .session_store()
                .remove_profile_binding(session_id)
                .await
        {
            warn!(session_id, error = %err, "failed to remove session profile binding");
        }
        removed
    }

    pub async fn shutdown(&self) {
        let runtimes = std::mem::take(&mut *self.runtimes.lock().await);
        for runtime in runtimes.into_values() {
            runtime.agent.handle().shutdown().await;
        }
    }

    #[cfg(test)]
    async fn cached_runtime_count(&self) -> usize {
        self.runtimes.lock().await.len()
    }
}

impl ProfileRuntimeManager<Arc<dyn ProfileCatalog>> {
    pub async fn with_default_infra_boxed(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self::with_infra_boxed(
            catalog,
            active_profile_id,
            AgentInfra::default_shared().await?,
        ))
    }

    pub fn with_infra_boxed(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
        shared_infra: AgentInfra,
    ) -> Self {
        Self {
            catalog,
            shared_infra,
            active_profile_id: Mutex::new(active_profile_id.into()),
            runtimes: Mutex::new(HashMap::new()),
            session_bindings: Mutex::new(HashMap::new()),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
        }
    }
}

async fn build_profile_runtime(
    document: ProfileDocument,
    shared_infra: AgentInfra,
) -> Result<ProfileRuntime> {
    let metadata = document.metadata;
    let agent = match document.config {
        Config::Single(config) => {
            Agent::from_single_config_with_infra(*config, shared_infra).await?
        }
        Config::Multi(config) => {
            #[cfg(feature = "remote")]
            let infra = shared_infra.clone();
            #[cfg(not(feature = "remote"))]
            let infra = shared_infra;

            Agent::from_quorum_config_with_infra(*config, infra).await?
        }
    };

    Ok(ProfileRuntime { metadata, agent })
}

fn list_local_profiles(dir: &Path) -> Result<Vec<ProfileMetadata>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    if !dir.is_dir() {
        return Err(anyhow!("local profile path is not a directory"));
    }

    let mut profiles = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };

        let content = std::fs::read_to_string(&path)?;
        let file_meta = parse_profile_file_metadata(&content)?.unwrap_or_default();
        let id = file_meta.id.unwrap_or_else(|| stem.to_string());
        let name = file_meta.name.unwrap_or_else(|| id.clone());
        let metadata = ProfileMetadata {
            id,
            name,
            description: file_meta.description,
            tags: file_meta.tags,
            source: ProfileSource::LocalPath { path: path.clone() },
            config_kind: infer_config_kind_from_toml(&content).ok().flatten(),
            fingerprint: None,
        };
        profiles.insert(path, metadata);
    }

    Ok(profiles.into_values().collect())
}

fn parse_profile_file_metadata(content: &str) -> Result<Option<ProfileFileMetadata>> {
    let envelope: ProfileMetadataEnvelope = toml::from_str(content)?;
    Ok(envelope.profile)
}

fn infer_config_kind_from_toml(content: &str) -> Result<Option<ProfileConfigKind>> {
    let value: toml::Value = toml::from_str(content)?;
    if value.get("agent").is_some() {
        Ok(Some(ProfileConfigKind::Single))
    } else if value.get("quorum").is_some() || value.get("planner").is_some() {
        Ok(Some(ProfileConfigKind::Quorum))
    } else {
        Ok(None)
    }
}

fn source_sort_key(source: &ProfileSource) -> String {
    source.storage_label()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "remote")]
    use crate::agent::remote::test_helpers::fixtures::{get_test_mesh, random_node_id};
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::empty_plugin_registry;
    #[cfg(feature = "remote")]
    use querymt::error::LLMError;
    use std::sync::Arc;

    #[cfg(feature = "remote")]
    use std::time::Duration;

    fn temp_profile_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("querymt-profiles-")
            .tempdir()
            .expect("failed to create temp profile dir")
    }

    fn write_profile(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("failed to write temp profile");
        path
    }

    async fn test_infra() -> (AgentInfra, tempfile::TempDir) {
        let (registry, temp_dir) = empty_plugin_registry().expect("empty registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("in-memory storage"),
        );
        (
            AgentInfra {
                plugin_registry: Arc::new(registry),
                storage: Some(storage),
                session_mcp_attachment_source: None,
            },
            temp_dir,
        )
    }

    #[cfg(feature = "remote")]
    async fn remote_provider_call_error(runtime: &ProfileRuntime) -> String {
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let provider = &runtime.agent().handle().config.provider;
        let session_handle = provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();
        runtime
            .agent()
            .storage_backend()
            .session_store()
            .set_session_provider_node_id(&session_id, Some(&random_node_id()))
            .await
            .expect("set provider_node_id");

        let provider = provider
            .build_provider_for_session(&session_id)
            .await
            .expect("mesh-backed provider should build");
        match provider.chat_with_tools(&[], None).await {
            Err(LLMError::ProviderError(message)) => message,
            other => panic!("expected provider error from mesh lookup; got: {other:?}"),
        }
    }

    #[test]
    fn profile_metadata_builds_session_binding() {
        let metadata = ProfileMetadata {
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            description: None,
            tags: Vec::new(),
            source: ProfileSource::Embedded {
                key: "default".to_string(),
            },
            config_kind: Some(ProfileConfigKind::Single),
            fingerprint: Some("fp-1".to_string()),
        };

        let binding = metadata.session_binding();

        assert_eq!(binding.profile_id, "alpha");
        assert_eq!(binding.profile_fingerprint.as_deref(), Some("fp-1"));
        assert_eq!(binding.profile_source.as_deref(), Some("embedded:default"));
        assert_eq!(binding.profile_config_kind.as_deref(), Some("single"));
    }

    #[tokio::test]
    async fn embedded_default_lists_and_loads() {
        let catalog = LocalProfileCatalog::default();
        let profiles = catalog.list_profiles().await.expect("profiles should list");
        let default = profiles
            .iter()
            .find(|profile| profile.id == DEFAULT_EMBEDDED_PROFILE_KEY)
            .expect("default profile should be listed");
        assert_eq!(default.config_kind, Some(ProfileConfigKind::Single));
        assert!(matches!(default.source, ProfileSource::Embedded { .. }));

        let document = catalog
            .load_profile(DEFAULT_EMBEDDED_PROFILE_KEY)
            .await
            .expect("default profile should load through config loader");
        assert!(matches!(document.config, Config::Single(_)));
        assert_eq!(
            document.metadata.config_kind,
            Some(ProfileConfigKind::Single)
        );
    }

    #[tokio::test]
    async fn standard_embedded_profile_catalog_lists_and_loads_profiles() {
        let catalog = standard_embedded_profile_catalog_builder()
            .expect("standard catalog should build")
            .build();
        let profiles = catalog.list_profiles().await.expect("profiles should list");

        assert_eq!(profiles.len(), 2);
        let default = profiles
            .iter()
            .find(|profile| profile.id == DEFAULT_EMBEDDED_PROFILE_KEY)
            .expect("default profile should be listed");
        assert_eq!(default.name, "Default");
        assert_eq!(default.tags, vec!["coding", "single-agent"]);
        assert_eq!(default.config_kind, Some(ProfileConfigKind::Single));
        assert!(matches!(default.source, ProfileSource::EmbeddedToml { .. }));

        let coder_delegate = profiles
            .iter()
            .find(|profile| profile.id == "coder-delegate")
            .expect("coder delegate profile should be listed");
        assert_eq!(coder_delegate.name, "Coder Delegate");
        assert_eq!(coder_delegate.config_kind, Some(ProfileConfigKind::Quorum));
        assert!(matches!(
            coder_delegate.source,
            ProfileSource::EmbeddedToml { .. }
        ));

        let default_document = catalog
            .load_profile(DEFAULT_EMBEDDED_PROFILE_KEY)
            .await
            .expect("default profile should load");
        assert!(matches!(default_document.config, Config::Single(_)));

        let delegate_document = catalog
            .load_profile("coder-delegate")
            .await
            .expect("coder delegate profile should load");
        assert!(matches!(delegate_document.config, Config::Multi(_)));
    }

    #[test]
    fn embedded_profile_config_inlines_standard_prompt_refs() {
        let config = embedded_profile_config("single_coder.toml", STANDARD_SINGLE_CODER_CONFIG)
            .expect("embedded config should load");
        let value: toml::Value =
            toml::from_str(&config).expect("embedded config should parse as TOML");
        let system = value
            .get("agent")
            .and_then(toml::Value::as_table)
            .and_then(|agent| agent.get("system"))
            .and_then(toml::Value::as_array)
            .expect("embedded config should contain [agent].system array");
        let inlined: Vec<&str> = system
            .iter()
            .map(|part| part.as_str().expect("system part must be an inline string"))
            .collect();

        assert_eq!(
            inlined,
            vec![
                include_str!("../examples/prompts/default_system.txt"),
                include_str!("../examples/prompts/code_meta.jinja2"),
            ]
        );
    }

    #[test]
    fn embedded_prompt_asset_key_resolves_standard_prompt_refs() {
        for file_ref in [
            "../prompts/default_system.txt",
            "../prompts/code_meta.jinja2",
        ] {
            let asset_key = embedded_prompt_asset_key(file_ref)
                .unwrap_or_else(|| panic!("{file_ref} should map to an embedded prompt asset"));
            assert!(
                EmbeddedExampleAssets::get(&asset_key).is_some(),
                "{file_ref} should resolve to an embedded prompt asset key '{asset_key}'"
            );
        }
    }

    #[test]
    fn embedded_prompt_asset_key_rejects_path_escape() {
        assert!(embedded_prompt_asset_key("../../outside.txt").is_none());
    }

    #[tokio::test]
    async fn embedded_toml_profile_lists_and_loads_inline_config() {
        let inline = r#"
[agent]
provider = "test"
model = "mock"
tools = []
system = ["inline"]
"#;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .embedded_config_toml("default", "Default", None, inline)
            .build();

        let profiles = catalog.list_profiles().await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "default");
        assert!(matches!(
            profiles[0].source,
            ProfileSource::EmbeddedToml { .. }
        ));
        assert_eq!(profiles[0].config_kind, Some(ProfileConfigKind::Single));

        let document = catalog.load_profile("default").await.unwrap();
        assert!(matches!(document.config, Config::Single(_)));
    }

    #[tokio::test]
    async fn embedded_profile_toml_uses_profile_metadata() {
        let inline = r#"
[profile]
id = "metadata-profile"
name = "Metadata Profile"
description = "Loaded from TOML metadata"
tags = ["coding", "embedded"]

[agent]
provider = "test"
model = "mock"
tools = []
system = ["inline"]
"#;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .embedded_profile_toml(inline)
            .expect("embedded profile metadata should parse")
            .build();

        let profiles = catalog.list_profiles().await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "metadata-profile");
        assert_eq!(profiles[0].name, "Metadata Profile");
        assert_eq!(
            profiles[0].description.as_deref(),
            Some("Loaded from TOML metadata")
        );
        assert_eq!(profiles[0].tags, vec!["coding", "embedded"]);
        assert_eq!(profiles[0].config_kind, Some(ProfileConfigKind::Single));

        let document = catalog.load_profile("metadata-profile").await.unwrap();
        assert!(matches!(document.config, Config::Single(_)));
    }

    #[test]
    fn embedded_profile_toml_requires_profile_metadata() {
        let missing_profile = LocalProfileCatalog::builder().embedded_profile_toml(
            r#"
[agent]
provider = "test"
model = "mock"
system = "inline"
"#,
        );
        assert!(
            missing_profile
                .expect_err("missing [profile] should fail")
                .to_string()
                .contains("missing required [profile] metadata")
        );

        let missing_id = LocalProfileCatalog::builder().embedded_profile_toml(
            r#"
[profile]
name = "Missing ID"

[agent]
provider = "test"
model = "mock"
system = "inline"
"#,
        );
        assert!(
            missing_id
                .expect_err("missing id should fail")
                .to_string()
                .contains("requires id")
        );

        let missing_name = LocalProfileCatalog::builder().embedded_profile_toml(
            r#"
[profile]
id = "missing-name"

[agent]
provider = "test"
model = "mock"
system = "inline"
"#,
        );
        assert!(
            missing_name
                .expect_err("missing name should fail")
                .to_string()
                .contains("requires name")
        );
    }

    #[tokio::test]
    async fn local_dir_lists_valid_toml_files_with_deterministic_ids() {
        let dir = temp_profile_dir();
        let alpha = write_profile(
            dir.path(),
            "alpha-coder.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        write_profile(dir.path(), "ignored.txt", "not toml");

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let profiles = catalog.list_profiles().await.expect("profiles should list");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "alpha-coder");
        assert_eq!(profiles[0].name, "alpha-coder");
        assert!(profiles[0].tags.is_empty());
        assert_eq!(profiles[0].config_kind, Some(ProfileConfigKind::Single));
        assert_eq!(profiles[0].source, ProfileSource::LocalPath { path: alpha });
    }

    #[tokio::test]
    async fn local_profile_metadata_overrides_catalog_fields_and_loads_by_id() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "generic.toml",
            r#"
[profile]
id = "coder-delegate"
name = "Coder Delegate"
description = "Planner/orchestrator profile."
tags = ["coding", "delegation"]

[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let profiles = catalog.list_profiles().await.expect("profiles should list");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "coder-delegate");
        assert_eq!(profiles[0].name, "Coder Delegate");
        assert_eq!(
            profiles[0].description.as_deref(),
            Some("Planner/orchestrator profile.")
        );
        assert_eq!(profiles[0].tags, vec!["coding", "delegation"]);

        let document = catalog
            .load_profile("coder-delegate")
            .await
            .expect("metadata id should load");
        assert!(matches!(document.config, Config::Single(_)));
    }

    #[tokio::test]
    async fn duplicate_profile_ids_return_actionable_error() {
        let dir = temp_profile_dir();
        let content = r#"
[profile]
id = "shared"

[agent]
provider = "test"
model = "test-model"
system = "inline"
"#;
        write_profile(dir.path(), "alpha.toml", content);
        write_profile(dir.path(), "beta.toml", content);

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let err = catalog
            .list_profiles()
            .await
            .expect_err("duplicate profile ids should fail listing");
        let message = err.to_string();
        assert!(
            message.contains("Duplicate profile id"),
            "message was: {message}"
        );
        assert!(message.contains("[profile].id"), "message was: {message}");
        assert!(message.contains("alpha.toml"), "message was: {message}");
        assert!(message.contains("beta.toml"), "message was: {message}");

        let err = catalog
            .load_profile("shared")
            .await
            .expect_err("duplicate profile ids should fail loading");
        assert!(
            err.to_string().contains("Duplicate profile id"),
            "message was: {err}"
        );
    }

    #[tokio::test]
    async fn missing_default_user_dir_is_skipped() {
        let dir = temp_profile_dir();
        let missing = dir.path().join("missing").join("profiles");
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .default_user_dir(missing)
            .build();

        let profiles = catalog
            .list_profiles()
            .await
            .expect("missing default user profile dir should be ignored");
        assert!(profiles.is_empty());
    }

    #[tokio::test]
    async fn default_user_dir_lists_profiles_when_enabled() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "user-coder.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .default_user_dir(dir.path())
            .build();

        let profiles = catalog
            .list_profiles()
            .await
            .expect("default user profiles should list");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "user-coder");
    }

    #[tokio::test]
    async fn duplicate_ids_across_embedded_and_default_user_dir_error() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "default.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let catalog = LocalProfileCatalog::builder()
            .default_user_dir(dir.path())
            .build();

        let err = catalog
            .list_profiles()
            .await
            .expect_err("default user profile should conflict with embedded default");
        let message = err.to_string();
        assert!(
            message.contains("Duplicate profile id"),
            "message was: {message}"
        );
        assert!(
            message.contains("embedded:default"),
            "message was: {message}"
        );
        assert!(message.contains("default.toml"), "message was: {message}");
    }

    #[tokio::test]
    async fn duplicate_ids_across_local_dirs_error() {
        let first = temp_profile_dir();
        let second = temp_profile_dir();
        let content = r#"
[profile]
id = "shared"

[agent]
provider = "test"
model = "test-model"
system = "inline"
"#;
        write_profile(first.path(), "alpha.toml", content);
        write_profile(second.path(), "beta.toml", content);
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(first.path())
            .local_dir(second.path())
            .build();

        let err = catalog
            .list_profiles()
            .await
            .expect_err("duplicate ids across local dirs should fail listing");
        let message = err.to_string();
        assert!(
            message.contains("Duplicate profile id"),
            "message was: {message}"
        );
        assert!(message.contains("alpha.toml"), "message was: {message}");
        assert!(message.contains("beta.toml"), "message was: {message}");
    }

    #[tokio::test]
    async fn local_profile_loads_to_existing_config_enum() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "quorum.toml",
            r#"
[quorum]

[planner]
provider = "test"
model = "planner-model"
"#,
        );

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let document = catalog.load_profile("quorum").await.expect("profile loads");
        assert!(matches!(document.config, Config::Multi(_)));
        assert_eq!(
            document.metadata.config_kind,
            Some(ProfileConfigKind::Quorum)
        );
    }

    #[tokio::test]
    async fn local_profile_resolves_file_prompt_relative_to_profile_dir() {
        let dir = temp_profile_dir();
        std::fs::write(dir.path().join("prompt.txt"), "prompt from file")
            .expect("failed to write prompt file");
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = [{ file = "prompt.txt" }]
"#,
        );

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let document = catalog.load_profile("alpha").await.expect("profile loads");

        match document.config {
            Config::Single(single) => {
                assert!(matches!(
                    &single.agent.system[0],
                    crate::config::SystemPart::Inline(prompt) if prompt == "prompt from file"
                ));
            }
            other => panic!("expected single-agent config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_local_profile_returns_actionable_load_error() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "broken.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
unknown = true
"#,
        );

        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let err = catalog
            .load_profile("broken")
            .await
            .expect_err("invalid profile should fail on load");
        let message = err.to_string();
        assert!(message.contains("broken.toml"), "message was: {message}");
        assert!(
            message.contains("Failed to load local profile"),
            "message was: {message}"
        );
    }

    #[tokio::test]
    async fn missing_profile_returns_not_found_error() {
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .build();
        let err = catalog
            .load_profile("remote-service-profile")
            .await
            .expect_err("missing profile should fail");
        assert!(err.to_string().contains("was not found"));
    }

    #[tokio::test]
    async fn runtime_manager_lazily_materializes_and_caches_profiles() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "alpha", infra);

        assert_eq!(manager.cached_runtime_count().await, 0);
        let first = manager
            .runtime_for_profile("alpha")
            .await
            .expect("runtime materializes");
        let second = manager
            .runtime_for_profile("alpha")
            .await
            .expect("runtime is cached");

        assert_eq!(first.profile_id(), "alpha");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(manager.cached_runtime_count().await, 1);
        manager.shutdown().await;
        assert_eq!(manager.cached_runtime_count().await, 0);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn set_mesh_applies_to_materialized_profile_runtime() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "alpha", infra);
        let runtime = manager
            .runtime_for_profile("alpha")
            .await
            .expect("runtime materializes");

        manager.set_mesh(get_test_mesh().await.clone()).await;
        let message = remote_provider_call_error(&runtime).await;

        assert!(
            !message.contains("no mesh handle available"),
            "profile runtime did not receive root mesh: {message}"
        );
        assert!(
            message.contains("provider_host::"),
            "expected mesh lookup error after propagation; got: {message}"
        );
        manager.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn runtime_for_profile_cannot_miss_concurrent_mesh_set() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = Arc::new(ProfileRuntimeManager::with_infra(catalog, "alpha", infra));
        let mesh = get_test_mesh().await.clone();

        let runtime_task = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.runtime_for_profile("alpha").await })
        };
        tokio::time::sleep(Duration::from_millis(1)).await;
        manager.set_mesh(mesh).await;

        let runtime = runtime_task
            .await
            .expect("runtime task should not panic")
            .expect("runtime materializes");
        let message = remote_provider_call_error(&runtime).await;

        assert!(
            !message.contains("no mesh handle available"),
            "runtime created during set_mesh race missed root mesh: {message}"
        );
        manager.shutdown().await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn set_mesh_applies_to_future_profile_runtime() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "alpha", infra);

        manager.set_mesh(get_test_mesh().await.clone()).await;
        let runtime = manager
            .runtime_for_profile("alpha")
            .await
            .expect("runtime materializes");
        let message = remote_provider_call_error(&runtime).await;

        assert!(
            !message.contains("no mesh handle available"),
            "future profile runtime did not receive root mesh: {message}"
        );
        assert!(
            message.contains("provider_host::"),
            "expected mesh lookup error after propagation; got: {message}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_manager_tracks_session_bindings_in_memory() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "alpha", infra);

        let binding = manager
            .bind_session_to_profile("session-1", "alpha")
            .await
            .expect("session binds to profile");

        let expected_source = format!("local:{}", dir.path().join("alpha.toml").display());
        assert_eq!(binding.profile_id, "alpha");
        assert_eq!(
            binding.profile_source.as_deref(),
            Some(expected_source.as_str())
        );
        assert_eq!(
            manager.session_binding("session-1").await.as_ref(),
            Some(&binding)
        );
        assert_eq!(
            manager.session_binding("session-1").await.as_ref(),
            Some(&binding)
        );
        assert_eq!(
            manager.forget_session_binding("session-1").await,
            Some(binding)
        );
        assert!(manager.session_binding("session-1").await.is_none());

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_bindings_read_through_db_across_managers() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        write_profile(
            dir.path(),
            "beta.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager_a = ProfileRuntimeManager::with_infra(catalog.clone(), "alpha", infra.clone());
        let manager_b = ProfileRuntimeManager::with_infra(catalog, "beta", infra.clone());

        let binding = manager_a
            .bind_session_to_profile("session-1", "alpha")
            .await
            .expect("session binds in manager A");

        assert_eq!(manager_b.cached_runtime_count().await, 0);
        assert_eq!(
            manager_b.session_binding("session-1").await.as_ref(),
            Some(&binding)
        );
        assert_eq!(manager_b.cached_runtime_count().await, 1);
        assert_eq!(
            manager_b.session_binding("session-1").await.as_ref(),
            Some(&binding)
        );
        assert_eq!(
            infra
                .storage
                .as_ref()
                .expect("shared storage")
                .session_store()
                .get_profile_binding("session-1")
                .await
                .expect("binding row"),
            Some("alpha".to_string())
        );

        manager_a.shutdown().await;
        manager_b.shutdown().await;
    }

    #[tokio::test]
    async fn missing_or_unavailable_db_session_binding_returns_none() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "beta.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        infra
            .storage
            .as_ref()
            .expect("shared storage")
            .session_store()
            .set_profile_binding("unknown-session", "alpha")
            .await
            .expect("binding row");
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "beta", infra);

        assert!(manager.session_binding("missing-session").await.is_none());
        assert!(manager.session_binding("unknown-session").await.is_none());

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn forget_session_binding_removes_db_entry() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "alpha.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let store = infra
            .storage
            .as_ref()
            .expect("shared storage")
            .session_store();
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager_a = ProfileRuntimeManager::with_infra(catalog.clone(), "alpha", infra.clone());
        let manager_b = ProfileRuntimeManager::with_infra(catalog, "alpha", infra);

        let binding = manager_a
            .bind_session_to_profile("session-1", "alpha")
            .await
            .expect("session binds in manager A");
        assert_eq!(
            store
                .get_profile_binding("session-1")
                .await
                .expect("binding row"),
            Some("alpha".to_string())
        );
        assert_eq!(
            manager_a.forget_session_binding("session-1").await,
            Some(binding)
        );

        assert!(manager_b.session_binding("session-1").await.is_none());
        assert_eq!(
            store
                .get_profile_binding("session-1")
                .await
                .expect("binding removed"),
            None
        );

        manager_a.shutdown().await;
        manager_b.shutdown().await;
    }

    #[tokio::test]
    async fn active_runtime_builds_single_profile_with_shared_storage() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "single.toml",
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let shared = infra.storage.as_ref().expect("shared storage").clone();
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "single", infra);

        let runtime = manager
            .active_runtime()
            .await
            .expect("single runtime builds");
        assert_eq!(runtime.profile_id(), "single");
        assert!(!runtime.agent().is_multi());
        assert!(Arc::ptr_eq(&shared, &runtime.agent().storage_backend()));

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn profile_config_rejects_db_field() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "single-db.toml",
            r#"
[agent]
db = "./profile.db"
provider = "test"
model = "test-model"
system = "inline"
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "single-db", infra);

        match manager.active_runtime().await {
            Ok(_) => panic!("profile db config field should be rejected"),
            Err(err) => {
                let message = format!("{err:#}");
                assert!(
                    message.contains("Failed to deserialize single agent config"),
                    "unexpected error: {message}"
                );
            }
        }

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_manager_materializes_quorum_profile_with_injected_infra() {
        let dir = temp_profile_dir();
        write_profile(
            dir.path(),
            "team.toml",
            r#"
[quorum]
delegation = true
verification = false
snapshot_policy = "none"

[planner]
provider = "test"
model = "planner-model"
system = "plan"
tools = ["delegate"]

[[delegates]]
id = "coder"
provider = "test"
model = "coder-model"
system = "code"
capabilities = ["coding"]
"#,
        );
        let (infra, _registry_dir) = test_infra().await;
        let shared = infra.storage.as_ref().expect("shared storage").clone();
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let manager = ProfileRuntimeManager::with_infra(catalog, "team", infra);

        let runtime = manager
            .active_runtime()
            .await
            .expect("quorum runtime builds");
        assert_eq!(runtime.profile_id(), "team");
        assert!(runtime.agent().is_multi());
        assert!(Arc::ptr_eq(&shared, &runtime.agent().storage_backend()));

        manager.shutdown().await;
    }
}
