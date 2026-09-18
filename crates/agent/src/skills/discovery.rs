use crate::skills::parser::{PROTOCOL_SKILL_FILENAME, SKILL_FILENAME, parse_skill_file_ex};
use crate::skills::types::{Skill, SkillSource};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Discover skills from a single source.
///
/// Most sources follow the shallow convention
/// `base_path/skill-name/SKILL.md` (a maximum depth of 2). The project-level
/// `.agents/skills` directory is walked recursively so nested layouts such as
/// `base_path/<category>/<skill-name>/SKILL.md` are also discovered.
///
/// Inside `.agents/skills` sources, the `.agents` Protocol lowercase
/// `skill.md` spelling is also accepted, protocol `id`/`enabled` metadata is
/// honored, and an entry defining both spellings is rejected instead of
/// choosing a winner by platform-specific file ordering.
pub fn discover_from_source(source: &SkillSource) -> Result<Vec<Skill>> {
    let base_path = match source {
        SkillSource::Global(p) | SkillSource::Project(p) | SkillSource::Configured(p) => p,
        SkillSource::Remote { cached_at, .. } => cached_at,
    };

    if !base_path.exists() {
        return Ok(vec![]);
    }

    let protocol = is_agents_skills_source(source);

    // Use ignore crate to respect .gitignore. Only project `.agents/skills` is
    // walked without a depth limit; every other source stays shallow.
    let mut walker = ignore::WalkBuilder::new(base_path);
    if !is_recursive_project_agents_skills(source) {
        walker.max_depth(Some(2)); // Only look 2 levels deep: base_path/skill-name/SKILL.md
    }

    // Group matched definition files by their entry directory so both
    // spellings can be compared deterministically after the walk.
    let mut candidates: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for entry in walker.hidden(false).build() {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy();
        let matched =
            file_name == SKILL_FILENAME || (protocol && file_name == PROTOCOL_SKILL_FILENAME);
        if matched && let Some(parent) = entry.path().parent() {
            candidates
                .entry(parent.to_path_buf())
                .or_default()
                .push(entry.path().to_path_buf());
        }
    }

    let mut skills = Vec::new();
    for (entry_dir, mut definition_files) in candidates {
        if definition_files.len() > 1 {
            definition_files.sort();
            log::warn!(
                "Skill entry {} defines both `{}` and `{}`; keep exactly one definition",
                entry_dir.display(),
                PROTOCOL_SKILL_FILENAME,
                SKILL_FILENAME
            );
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
                skills.push(skill);
            }
            Err(e) => {
                log::warn!("Failed to parse skill at {}: {}", path.display(), e);
            }
        }
    }

    Ok(skills)
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
/// supports fully recursive skill discovery.
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
fn merge_discovered(
    all_skills: &mut Vec<Skill>,
    seen_names: &mut std::collections::HashMap<String, (u8, PathBuf)>,
    source: &SkillSource,
    skills: Vec<Skill>,
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
            } else {
                log::warn!(
                    "Duplicate skill '{}' found at {:?}, ignoring (already loaded from {:?})",
                    name,
                    skill.path,
                    existing_path
                );
            }
        } else {
            seen_names.insert(name.clone(), (source.priority(), skill.path.clone()));
            all_skills.push(skill);
        }
    }
}

fn discover_all_with(
    sources: &[SkillSource],
    include_external: bool,
    discover_one: &dyn Fn(&SkillSource) -> Result<Vec<Skill>>,
    strict: bool,
) -> Result<Vec<Skill>> {
    let mut all_skills = Vec::new();
    let mut seen_names = std::collections::HashMap::new();

    for source in selected_sources(sources, include_external) {
        match discover_one(&source) {
            Ok(skills) => merge_discovered(&mut all_skills, &mut seen_names, &source, skills),
            Err(e) => {
                if strict {
                    return Err(anyhow::anyhow!(
                        "failed to discover skills from {:?}: {}",
                        source,
                        e
                    ));
                }
                log::warn!("Failed to discover skills from {:?}: {}", source, e);
            }
        }
    }

    Ok(all_skills)
}

/// Discover all skills from multiple sources with deduplication.
///
/// Best-effort: a selected source that fails traversal is logged and skipped,
/// so the returned set may be partial. Use [`discover_all_strict`] when a
/// complete snapshot is required.
pub fn discover_all(sources: &[SkillSource], include_external: bool) -> Result<Vec<Skill>> {
    discover_all_with(sources, include_external, &discover_from_source, false)
}

/// Strict discovery for transactional refresh: returns an error when any
/// selected source cannot be traversed completely.
///
/// A successful return therefore proves the result is a complete snapshot.
/// Nonexistent source directories still count as empty sources, and malformed
/// individual skill definitions are diagnosed and skipped so a single bad
/// entry does not invalidate otherwise successful discovery.
pub fn discover_all_strict(sources: &[SkillSource], include_external: bool) -> Result<Vec<Skill>> {
    discover_all_with(sources, include_external, &discover_from_source, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

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
        let skills = discover_from_source(&source).unwrap();
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn test_discover_single_skill() {
        let dir = TempDir::new().unwrap();
        create_skill(dir.path(), "test-skill", "1.0");

        let source = SkillSource::Global(dir.path().to_path_buf());
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_all(&sources, true).unwrap();
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

        let skills = discover_all(&sources, true).unwrap();
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
        let skills = discover_all(&sources, false).unwrap();
        assert_eq!(skills.len(), 0);

        // With include_external=true, we get both
        let skills = discover_all(&sources, true).unwrap();
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

        let skills = discover_all(&default_search_paths(project.path()), true).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
    fn test_strict_discovery_missing_source_is_empty() {
        let dir = TempDir::new().unwrap();
        create_skill_with_description(dir.path(), "healthy", "Healthy skill");

        let sources = vec![
            SkillSource::Global(dir.path().to_path_buf()),
            SkillSource::Global(PathBuf::from("/nonexistent/path")),
        ];

        // A missing source counts as empty, not as a failure.
        let skills = discover_all_strict(&sources, true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.effective_id(), "healthy");
    }

    #[test]
    fn test_strict_discovery_skips_malformed_entries() {
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

        let skills = discover_all_strict(&sources, true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.effective_id(), "valid-skill");
    }

    #[test]
    fn test_strict_discovery_fails_when_source_traversal_fails() {
        let injected_error = || anyhow::anyhow!("injected traversal failure");
        let fail_discovery =
            |_source: &SkillSource| -> Result<Vec<Skill>> { Err(injected_error()) };

        let sources = vec![SkillSource::Global(PathBuf::from("/does/not/matter"))];

        // Strict mode propagates the traversal error instead of dropping it.
        let result = discover_all_with(&sources, true, &fail_discovery, true);
        assert!(result.is_err());

        // Best-effort mode keeps suppressing it.
        let result = discover_all_with(&sources, true, &fail_discovery, false);
        assert_eq!(result.unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_strict_discovery_fails_on_unreadable_source() {
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

        // Strict discovery refuses to return a partial snapshot...
        assert!(discover_all_strict(&sources, true).is_err());
        // ...while best-effort discovery still returns the healthy source.
        let best_effort = discover_all(&sources, true).unwrap();
        assert_eq!(best_effort.len(), 1);
        assert_eq!(best_effort[0].metadata.effective_id(), "healthy");

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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "Code Review");
        assert_eq!(skills[0].metadata.effective_id(), "review");
        assert!(skills[0].metadata.is_enabled());
    }

    #[test]
    fn test_protocol_duplicate_case_spellings_are_rejected() {
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

        let source = SkillSource::Project(skills_root);
        let skills = discover_from_source(&source).unwrap();
        if exact_names.contains("skill.md") && exact_names.contains("SKILL.md") {
            // Duplicate definition: neither spelling wins.
            assert_eq!(skills.len(), 0);
        } else {
            assert_eq!(skills.len(), 1);
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_from_source(&source).unwrap();
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
        let skills = discover_all(&sources, true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "Workspace Review");
        assert_eq!(skills[0].metadata.effective_id(), "review");
    }
}
