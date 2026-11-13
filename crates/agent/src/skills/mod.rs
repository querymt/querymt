//! Agent Skills implementation - agentskills.io specification
//!
//! This module implements lazy-loading skills discovery and execution.
//! Skills are discovered from multiple sources but loaded on-demand via
//! the `skill` tool, keeping initial context small while allowing agents
//! to gain domain-specific knowledge when needed.

pub mod catalog;
pub mod discovery;
#[cfg(test)]
mod e2e_reload_tests;
pub mod mcp;
pub mod parser;
pub mod permissions;
pub mod registry;
pub mod remote;
pub mod tool;
pub mod types;

pub use catalog::{ApprovalMode, SkillCatalog};
pub use discovery::{default_search_paths, discover_all, discover_from_source};
pub use parser::{PROTOCOL_SKILL_FILENAME, SKILL_FILENAME, parse_skill_file, parse_skill_file_ex};
pub use permissions::{PermissionLevel, SkillPermissions};
pub use registry::SkillRegistry;
pub use tool::SkillTool;
pub use types::{Skill, SkillMetadata, SkillSource, ToolAccessPolicy};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SkillsConfig;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Task 5.4: schema collection through `SkillCatalog::from_config` +
    /// `ToolRegistry::definitions()` refreshes the skill tool, so standalone
    /// construction tracks the filesystem without a restart.
    ///
    /// Quorum delegate/planner construction shares this exact builder
    /// (agent/api/quorum.rs), so this coverage applies there unchanged; no
    /// separate quorum test is needed.
    #[test]
    fn test_definitions_collection_refreshes_built_skill_tool() {
        let project = TempDir::new().unwrap();
        let skills_dir = project.path().join("custom-skills");
        let skill_dir = skills_dir.join("integration-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: integration-skill\ndescription: Before edit\n---\nContent\n",
        )
        .unwrap();

        // Only configured sources are used so the test does not depend on
        // developer-global skill directories.
        let config = SkillsConfig {
            paths: vec![skills_dir.clone()],
            include_external: false,
            ..Default::default()
        };

        let catalog = SkillCatalog::from_config(&config, project.path());
        let rebind = || {
            let mut registry = crate::tools::ToolRegistry::new();
            registry.add(Arc::new(catalog.bind(
                Some(project.path()),
                crate::agent::core::McpToolState::empty(),
                Default::default(),
            )));
            registry
        };
        let registry = rebind();

        // Initial schema advertises the discovered skill.
        let skill_def = |defs: &[querymt::chat::Tool]| {
            defs.iter()
                .find(|tool| tool.function.name == SkillTool::NAME)
                .unwrap()
                .clone()
        };

        let defs = registry.definitions();
        let def = skill_def(&defs);
        assert!(def.function.description.contains("integration-skill"));

        // Edit an existing skill: the next collected schema reflects it.
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: integration-skill\ndescription: After edit\n---\nContent\n",
        )
        .unwrap();
        let defs = registry.definitions();
        let def = skill_def(&defs);
        assert!(def.function.description.contains("After edit"));

        // Add a skill: it is advertised without reconstructing the tool.
        let added_dir = skills_dir.join("added-skill");
        fs::create_dir_all(&added_dir).unwrap();
        fs::write(
            added_dir.join("SKILL.md"),
            "---\nname: added-skill\ndescription: Added later\n---\nContent\n",
        )
        .unwrap();
        let defs = registry.definitions();
        let def = skill_def(&defs);
        assert!(def.function.description.contains("added-skill"));
        assert_eq!(
            tool::listed_skill_ids(&def),
            vec!["added-skill", "integration-skill"]
        );

        // Remove a skill: it disappears from the next collected schema.
        fs::remove_dir_all(&added_dir).unwrap();
        let defs = registry.definitions();
        let def = skill_def(&defs);
        assert_eq!(tool::listed_skill_ids(&def), vec!["integration-skill"]);
    }
}
