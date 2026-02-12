use crate::skills::discovery;
use crate::skills::types::Skill;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Registry for managing loaded skills
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    /// Skills indexed by name
    by_name: HashMap<String, Arc<Skill>>,
    /// Skills indexed by path (for deduplication)
    by_path: HashMap<PathBuf, String>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a skill
    pub fn register(&mut self, skill: Skill) {
        let name = skill.metadata.name.clone();
        let path = skill.path.clone();
        let skill = Arc::new(skill);
        self.by_name.insert(name.clone(), skill);
        self.by_path.insert(path, name);
    }

    /// Load skills from discovery sources
    pub fn load_from_sources(
        &mut self,
        sources: &[crate::skills::types::SkillSource],
        include_external: bool,
    ) -> Result<usize> {
        let skills = discovery::discover_all(sources, include_external)?;
        let count = skills.len();
        for skill in skills {
            self.register(skill);
        }
        Ok(count)
    }

    /// Get skill by name
    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        self.by_name.get(name).cloned()
    }

    /// Get all skills
    pub fn all(&self) -> impl Iterator<Item = &Arc<Skill>> {
        self.by_name.values()
    }

    /// Get skills compatible with an agent identifier
    pub fn compatible_with(&self, agent_id: &str) -> Vec<Arc<Skill>> {
        self.by_name
            .values()
            .filter(|s| {
                match &s.metadata.compatibility {
                    None => true, // No compatibility = works everywhere
                    Some(compat) => compat.iter().any(|c| c == agent_id || c == "*"),
                }
            })
            .cloned()
            .collect()
    }

    /// List all skill names
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_name.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Format skill list for tool description
    pub fn list_for_description(&self, agent_id: Option<&str>) -> String {
        let skills = if let Some(id) = agent_id {
            self.compatible_with(id)
        } else {
            self.all().cloned().collect()
        };

        if skills.is_empty() {
            return "No skills available".to_string();
        }

        skills
            .iter()
            .map(|s| {
                let tags = s
                    .metadata
                    .tags
                    .as_ref()
                    .map(|t| format!(" [{}]", t.join(", ")))
                    .unwrap_or_default();
                format!("- {}: {}{}", s.metadata.name, s.metadata.description, tags)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{SkillMetadata, SkillSource};
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_skill(
        name: &str,
        description: &str,
        compatibility: Option<Vec<String>>,
    ) -> Skill {
        Skill {
            path: PathBuf::from(format!("/tmp/{}", name)),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: description.to_string(),
                version: None,
                license: None,
                compatibility,
                allowed_tools: None,
                tags: None,
                author: None,
                extra: HashMap::new(),
            },
            content: "Test content".to_string(),
            source: SkillSource::Global(PathBuf::from("/tmp")),
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = SkillRegistry::new();
        let skill = create_test_skill("test-skill", "A test skill", None);

        registry.register(skill);

        let retrieved = registry.get("test-skill");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().metadata.name, "test-skill");
    }

    #[test]
    fn test_get_nonexistent() {
        let registry = SkillRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_list_names() {
        let mut registry = SkillRegistry::new();
        registry.register(create_test_skill("skill-a", "First", None));
        registry.register(create_test_skill("skill-b", "Second", None));
        registry.register(create_test_skill("skill-c", "Third", None));

        let names = registry.names();
        assert_eq!(names.len(), 3);
        // Should be sorted
        assert_eq!(names, vec!["skill-a", "skill-b", "skill-c"]);
    }

    #[test]
    fn test_compatibility_filter() {
        let mut registry = SkillRegistry::new();

        registry.register(create_test_skill("universal", "Universal skill", None));
        registry.register(create_test_skill(
            "querymt-only",
            "QueryMT only",
            Some(vec!["querymt".to_string()]),
        ));
        registry.register(create_test_skill(
            "claude-only",
            "Claude only",
            Some(vec!["claude-code".to_string()]),
        ));
        registry.register(create_test_skill(
            "wildcard",
            "Wildcard",
            Some(vec!["*".to_string()]),
        ));

        let querymt_skills = registry.compatible_with("querymt");
        assert_eq!(querymt_skills.len(), 3); // universal, querymt-only, wildcard

        let claude_skills = registry.compatible_with("claude-code");
        assert_eq!(claude_skills.len(), 3); // universal, claude-only, wildcard

        let other_skills = registry.compatible_with("other-agent");
        assert_eq!(other_skills.len(), 2); // universal, wildcard
    }

    #[test]
    fn test_list_for_description() {
        let mut registry = SkillRegistry::new();
        registry.register(create_test_skill("skill-a", "First skill", None));
        registry.register(create_test_skill("skill-b", "Second skill", None));

        let desc = registry.list_for_description(None);
        assert!(desc.contains("skill-a: First skill"));
        assert!(desc.contains("skill-b: Second skill"));
    }

    #[test]
    fn test_empty_registry_description() {
        let registry = SkillRegistry::new();
        let desc = registry.list_for_description(None);
        assert_eq!(desc, "No skills available");
    }

    #[test]
    fn test_load_from_sources() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("test-skill");
        fs::create_dir(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: test-skill
description: A test skill
---
Content
"#,
        )
        .unwrap();

        let mut registry = SkillRegistry::new();
        let sources = vec![SkillSource::Global(dir.path().to_path_buf())];
        let count = registry.load_from_sources(&sources, true).unwrap();

        assert_eq!(count, 1);
        assert!(registry.get("test-skill").is_some());
    }
}
