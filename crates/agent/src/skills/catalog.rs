//! Agent-level skill catalog.
//!
//! The catalog owns everything about skills that does not depend on a single
//! session: discovery sources, permissions, the per-workspace registry cache,
//! the `mcp` switch and the approval mode. A [`SkillTool`](super::SkillTool)
//! is a cheap value bound to one session and turn on top of a catalog.

use crate::config::SkillsConfig;
use crate::skills::discovery;
use crate::skills::permissions::SkillPermissions;
use crate::skills::registry::SkillRegistry;
use crate::skills::tool::SkillTool;
use crate::skills::types::{Skill, SkillSource};
use crate::tools::ToolError;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Discovery function used by registry refreshes. Swappable in tests to
/// inject deterministic discovery failures.
pub(crate) type DiscoverFn =
    dyn Fn(&[SkillSource], bool) -> anyhow::Result<Vec<Skill>> + Send + Sync;

/// How a permission result of `Ask` is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    /// Prompt the user through the tool context.
    #[default]
    Interactive,
    /// Nobody can answer a prompt (for example `.agents` delegation
    /// targets): every `Ask` fails immediately.
    NonInteractive,
}

/// Agent-level skill state shared by every session of an agent.
pub struct SkillCatalog {
    registries: Arc<Mutex<HashMap<PathBuf, SkillRegistry>>>,
    permissions: Arc<SkillPermissions>,
    /// Discovery sources for the project root. Global and configured sources
    /// remain common; project sources are re-derived for a session workspace.
    sources: Vec<SkillSource>,
    project_root: PathBuf,
    include_external: bool,
    mcp: bool,
    approval: ApprovalMode,
    /// `(local source, server)` collision pairs already warned about.
    warned_collisions: Mutex<HashSet<(String, String)>>,
    discovery_fn: Arc<DiscoverFn>,
    /// Source of refresh tickets: taken before discovery starts, so a larger
    /// ticket means a refresh that began later.
    refresh_tickets: AtomicU64,
    /// Newest ticket published per workspace; an older refresh finishing
    /// late is dropped instead of replacing newer state.
    published_tickets: Mutex<HashMap<PathBuf, u64>>,
}

impl SkillCatalog {
    /// Discover the configured skills and build the catalog for an agent.
    pub(crate) fn from_config(config: &SkillsConfig, project_root: &Path) -> Arc<Self> {
        let mut search_paths = discovery::default_search_paths(project_root);
        for custom_path in &config.paths {
            search_paths.push(SkillSource::Configured(custom_path.clone()));
        }

        let mut registry = SkillRegistry::new();
        match registry.load_from_sources(&search_paths, config.include_external) {
            Ok(count) => {
                if count > 0 {
                    log::info!("Skills system initialized: {count} skills discovered");
                    log::debug!("Discovered skills: {}", registry.names().join(", "));
                } else {
                    log::debug!(
                        "Skills system enabled but no skills found in {} search paths",
                        search_paths.len()
                    );
                }
            }
            Err(error) => {
                log::warn!(
                    "Failed to discover skills: {}. The skill tool will have no discovered skills.",
                    error
                );
            }
        }

        let mut catalog = Self::from_parts(
            Arc::new(Mutex::new(registry)),
            Arc::new(config.permissions.clone()),
            search_paths,
            config.include_external,
            project_root.to_path_buf(),
        );
        catalog.mcp = config.mcp;
        Arc::new(catalog)
    }

    /// Build a catalog from already-resolved parts.
    pub(crate) fn from_parts(
        registry: Arc<Mutex<SkillRegistry>>,
        permissions: Arc<SkillPermissions>,
        sources: Vec<SkillSource>,
        include_external: bool,
        project_root: PathBuf,
    ) -> Self {
        let initial_registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut registries = HashMap::new();
        registries.insert(project_root.clone(), initial_registry);

        Self {
            registries: Arc::new(Mutex::new(registries)),
            permissions,
            sources,
            project_root,
            include_external,
            mcp: true,
            approval: ApprovalMode::Interactive,
            warned_collisions: Mutex::new(HashSet::new()),
            discovery_fn: Arc::new(discovery::discover_all_strict),
            refresh_tickets: AtomicU64::new(0),
            published_tickets: Mutex::new(HashMap::new()),
        }
    }

    /// Use the given approval mode.
    pub fn with_approval(mut self, approval: ApprovalMode) -> Self {
        self.approval = approval;
        self
    }

    /// Enable or disable MCP skills.
    pub fn with_mcp(mut self, mcp: bool) -> Self {
        self.mcp = mcp;
        self
    }

    /// Override the discovery function used by refreshes (test-only).
    #[cfg(test)]
    pub(crate) fn with_discovery_fn(mut self, discovery_fn: Arc<DiscoverFn>) -> Self {
        self.discovery_fn = discovery_fn;
        self
    }

    pub fn permissions(&self) -> &SkillPermissions {
        &self.permissions
    }

    pub fn approval(&self) -> ApprovalMode {
        self.approval
    }

    /// Whether skills published by MCP servers are offered.
    pub fn mcp_enabled(&self) -> bool {
        self.mcp
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Whether a collision between `local` and `server` is reported for the
    /// first time (and records it).
    pub(crate) fn first_collision_report(&self, local: &str, server: &str) -> bool {
        self.warned_collisions
            .lock()
            .map(|mut set| set.insert((local.to_string(), server.to_string())))
            .unwrap_or(false)
    }

    /// Bind the catalog to one session and turn.
    pub fn bind(
        self: &Arc<Self>,
        workspace: Option<&Path>,
        mcp_state: Arc<crate::agent::core::McpToolState>,
        tool_config: crate::agent::core::ToolConfig,
    ) -> SkillTool {
        SkillTool::bind(
            Arc::clone(self),
            workspace.unwrap_or(&self.project_root).to_path_buf(),
            mcp_state,
            tool_config,
        )
    }

    /// Workspace a call resolves against: the call's cwd, else `fallback`.
    pub(crate) fn effective_workspace(cwd: Option<&Path>, fallback: &Path) -> PathBuf {
        cwd.unwrap_or(fallback).to_path_buf()
    }

    fn sources_for_workspace(&self, workspace: &Path) -> Vec<SkillSource> {
        let mut sources = Vec::new();

        // Keep common sources in priority order around the workspace-derived
        // project paths: global < project < configured/remote.
        sources.extend(
            self.sources
                .iter()
                .filter(|source| matches!(source, SkillSource::Global(_)))
                .cloned(),
        );
        if workspace == self.project_root {
            sources.extend(
                self.sources
                    .iter()
                    .filter(|source| matches!(source, SkillSource::Project(_)))
                    .cloned(),
            );
        } else {
            sources.extend(
                discovery::default_search_paths(workspace)
                    .into_iter()
                    .filter(|source| matches!(source, SkillSource::Project(_))),
            );
        }
        sources.extend(
            self.sources
                .iter()
                .filter(|source| {
                    matches!(
                        source,
                        SkillSource::Configured(_) | SkillSource::Remote { .. }
                    )
                })
                .cloned(),
        );
        sources
    }

    /// Re-discover skills for one workspace and atomically replace only that
    /// workspace's registry snapshot.
    pub(crate) fn refresh_registry(&self, workspace: &Path) -> anyhow::Result<()> {
        let ticket = self.refresh_tickets.fetch_add(1, Ordering::SeqCst) + 1;
        let sources = self.sources_for_workspace(workspace);
        match (self.discovery_fn)(&sources, self.include_external) {
            Ok(skills) => self.publish_refresh(workspace, ticket, skills),
            Err(error) => {
                log::warn!(
                    "Failed to refresh skills for {}: {error:#}. Retaining previously discovered skills.",
                    workspace.display()
                );
                Err(error)
            }
        }
    }

    /// Async variant of [`Self::refresh_registry`] for the tool-call path:
    /// discovery runs off the Tokio worker via `spawn_blocking`, and the
    /// registry lock is only held to publish the results.
    pub(crate) async fn refresh_registry_async(&self, workspace: &Path) -> anyhow::Result<()> {
        let ticket = self.refresh_tickets.fetch_add(1, Ordering::SeqCst) + 1;
        let workspace = workspace.to_path_buf();
        let sources = self.sources_for_workspace(&workspace);
        let include_external = self.include_external;
        let discovery_fn = Arc::clone(&self.discovery_fn);

        let discovered =
            tokio::task::spawn_blocking(move || (discovery_fn)(&sources, include_external))
                .await
                .map_err(|e| anyhow::anyhow!("skill discovery task failed: {e}"))?;

        let skills = match discovered {
            Ok(skills) => skills,
            Err(error) => {
                log::warn!(
                    "Failed to refresh skills for {}: {error:#}. Retaining previously discovered skills.",
                    workspace.display()
                );
                return Err(error);
            }
        };

        self.publish_refresh(&workspace, ticket, skills)
    }

    /// Publish a discovery result unless a refresh that started later has
    /// already published for this workspace.
    fn publish_refresh(
        &self,
        workspace: &Path,
        ticket: u64,
        skills: Vec<Skill>,
    ) -> anyhow::Result<()> {
        let mut registries = self
            .registries
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
        let mut published = self
            .published_tickets
            .lock()
            .map_err(|_| anyhow::anyhow!("refresh ticket lock poisoned"))?;
        if published
            .get(workspace)
            .is_some_and(|newest| *newest > ticket)
        {
            log::debug!(
                "Dropping stale skill refresh for {}: a newer one was already published",
                workspace.display()
            );
            return Ok(());
        }
        published.insert(workspace.to_path_buf(), ticket);
        let count = registries
            .entry(workspace.to_path_buf())
            .or_default()
            .reload_with(skills);
        log::debug!(
            "Skill registry refreshed for {}: {count} skills available",
            workspace.display()
        );
        Ok(())
    }

    /// Whether a snapshot for `workspace` has been loaded.
    pub(crate) fn has_workspace(&self, workspace: &Path) -> bool {
        self.registries
            .lock()
            .map(|registries| registries.contains_key(workspace))
            .unwrap_or(false)
    }

    /// Look up a skill by callable ID in one workspace snapshot.
    pub(crate) fn lookup_skill(
        &self,
        workspace: &Path,
        id: &str,
    ) -> Result<Option<Arc<Skill>>, ToolError> {
        let registries = self
            .registries
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("Registry lock poisoned")))?;
        Ok(registries
            .get(workspace)
            .and_then(|registry| registry.get(id)))
    }

    /// Sorted callable IDs of one workspace snapshot.
    pub(crate) fn local_ids(&self, workspace: &Path) -> Vec<String> {
        self.registries
            .lock()
            .ok()
            .and_then(|registries| {
                registries.get(workspace).map(|registry| {
                    registry
                        .names()
                        .into_iter()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    }

    /// Refresh one workspace and return its description list.
    ///
    /// On refresh failure the previous snapshot is retained.
    pub(crate) fn refreshed_listing(&self, workspace: &Path) -> String {
        let _ = self.refresh_registry(workspace);

        match self.registries.lock() {
            Ok(registries) => match registries.get(workspace) {
                Some(registry) => registry.list_for_description(),
                None => "No skills available".to_string(),
            },
            Err(_) => "Registry unavailable".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::SkillMetadata;
    use std::sync::mpsc;

    fn skill(name: &str) -> Skill {
        Skill {
            path: PathBuf::from("/ignored").join(name),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: format!("{name} skill"),
                id: None,
                enabled: None,
                version: None,
                license: None,
                compatibility: None,
                allowed_tools: None,
                tags: None,
                author: None,
                extra: HashMap::new(),
            },
            content: name.to_string(),
            source: SkillSource::Global(PathBuf::from("/ignored")),
        }
    }

    #[tokio::test]
    async fn stale_refresh_does_not_replace_a_newer_snapshot() {
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let started_tx = Mutex::new(started_tx);
        let release_rx = Mutex::new(release_rx);
        let calls = AtomicU64::new(0);
        // The first discovery stalls and returns an outdated result; later
        // ones return the newer one.
        let discovery_fn = Arc::new(
            move |_: &[SkillSource], _: bool| -> anyhow::Result<Vec<Skill>> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    started_tx.lock().unwrap().send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                    Ok(vec![skill("old")])
                } else {
                    Ok(vec![skill("new")])
                }
            },
        );
        let catalog = Arc::new(
            SkillCatalog::from_parts(
                Arc::new(Mutex::new(SkillRegistry::new())),
                Arc::new(SkillPermissions::default()),
                vec![],
                false,
                PathBuf::from("."),
            )
            .with_discovery_fn(discovery_fn),
        );
        let workspace = PathBuf::from("/ws");

        let slow = {
            let (catalog, workspace) = (Arc::clone(&catalog), workspace.clone());
            tokio::spawn(async move { catalog.refresh_registry_async(&workspace).await })
        };
        // Wait until the slow refresh is inside discovery (ticket taken).
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();

        // A refresh that starts later finishes first.
        catalog.refresh_registry(&workspace).unwrap();
        release_tx.send(()).unwrap();
        slow.await.unwrap().unwrap();

        assert!(catalog.lookup_skill(&workspace, "new").unwrap().is_some());
        assert!(
            catalog.lookup_skill(&workspace, "old").unwrap().is_none(),
            "the older refresh must not replace the newer snapshot"
        );
    }

    #[test]
    fn has_workspace_reports_loaded_snapshots_only() {
        let catalog = SkillCatalog::from_parts(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![],
            false,
            PathBuf::from("/root"),
        )
        .with_discovery_fn(Arc::new(|_, _| Ok(vec![])));
        assert!(catalog.has_workspace(Path::new("/root")));
        assert!(!catalog.has_workspace(Path::new("/other")));
        catalog.refresh_registry(Path::new("/other")).unwrap();
        assert!(catalog.has_workspace(Path::new("/other")));
    }
}
