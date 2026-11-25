use crate::skills::types::Skill;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Registry for managing loaded skills
///
/// Skills are keyed by their callable ID ([`SkillMetadata::effective_id()`]):
/// the explicit protocol `id`, or the existing name-based fallback when `id`
/// is absent. A protocol skill's human-readable `name` stays descriptive
/// metadata and never becomes a second, callable alias.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    /// Skills indexed by callable effective ID
    by_name: HashMap<String, Arc<Skill>>,
    /// Skills indexed by path (for deduplication)
    by_path: HashMap<PathBuf, String>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a skill
    ///
    /// The skill is keyed by its callable effective ID. When the explicit
    /// protocol `id` differs from the display `name`, only the effective ID is
    /// callable; the name remains descriptive metadata.
    pub fn register(&mut self, skill: Skill) {
        let id = skill.metadata.effective_id().to_string();
        let path = skill.path.clone();
        let skill = Arc::new(skill);
        self.by_name.insert(id.clone(), skill);
        self.by_path.insert(path, id);
    }

    /// Atomically replace the registry contents with `skills`.
    ///
    /// Both indexes are rebuilt off to the side and swapped in together, so
    /// readers protected by the registry lock always observe one coherent
    /// state. Skills are keyed by [`SkillMetadata::effective_id()`].
    pub(crate) fn reload_with(&mut self, skills: Vec<Skill>) -> usize {
        let mut by_name = HashMap::new();
        let mut by_path = HashMap::new();
        for skill in skills {
            let id = skill.metadata.effective_id().to_string();
            let path = skill.path.clone();
            let skill = Arc::new(skill);
            by_name.insert(id.clone(), skill);
            by_path.insert(path, id);
        }
        let count = by_name.len();
        self.by_name = by_name;
        self.by_path = by_path;
        count
    }

    /// Get skill by callable effective ID
    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        self.by_name.get(name).cloned()
    }

    /// Get all skills
    pub fn all(&self) -> impl Iterator<Item = &Arc<Skill>> {
        self.by_name.values()
    }

    /// List all callable skill IDs, sorted
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_name.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Format skill list for tool description
    ///
    /// Each entry leads with the callable effective ID. When the
    /// human-readable display name differs, it is included descriptively so
    /// the schema remains understandable without becoming an alias.
    pub fn list_for_description(&self) -> String {
        let mut skills: Vec<_> = self.all().collect();
        skills.sort_by(|left, right| {
            left.metadata
                .effective_id()
                .cmp(right.metadata.effective_id())
        });

        if skills.is_empty() {
            return "No skills available".to_string();
        }

        skills
            .iter()
            .map(|skill| {
                let id = skill.metadata.effective_id();
                let display_name = if skill.metadata.name != id {
                    format!(" (display name: {})", skill.metadata.name)
                } else {
                    String::new()
                };
                let tags = skill
                    .metadata
                    .tags
                    .as_ref()
                    .map(|tags| format!(" [{}]", tags.join(", ")))
                    .unwrap_or_default();
                format!(
                    "- {}: {}{}{}",
                    id, skill.metadata.description, display_name, tags
                )
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

    fn create_test_skill(name: &str, description: &str, compatibility: Option<&str>) -> Skill {
        Skill {
            path: PathBuf::from(format!("/tmp/{}", name)),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: description.to_string(),
                id: None,
                enabled: None,
                version: None,
                license: None,
                compatibility: compatibility.map(str::to_string),
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
    fn test_compatibility_requirement_does_not_filter_skills() {
        let mut registry = SkillRegistry::new();
        registry.register(create_test_skill(
            "openspec-propose",
            "Propose an OpenSpec change",
            Some("Requires openspec CLI."),
        ));

        let description = registry.list_for_description();
        assert!(description.contains("openspec-propose"));
    }

    #[test]
    fn test_list_for_description() {
        let mut registry = SkillRegistry::new();
        registry.register(create_test_skill("skill-a", "First skill", None));
        registry.register(create_test_skill("skill-b", "Second skill", None));

        let desc = registry.list_for_description();
        assert!(desc.contains("skill-a: First skill"));
        assert!(desc.contains("skill-b: Second skill"));
    }

    #[test]
    fn test_empty_registry_description() {
        let registry = SkillRegistry::new();
        let desc = registry.list_for_description();
        assert_eq!(desc, "No skills available");
    }

    /// Replace the registry with a fresh discovery of `sources`.
    fn reload(registry: &mut SkillRegistry, sources: &[SkillSource]) -> usize {
        let report = crate::skills::discovery::discover(sources, &Default::default());
        registry.reload_with(report.skills)
    }

    /// Write a skill definition named `name` with the given description.
    fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nContent\n"),
        )
        .unwrap();
    }

    #[test]
    fn reload_sees_edits() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "edited-skill", "Before edit");

        let mut registry = SkillRegistry::new();
        let sources = vec![SkillSource::Global(dir.path().to_path_buf())];
        reload(&mut registry, &sources);
        assert_eq!(
            registry.get("edited-skill").unwrap().metadata.description,
            "Before edit"
        );

        // Edit the definition file and confirm the reload picks it up.
        write_skill(dir.path(), "edited-skill", "After edit");
        reload(&mut registry, &sources);
        assert_eq!(
            registry.get("edited-skill").unwrap().metadata.description,
            "After edit"
        );
    }

    #[test]
    fn test_register_keys_by_effective_id() {
        let mut registry = SkillRegistry::new();
        let mut skill = create_test_skill("Fancy Display Name", "A protocol skill", None);
        skill.metadata.id = Some("stable-id".to_string());
        registry.register(skill);

        // Callable by the explicit stable ID...
        let retrieved = registry.get("stable-id").expect("callable by effective id");
        assert_eq!(retrieved.metadata.name, "Fancy Display Name");
        assert_eq!(registry.names(), vec!["stable-id"]);

        // ...but the differing display name is not an invocation alias.
        assert!(registry.get("Fancy Display Name").is_none());
    }

    #[test]
    fn test_register_replaces_duplicate_effective_id() {
        let mut registry = SkillRegistry::new();

        let mut first = create_test_skill("display", "First version", None);
        first.metadata.id = Some("dup".to_string());
        registry.register(first);

        let mut second = create_test_skill("display", "Second version", None);
        second.metadata.id = Some("dup".to_string());
        registry.register(second);

        assert_eq!(registry.names(), vec!["dup"]);
        assert_eq!(
            registry.get("dup").unwrap().metadata.description,
            "Second version"
        );
        assert_eq!(registry.by_name.len(), 1);
    }

    #[test]
    fn reload_resolves_precedence_by_effective_id() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();

        // Same stable ID in both sources; the project source must win even
        // though the display names differ.
        let global_skill_dir = global_dir.path().join("review");
        fs::create_dir_all(&global_skill_dir).unwrap();
        fs::write(
            global_skill_dir.join("SKILL.md"),
            "---\nname: Global Review\nid: review\ndescription: Global version.\n---\nBody\n",
        )
        .unwrap();
        let project_skill_dir = project_dir.path().join("review");
        fs::create_dir_all(&project_skill_dir).unwrap();
        fs::write(
            project_skill_dir.join("SKILL.md"),
            "---\nname: Workspace Review\nid: review\ndescription: Workspace version.\n---\nBody\n",
        )
        .unwrap();

        let sources = vec![
            SkillSource::Global(global_dir.path().to_path_buf()),
            SkillSource::Project(project_dir.path().to_path_buf()),
        ];

        let mut registry = SkillRegistry::new();
        let count = reload(&mut registry, &sources);

        assert_eq!(count, 1);
        let skill = registry.get("review").unwrap();
        assert_eq!(skill.metadata.description, "Workspace version.");
        assert_eq!(skill.metadata.name, "Workspace Review");
        assert_eq!(registry.names(), vec!["review"]);
        assert_eq!(registry.by_path.len(), 1);
    }

    #[test]
    fn test_list_for_description_leads_with_effective_id() {
        let mut registry = SkillRegistry::new();
        let mut skill = create_test_skill("Code Review", "Reviews code changes.", None);
        skill.metadata.id = Some("review".to_string());
        registry.register(skill);

        let desc = registry.list_for_description();
        assert!(desc.contains("- review: Reviews code changes."));
        assert!(desc.contains("display name: Code Review"));
    }
}
