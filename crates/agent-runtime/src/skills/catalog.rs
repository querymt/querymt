//! Agent-level skill catalog.
//!
//! The catalog owns everything about skills that does not depend on a single
//! session: discovery sources, permissions, the per-workspace registry cache,
//! the `mcp` switch and the approval mode. A [`SkillTool`](super::SkillTool)
//! is a cheap value bound to one session and turn on top of a catalog.

use crate::config::{SkillsConfig, ToolSpec};
use crate::skills::discovery::{self, SkillScanOptions, SkillScanReport};
use crate::skills::permissions::SkillPermissions;
use crate::skills::registry::SkillRegistry;
use crate::skills::tool::SkillTool;
use crate::skills::types::{Skill, SkillInventoryEntry, SkillProblem, SkillSource};
use crate::tools::ToolError;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Discovery function used by registry refreshes. Swappable in tests to
/// inject deterministic discovery failures.
pub(crate) type DiscoverFn =
    dyn Fn(&[SkillSource], &SkillScanOptions) -> SkillScanReport + Send + Sync;

/// Agent-level skill state shared by every session of an agent.
pub struct SkillCatalog {
    registries: Arc<Mutex<HashMap<PathBuf, SkillRegistry>>>,
    permissions: Arc<SkillPermissions>,
    /// Discovery sources for the project root. Global and configured sources
    /// remain common; project sources are re-derived for a session workspace.
    sources: Vec<SkillSource>,
    project_root: PathBuf,
    include_external: bool,
    nested_depth: u8,
    mcp: bool,
    /// `(local source, server)` collision pairs already warned about.
    warned_collisions: Mutex<HashSet<(String, String)>>,
    discovery_fn: Arc<DiscoverFn>,
    /// Source of refresh tickets: taken before discovery starts, so a larger
    /// ticket means a refresh that began later.
    refresh_tickets: AtomicU64,
    /// Ticket, problem and snapshot bookkeeping per workspace. Always locked
    /// after `registries`.
    refresh_state: Mutex<RefreshState>,
}

/// Everything one refresh needs to scan, captured before it starts.
struct RefreshJob {
    ticket: u64,
    workspace: PathBuf,
    sources: Vec<SkillSource>,
    options: SkillScanOptions,
    discovery_fn: Arc<DiscoverFn>,
    started: Instant,
}

/// Cost of the newest skill refresh of one workspace.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SkillRefreshStats {
    /// Wall time from taking the ticket to the end of the scan.
    pub elapsed: Duration,
    /// Directory entries the scan visited.
    pub visited_entries: usize,
}

impl RefreshJob {
    fn scan(&self) -> SkillScanReport {
        (self.discovery_fn)(&self.sources, &self.options)
    }
}

/// Per-workspace refresh bookkeeping.
#[derive(Default)]
struct RefreshState {
    /// Newest ticket recorded per workspace; an older refresh finishing late
    /// is dropped instead of replacing newer state.
    tickets: HashMap<PathBuf, u64>,
    /// Problems of the newest refresh per workspace.
    problems: HashMap<PathBuf, Vec<SkillProblem>>,
    /// Workspaces that have a published skill set.
    snapshots: HashSet<PathBuf>,
    /// Cost of the newest refresh per workspace.
    stats: HashMap<PathBuf, SkillRefreshStats>,
}

impl SkillCatalog {
    /// Discover the configured skills and build the catalog for an agent.
    #[doc(hidden)]
    pub fn from_config(config: &SkillsConfig, project_root: &Path) -> Arc<Self> {
        let mut search_paths = discovery::default_search_paths(project_root);
        for custom_path in &config.paths {
            search_paths.push(SkillSource::Configured(custom_path.clone()));
        }

        let mut catalog = Self::from_parts(
            SkillRegistry::new(),
            Arc::new(config.permissions.clone()),
            search_paths,
            config.include_external,
            project_root.to_path_buf(),
        );
        catalog.mcp = config.mcp;
        catalog.nested_depth = config.nested_depth;

        // Startup uses the same path as every later refresh. A failure
        // leaves the catalog empty and is already logged.
        let _ = catalog.refresh_registry(project_root);
        let count = catalog.local_ids(project_root).len();
        if count > 0 {
            log::info!("Skills system initialized: {count} skills discovered");
            log::debug!(
                "Discovered skills: {}",
                catalog.local_ids(project_root).join(", ")
            );
        } else {
            log::debug!(
                "Skills system enabled but no skills found in {} search paths",
                catalog.sources.len()
            );
        }
        Arc::new(catalog)
    }

    /// Whether a tool list names the `skill` tool explicitly.
    pub fn names_skill_tool(tools: &[ToolSpec]) -> bool {
        tools
            .iter()
            .any(|tool| tool.is_builtin(crate::skills::SkillTool::NAME))
    }

    /// The catalog an agent gets, or `None` when it gets no skill tool: skills
    /// must be configured and enabled, and the tool list must be empty
    /// (every tool) or name `skill`.
    pub fn for_agent(
        skills: Option<&SkillsConfig>,
        tools: &[ToolSpec],
        project_root: &Path,
    ) -> Option<Arc<Self>> {
        Self::skills_for_agent(skills, tools).map(|skills| Self::from_config(skills, project_root))
    }

    /// The skills configuration under which an agent gets the skill tool, or
    /// `None` when it gets none. This is the predicate behind [`Self::for_agent`],
    /// for callers that need the decision without discovering skills.
    pub fn skills_for_agent<'a>(
        skills: Option<&'a SkillsConfig>,
        tools: &[ToolSpec],
    ) -> Option<&'a SkillsConfig> {
        let skills = skills.filter(|skills| skills.enabled)?;
        (tools.is_empty() || Self::names_skill_tool(tools)).then_some(skills)
    }

    /// Build a catalog from already-resolved parts.
    pub(crate) fn from_parts(
        initial_registry: SkillRegistry,
        permissions: Arc<SkillPermissions>,
        sources: Vec<SkillSource>,
        include_external: bool,
        project_root: PathBuf,
    ) -> Self {
        let mut state = RefreshState::default();
        if !initial_registry.names().is_empty() {
            state.snapshots.insert(project_root.clone());
        }
        let mut registries = HashMap::new();
        registries.insert(project_root.clone(), initial_registry);

        Self {
            registries: Arc::new(Mutex::new(registries)),
            permissions,
            sources,
            project_root,
            include_external,
            nested_depth: discovery::DEFAULT_NESTED_DEPTH,
            mcp: true,
            warned_collisions: Mutex::new(HashSet::new()),
            discovery_fn: Arc::new(discovery::discover),
            refresh_tickets: AtomicU64::new(0),
            refresh_state: Mutex::new(state),
        }
    }

    /// Set how deep a project `.agents/skills` folder is scanned.
    pub fn with_nested_depth(mut self, nested_depth: u8) -> Self {
        self.nested_depth = nested_depth;
        self
    }

    fn scan_options(&self) -> SkillScanOptions {
        SkillScanOptions {
            include_external: self.include_external,
            nested_depth: self.nested_depth,
        }
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
        skill_grants: Arc<parking_lot::Mutex<HashSet<String>>>,
    ) -> SkillTool {
        SkillTool::bind(
            Arc::clone(self),
            workspace.unwrap_or(&self.project_root).to_path_buf(),
            mcp_state,
            tool_config,
            skill_grants,
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

    /// Start a refresh: take the ticket and capture everything the scan needs,
    /// so it can run inline or on a blocking thread.
    fn begin_refresh(&self, workspace: &Path) -> RefreshJob {
        RefreshJob {
            ticket: self.refresh_tickets.fetch_add(1, Ordering::SeqCst) + 1,
            workspace: workspace.to_path_buf(),
            sources: self.sources_for_workspace(workspace),
            options: self.scan_options(),
            discovery_fn: Arc::clone(&self.discovery_fn),
            started: Instant::now(),
        }
    }

    /// Re-discover skills for one workspace and atomically replace only that
    /// workspace's registry snapshot.
    pub(crate) fn refresh_registry(&self, workspace: &Path) -> anyhow::Result<()> {
        let job = self.begin_refresh(workspace);
        let report = job.scan();
        self.finish_refresh(&job, report)
    }

    /// Async variant of [`Self::refresh_registry`] for the tool-call path:
    /// discovery runs off the Tokio worker via `spawn_blocking`, and the
    /// registry lock is only held to publish the results.
    pub(crate) async fn refresh_registry_async(&self, workspace: &Path) -> anyhow::Result<()> {
        let job = self.begin_refresh(workspace);
        let (job, report) = tokio::task::spawn_blocking(move || {
            let report = job.scan();
            (job, report)
        })
        .await
        .map_err(|e| anyhow::anyhow!("skill discovery task failed: {e}"))?;
        self.finish_refresh(&job, report)
    }

    /// Record the problems of a scan and publish its skills, unless a
    /// refresh that started later already published for this workspace.
    ///
    /// A scan with an unreadable source folder keeps the previously
    /// published skill set and fails; when nothing was published yet for the
    /// workspace, the skills of the readable sources are published instead.
    /// Problems are recorded in every case.
    fn finish_refresh(&self, job: &RefreshJob, report: SkillScanReport) -> anyhow::Result<()> {
        let workspace = job.workspace.as_path();
        let ticket = job.ticket;
        let stats = SkillRefreshStats {
            elapsed: job.started.elapsed(),
            visited_entries: report.visited_entries,
        };
        log::debug!(
            "Skill refresh for {}: {} entries visited in {:.3} ms",
            workspace.display(),
            stats.visited_entries,
            stats.elapsed.as_secs_f64() * 1000.0
        );
        for problem in &report.problems {
            log::warn!(
                "Skill problem ({}) at {}: {}",
                problem.kind,
                problem.path.display(),
                problem.message
            );
        }
        let mut registries = self
            .registries
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
        let mut state = self
            .refresh_state
            .lock()
            .map_err(|_| anyhow::anyhow!("refresh state lock poisoned"))?;
        if state
            .tickets
            .get(workspace)
            .is_some_and(|newest| *newest > ticket)
        {
            log::debug!(
                "Dropping stale skill refresh for {}: a newer one was already published",
                workspace.display()
            );
            return Ok(());
        }
        state.tickets.insert(workspace.to_path_buf(), ticket);
        state.stats.insert(workspace.to_path_buf(), stats);
        state
            .problems
            .insert(workspace.to_path_buf(), report.problems);
        if report.unreadable_sources && state.snapshots.contains(workspace) {
            let error = anyhow::anyhow!("a skill source folder could not be read");
            log::warn!(
                "Failed to refresh skills for {}: {error:#}. Retaining previously discovered skills.",
                workspace.display()
            );
            return Err(error);
        }
        let count = registries
            .entry(workspace.to_path_buf())
            .or_default()
            .reload_with(report.skills);
        state.snapshots.insert(workspace.to_path_buf());
        log::debug!(
            "Skill registry refreshed for {}: {count} local skills available",
            workspace.display()
        );
        Ok(())
    }

    /// The problems of the newest skill refresh of one workspace, ordered by
    /// path. Empty when the workspace was never refreshed.
    pub fn problems(&self, workspace: &Path) -> Vec<SkillProblem> {
        self.refresh_state
            .lock()
            .ok()
            .and_then(|state| state.problems.get(workspace).cloned())
            .unwrap_or_default()
    }

    /// Wall time and visited entry count of the newest refresh of one
    /// workspace, or `None` when it was never refreshed.
    pub(crate) fn last_refresh_stats(&self, workspace: &Path) -> Option<SkillRefreshStats> {
        self.refresh_state
            .lock()
            .ok()
            .and_then(|state| state.stats.get(workspace).copied())
    }

    /// The local skills offered for one workspace, ordered by callable ID.
    pub fn inventory(&self, workspace: &Path) -> Vec<SkillInventoryEntry> {
        let Ok(registries) = self.registries.lock() else {
            return Vec::new();
        };
        let mut entries: Vec<SkillInventoryEntry> = registries
            .get(workspace)
            .map(|registry| {
                registry
                    .all()
                    .map(|skill| SkillInventoryEntry::from_skill(skill))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
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
    use crate::skills::types::{SkillMetadata, SkillProblemKind};
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
            move |_: &[SkillSource], _: &SkillScanOptions| -> SkillScanReport {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    started_tx.lock().unwrap().send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                    SkillScanReport {
                        skills: vec![skill("old")],
                        ..Default::default()
                    }
                } else {
                    SkillScanReport {
                        skills: vec![skill("new")],
                        ..Default::default()
                    }
                }
            },
        );
        let catalog = Arc::new(
            SkillCatalog::from_parts(
                SkillRegistry::new(),
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
            SkillRegistry::new(),
            Arc::new(SkillPermissions::default()),
            vec![],
            false,
            PathBuf::from("/root"),
        )
        .with_discovery_fn(Arc::new(|_, _| SkillScanReport::default()));
        assert!(catalog.has_workspace(Path::new("/root")));
        assert!(!catalog.has_workspace(Path::new("/other")));
        catalog.refresh_registry(Path::new("/other")).unwrap();
        assert!(catalog.has_workspace(Path::new("/other")));
    }

    fn empty_catalog(sources: Vec<SkillSource>) -> SkillCatalog {
        SkillCatalog::from_parts(
            SkillRegistry::new(),
            Arc::new(SkillPermissions::default()),
            sources,
            true,
            PathBuf::from("/root"),
        )
    }

    fn problem(kind: SkillProblemKind, path: &str) -> SkillProblem {
        SkillProblem {
            path: PathBuf::from(path),
            source: PathBuf::from("/src"),
            kind,
            message: "injected".to_string(),
        }
    }

    fn write_skill_file(dir: &Path, name: &str, frontmatter: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\nBody\n"),
        )
        .unwrap();
    }

    #[test]
    fn first_discovery_with_an_unreadable_source_offers_the_readable_sources() {
        let catalog = empty_catalog(vec![]).with_discovery_fn(Arc::new(|_, _| SkillScanReport {
            skills: vec![skill("readable")],
            problems: vec![problem(SkillProblemKind::SourceUnreadable, "/locked")],
            unreadable_sources: true,
            visited_entries: 0,
        }));
        let workspace = Path::new("/ws");

        // Nothing was published before, so the refresh succeeds.
        catalog.refresh_registry(workspace).unwrap();
        assert!(
            catalog
                .lookup_skill(workspace, "readable")
                .unwrap()
                .is_some()
        );
        let problems = catalog.problems(workspace);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, SkillProblemKind::SourceUnreadable);
    }

    #[test]
    fn unreadable_source_is_recorded_while_the_earlier_skill_set_is_kept() {
        let calls = AtomicU64::new(0);
        let catalog = empty_catalog(vec![]).with_discovery_fn(Arc::new(move |_, _| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                SkillScanReport {
                    skills: vec![skill("kept")],
                    ..Default::default()
                }
            } else {
                SkillScanReport {
                    skills: vec![skill("partial")],
                    problems: vec![problem(SkillProblemKind::SourceUnreadable, "/locked")],
                    unreadable_sources: true,
                    visited_entries: 0,
                }
            }
        }));
        let workspace = Path::new("/ws");

        catalog.refresh_registry(workspace).unwrap();
        assert!(catalog.problems(workspace).is_empty());

        assert!(catalog.refresh_registry(workspace).is_err());
        let problems = catalog.problems(workspace);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, SkillProblemKind::SourceUnreadable);
        assert_eq!(problems[0].path, PathBuf::from("/locked"));
        assert!(catalog.lookup_skill(workspace, "kept").unwrap().is_some());
        assert!(
            catalog
                .lookup_skill(workspace, "partial")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn fixed_problem_disappears_on_the_next_refresh() {
        let dir = tempfile::TempDir::new().unwrap();
        // Missing `description` makes the definition malformed.
        write_skill_file(dir.path(), "broken", "name: broken\n");
        let catalog = empty_catalog(vec![SkillSource::Global(dir.path().to_path_buf())]);
        let workspace = Path::new("/ws");

        catalog.refresh_registry(workspace).unwrap();
        let problems = catalog.problems(workspace);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, SkillProblemKind::InvalidDefinition);
        assert_eq!(problems[0].path, dir.path().join("broken").join("SKILL.md"));

        write_skill_file(
            dir.path(),
            "broken",
            "name: broken\ndescription: Fixed now\n",
        );
        catalog.refresh_registry(workspace).unwrap();
        assert!(catalog.problems(workspace).is_empty());
        assert!(catalog.lookup_skill(workspace, "broken").unwrap().is_some());
    }

    #[test]
    fn refresh_records_visited_entries_and_wall_time() {
        let dir = tempfile::TempDir::new().unwrap();
        write_skill_file(dir.path(), "one", "name: one\ndescription: One\n");
        write_skill_file(dir.path(), "two", "name: two\ndescription: Two\n");
        let catalog = empty_catalog(vec![SkillSource::Global(dir.path().to_path_buf())]);
        let workspace = Path::new("/ws");

        assert!(catalog.last_refresh_stats(workspace).is_none());
        catalog.refresh_registry(workspace).unwrap();
        let stats = catalog.last_refresh_stats(workspace).unwrap();
        // The source folder, two skill folders and two definition files.
        assert!(stats.visited_entries >= 5, "{stats:?}");
        assert!(stats.elapsed > Duration::ZERO);
    }

    /// Measurement for design D12, not a regression test: ten consecutive
    /// refreshes over this repository's own project skill folders (including
    /// the linked `.agents/skills/querymt-dev`).
    /// Run with `--run-ignored only --no-capture`.
    #[test]
    #[ignore = "measurement against this repository's skill folders"]
    fn measure_refresh_over_this_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let sources: Vec<SkillSource> = discovery::default_search_paths(&root)
            .into_iter()
            .filter(|source| matches!(source, SkillSource::Project(_)))
            .collect();
        let catalog = SkillCatalog::from_parts(
            SkillRegistry::new(),
            Arc::new(SkillPermissions::default()),
            sources,
            true,
            root.clone(),
        );
        for run in 1..=10 {
            catalog.refresh_registry(&root).unwrap();
            let stats = catalog.last_refresh_stats(&root).unwrap();
            println!(
                "refresh {run}: {} entries visited, {:.3} ms, {} skills offered, {} problems",
                stats.visited_entries,
                stats.elapsed.as_secs_f64() * 1000.0,
                catalog.inventory(&root).len(),
                catalog.problems(&root).len()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_entry_inside_a_source_does_not_fail_the_refresh() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        write_skill_file(dir.path(), "valid", "name: valid\ndescription: Valid\n");
        write_skill_file(dir.path(), "locked", "name: locked\ndescription: Locked\n");
        let locked = dir.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: privileged user can still read the locked directory");
            return;
        }

        let catalog = empty_catalog(vec![SkillSource::Global(dir.path().to_path_buf())]);
        let workspace = Path::new("/ws");
        let result = catalog.refresh_registry(workspace);
        let ids = catalog.local_ids(workspace);
        let problems = catalog.problems(workspace);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        result.unwrap();
        assert_eq!(ids, vec!["valid".to_string()]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].kind, SkillProblemKind::EntryUnreadable);
        assert_eq!(problems[0].path, locked);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_lists_offered_skills_with_their_origin() {
        let dir = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        write_skill_file(dir.path(), "plain", "name: plain\ndescription: Plain\n");
        write_skill_file(
            outside.path(),
            "target",
            "name: linked\ndescription: Linked\n",
        );
        std::os::unix::fs::symlink(outside.path().join("target"), dir.path().join("linked"))
            .unwrap();

        let catalog = empty_catalog(vec![SkillSource::Global(dir.path().to_path_buf())]);
        let workspace = Path::new("/ws");
        catalog.refresh_registry(workspace).unwrap();

        let inventory = catalog.inventory(workspace);
        let ids: Vec<&str> = inventory.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec!["linked", "plain"]);
        assert_eq!(inventory[0].source_folder, dir.path());
        assert_eq!(inventory[0].directory, dir.path().join("linked"));
        assert_eq!(
            inventory[0].link_target,
            Some(outside.path().join("target").canonicalize().unwrap())
        );
        assert_eq!(inventory[1].link_target, None);
    }

    #[cfg(unix)]
    #[test]
    fn inventory_records_the_target_of_a_linked_source_folder() {
        let real = tempfile::TempDir::new().unwrap();
        let holder = tempfile::TempDir::new().unwrap();
        write_skill_file(real.path(), "review", "name: review\ndescription: Review\n");
        let source = holder.path().join("skills-link");
        std::os::unix::fs::symlink(real.path(), &source).unwrap();

        let catalog = empty_catalog(vec![SkillSource::Global(source.clone())]);
        let workspace = Path::new("/ws");
        catalog.refresh_registry(workspace).unwrap();

        let inventory = catalog.inventory(workspace);
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].directory, source.join("review"));
        assert_eq!(
            inventory[0].link_target,
            Some(real.path().join("review").canonicalize().unwrap())
        );
    }

    #[test]
    fn configured_skill_folder_is_offered_in_any_workspace() {
        use crate::config::SkillsConfig;

        let temp = tempfile::TempDir::new().unwrap();
        let skill_dir = temp.path().join("scholarly-sources");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: scholarly-sources\ndescription: Find papers\n---\nBody\n",
        )
        .unwrap();
        let config = SkillsConfig {
            include_external: false,
            paths: vec![skill_dir],
            ..Default::default()
        };
        let project_root = temp.path().join("project");
        let worktree = temp.path().join("worktree");

        let catalog = SkillCatalog::from_config(&config, &project_root);
        catalog.refresh_registry(&worktree).unwrap();

        for workspace in [&project_root, &worktree] {
            assert_eq!(
                catalog.local_ids(workspace),
                vec!["scholarly-sources"],
                "{}",
                workspace.display()
            );
        }
    }

    #[test]
    fn for_agent_applies_one_rule_for_the_skill_tool() {
        use crate::config::{SkillsConfig, ToolSpec};

        let root = tempfile::TempDir::new().unwrap();
        let enabled = SkillsConfig {
            include_external: false,
            ..Default::default()
        };
        let disabled = SkillsConfig {
            enabled: false,
            ..Default::default()
        };
        let (specs, error) = crate::delegation::member::parse_tool_specs(["skill"]);
        assert!(error.is_none());
        let (other, error) = crate::delegation::member::parse_tool_specs(["shell"]);
        assert!(error.is_none());
        let none: Vec<ToolSpec> = Vec::new();

        let for_agent = |skills: Option<&SkillsConfig>, tools: &[ToolSpec]| {
            SkillCatalog::for_agent(skills, tools, root.path()).is_some()
        };
        assert!(for_agent(Some(&enabled), &none), "empty list admits skill");
        assert!(for_agent(Some(&enabled), &specs), "named skill is admitted");
        assert!(!for_agent(Some(&enabled), &other), "other tools only");
        assert!(!for_agent(Some(&disabled), &none), "skills disabled");
        assert!(!for_agent(None, &none), "skills not configured");
        assert!(SkillCatalog::names_skill_tool(&specs));
        assert!(!SkillCatalog::names_skill_tool(&none));
    }
}
