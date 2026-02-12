use crate::skills::parser::{SKILL_FILENAME, parse_skill_file};
use crate::skills::types::{Skill, SkillSource};
use anyhow::Result;
use std::path::Path;

/// Default discovery paths for cross-tool compatibility
pub fn default_search_paths(project_root: &Path) -> Vec<SkillSource> {
    let mut paths = vec![];

    // Global paths (lowest priority)
    if let Some(home) = dirs::home_dir() {
        paths.push(SkillSource::Global(home.join(".qmt/skills")));
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

/// Discover skills from a single source (non-recursive in each immediate subdirectory)
pub fn discover_from_source(source: &SkillSource) -> Result<Vec<Skill>> {
    let base_path = match source {
        SkillSource::Global(p) | SkillSource::Project(p) | SkillSource::Configured(p) => p,
        SkillSource::Remote { cached_at, .. } => cached_at,
    };

    if !base_path.exists() {
        return Ok(vec![]);
    }

    let mut skills = Vec::new();

    // Use ignore crate to respect .gitignore
    for entry in ignore::WalkBuilder::new(base_path)
        .max_depth(Some(2)) // Only look 2 levels deep: base_path/skill-name/SKILL.md
        .hidden(false)
        .build()
    {
        let entry = entry?;
        if entry.file_name() == SKILL_FILENAME {
            match parse_skill_file(entry.path(), source.clone()) {
                Ok(skill) => {
                    log::debug!(
                        "Discovered skill '{}' at {:?}",
                        skill.metadata.name,
                        entry.path()
                    );
                    skills.push(skill);
                }
                Err(e) => {
                    log::warn!("Failed to parse skill at {}: {}", entry.path().display(), e);
                }
            }
        }
    }

    Ok(skills)
}

/// Discover all skills from multiple sources with deduplication
pub fn discover_all(sources: &[SkillSource], include_external: bool) -> Result<Vec<Skill>> {
    let mut all_skills = Vec::new();
    let mut seen_names = std::collections::HashMap::new();

    let sources_to_search: Vec<_> = if include_external {
        sources.to_vec()
    } else {
        sources
            .iter()
            .filter(|s| matches!(s, SkillSource::Configured(_)))
            .cloned()
            .collect()
    };

    for source in sources_to_search {
        match discover_from_source(&source) {
            Ok(skills) => {
                for skill in skills {
                    let name = skill.metadata.name.clone();

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
                            all_skills.retain(|s: &Skill| s.metadata.name != name);
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
            Err(e) => {
                log::warn!("Failed to discover skills from {:?}: {}", source, e);
            }
        }
    }

    Ok(all_skills)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn create_skill(dir: &Path, name: &str, version: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir(&skill_dir).unwrap();
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
    fn test_nonexistent_path() {
        let source = SkillSource::Global(PathBuf::from("/nonexistent/path"));
        let skills = discover_from_source(&source).unwrap();
        assert_eq!(skills.len(), 0);
    }
}
