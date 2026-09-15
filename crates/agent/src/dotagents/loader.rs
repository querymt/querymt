//! Disk-reading layer assembly for `.agents` Protocol roots (task 1.6).
//!
//! This module is the bridge between confinement ([`super::confine`]), the
//! per-file parsers ([`super::parsers`]), and deterministic merging
//! ([`super::merge`]). It reads exactly the protocol layout from one layer
//! root and produces a [`DotagentsLayerContent`]:
//!
//! ```text
//! <root>/agents.md                  singleton
//! <root>/system-prompt.md           singleton
//! <root>/mcp.json                   keyed by mcpServers entry name
//! <root>/models.json                keyed by preset name
//! <root>/skills/<id>/skill.md       keyed by id (or directory name)
//! <root>/skills/<id>/SKILL.md       keyed by id (or directory name)
//! <root>/agents/<id>/agent.md       keyed by id (or directory name)
//! <root>/agents/<id>/config.json    adjacent supported settings
//! <root>/tasks/<id>/task.md         keyed by id (or directory name)
//! <root>/memories/<id>.md           keyed by file stem (or id)
//! ```
//!
//! Assembly is **side-effect free**: it only reads regular files that pass
//! confinement, never creates directories, never starts a process, and never
//! persists anything. Directory iteration is sorted so identical trees always
//! produce identical layer content, and entries that normalize to the same ID
//! within one layer are rejected with a duplicate diagnostic
//! ([`super::merge::insert_unique`]).
//!
//! Unsupported top-level protocol entries (`speakmcp-settings.json`,
//! `layouts/`, `.backups/`) are detected and recorded as
//! [`super::manifest::DotagentsUnsupported`] rather than silently ignored.

use super::confine::{ConfineResult, ConfinementPolicy};
use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::layer::{DotagentsLayer, DotagentsSource};
use super::manifest::{DotagentsManifest, DotagentsUnsupported, DotagentsUnsupportedKind};
use super::merge::{DotagentsLayerContent, insert_unique, normalize_id, source_of};
use super::parsers::{
    parse_agent_config, parse_agent_document, parse_mcp_document, parse_memory_document,
    parse_models_document, parse_prompt_document, parse_task_document,
};
use super::{
    DotagentsLoadOptions, DotagentsStrictness, ResolvedLayerRoot, discover_layer_roots,
    merge_layers,
};
use std::path::{Path, PathBuf};

/// Top-level protocol entry names this implementation does not interpret.
///
/// These are detected and recorded for diagnostics, per the documented
/// support matrix.
const UNSUPPORTED_ENTRIES: &[(&str, DotagentsUnsupportedKind)] = &[
    (
        "speakmcp-settings.json",
        DotagentsUnsupportedKind::SpeakMcpSettings,
    ),
    ("layouts", DotagentsUnsupportedKind::Layouts),
    (".backups", DotagentsUnsupportedKind::Backups),
];

/// Assemble one layer's content from its root directory.
///
/// Returns empty content when the root does not exist or cannot be
/// canonicalized; no diagnostic is emitted for an absent root because absence
/// is the normal "layer not configured" case.
pub fn assemble_layer(
    layer: DotagentsLayer,
    root: &Path,
    trusted_roots: &[PathBuf],
) -> DotagentsLayerContent {
    let mut content = DotagentsLayerContent::new(layer);

    let Some(policy) = ConfinementPolicy::for_layer(layer, root, trusted_roots) else {
        return content;
    };

    read_singletons(&policy, &mut content);
    read_mcp(&policy, &mut content);
    read_models(&policy, &mut content);
    read_skills(&policy, &mut content);
    read_agents(&policy, &mut content);
    read_tasks(&policy, &mut content);
    read_memories(&policy, &mut content);
    detect_unsupported(&policy, &mut content);

    content
}

/// A strict-mode load failure containing the inspectable resolved manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsLoadError {
    /// The fully resolved manifest, including all source-aware diagnostics.
    pub manifest: DotagentsManifest,
}

impl std::fmt::Display for DotagentsLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            ".agents resolution failed with {} error diagnostic(s)",
            self.manifest.errors().count()
        )
    }
}

impl std::error::Error for DotagentsLoadError {}

/// Side-effect-free `.agents` Protocol loader.
#[derive(Debug, Clone)]
pub struct DotagentsLoader {
    options: DotagentsLoadOptions,
}

impl DotagentsLoader {
    /// Create a loader from explicit load options.
    pub fn new(options: DotagentsLoadOptions) -> Self {
        Self { options }
    }

    /// Resolve enabled protocol roots into a deterministic manifest.
    ///
    /// Compatibility mode returns valid siblings alongside diagnostics. Strict
    /// mode returns [`DotagentsLoadError`] when any error diagnostic exists.
    /// Neither mode starts MCP servers, schedules tasks, or imports memories.
    pub fn load(&self) -> Result<DotagentsManifest, Box<DotagentsLoadError>> {
        let roots = discover_layer_roots(&self.options);
        let layers: Vec<DotagentsLayerContent> = roots
            .iter()
            .filter(|root| root.exists)
            .map(|root| assemble_resolved_layer(root, self.options.trusted_roots()))
            .collect();
        let mut manifest = merge_layers(&layers);
        manifest.layers = roots.iter().map(ResolvedLayerRoot::as_source_ref).collect();
        manifest.sort_deterministically();

        if self.options.strictness() == DotagentsStrictness::Strict && manifest.has_errors() {
            Err(Box::new(DotagentsLoadError { manifest }))
        } else {
            Ok(manifest)
        }
    }

    /// Resolve a manifest without runtime side effects.
    pub fn preview(&self) -> Result<DotagentsManifest, Box<DotagentsLoadError>> {
        self.load()
    }

    /// The options used by this loader.
    pub fn options(&self) -> &DotagentsLoadOptions {
        &self.options
    }
}

impl Default for DotagentsLoader {
    fn default() -> Self {
        Self::new(DotagentsLoadOptions::default())
    }
}

fn assemble_resolved_layer(
    root: &ResolvedLayerRoot,
    trusted_roots: &[PathBuf],
) -> DotagentsLayerContent {
    assemble_layer(root.layer, &root.root, trusted_roots)
}

/// Resolve builder protocol inputs without triggering runtime side effects.
pub(crate) fn resolve_for_builder(
    supplied: Option<DotagentsManifest>,
    options: Option<DotagentsLoadOptions>,
    workspace: Option<&Path>,
) -> Result<Option<DotagentsManifest>, Box<DotagentsLoadError>> {
    if let Some(manifest) = supplied {
        return Ok(Some(manifest));
    }
    let Some(mut options) = options else {
        return Ok(None);
    };
    if !options.is_enabled() {
        return Ok(None);
    }
    if options.workspace().is_none()
        && options.workspace_root_override().is_none()
        && let Some(workspace) = workspace
    {
        options = options.with_workspace(workspace);
    }
    DotagentsLoader::new(options).load().map(Some)
}

/// Append protocol singleton bodies in the required order.
pub(crate) fn compose_prompt(params: &mut querymt::LLMParams, manifest: &DotagentsManifest) {
    if let Some(prompt) = &manifest.system_prompt
        && !prompt.is_empty()
    {
        params.system.push(prompt.body.clone());
    }
    if let Some(prompt) = &manifest.agents_md
        && !prompt.is_empty()
    {
        params.system.push(prompt.body.clone());
    }
}

/// Read a layer-relative text file, returning `None` when it does not exist.
fn read_optional(policy: &ConfinementPolicy, relative: &Path) -> Option<ConfineResult<String>> {
    // Only attempt files that exist, so an absent optional file is not an
    // error. Confinement still applies once we decide to read.
    let lexical = policy.allowed_root().join(relative);
    if !lexical.exists() {
        return None;
    }
    Some(super::confine::read_confined_text(policy, relative))
}

/// Push a diagnostic produced while reading a singleton or collection entry.
fn push_error(content: &mut DotagentsLayerContent, error: DotagentsDiagnostic) {
    content.diagnostics.push(error);
}

fn mark_invalid_prompt(content: &mut DotagentsLayerContent, file: &str) {
    if file == "agents.md" {
        content.invalid_agents_md = true;
    } else {
        content.invalid_system_prompt = true;
    }
}

/// Read the two Markdown singletons.
fn read_singletons(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    for file in ["agents.md", "system-prompt.md"] {
        let relative = Path::new(file);
        let Some(result) = read_optional(policy, relative) else {
            continue;
        };
        let source =
            DotagentsSource::singleton(policy.layer(), policy.allowed_root().join(relative));
        match result {
            Ok(text) => match parse_prompt_document(&text, source) {
                Ok(prompt) => {
                    if file == "agents.md" {
                        content.agents_md = Some(prompt);
                    } else {
                        content.system_prompt = Some(prompt);
                    }
                }
                Err(error) => {
                    mark_invalid_prompt(content, file);
                    push_error(content, *error);
                }
            },
            Err(error) => {
                mark_invalid_prompt(content, file);
                push_error(content, *error);
            }
        }
    }
}

/// Read `mcp.json`, isolating invalid servers from valid siblings.
fn read_mcp(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let relative = Path::new("mcp.json");
    let Some(result) = read_optional(policy, relative) else {
        return;
    };
    let source = DotagentsSource::singleton(policy.layer(), policy.allowed_root().join(relative));

    let text = match result {
        Ok(text) => text,
        Err(error) => {
            content.invalid_mcp = true;
            push_error(content, *error);
            return;
        }
    };

    match parse_mcp_document(&text, source) {
        Ok(parsed) => {
            for (name, server) in parsed.servers {
                content.mcp_servers.insert(normalize_id(&name), server);
            }
            content.diagnostics.extend(parsed.diagnostics);
        }
        Err(error) => {
            content.invalid_mcp = true;
            push_error(content, *error);
        }
    }
}

/// Read `models.json`, isolating invalid presets from valid siblings.
fn read_models(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let relative = Path::new("models.json");
    let Some(result) = read_optional(policy, relative) else {
        return;
    };
    let source = DotagentsSource::singleton(policy.layer(), policy.allowed_root().join(relative));

    let text = match result {
        Ok(text) => text,
        Err(error) => {
            content.invalid_models = true;
            push_error(content, *error);
            return;
        }
    };

    match parse_models_document(&text, source) {
        Ok(parsed) => {
            for (name, preset) in parsed.presets {
                content.model_presets.insert(normalize_id(&name), preset);
            }
            content.diagnostics.extend(parsed.diagnostics);
        }
        Err(error) => {
            content.invalid_models = true;
            push_error(content, *error);
        }
    }
}

/// Read `skills/<id>/{skill.md,SKILL.md}`.
///
/// Both spellings are accepted. When an entry directory contains both, that is
/// a duplicate definition within one protocol entry, so it is an error rather
/// than a platform-order-dependent choice.
fn read_skills(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let Some(entries) = sorted_subdirectories(policy, Path::new("skills")) else {
        return;
    };

    for entry in entries {
        let entry_id = entry.file_name_lossy();
        let dir = PathBuf::from("skills").join(&entry_id);
        let lower = dir.join("skill.md");
        let upper = dir.join("SKILL.md");

        // Inspect directory-entry names exactly. On case-insensitive
        // filesystems, `Path::exists` reports both spellings for one file.
        let exact_names: std::collections::BTreeSet<String> =
            std::fs::read_dir(policy.allowed_root().join(&dir))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect();
        let has_lower = exact_names.contains("skill.md");
        let has_upper = exact_names.contains("SKILL.md");

        if has_lower && has_upper {
            let source = DotagentsSource::entry(
                policy.layer(),
                policy.allowed_root().join(&lower),
                entry_id.clone(),
            );
            content.diagnostics.push(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::DuplicateId,
                    format!(
                        "skill `{entry_id}` defines both `skill.md` and `SKILL.md`; \
                         keep exactly one definition"
                    ),
                )
                .with_source(source),
            );
            continue;
        }

        let relative = if has_lower {
            lower
        } else if has_upper {
            upper
        } else {
            continue;
        };

        let source = DotagentsSource::entry(
            policy.layer(),
            policy.allowed_root().join(&relative),
            entry_id.clone(),
        );
        match read_optional(policy, &relative) {
            Some(Ok(text)) => {
                match super::parsers::parse_skill_document(&text, &entry_id, source.clone()) {
                    Ok(skill) => {
                        let id = normalize_id(&skill.id);
                        insert_unique(
                            &mut content.skills,
                            &id,
                            skill,
                            source_of::skill,
                            &source,
                            &mut content.diagnostics,
                            "skill",
                        );
                    }
                    Err(error) => push_error(content, *error),
                }
            }
            Some(Err(error)) => push_error(content, *error),
            None => {}
        }
    }
}

/// Read `agents/<id>/agent.md` plus an adjacent `config.json`.
fn read_agents(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let Some(entries) = sorted_subdirectories(policy, Path::new("agents")) else {
        return;
    };

    for entry in entries {
        let entry_id = entry.file_name_lossy();
        let relative = PathBuf::from("agents").join(&entry_id).join("agent.md");
        let source = DotagentsSource::entry(
            policy.layer(),
            policy.allowed_root().join(&relative),
            entry_id.clone(),
        );

        let text = match read_optional(policy, &relative) {
            Some(Ok(text)) => text,
            Some(Err(error)) => {
                push_error(content, *error);
                continue;
            }
            None => continue,
        };

        let mut parsed = match parse_agent_document(&text, &entry_id, source.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                push_error(content, *error);
                continue;
            }
        };

        let Some(mut agent) = parsed.agent.take() else {
            content.diagnostics.append(&mut parsed.diagnostics);
            continue;
        };

        // Apply the adjacent config.json, when present. A malformed config is
        // an isolated error for this profile rather than a layer-wide failure.
        let config_relative = PathBuf::from("agents").join(&entry_id).join("config.json");
        if let Some(result) = read_optional(policy, &config_relative) {
            let config_source = DotagentsSource::entry(
                policy.layer(),
                policy.allowed_root().join(&config_relative),
                entry_id.clone(),
            );
            match result {
                Ok(config_text) => match parse_agent_config(&config_text, config_source) {
                    Ok(config) => agent.config = config,
                    Err(error) => {
                        parsed.diagnostics.push(*error);
                    }
                },
                Err(error) => parsed.diagnostics.push(*error),
            }
        }

        let id = normalize_id(&agent.id);
        insert_unique(
            &mut content.agents,
            &id,
            agent,
            source_of::agent,
            &source,
            &mut content.diagnostics,
            "agent",
        );
        content.diagnostics.append(&mut parsed.diagnostics);
    }
}

/// Read `tasks/<id>/task.md`.
fn read_tasks(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let Some(entries) = sorted_subdirectories(policy, Path::new("tasks")) else {
        return;
    };

    for entry in entries {
        let entry_id = entry.file_name_lossy();
        let relative = PathBuf::from("tasks").join(&entry_id).join("task.md");
        let source = DotagentsSource::entry(
            policy.layer(),
            policy.allowed_root().join(&relative),
            entry_id.clone(),
        );

        let text = match read_optional(policy, &relative) {
            Some(Ok(text)) => text,
            Some(Err(error)) => {
                push_error(content, *error);
                continue;
            }
            None => continue,
        };

        match parse_task_document(&text, &entry_id, source.clone()) {
            Ok(task) => {
                let id = normalize_id(&task.id);
                insert_unique(
                    &mut content.tasks,
                    &id,
                    task,
                    source_of::task,
                    &source,
                    &mut content.diagnostics,
                    "task",
                );
            }
            Err(error) => push_error(content, *error),
        }
    }
}

/// Read `memories/<id>.md` (flat files, not directories).
fn read_memories(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    let Some(entries) = sorted_dir_entries(policy, Path::new("memories")) else {
        return;
    };

    for entry in entries {
        let file_name = entry.file_name_lossy();
        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        let relative = PathBuf::from("memories").join(&file_name);
        let source = DotagentsSource::entry(
            policy.layer(),
            policy.allowed_root().join(&relative),
            stem.to_string(),
        );

        let text = match read_optional(policy, &relative) {
            Some(Ok(text)) => text,
            Some(Err(error)) => {
                push_error(content, *error);
                continue;
            }
            None => continue,
        };

        match parse_memory_document(&text, stem, source.clone()) {
            Ok(memory) => {
                let id = normalize_id(&memory.id);
                insert_unique(
                    &mut content.memories,
                    &id,
                    memory,
                    source_of::memory,
                    &source,
                    &mut content.diagnostics,
                    "memory",
                );
            }
            Err(error) => push_error(content, *error),
        }
    }
}

/// Record unsupported top-level protocol entries for diagnostics.
fn detect_unsupported(policy: &ConfinementPolicy, content: &mut DotagentsLayerContent) {
    for (name, kind) in UNSUPPORTED_ENTRIES {
        let lexical = policy.allowed_root().join(name);
        if !lexical.exists() {
            continue;
        }
        content.unsupported.push(DotagentsUnsupported {
            kind: *kind,
            id: (*name).to_string(),
            source: DotagentsSource::singleton(policy.layer(), lexical),
        });
        content.diagnostics.push(
            DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::Other,
                format!(
                    "`{name}` is not supported by this implementation and was ignored \
                     (recorded for inspection)"
                ),
            ),
            // A warning, not an error: unsupported content must not block
            // supported protocol features.
        );
    }
}

/// A directory entry with a lossy name, used for deterministic iteration.
struct DirEntry {
    name: String,
    is_dir: bool,
}

impl DirEntry {
    fn file_name_lossy(&self) -> String {
        self.name.clone()
    }
}

/// List the sorted child entries of a layer-relative directory.
///
/// Returns `None` when the directory is absent. Collection layout differs by
/// kind: `skills/`, `agents/`, and `tasks/` are directory-per-entry, while
/// `memories/` is flat files. The caller decides which kind it expects by
/// filtering on [`DirEntry::is_dir`].
fn sorted_dir_entries(policy: &ConfinementPolicy, relative: &Path) -> Option<Vec<DirEntry>> {
    let dir = policy.allowed_root().join(relative);
    let read = std::fs::read_dir(&dir).ok()?;

    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in read.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            // Non-UTF8 entry names are skipped deterministically rather than
            // causing a lossy or platform-dependent ID.
            continue;
        };
        let is_dir = entry.path().is_dir();
        entries.push(DirEntry { name, is_dir });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Some(entries)
}

/// List only directory children (for `skills/`, `agents/`, `tasks/`).
fn sorted_subdirectories(policy: &ConfinementPolicy, relative: &Path) -> Option<Vec<DirEntry>> {
    let mut entries = sorted_dir_entries(policy, relative)?;
    entries.retain(|entry| entry.is_dir);
    Some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn absent_root_yields_empty_content_without_side_effects() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join(".agents");
        let content = assemble_layer(DotagentsLayer::Workspace, &missing, &[]);

        assert!(content.agents_md.is_none());
        assert!(content.mcp_servers.is_empty());
        assert!(content.diagnostics.is_empty());
        // Assembly must not create the root.
        assert!(!missing.exists());
    }

    #[test]
    fn reads_singletons_and_json() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "agents.md", "Instructions.\n");
        write(tmp.path(), "system-prompt.md", "System.\n");
        write(
            tmp.path(),
            "mcp.json",
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        );
        write(
            tmp.path(),
            "models.json",
            r#"{"models":{"fast":{"provider":"p","model":"m"}}}"#,
        );

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert_eq!(content.agents_md.unwrap().body, "Instructions.\n");
        assert_eq!(content.system_prompt.unwrap().body, "System.\n");
        assert_eq!(content.mcp_servers.len(), 1);
        assert!(content.mcp_servers.contains_key("fs"));
        assert!(content.model_presets.contains_key("fast"));
        assert!(content.diagnostics.is_empty());
    }

    #[test]
    fn reads_collections_deterministically() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "skills/a/skill.md",
            "---\ndescription: a\n---\nA\n",
        );
        write(
            tmp.path(),
            "skills/b/SKILL.md",
            "---\ndescription: b\n---\nB\n",
        );
        write(
            tmp.path(),
            "agents/helper/agent.md",
            "---\ndescription: h\n---\nH\n",
        );
        write(
            tmp.path(),
            "agents/helper/config.json",
            r#"{"model":"fast","tools":["read"]}"#,
        );
        write(
            tmp.path(),
            "tasks/digest/task.md",
            "---\nintervalMinutes: 60\n---\nPrompt.\n",
        );
        write(tmp.path(), "memories/note.md", "Remember.\n");

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert_eq!(
            content.skills.len(),
            2,
            "diagnostics: {:?}",
            content.diagnostics
        );
        assert!(content.skills.contains_key("a"));
        assert!(content.skills.contains_key("b"));
        let agent = content.agents.get("helper").unwrap();
        assert_eq!(agent.config.model_preset.as_deref(), Some("fast"));
        assert_eq!(
            agent.config.tools.as_deref(),
            Some(&["read".to_string()][..])
        );
        assert_eq!(content.tasks["digest"].interval_minutes, Some(60));
        assert_eq!(content.memories["note"].body, "Remember.\n");
    }

    #[test]
    fn invalid_sibling_is_isolated() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tasks/good/task.md",
            "---\nintervalMinutes: 5\n---\nGood.\n",
        );
        // An empty prompt body makes this task invalid.
        write(
            tmp.path(),
            "tasks/bad/task.md",
            "---\nintervalMinutes: 5\n---\n\n",
        );

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert!(content.tasks.contains_key("good"));
        assert!(!content.tasks.contains_key("bad"));
        assert_eq!(content.diagnostics.len(), 1);
    }

    #[test]
    fn duplicate_skill_spellings_in_one_entry_is_error_when_distinct() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "skills/review/skill.md",
            "---\ndescription: a\n---\nA\n",
        );
        write(
            tmp.path(),
            "skills/review/SKILL.md",
            "---\ndescription: b\n---\nB\n",
        );

        let exact_names: std::collections::BTreeSet<String> =
            fs::read_dir(tmp.path().join("skills/review"))
                .unwrap()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect();
        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);

        if exact_names.contains("skill.md") && exact_names.contains("SKILL.md") {
            // Case-sensitive filesystem: neither spelling wins by platform order.
            assert!(!content.skills.contains_key("review"));
            assert_eq!(content.diagnostics.len(), 1);
            assert_eq!(
                content.diagnostics[0].code,
                DotagentsDiagnosticCode::DuplicateId
            );
        } else {
            // Case-insensitive filesystem: both writes name one directory entry.
            assert!(content.skills.contains_key("review"));
            assert!(content.diagnostics.is_empty());
        }
    }

    #[test]
    fn duplicate_explicit_skill_ids_within_layer_are_rejected() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "skills/first/skill.md",
            "---\nid: shared\ndescription: first\n---\nFirst\n",
        );
        write(
            tmp.path(),
            "skills/second/skill.md",
            "---\nid: shared\ndescription: second\n---\nSecond\n",
        );

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert_eq!(content.skills.len(), 1);
        assert_eq!(content.skills["shared"].description, "first");
        assert_eq!(content.diagnostics.len(), 1);
        assert_eq!(
            content.diagnostics[0].code,
            DotagentsDiagnosticCode::DuplicateId
        );
    }

    #[test]
    fn unsupported_top_level_entries_are_recorded_as_warnings() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "speakmcp-settings.json", "{}");
        fs::create_dir_all(tmp.path().join("layouts")).unwrap();
        write(tmp.path(), "agents.md", "Instructions.\n");

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert!(content.agents_md.is_some());
        assert_eq!(content.unsupported.len(), 2);
        // Unsupported content must not block supported features.
        assert!(content.diagnostics.iter().all(|d| !d.is_error()));
    }

    #[test]
    fn agents_md_frontmatter_does_not_leak_into_prompt_body() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "agents.md", "---\nname: x\n---\n# Body\n");

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        let body = content.agents_md.unwrap().body;
        assert!(!body.contains("name: x"));
        assert!(body.contains("# Body"));
    }

    #[test]
    fn memories_directory_is_flat_files_only() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "memories/note.md", "Remember.\n");
        // A nested directory must not be treated as a memory.
        fs::create_dir_all(tmp.path().join("memories/nested")).unwrap();

        let content = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert_eq!(content.memories.len(), 1);
        assert!(content.memories.contains_key("note"));
    }

    #[test]
    fn assembly_is_stable_across_repeated_runs() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "skills/a/skill.md",
            "---\ndescription: a\n---\nA\n",
        );
        write(
            tmp.path(),
            "skills/b/skill.md",
            "---\ndescription: b\n---\nB\n",
        );
        write(tmp.path(), "memories/m.md", "M\n");

        let first = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        let second = assemble_layer(DotagentsLayer::Workspace, tmp.path(), &[]);
        assert_eq!(
            first.skills.keys().collect::<Vec<_>>(),
            second.skills.keys().collect::<Vec<_>>()
        );
        assert_eq!(first.memories.len(), second.memories.len());
    }

    #[test]
    fn compose_prompt_orders_explicit_system_then_protocol_singletons() {
        let source = DotagentsSource::singleton(DotagentsLayer::Workspace, "system-prompt.md");
        let mut manifest = DotagentsManifest::empty();
        manifest.system_prompt = Some(super::super::DotagentsPrompt {
            metadata: Default::default(),
            body: "protocol system".to_string(),
            source: source.clone(),
            fingerprint: "system".to_string(),
        });
        manifest.agents_md = Some(super::super::DotagentsPrompt {
            metadata: Default::default(),
            body: "protocol instructions".to_string(),
            source,
            fingerprint: "agents".to_string(),
        });
        let mut params = querymt::LLMParams::new().system("explicit");

        compose_prompt(&mut params, &manifest);

        assert_eq!(
            params.system,
            vec!["explicit", "protocol system", "protocol instructions"]
        );
    }

    #[test]
    fn compose_prompt_ignores_empty_protocol_bodies() {
        let source = DotagentsSource::singleton(DotagentsLayer::Workspace, "agents.md");
        let mut manifest = DotagentsManifest::empty();
        manifest.agents_md = Some(super::super::DotagentsPrompt {
            metadata: Default::default(),
            body: "  \n".to_string(),
            source,
            fingerprint: "empty".to_string(),
        });
        let mut params = querymt::LLMParams::new().system("explicit");

        compose_prompt(&mut params, &manifest);

        assert_eq!(params.system, vec!["explicit"]);
    }

    #[test]
    fn compatibility_mode_keeps_valid_siblings_with_diagnostics() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tasks/good/task.md",
            "---\nintervalMinutes: 5\n---\nGood.\n",
        );
        write(
            tmp.path(),
            "tasks/bad/task.md",
            "---\nintervalMinutes: 5\n---\n\n",
        );
        let options = DotagentsLoadOptions::enabled()
            .with_global_enabled(false)
            .with_workspace_root(tmp.path())
            .with_strictness(DotagentsStrictness::Compatibility);

        let manifest = DotagentsLoader::new(options).load().unwrap();
        assert!(manifest.tasks.contains_key("good"));
        assert!(!manifest.tasks.contains_key("bad"));
        assert!(manifest.has_errors());
    }

    #[test]
    fn strict_mode_returns_manifest_with_all_diagnostics() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tasks/good/task.md",
            "---\nintervalMinutes: 5\n---\nGood.\n",
        );
        write(
            tmp.path(),
            "tasks/bad/task.md",
            "---\nintervalMinutes: 5\n---\n\n",
        );
        let options = DotagentsLoadOptions::enabled()
            .with_global_enabled(false)
            .with_workspace_root(tmp.path())
            .with_strictness(DotagentsStrictness::Strict);

        let error = DotagentsLoader::new(options).load().unwrap_err();
        assert!(error.manifest.tasks.contains_key("good"));
        assert!(error.manifest.has_errors());
        assert_eq!(error.manifest.errors().count(), 1);
    }

    #[test]
    fn invalid_workspace_singleton_blocks_only_that_activation() {
        let global = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        write(global.path(), "agents.md", "Global instructions.\n");
        write(global.path(), "system-prompt.md", "Global system.\n");
        write(
            workspace.path(),
            "agents.md",
            "---\nbroken line\n---\nBody\n",
        );
        write(workspace.path(), "system-prompt.md", "Workspace system.\n");
        let options = DotagentsLoadOptions::enabled()
            .with_global_root(global.path())
            .with_workspace_root(workspace.path());

        let manifest = DotagentsLoader::new(options).load().unwrap();
        assert!(manifest.agents_md.is_none());
        assert_eq!(
            manifest.system_prompt.as_ref().unwrap().body,
            "Workspace system.\n"
        );
        assert_eq!(manifest.errors().count(), 1);
    }

    #[test]
    fn invalid_workspace_json_blocks_only_its_section() {
        let global = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        write(
            global.path(),
            "mcp.json",
            r#"{"mcpServers":{"global":{"command":"g"}}}"#,
        );
        write(
            global.path(),
            "models.json",
            r#"{"models":{"global":{"provider":"p","model":"m"}}}"#,
        );
        write(workspace.path(), "mcp.json", "{broken");
        write(
            workspace.path(),
            "models.json",
            r#"{"models":{"workspace":{"provider":"p","model":"w"}}}"#,
        );
        let options = DotagentsLoadOptions::enabled()
            .with_global_root(global.path())
            .with_workspace_root(workspace.path());

        let manifest = DotagentsLoader::new(options).load().unwrap();
        assert!(manifest.mcp_servers.is_empty());
        assert!(manifest.model_presets.contains_key("global"));
        assert!(manifest.model_presets.contains_key("workspace"));
        assert_eq!(manifest.errors().count(), 1);
    }
}
