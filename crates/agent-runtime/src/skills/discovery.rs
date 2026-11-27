use crate::skills::parser::{PROTOCOL_SKILL_FILENAME, SKILL_FILENAME, parse_skill_file_ex};
use crate::skills::types::{Skill, SkillProblem, SkillProblemKind, SkillSource};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Default number of directory levels scanned below a project
/// `.agents/skills` folder.
pub const DEFAULT_NESTED_DEPTH: u8 = 4;
/// Smallest accepted `nested_depth` (direct children only).
pub const MIN_NESTED_DEPTH: u8 = 1;
/// Largest accepted `nested_depth`.
pub const MAX_NESTED_DEPTH: u8 = 16;

/// Settings of one skill scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillScanOptions {
    /// When false only explicitly configured sources are scanned.
    pub include_external: bool,
    /// Directory levels below a project `.agents/skills` folder that may hold
    /// a skill directory. Every other source accepts direct children only.
    pub nested_depth: u8,
}

impl Default for SkillScanOptions {
    fn default() -> Self {
        Self {
            include_external: true,
            nested_depth: DEFAULT_NESTED_DEPTH,
        }
    }
}

/// Result of scanning one source or a list of sources.
#[derive(Debug, Clone, Default)]
pub struct SkillScanReport {
    /// Merged, precedence-resolved, enabled skills.
    pub skills: Vec<Skill>,
    /// Everything that was found and not offered, ordered by path.
    pub problems: Vec<SkillProblem>,
    /// Whether any selected source folder existed but could not be read.
    pub unreadable_sources: bool,
    /// Number of directory entries the walk visited.
    pub visited_entries: usize,
}

/// Default discovery paths for cross-tool compatibility
pub fn default_search_paths(project_root: &Path) -> Vec<SkillSource> {
    let mut paths = vec![];

    // Global paths (lowest priority)
    if let Ok(cfg_dir) = querymt_utils::providers::config_dir() {
        paths.push(SkillSource::Global(cfg_dir.join("skills")));
    }
    if let Some(home) = dirs::home_dir() {
        paths.push(SkillSource::Global(home.join(".claude/skills")));
        paths.push(SkillSource::Global(home.join(".agents/skills")));
    }

    // Project paths (higher priority, overrides global)
    paths.push(SkillSource::Project(project_root.join(".skills")));
    paths.push(SkillSource::Project(project_root.join(".claude/skills")));
    paths.push(SkillSource::Project(project_root.join(".agents/skills")));
    paths.push(SkillSource::Project(project_root.join(".qmt/skills")));

    paths
}

/// The folder a source reads from.
pub fn source_base_path(source: &SkillSource) -> &Path {
    match source {
        SkillSource::Global(p) | SkillSource::Project(p) | SkillSource::Configured(p) => p,
        SkillSource::Remote { cached_at, .. } => cached_at,
    }
}

fn problem(
    path: &Path,
    source: &Path,
    kind: SkillProblemKind,
    message: impl Into<String>,
) -> SkillProblem {
    SkillProblem {
        path: path.to_path_buf(),
        source: source.to_path_buf(),
        kind,
        message: message.into(),
    }
}

/// Discover skills from a single source.
///
/// Most sources follow the shallow convention
/// `base_path/skill-name/SKILL.md` (a maximum depth of 2). The project-level
/// `.agents/skills` directory is walked down to `nested_depth` levels so
/// nested layouts such as `base_path/<category>/<skill-name>/SKILL.md` are
/// also discovered.
///
/// Symbolic links are followed wherever they point, and version-control
/// ignore files do not apply. Entries that cannot be read, broken links,
/// link loops and invalid definitions are reported as problems and do not
/// hide the other skills of the source. Only a source folder that exists but
/// cannot be read sets `unreadable_sources`.
///
/// Inside `.agents/skills` sources, the `.agents` Protocol lowercase
/// `skill.md` spelling is also accepted, protocol `id`/`enabled` metadata is
/// honored, and an entry defining both spellings is rejected instead of
/// choosing a winner by platform-specific file ordering.
pub fn discover_from_source(source: &SkillSource, nested_depth: u8) -> SkillScanReport {
    let base_path = source_base_path(source);
    let mut report = SkillScanReport::default();

    match base_path.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            // `try_exists` follows links, so a link to a removed folder looks
            // absent. The folder is gone, so no skills are offered from it, but
            // the dangling link is worth a message.
            if is_broken_link(base_path) {
                report.problems.push(problem(
                    base_path,
                    base_path,
                    SkillProblemKind::BrokenLink,
                    "link target does not exist",
                ));
            }
            return report;
        }
        Err(error) => {
            report.unreadable_sources = true;
            report.problems.push(problem(
                base_path,
                base_path,
                SkillProblemKind::SourceUnreadable,
                format!("cannot read skill source: {error}"),
            ));
            return report;
        }
    }

    let protocol = is_agents_skills_source(source);

    // Standard filters (hidden files, ignore files) are off and links are
    // followed. Only project `.agents/skills` honors `nested_depth`; every
    // other source stays shallow: base_path/skill-name/SKILL.md.
    let max_depth = if is_recursive_project_agents_skills(source) {
        usize::from(nested_depth.clamp(MIN_NESTED_DEPTH, MAX_NESTED_DEPTH)) + 1
    } else {
        2
    };
    let walker = ignore::WalkBuilder::new(base_path)
        .standard_filters(false)
        .hidden(false)
        .follow_links(true)
        .max_depth(Some(max_depth))
        .build();

    // Group matched definition files by their entry directory so both
    // spellings can be compared deterministically after the walk.
    let mut candidates: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_walk_error(&error, base_path, &mut report);
                continue;
            }
        };
        report.visited_entries += 1;
        let file_name = entry.file_name().to_string_lossy();
        let matched =
            file_name == SKILL_FILENAME || (protocol && file_name == PROTOCOL_SKILL_FILENAME);
        // A definition directly in a global or project source folder is not
        // a skill directory: skills live in directories below it. A
        // configured path may name a single skill folder, so its own
        // definition counts.
        if matched
            && let Some(parent) = entry.path().parent()
            && (parent != base_path || matches!(source, SkillSource::Configured(_)))
        {
            candidates
                .entry(parent.to_path_buf())
                .or_default()
                .push(entry.path().to_path_buf());
        }
    }

    for (entry_dir, mut definition_files) in candidates {
        if definition_files.len() > 1 {
            definition_files.sort();
            report.problems.push(problem(
                &entry_dir,
                base_path,
                SkillProblemKind::DuplicateDefinition,
                format!(
                    "defines both `{PROTOCOL_SKILL_FILENAME}` and `{SKILL_FILENAME}`; keep exactly one definition"
                ),
            ));
            continue;
        }
        let path = &definition_files[0];
        match parse_skill_file_ex(path, source.clone(), protocol) {
            Ok(skill) => {
                if protocol && !skill.metadata.is_enabled() {
                    log::debug!(
                        "Skipping disabled skill '{}' at {}",
                        skill.metadata.effective_id(),
                        path.display()
                    );
                    continue;
                }
                log::debug!("Discovered skill '{}' at {:?}", skill.metadata.name, path);
                report.skills.push(skill);
            }
            Err(error) => {
                report.problems.push(problem(
                    path,
                    base_path,
                    SkillProblemKind::InvalidDefinition,
                    format!("{error:#}"),
                ));
            }
        }
    }

    report.problems.sort_by(|a, b| a.path.cmp(&b.path));
    report
}

/// Turn one walker error into a problem.
fn record_walk_error(error: &ignore::Error, base_path: &Path, report: &mut SkillScanReport) {
    if let ignore::Error::Partial(errors) = error {
        for error in errors {
            record_walk_error(error, base_path, report);
        }
        return;
    }

    let path = walk_error_path(error).unwrap_or(base_path);
    let (kind, message) = if let Some((ancestor, child)) = walk_error_loop(error) {
        (
            SkillProblemKind::LinkLoop,
            format!(
                "link {} points back to its ancestor {}",
                child.display(),
                ancestor.display()
            ),
        )
    } else if error.depth() == Some(0) {
        (
            SkillProblemKind::SourceUnreadable,
            format!("cannot read skill source: {}", io_message(error)),
        )
    } else if is_broken_link(path) {
        (
            SkillProblemKind::BrokenLink,
            "link target does not exist".to_string(),
        )
    } else {
        (
            SkillProblemKind::EntryUnreadable,
            format!("cannot read entry: {}", io_message(error)),
        )
    };
    if kind == SkillProblemKind::SourceUnreadable {
        report.unreadable_sources = true;
    }
    report
        .problems
        .push(problem(path, base_path, kind, message));
}

fn io_message(error: &ignore::Error) -> String {
    error
        .io_error()
        .map(ToString::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn walk_error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child),
        _ => None,
    }
}

fn walk_error_loop(error: &ignore::Error) -> Option<(&Path, &Path)> {
    match error {
        ignore::Error::Loop { ancestor, child } => Some((ancestor, child)),
        ignore::Error::WithPath { err, .. }
        | ignore::Error::WithDepth { err, .. }
        | ignore::Error::WithLineNumber { err, .. } => walk_error_loop(err),
        _ => None,
    }
}

/// A symbolic link whose target does not exist.
fn is_broken_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        && std::fs::metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// Whether this source is an `.agents/skills` directory, which accepts
/// protocol `skill.md`/`SKILL.md` spellings and protocol metadata.
fn is_agents_skills_source(source: &SkillSource) -> bool {
    let path = match source {
        SkillSource::Global(path) | SkillSource::Project(path) | SkillSource::Configured(path) => {
            path
        }
        SkillSource::Remote { .. } => return false,
    };
    let file_name = path.file_name().and_then(|name| name.to_str());
    let parent_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    file_name == Some("skills") && parent_name == Some(".agents")
}

/// Whether this source is a project-level `.agents/skills` directory, which
/// supports nested skill discovery down to `nested_depth`.
fn is_recursive_project_agents_skills(source: &SkillSource) -> bool {
    matches!(source, SkillSource::Project(_)) && is_agents_skills_source(source)
}

/// Filter sources by the `include_external` policy: when external sources
/// are disabled, only explicitly configured paths are searched.
fn selected_sources(sources: &[SkillSource], include_external: bool) -> Vec<SkillSource> {
    if include_external {
        sources.to_vec()
    } else {
        sources
            .iter()
            .filter(|s| matches!(s, SkillSource::Configured(_)))
            .cloned()
            .collect()
    }
}

/// Merge discovered skills into `all_skills`, deduplicating by stable ID.
///
/// Protocol skills override by stable ID; skills without an explicit ID keep
/// the legacy name-based key. Higher-priority sources override lower ones.
/// Only a skill that collides with one of equal precedence is recorded as a
/// `duplicate_id` problem. A higher-precedence skill overrides a lower one
/// and a lower-precedence skill yields to a higher one, in either order of
/// arrival, and neither is a problem.
fn merge_discovered(
    all_skills: &mut Vec<Skill>,
    seen_names: &mut HashMap<String, (u8, PathBuf)>,
    source: &SkillSource,
    skills: Vec<Skill>,
    problems: &mut Vec<SkillProblem>,
) {
    for skill in skills {
        let name = skill.metadata.effective_id().to_string();

        // Check for duplicates
        if let Some((existing_priority, existing_path)) = seen_names.get(&name) {
            let new_priority = source.priority();
            if new_priority > *existing_priority {
                log::info!(
                    "Skill '{}' from {:?} overrides version from {:?}",
                    name,
                    skill.path,
                    existing_path
                );
                seen_names.insert(name.clone(), (new_priority, skill.path.clone()));
                all_skills.retain(|s: &Skill| s.metadata.effective_id() != name);
                all_skills.push(skill);
            } else if new_priority < *existing_priority {
                log::info!(
                    "Skill '{}' from {:?} is overridden by {:?}",
                    name,
                    skill.path,
                    existing_path
                );
            } else {
                problems.push(problem(
                    &skill.path,
                    source_base_path(source),
                    SkillProblemKind::DuplicateId,
                    format!(
                        "callable ID `{name}` is already provided by {}",
                        existing_path.display()
                    ),
                ));
            }
        } else {
            seen_names.insert(name.clone(), (source.priority(), skill.path.clone()));
            all_skills.push(skill);
        }
    }
}

/// Discover all skills from multiple sources with deduplication.
///
/// Never fails: everything that went wrong is in the report. A caller that
/// needs a complete snapshot checks `unreadable_sources`.
pub fn discover(sources: &[SkillSource], options: &SkillScanOptions) -> SkillScanReport {
    let mut report = SkillScanReport::default();
    let mut seen_names = HashMap::new();

    for source in selected_sources(sources, options.include_external) {
        let scan = discover_from_source(&source, options.nested_depth);
        report.unreadable_sources |= scan.unreadable_sources;
        report.visited_entries += scan.visited_entries;
        report.problems.extend(scan.problems);
        merge_discovered(
            &mut report.skills,
            &mut seen_names,
            &source,
            scan.skills,
            &mut report.problems,
        );
    }

    report.problems.sort_by(|a, b| a.path.cmp(&b.path));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn discover_one(source: &SkillSource) -> SkillScanReport {
        discover_from_source(source, DEFAULT_NESTED_DEPTH)
    }

    fn discover_skills(sources: &[SkillSource], include_external: bool) -> Vec<Skill> {
        discover(
            sources,
            &SkillScanOptions {
                include_external,
                ..Default::default()
            },
        )
        .skills
    }

    fn create_skill(dir: &Path, name: &str, version: &str) {
        let skill_dir = dir.join(name);
        // create_dir_all so nested skills (e.g. `.agents/skills/<cat>/<skill>`) work
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {}
description: Version {}
---
Content
"#,
                name, version
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_discover_from_empty_source() {
        let dir = TempDir::new().unwrap();
        let source = SkillSource::Global(dir.path().to_path_buf());
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn test_discover_single_skill() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "test-skill", "1.0");

        let source = SkillSource::Global(dir.path().to_path_buf());
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "test-skill");
    }

    #[test]
    fn test_discover_multiple_skills() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "skill-one", "1.0");
        create_skill(dir.path(), "skill-two", "1.0");
        create_skill(dir.path(), "skill-three", "1.0");

        let source = SkillSource::Global(dir.path().to_path_buf());
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn test_project_agents_skills_discovered_recursively() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");

        create_skill(&skills_root, "agents-recursive-shallow", "1.0");
        // Mirrors `.agents/skills/<category>/<skill-name>/SKILL.md`
        let nested = skills_root.join("openspec").join("openspec-explore");
        create_skill(&nested, "agents-recursive-nested", "1.0");

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        let mut names: Vec<_> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["agents-recursive-nested", "agents-recursive-shallow"]
        );
    }

    #[test]
    fn test_non_agents_project_source_stays_shallow() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".qmt").join("skills");

        create_skill(&skills_root, "shallow-skill", "1.0");
        // Beyond the default max depth of 2 — should not be discovered
        let nested = skills_root.join("category");
        create_skill(&nested, "nested-skill", "1.0");

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "shallow-skill");
    }

    #[test]
    fn test_global_agents_source_stays_shallow() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");

        create_skill(&skills_root, "shallow-skill", "1.0");
        // Recursion is project-only, even for `.agents/skills`
        let nested = skills_root.join("category");
        create_skill(&nested, "nested-skill", "1.0");

        let source = SkillSource::Global(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "shallow-skill");
    }

    #[test]
    fn test_default_search_paths_include_nested_project_agents_skills() {
        let project = TempDir::new().unwrap();
        let skills_root = project.path().join(".agents").join("skills");

        create_skill(&skills_root, "agents-recursive-shallow", "1.0");
        let nested = skills_root.join("openspec").join("openspec-propose");
        create_skill(&nested, "agents-recursive-nested", "1.0");

        let sources = default_search_paths(project.path());
        let skills = discover_skills(&sources, true);
        let names: Vec<_> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
        assert!(names.contains(&"agents-recursive-shallow"));
        assert!(names.contains(&"agents-recursive-nested"));
    }

    #[test]
    fn test_project_overrides_global() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();

        // Create same-named skill in both locations
        create_skill(global_dir.path(), "test-skill", "1.0");
        create_skill(project_dir.path(), "test-skill", "2.0");

        let sources = vec![
            SkillSource::Global(global_dir.path().to_path_buf()),
            SkillSource::Project(project_dir.path().to_path_buf()),
        ];

        let skills = discover_skills(&sources, true);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.description, "Version 2.0");
    }

    #[test]
    fn test_include_external_false() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();

        create_skill(global_dir.path(), "global-skill", "1.0");
        create_skill(project_dir.path(), "project-skill", "1.0");

        let sources = vec![
            SkillSource::Global(global_dir.path().to_path_buf()),
            SkillSource::Project(project_dir.path().to_path_buf()),
        ];

        // With include_external=false, only configured sources are searched
        // Since we don't have any configured sources, we should get 0 skills
        let skills = discover_skills(&sources, false);
        assert_eq!(skills.len(), 0);

        // With include_external=true, we get both
        let skills = discover_skills(&sources, true);
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn test_discovers_openspec_skill_from_agents_directory() {
        let project = TempDir::new().unwrap();
        let skill_dir = project.path().join(".agents/skills/openspec-propose");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: openspec-propose
description: Propose a new change with all artifacts generated in one step.
allowed-tools: Bash(openspec:*)
compatibility: Requires openspec CLI.
metadata:
  author: openspec
  version: "1.0"
---
OpenSpec instructions.
"#,
        )
        .unwrap();

        // Project sources only: the developer's own global skill folders
        // must not influence the result.
        let sources: Vec<SkillSource> = default_search_paths(project.path())
            .into_iter()
            .filter(|source| matches!(source, SkillSource::Project(_)))
            .collect();
        let skills = discover_skills(&sources, true);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "openspec-propose");
        assert_eq!(
            skills[0].metadata.compatibility.as_deref(),
            Some("Requires openspec CLI.")
        );
    }

    #[test]
    fn test_nonexistent_path() {
        let source = SkillSource::Global(PathBuf::from("/nonexistent/path"));
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 0);
    }

    /// Write a shallow non-protocol skill definition into `dir`.
    fn create_skill_with_description(dir: &Path, name: &str, description: &str) {
        create_skill(dir, name, "1.0");
        let skill_file = dir.join(name).join("SKILL.md");
        fs::write(
            &skill_file,
            format!("---\nname: {name}\ndescription: {description}\n---\nContent\n"),
        )
        .unwrap();
    }

    #[test]
    fn missing_source_is_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        create_skill_with_description(dir.path(), "healthy", "Healthy skill");

        let sources = vec![
            SkillSource::Global(dir.path().to_path_buf()),
            SkillSource::Global(PathBuf::from("/nonexistent/path")),
        ];

        // A missing source counts as empty, not as a failure.
        let report = discover(&sources, &SkillScanOptions::default());
        assert!(!report.unreadable_sources);
        assert!(report.problems.is_empty());
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.effective_id(), "healthy");
    }

    #[test]
    fn invalid_skill_does_not_block_valid_skills() {
        let dir = TempDir::new().unwrap();
        create_skill_with_description(dir.path(), "valid-skill", "Valid skill");

        // Missing required `description` makes this entry malformed.
        let malformed_dir = dir.path().join("malformed-skill");
        fs::create_dir(&malformed_dir).unwrap();
        fs::write(
            malformed_dir.join("SKILL.md"),
            "---\nname: malformed-skill\n---\nBody\n",
        )
        .unwrap();

        let sources = vec![SkillSource::Global(dir.path().to_path_buf())];

        let report = discover(&sources, &SkillScanOptions::default());
        assert!(!report.unreadable_sources);
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.effective_id(), "valid-skill");
    }

    #[test]
    fn malformed_definition_is_recorded_as_invalid() {
        let dir = TempDir::new().unwrap();
        create_skill_with_description(dir.path(), "valid-skill", "Valid skill");
        let malformed_dir = dir.path().join("malformed-skill");
        fs::create_dir(&malformed_dir).unwrap();
        fs::write(
            malformed_dir.join("SKILL.md"),
            "---\nname: malformed-skill\n---\nBody\n",
        )
        .unwrap();

        let report = discover(
            &[SkillSource::Global(dir.path().to_path_buf())],
            &SkillScanOptions::default(),
        );
        assert_eq!(report.problems.len(), 1);
        let problem = &report.problems[0];
        assert_eq!(problem.kind, SkillProblemKind::InvalidDefinition);
        assert_eq!(problem.path, malformed_dir.join("SKILL.md"));
        assert_eq!(problem.source, dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_source_is_reported_and_other_sources_are_read() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        create_skill_with_description(dir.path(), "healthy", "Healthy skill");

        let unreadable = TempDir::new().unwrap();
        create_skill_with_description(unreadable.path(), "locked", "Locked skill");
        fs::set_permissions(unreadable.path(), fs::Permissions::from_mode(0o000)).unwrap();

        // Self-check: under privileged users (e.g. root in CI) the directory
        // remains readable and the walk would succeed, so skip reliably.
        if fs::read_dir(unreadable.path()).is_ok() {
            fs::set_permissions(unreadable.path(), fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: privileged user can still read the locked directory");
            return;
        }

        let sources = vec![
            SkillSource::Global(dir.path().to_path_buf()),
            SkillSource::Global(unreadable.path().to_path_buf()),
        ];

        let report = discover(&sources, &SkillScanOptions::default());
        assert!(report.unreadable_sources);
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].kind, SkillProblemKind::SourceUnreadable);
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.effective_id(), "healthy");

        fs::set_permissions(unreadable.path(), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Write a protocol-style lowercase `skill.md` with the given frontmatter.
    fn create_protocol_skill(dir: &Path, frontmatter: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("skill.md"),
            format!("---\n{frontmatter}---\nProtocol body\n"),
        )
        .unwrap();
    }

    #[test]
    fn test_protocol_lowercase_skill_md_discovered_with_directory_id() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &skills_root.join("review"),
            "description: Reviews code changes.\n",
        );

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        // Stable ID is the entry directory name when no explicit `id` exists.
        assert_eq!(skills[0].metadata.effective_id(), "review");
        assert_eq!(skills[0].metadata.name, "review");
        assert_eq!(skills[0].metadata.description, "Reviews code changes.");
        assert!(skills[0].content.contains("Protocol body"));
    }

    #[test]
    fn test_protocol_lowercase_skill_with_explicit_id() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &skills_root.join("review"),
            "id: custom-review\ndescription: Reviews code changes.\n",
        );

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.effective_id(), "custom-review");
    }

    #[test]
    fn test_protocol_uppercase_skill_with_protocol_metadata() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        let skill_dir = skills_root.join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: Code Review
id: review
enabled: true
description: Reviews code changes.
---
Body
"#,
        )
        .unwrap();

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "Code Review");
        assert_eq!(skills[0].metadata.effective_id(), "review");
        assert!(skills[0].metadata.is_enabled());
    }

    #[test]
    fn both_definition_spellings_in_one_folder_are_recorded() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        let skill_dir = skills_root.join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Upper\ndescription: Uppercase definition.\n---\nUpper body\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("skill.md"),
            "---\nname: Lower\ndescription: Lowercase definition.\n---\nLower body\n",
        )
        .unwrap();

        // On case-insensitive filesystems the second write replaces the first
        // file, so only branch on distinct entries where both spellings exist.
        let exact_names: std::collections::BTreeSet<String> = fs::read_dir(&skill_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        let source = SkillSource::Project(skills_root.clone());
        let report = discover_one(&source);
        if exact_names.contains("skill.md") && exact_names.contains("SKILL.md") {
            // Duplicate definition: neither spelling wins, and the entry
            // directory is recorded.
            assert_eq!(report.skills.len(), 0);
            assert_eq!(report.problems.len(), 1);
            assert_eq!(
                report.problems[0].kind,
                SkillProblemKind::DuplicateDefinition
            );
            assert_eq!(report.problems[0].path, skill_dir);
            assert_eq!(report.problems[0].source, skills_root);
        } else {
            assert_eq!(report.skills.len(), 1);
            assert!(report.problems.is_empty());
        }
    }

    #[test]
    fn test_protocol_disabled_skill_not_discovered() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &skills_root.join("review"),
            "description: Reviews code changes.\nenabled: false\n",
        );

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        // Disabled skills are not exposed to the skill tool.
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn test_protocol_lowercase_skill_discovered_recursively() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &skills_root.join("openspec").join("openspec-explore"),
            "description: Explore an idea.\n",
        );

        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.effective_id(), "openspec-explore");
    }

    #[test]
    fn test_non_protocol_sources_ignore_lowercase_skill_md() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".qmt").join("skills");
        create_protocol_skill(
            &skills_root.join("review"),
            "description: Reviews code changes.\n",
        );

        // Lowercase `skill.md` is a protocol spelling; other sources keep
        // requiring uppercase `SKILL.md`.
        let source = SkillSource::Project(skills_root);
        let skills = discover_one(&source).skills;
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn test_protocol_workspace_skill_overrides_global_by_id() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();

        let global_skills = global_dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &global_skills.join("review"),
            "id: review\nname: Global Review\ndescription: Global version.\n",
        );
        let project_skills = project_dir.path().join(".agents").join("skills");
        create_protocol_skill(
            &project_skills.join("review"),
            "id: review\nname: Workspace Review\ndescription: Workspace version.\n",
        );

        let sources = vec![
            SkillSource::Global(global_skills),
            SkillSource::Project(project_skills),
        ];
        let skills = discover_skills(&sources, true);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "Workspace Review");
        assert_eq!(skills[0].metadata.effective_id(), "review");
    }

    fn project_source(workspace: &Path) -> SkillSource {
        SkillSource::Project(workspace.join(".agents").join("skills"))
    }

    fn ids(skills: &[Skill]) -> Vec<String> {
        let mut ids: Vec<String> = skills
            .iter()
            .map(|skill| skill.metadata.effective_id().to_string())
            .collect();
        ids.sort();
        ids
    }

    #[cfg(unix)]
    #[test]
    fn skill_folder_linked_from_outside_the_workspace_is_offered() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        create_skill(outside.path(), "shared-target", "1.0");
        let skills_root = workspace.path().join(".agents").join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("shared-target"),
            skills_root.join("shared"),
        )
        .unwrap();

        let skills = discover_one(&project_source(workspace.path())).skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "shared-target");
        assert_eq!(skills[0].content.trim(), "Content");
        // The skill keeps the path under the source folder.
        assert_eq!(skills[0].path, skills_root.join("shared"));
    }

    #[cfg(unix)]
    #[test]
    fn skill_inside_a_linked_category_folder_is_offered() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        create_skill(outside.path(), "review", "1.0");
        let skills_root = workspace.path().join(".agents").join("skills");
        fs::create_dir_all(&skills_root).unwrap();
        std::os::unix::fs::symlink(outside.path(), skills_root.join("team")).unwrap();

        let skills = discover_one(&project_source(workspace.path())).skills;
        assert_eq!(ids(&skills), vec!["review"]);
    }

    #[cfg(unix)]
    #[test]
    fn broken_skill_link_does_not_hide_sibling_skills() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "valid", "1.0");
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("does-not-exist"), &link).unwrap();

        let report = discover_one(&SkillSource::Global(dir.path().to_path_buf()));
        assert_eq!(ids(&report.skills), vec!["valid"]);
        assert!(!report.unreadable_sources);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].kind, SkillProblemKind::BrokenLink);
        assert_eq!(report.problems[0].path, link);
        assert_eq!(report.problems[0].source, dir.path());
    }

    #[cfg(unix)]
    #[test]
    fn link_loop_is_reported_and_discovery_completes() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        create_skill(&skills_root, "valid", "1.0");
        let link = skills_root.join("valid").join("back");
        std::os::unix::fs::symlink(&skills_root, &link).unwrap();

        let report = discover_one(&project_source(workspace.path()));
        assert_eq!(ids(&report.skills), vec!["valid"]);
        assert!(!report.unreadable_sources);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.kind == SkillProblemKind::LinkLoop && p.path == link),
            "{:?}",
            report.problems
        );
    }

    #[test]
    fn gitignored_skill_folder_is_offered() {
        let workspace = TempDir::new().unwrap();
        // The `ignore` crate only honors `.gitignore` inside a git work tree.
        let git_init = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(workspace.path())
            .status();
        if !git_init.is_ok_and(|status| status.success()) {
            fs::create_dir(workspace.path().join(".git")).unwrap();
        }
        fs::write(
            workspace.path().join(".gitignore"),
            ".agents/skills/local-notes\n",
        )
        .unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        create_skill(&skills_root, "local-notes", "1.0");
        create_skill(&skills_root, "tracked", "1.0");

        let skills = discover_one(&project_source(workspace.path())).skills;
        assert_eq!(ids(&skills), vec!["local-notes", "tracked"]);
    }

    fn write_review(dir: &Path) {
        create_skill(dir, "review", "1.0");
    }

    #[test]
    fn skill_four_levels_deep_is_offered_by_default() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        write_review(&skills_root.join("a").join("b").join("c"));

        let skills = discover_one(&project_source(workspace.path())).skills;
        assert_eq!(ids(&skills), vec!["review"]);
    }

    #[test]
    fn skill_five_levels_deep_is_not_offered_by_default() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        write_review(&skills_root.join("a").join("b").join("c").join("d"));

        let report = discover_one(&project_source(workspace.path()));
        assert!(report.skills.is_empty());
        assert!(report.problems.is_empty());
    }

    #[test]
    fn nested_depth_of_one_offers_direct_children_only() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        create_skill(&skills_root, "top", "1.0");
        create_skill(&skills_root.join("group"), "inner", "1.0");

        let source = project_source(workspace.path());
        let skills = discover_from_source(&source, 1).skills;
        assert_eq!(ids(&skills), vec!["top"]);
        // A larger depth does offer the nested one.
        let skills = discover_from_source(&source, 2).skills;
        assert_eq!(ids(&skills), vec!["inner", "top"]);
    }

    #[test]
    fn nested_depth_does_not_apply_to_other_sources() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".claude").join("skills");
        create_skill(&skills_root.join("group"), "inner", "1.0");

        let source = SkillSource::Project(skills_root);
        assert!(discover_from_source(&source, 4).skills.is_empty());
        assert!(discover_from_source(&source, 16).skills.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_source_link_is_reported_as_a_broken_link() {
        let workspace = TempDir::new().unwrap();
        let agents = workspace.path().join(".agents");
        fs::create_dir_all(&agents).unwrap();
        let link = agents.join("skills");
        std::os::unix::fs::symlink(workspace.path().join("removed"), &link).unwrap();

        let report = discover_one(&project_source(workspace.path()));
        assert!(report.skills.is_empty());
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].kind, SkillProblemKind::BrokenLink);
        assert_eq!(report.problems[0].path, link);
        // The folder is gone, so this is not an unreadable source.
        assert!(!report.unreadable_sources);
    }

    #[test]
    fn definition_directly_in_the_source_folder_is_not_a_skill() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        create_skill(&skills_root, "review", "1.0");
        fs::write(
            skills_root.join("SKILL.md"),
            "---\nname: root\ndescription: Root definition\n---\nBody\n",
        )
        .unwrap();

        let source = project_source(workspace.path());
        for depth in [1, DEFAULT_NESTED_DEPTH] {
            let report = discover_from_source(&source, depth);
            assert_eq!(ids(&report.skills), vec!["review"], "depth {depth}");
            assert!(report.problems.is_empty(), "{:?}", report.problems);
        }
        let shallow = SkillSource::Global(skills_root);
        assert_eq!(ids(&discover_one(&shallow).skills), vec!["review"]);
    }

    #[test]
    fn configured_path_may_be_a_skill_folder() {
        let dir = TempDir::new().unwrap();
        let single = dir.path().join("scholarly-sources");
        fs::create_dir_all(&single).unwrap();
        fs::write(
            single.join("SKILL.md"),
            "---\nname: scholarly-sources\ndescription: Root skill\n---\nBody\n",
        )
        .unwrap();

        // The folder is the skill, with only configured sources selected.
        let source = SkillSource::Configured(single.clone());
        let skills = discover_skills(std::slice::from_ref(&source), false);
        assert_eq!(ids(&skills), vec!["scholarly-sources"]);
        assert_eq!(skills[0].path, single);
        assert!(discover_one(&source).problems.is_empty());

        // Skill folders below it are still offered next to it.
        create_skill(&single, "child", "1.0");
        assert_eq!(
            ids(&discover_one(&source).skills),
            vec!["child", "scholarly-sources"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn problems_of_a_single_source_are_ordered_by_path() {
        let dir = TempDir::new().unwrap();
        // The walk meets the dangling link `z` before it parses `a`.
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("z")).unwrap();
        let malformed = dir.path().join("a");
        fs::create_dir(&malformed).unwrap();
        fs::write(malformed.join("SKILL.md"), "---\nname: a\n---\nBody\n").unwrap();

        let report = discover_one(&SkillSource::Global(dir.path().to_path_buf()));
        let paths: Vec<_> = report.problems.iter().map(|p| p.path.clone()).collect();
        assert_eq!(paths.len(), 2, "{:?}", report.problems);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn lower_precedence_skill_met_second_is_not_a_problem() {
        let workspace = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let project_root = workspace.path().join(".qmt").join("skills");
        create_skill_with_description(&project_root, "review", "From project");
        create_skill_with_description(global.path(), "review", "From global");

        // The higher-precedence source is searched first this time.
        let report = discover(
            &[
                SkillSource::Project(project_root),
                SkillSource::Global(global.path().to_path_buf()),
            ],
            &SkillScanOptions::default(),
        );
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.description, "From project");
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_definition_is_a_problem_and_does_not_block_siblings() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "healthy", "1.0");
        let endless = dir.path().join("endless");
        fs::create_dir(&endless).unwrap();
        std::os::unix::fs::symlink("/dev/zero", endless.join("SKILL.md")).unwrap();

        let report = discover_one(&SkillSource::Global(dir.path().to_path_buf()));
        assert_eq!(ids(&report.skills), vec!["healthy"]);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].kind, SkillProblemKind::InvalidDefinition);
        assert!(
            report.problems[0].message.contains("not a regular file"),
            "{}",
            report.problems[0].message
        );
    }

    #[test]
    fn oversized_definition_is_a_problem() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "healthy", "1.0");
        let big = dir.path().join("big");
        fs::create_dir(&big).unwrap();
        let body = "x".repeat(crate::skills::parser::SKILL_DEFINITION_MAX_BYTES as usize);
        fs::write(
            big.join("SKILL.md"),
            format!("---\nname: big\ndescription: Big\n---\n{body}"),
        )
        .unwrap();

        let report = discover_one(&SkillSource::Global(dir.path().to_path_buf()));
        assert_eq!(ids(&report.skills), vec!["healthy"]);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert_eq!(report.problems[0].kind, SkillProblemKind::InvalidDefinition);
        assert!(report.problems[0].message.contains("byte limit"));
    }

    #[test]
    fn equal_precedence_duplicate_id_is_recorded_and_the_first_is_kept() {
        let workspace = TempDir::new().unwrap();
        let claude = workspace.path().join(".claude").join("skills");
        let agents = workspace.path().join(".agents").join("skills");
        create_skill_with_description(&claude, "review", "From claude");
        create_skill_with_description(&agents, "review", "From agents");

        // Searched first: .claude/skills; both are project sources.
        let sources = vec![
            SkillSource::Project(claude),
            SkillSource::Project(agents.clone()),
        ];
        let report = discover(&sources, &SkillScanOptions::default());
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.description, "From claude");
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].kind, SkillProblemKind::DuplicateId);
        assert_eq!(report.problems[0].path, agents.join("review"));
        assert_eq!(report.problems[0].source, agents);
    }

    #[test]
    fn higher_precedence_override_is_not_a_problem() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();
        create_skill_with_description(global_dir.path(), "review", "Global");
        create_skill_with_description(project_dir.path(), "review", "Project");

        let sources = vec![
            SkillSource::Global(global_dir.path().to_path_buf()),
            SkillSource::Project(project_dir.path().to_path_buf()),
        ];
        let report = discover(&sources, &SkillScanOptions::default());
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].metadata.description, "Project");
        assert!(report.problems.is_empty(), "{:?}", report.problems);
    }

    #[test]
    fn disabled_skill_is_not_a_problem() {
        let workspace = TempDir::new().unwrap();
        let skills_root = workspace.path().join(".agents").join("skills");
        create_protocol_skill(
            &skills_root.join("draft"),
            "description: A draft.\nenabled: false\n",
        );

        let report = discover_one(&project_source(workspace.path()));
        assert!(report.skills.is_empty());
        assert!(report.problems.is_empty(), "{:?}", report.problems);
    }
}
