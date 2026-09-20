use crate::skills::discovery;
use crate::skills::permissions::{PermissionLevel, SkillPermissions};
use crate::skills::registry::SkillRegistry;
use crate::skills::types::{Skill, SkillSource};
use crate::tools::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::ToolResultPart;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Discovery function used by registry refreshes. Swappable in tests to
/// inject deterministic discovery failures.
type DiscoverFn = dyn Fn(&[SkillSource], bool) -> anyhow::Result<Vec<Skill>> + Send + Sync;

/// The skill tool that agents use to load skills on-demand
pub struct SkillTool {
    registry: Arc<Mutex<SkillRegistry>>,
    permissions: Arc<SkillPermissions>,
    /// Discovery sources retained from construction so the registry can be
    /// refreshed on demand without re-deriving configuration.
    sources: Vec<SkillSource>,
    include_external: bool,
    discovery_fn: Arc<DiscoverFn>,
}

impl SkillTool {
    pub const NAME: &'static str = "skill";

    pub fn new(
        registry: Arc<Mutex<SkillRegistry>>,
        permissions: Arc<SkillPermissions>,
        sources: Vec<SkillSource>,
        include_external: bool,
    ) -> Self {
        Self {
            registry,
            permissions,
            sources,
            include_external,
            discovery_fn: Arc::new(discovery::discover_all_strict),
        }
    }

    /// Override the discovery function used by refreshes (test-only).
    #[cfg(test)]
    fn with_discovery_fn(mut self, discovery_fn: Arc<DiscoverFn>) -> Self {
        self.discovery_fn = discovery_fn;
        self
    }

    /// Re-discover skills from the retained sources and replace the registry
    /// contents atomically.
    ///
    /// On failure the previous registry contents are retained and the error is
    /// returned to the caller (who is responsible for logging it).
    fn refresh_registry(&self) -> anyhow::Result<()> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
        match (self.discovery_fn)(&self.sources, self.include_external) {
            Ok(skills) => {
                let count = registry.reload_with(skills);
                log::debug!("Skill registry refreshed: {count} skills available");
                Ok(())
            }
            Err(error) => {
                log::warn!(
                    "Failed to refresh skills: {}. Retaining previously discovered skills.",
                    error
                );
                Err(error)
            }
        }
    }

    /// Async variant of [`Self::refresh_registry`] for the tool-call path:
    /// discovery runs off the Tokio worker via `spawn_blocking`, and the
    /// registry lock is only held to publish the results.
    async fn refresh_registry_async(&self) -> anyhow::Result<()> {
        let sources = self.sources.clone();
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
                    "Failed to refresh skills: {}. Retaining previously discovered skills.",
                    error
                );
                return Err(error);
            }
        };

        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
        let count = registry.reload_with(skills);
        log::debug!("Skill registry refreshed: {count} skills available");
        Ok(())
    }

    /// Look up a skill by callable ID.
    fn lookup_skill(&self, id: &str) -> Result<Option<Arc<Skill>>, ToolError> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("Registry lock poisoned")))?;
        Ok(registry.get(id))
    }

    /// Deterministic not-found error naming the requested callable ID and the
    /// sorted currently available callable IDs (explicitly stating when none
    /// are available).
    fn not_found_error(&self, id: &str) -> ToolError {
        let available = self
            .registry
            .lock()
            .map(|registry| {
                registry
                    .names()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let message = if available.is_empty() {
            format!("Skill '{id}' not found: no skills are currently available")
        } else {
            format!(
                "Skill '{id}' not found. Available skills: {}",
                available.join(", ")
            )
        };
        ToolError::InvalidRequest(message)
    }

    /// Format sample files from skill directory
    fn format_sample_files(
        &self,
        skill: &crate::skills::types::Skill,
    ) -> Result<String, ToolError> {
        let mut samples = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&skill.path) {
            for entry in entries.flatten().take(10) {
                if let Ok(file_type) = entry.file_type()
                    && (file_type.is_file() || file_type.is_dir())
                {
                    let name = entry.file_name();
                    let kind = if file_type.is_dir() { "dir" } else { "file" };
                    samples.push(format!("  - {} ({})", name.to_string_lossy(), kind));
                }
            }
        }

        if samples.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!(
                "<sample_files>\n{}\n</sample_files>\n",
                samples.join("\n")
            ))
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn definition(&self) -> querymt::chat::Tool {
        // Refresh before snapshotting so the model-facing schema reflects the
        // current filesystem state. On refresh failure the previous registry
        // contents are retained and snapshotted.
        let _ = self.refresh_registry();

        // Take one coherent description/enum snapshot under the registry lock.
        let (skill_list, skill_names) = if let Ok(registry) = self.registry.lock() {
            let list = registry.list_for_description();
            let names = registry
                .names()
                .into_iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            (list, names)
        } else {
            ("Registry unavailable".to_string(), vec![])
        };

        querymt::chat::Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: Self::NAME.to_string(),
                description: format!(
                    "Load a skill to gain domain-specific knowledge and workflows.\n\n\
                    Available skills:\n{}\n\n\
                    Call with the skill name to load its content. Once loaded, the skill's \
                    instructions and workflows will be available for the remainder of the session.",
                    skill_list
                ),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Name of the skill to load",
                            "enum": skill_names
                        }
                    },
                    "required": ["name"]
                }),
                strict: None,
            },
        }
    }

    async fn call(
        &self,
        args: Value,
        ctx: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("'name' parameter required".into()))?;

        // Check permissions for the requested callable ID
        let permission = self.permissions.check(name);
        match permission {
            PermissionLevel::Deny => {
                return Err(ToolError::PermissionDenied(format!(
                    "Skill '{}' is blocked by configuration",
                    name
                )));
            }
            PermissionLevel::Ask => {
                // Use the question system to ask for permission
                let answers = ctx.ask_question(
                    &format!("skill-permission-{}", name),
                    &format!("The agent wants to load skill '{}'.\n\nThis will provide the agent with domain-specific knowledge and workflows from this skill.", name),
                    "Permission",
                    &[
                        ("Allow".to_string(), "Allow loading this skill".to_string()),
                        ("Deny".to_string(), "Deny loading this skill".to_string()),
                    ],
                    false,
                ).await?;

                if answers.is_empty() || answers[0] != "Allow" {
                    return Err(ToolError::PermissionDenied(format!(
                        "User denied loading skill '{}'",
                        name
                    )));
                }
            }
            PermissionLevel::Allow => {
                // Continue
            }
        }

        // Get skill from registry by callable ID; on a miss, refresh once and
        // retry so skills added after the last schema snapshot can still be
        // loaded.
        let mut skill = self.lookup_skill(name)?;
        if skill.is_none() {
            if let Err(error) = self.refresh_registry_async().await {
                log::warn!(
                    "Skill '{}' is not registered and the refresh failed: {}",
                    name,
                    error
                );
            }
            skill = self.lookup_skill(name)?;
        }

        let Some(skill) = skill else {
            return Err(self.not_found_error(name));
        };

        log::info!("Loading skill: {}", name);

        // Format output with structured wrapper
        let output = format!(
            "<skill_content name=\"{}\">\n\
            <description>{}</description>\n\
            <base_path>file://{}</base_path>\n\
            <content>\n{}\n</content>\n\
            {}\
            </skill_content>",
            skill.metadata.name,
            skill.metadata.description,
            skill.path.display(),
            skill.content,
            self.format_sample_files(&skill)?,
        );

        // Log tool restrictions if present
        if skill.metadata.allowed_tools.is_some() {
            log::debug!(
                "Skill '{}' has tool policy: {:?}",
                name,
                skill.metadata.tool_policy()
            );
            // TODO: Apply to session's active tool filter (Phase 5)
        }

        Ok(vec![ToolResultPart::text(output)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{Skill, SkillMetadata, SkillSource};
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn first_text_block(blocks: Vec<querymt::chat::ToolResultPart>) -> String {
        blocks
            .into_iter()
            .find_map(|b| match b {
                querymt::chat::ToolResultPart::Text { text } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Mock context whose `ask_question` answers are scripted. An empty
    /// script answers with no selections (i.e. the user denied the request).
    struct MockContext {
        session_id: String,
        ask_answers: Mutex<Vec<String>>,
    }

    impl MockContext {
        fn new() -> Self {
            Self {
                session_id: "test-session".to_string(),
                ask_answers: Mutex::new(vec![]),
            }
        }

        fn with_answers(answers: &[&str]) -> Self {
            Self {
                session_id: "test-session".to_string(),
                ask_answers: Mutex::new(answers.iter().map(|s| s.to_string()).collect()),
            }
        }
    }

    #[async_trait]
    impl ToolContext for MockContext {
        fn session_id(&self) -> &str {
            &self.session_id
        }

        fn cwd(&self) -> Option<&std::path::Path> {
            None
        }

        async fn record_progress(
            &self,
            _kind: &str,
            _content: String,
            _metadata: Option<serde_json::Value>,
        ) -> Result<String, ToolError> {
            Ok("progress-id".to_string())
        }

        async fn ask_question(
            &self,
            _question_id: &str,
            _question: &str,
            _header: &str,
            _options: &[(String, String)],
            _multiple: bool,
        ) -> Result<Vec<String>, ToolError> {
            let mut answers = self.ask_answers.lock().unwrap();
            if answers.is_empty() {
                return Ok(vec![]);
            }
            Ok(vec![answers.remove(0)])
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn create_test_skill(name: &str, description: &str) -> Skill {
        Skill {
            path: PathBuf::from(format!("/tmp/{}", name)),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: description.to_string(),
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
            content: format!("# {} Content\n\nTest content for {}", name, name),
            source: SkillSource::Global(PathBuf::from("/tmp")),
        }
    }

    /// Write a shallow `dir/<name>/SKILL.md` definition.
    fn write_skill(dir: &Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nContent for {name}\n"),
        )
        .unwrap();
    }

    fn tool_with_sources(
        registry: Arc<Mutex<SkillRegistry>>,
        sources: Vec<SkillSource>,
        permissions: Arc<SkillPermissions>,
    ) -> SkillTool {
        SkillTool::new(registry, permissions, sources, true)
    }

    fn enum_of(tool: &SkillTool) -> Vec<String> {
        let def = tool.definition();
        def.function.parameters["properties"]["name"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    async fn call_named(tool: &SkillTool, name: &str) -> Result<String, ToolError> {
        let ctx = MockContext::new();
        tool.call(json!({"name": name}), &ctx)
            .await
            .map(first_text_block)
    }

    #[tokio::test]
    async fn test_skill_tool_call() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let permissions = Arc::new(SkillPermissions::default());

        {
            let mut reg = registry.lock().unwrap();
            reg.register(create_test_skill("test-skill", "A test skill"));
        }

        // Empty sources: the registered skill is hit without any refresh.
        let tool = tool_with_sources(registry, vec![], permissions);
        let output = call_named(&tool, "test-skill").await.unwrap();
        assert!(output.contains("test-skill"));
        assert!(output.contains("A test skill"));
        assert!(output.contains("Test content for test-skill"));
    }

    #[tokio::test]
    async fn test_skill_not_found_lists_availability() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let permissions = Arc::new(SkillPermissions::default());

        // Empty sources and empty registry: the error states no skills are
        // available.
        let tool = tool_with_sources(registry, vec![], permissions);

        let error = call_named(&tool, "nonexistent").await.unwrap_err();
        match error {
            ToolError::InvalidRequest(message) => {
                assert!(message.contains("'nonexistent'"), "{message}");
                assert!(
                    message.contains("no skills are currently available"),
                    "{message}"
                );
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_skill_denied_by_permission() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("denied-skill".to_string(), PermissionLevel::Deny);
        let permissions = Arc::new(perms);

        {
            let mut reg = registry.lock().unwrap();
            reg.register(create_test_skill("denied-skill", "Denied skill"));
        }

        let tool = tool_with_sources(registry, vec![], permissions);

        let error = call_named(&tool, "denied-skill").await.unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)));
    }

    /// Task 2.2: permission checks and not-found output use callable
    /// effective IDs; a differing display name is not an invocation alias.
    #[tokio::test]
    async fn test_permissions_and_aliases_use_effective_id() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        let skill_dir = skills_root.join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("skill.md"),
            "---\nid: review\nname: Code Review\ndescription: Reviews code.\n---\nBody\n",
        )
        .unwrap();

        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("review".to_string(), PermissionLevel::Deny);
        let permissions = Arc::new(perms);

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(skills_root)],
            permissions,
        );

        // The stable ID is advertised and denied by configuration.
        assert_eq!(enum_of(&tool), vec!["review".to_string()]);
        let error = call_named(&tool, "review").await.unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)));

        // The differing display name is not a callable alias: it fails with a
        // not-found error rather than a permission denial.
        let error = call_named(&tool, "Code Review").await.unwrap_err();
        match error {
            ToolError::InvalidRequest(message) => {
                assert!(message.contains("'Code Review'"), "{message}");
                assert!(message.contains("review"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn test_skill_tool_definition() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skill-a", "First skill");
        write_skill(dir.path(), "skill-b", "Second skill");

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );

        let def = tool.definition();
        assert_eq!(def.function.name, SkillTool::NAME);
        assert!(!def.function.description.is_empty());
        let desc = &def.function.description;
        assert!(desc.contains("skill-a"));
        assert!(desc.contains("skill-b"));
        assert_eq!(
            def.function.parameters["properties"]["name"]["enum"],
            json!(["skill-a", "skill-b"])
        );
    }

    #[test]
    fn test_skill_tool_definition_includes_skill_with_compatibility_requirement() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("openspec-propose");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: openspec-propose
description: Propose an OpenSpec change
allowed-tools: Bash(openspec:*)
compatibility: Requires openspec CLI.
---
Body
"#,
        )
        .unwrap();

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );

        let definition = tool.definition();
        assert!(definition.function.description.contains("openspec-propose"));
        assert_eq!(
            definition.function.parameters["properties"]["name"]["enum"],
            json!(["openspec-propose"])
        );
    }

    /// Task 3.2: added, edited, and removed skill directories are reflected
    /// in successive `definition()` snapshots.
    #[test]
    fn test_definition_refreshes_add_edit_remove() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skill-a", "First version");

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );

        assert_eq!(enum_of(&tool), vec!["skill-a".to_string()]);

        // Add a skill directory; the next snapshot includes it.
        write_skill(dir.path(), "skill-b", "Added skill");
        let def = tool.definition();
        assert_eq!(
            enum_of(&tool),
            vec!["skill-a".to_string(), "skill-b".to_string()]
        );
        assert!(def.function.description.contains("Added skill"));

        // Edit a definition file; the next snapshot reflects the edit.
        write_skill(dir.path(), "skill-a", "Edited description");
        let def = tool.definition();
        assert!(def.function.description.contains("Edited description"));

        // Remove a skill directory; the next snapshot drops it.
        fs::remove_dir_all(dir.path().join("skill-a")).unwrap();
        let def = tool.definition();
        assert_eq!(enum_of(&tool), vec!["skill-b".to_string()]);
        assert!(!def.function.description.contains("skill-a"));
    }

    /// Task 3.3: concurrent `definition()` and `call()` workers each observe a
    /// complete, internally consistent registry snapshot, with no poisoned
    /// lock failures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_definition_and_call_are_consistent() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "alpha", "Alpha skill");
        write_skill(dir.path(), "beta", "Beta skill");
        write_skill(dir.path(), "gamma", "Gamma skill");

        let tool = Arc::new(tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        ));

        let mut handles = Vec::new();

        // Schema collectors observe complete snapshots.
        for _ in 0..4 {
            let tool = Arc::clone(&tool);
            handles.push(tokio::task::spawn_blocking(move || {
                for _ in 0..25 {
                    let def = tool.definition();
                    let names = def.function.parameters["properties"]["name"]["enum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect::<Vec<_>>();
                    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
                    for name in &names {
                        assert!(
                            def.function.description.contains(name.as_str()),
                            "description missing advertised skill {name}"
                        );
                    }
                }
            }));
        }

        // Invocations observe registered skills and succeed.
        for name in ["alpha", "beta", "gamma"] {
            for _ in 0..10 {
                let tool = Arc::clone(&tool);
                handles.push(tokio::spawn(async move {
                    let output = call_named(&tool, name).await.unwrap();
                    assert!(output.contains(name));
                }));
            }
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }

    /// Task 4.1: a skill added after the last schema snapshot is loadable via
    /// reload-on-miss.
    #[tokio::test]
    async fn test_call_reloads_on_miss() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skill-a", "First skill");

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );

        // Populate the schema snapshot before the new skill exists.
        assert_eq!(enum_of(&tool), vec!["skill-a".to_string()]);

        // Added after the last snapshot: no definition() in between.
        write_skill(dir.path(), "late-skill", "Late arrival");

        let output = call_named(&tool, "late-skill").await.unwrap();
        assert!(output.contains("late-skill"));
        assert!(output.contains("Late arrival"));
    }

    /// Task 4.1: invoking a removed skill fails with a deterministic error
    /// listing the currently available callable IDs.
    #[tokio::test]
    async fn test_call_removed_skill_errors_with_available_ids() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skill-a", "To be removed");
        write_skill(dir.path(), "skill-b", "Stays");

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );
        assert_eq!(
            enum_of(&tool),
            vec!["skill-a".to_string(), "skill-b".to_string()]
        );

        fs::remove_dir_all(dir.path().join("skill-a")).unwrap();

        // The next model-facing snapshot drops the removed skill...
        assert_eq!(enum_of(&tool), vec!["skill-b".to_string()]);

        // ...so the stale invocation fails deterministically.
        let error = call_named(&tool, "skill-a").await.unwrap_err();
        match error {
            ToolError::InvalidRequest(message) => {
                assert!(message.contains("'skill-a'"), "{message}");
                assert!(message.contains("skill-b"), "{message}");
                assert!(!message.contains("skill-a. Available"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    /// Task 4.1: a refresh failure preserves and reports the old snapshot.
    #[tokio::test]
    async fn test_call_with_failed_refresh_preserves_old_snapshot() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "healthy", "Healthy skill");

        let fail_after_first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let healthy_path = dir.path().join("healthy");
        let discovery_fn = {
            let fail_after_first = Arc::clone(&fail_after_first);
            Arc::new(move |_sources: &[SkillSource], _include_external: bool| {
                if fail_after_first.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Ok(vec![Skill {
                        path: healthy_path.clone(),
                        metadata: SkillMetadata {
                            name: "healthy".to_string(),
                            description: "Healthy skill".to_string(),
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
                        content: "Healthy content".to_string(),
                        source: SkillSource::Global(PathBuf::from("/ignored")),
                    }])
                } else {
                    Err(anyhow::anyhow!("injected refresh failure"))
                }
            })
        };

        let tool = SkillTool::new(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            true,
        )
        .with_discovery_fn(discovery_fn);

        // First refresh publishes the initial snapshot.
        assert_eq!(enum_of(&tool), vec!["healthy".to_string()]);

        // A skill added afterwards never becomes visible: refreshes fail.
        write_skill(dir.path(), "unreachable", "Never discovered");
        assert_eq!(enum_of(&tool), vec!["healthy".to_string()]);

        // Stale invocation: refresh fails, old snapshot is reported.
        let error = call_named(&tool, "unreachable").await.unwrap_err();
        match error {
            ToolError::InvalidRequest(message) => {
                assert!(message.contains("'unreachable'"), "{message}");
                assert!(message.contains("healthy"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        // The preserved skill remains loadable.
        let output = call_named(&tool, "healthy").await.unwrap();
        assert!(output.contains("Healthy content"));
    }

    /// Task 4.2: registered skills are not re-read on every invocation;
    /// content edits become visible after the next `definition()` refresh.
    #[tokio::test]
    async fn test_existing_skills_not_reread_per_invocation() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "edited-skill", "Before edit");

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![SkillSource::Project(dir.path().to_path_buf())],
            Arc::new(SkillPermissions::default()),
        );
        assert_eq!(enum_of(&tool), vec!["edited-skill".to_string()]);

        // Edit the definition file behind the snapshot...
        write_skill(dir.path(), "edited-skill", "After edit");

        // ...the pre-refresh call still serves the snapshot.
        let output = call_named(&tool, "edited-skill").await.unwrap();
        assert!(output.contains("Before edit"));
        assert!(!output.contains("After edit"));

        // The next schema refresh publishes the update.
        let def = tool.definition();
        assert!(def.function.description.contains("After edit"));
        let output = call_named(&tool, "edited-skill").await.unwrap();
        assert!(output.contains("After edit"));
    }

    /// Task 5.1: disabled protocol skills remain absent after refresh and
    /// duplicate/source precedence is unchanged.
    #[tokio::test]
    async fn test_refresh_keeps_disabled_skills_absent_and_precedence() {
        let global = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();

        let global_skills = global.path().join(".agents").join("skills");
        let project_skills = project.path().join(".agents").join("skills");

        // Same stable ID in both sources; project must win.
        let global_dir = global_skills.join("review");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("SKILL.md"),
            "---\nname: review\nid: review\ndescription: Global version\n---\nBody\n",
        )
        .unwrap();
        let project_dir = project_skills.join("review");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("skill.md"),
            "---\nid: review\ndescription: Workspace version\n---\nBody\n",
        )
        .unwrap();

        let tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            vec![
                SkillSource::Global(global_skills),
                SkillSource::Project(project_skills.clone()),
            ],
            Arc::new(SkillPermissions::default()),
        );

        assert_eq!(enum_of(&tool), vec!["review".to_string()]);
        let output = call_named(&tool, "review").await.unwrap();
        assert!(output.contains("Workspace version"));

        // A disabled protocol skill does not appear, even after a refresh.
        let disabled_dir = project_skills.join("off");
        fs::create_dir_all(&disabled_dir).unwrap();
        fs::write(
            disabled_dir.join("skill.md"),
            "---\nid: off\ndescription: Disabled skill\nenabled: false\n---\nBody\n",
        )
        .unwrap();

        assert_eq!(enum_of(&tool), vec!["review".to_string()]);
        let error = call_named(&tool, "off").await.unwrap_err();
        match error {
            ToolError::InvalidRequest(message) => {
                assert!(message.contains("'off'"), "{message}");
                assert!(message.contains("review"), "{message}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    /// Task 5.2: permission rules keyed by callable effective ID survive
    /// refresh for allow, ask, and deny; denied/missing aliases never prompt.
    #[tokio::test]
    async fn test_permissions_survive_refresh() {
        let dir = TempDir::new().unwrap();
        let skills_root = dir.path().join(".agents").join("skills");
        let skill_dir = skills_root.join("review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("skill.md"),
            "---\nid: review\nname: Code Review\ndescription: Reviews code.\n---\nBody\n",
        )
        .unwrap();

        let sources = vec![SkillSource::Project(skills_root)];

        // Deny keyed by effective ID: denied before and after refresh.
        let mut deny_perms = SkillPermissions::default();
        deny_perms
            .patterns
            .insert("review".to_string(), PermissionLevel::Deny);
        let deny_tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            sources.clone(),
            Arc::new(deny_perms),
        );
        for _ in 0..2 {
            assert_eq!(enum_of(&deny_tool), vec!["review".to_string()]);
            let error = call_named(&deny_tool, "review").await.unwrap_err();
            assert!(matches!(error, ToolError::PermissionDenied(_)));
        }

        // Ask keyed by effective ID: allowed on approval, denied on refusal.
        let mut ask_perms = SkillPermissions::default();
        ask_perms
            .patterns
            .insert("review".to_string(), PermissionLevel::Ask);
        let ask_tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            sources.clone(),
            Arc::new(ask_perms),
        );

        let ctx = MockContext::with_answers(&["Allow"]);
        let result = ask_tool.call(json!({"name": "review"}), &ctx).await;
        assert!(result.is_ok());

        let ctx = MockContext::with_answers(&["Deny"]);
        let error = ask_tool
            .call(json!({"name": "review"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PermissionDenied(_)));

        // A denied skill's display name is not prompted: it is simply not a
        // callable alias.
        let mut deny_alias_perms = SkillPermissions::default();
        deny_alias_perms
            .patterns
            .insert("review".to_string(), PermissionLevel::Deny);
        let deny_alias_tool = tool_with_sources(
            Arc::new(Mutex::new(SkillRegistry::new())),
            sources,
            Arc::new(deny_alias_perms),
        );
        let ctx = MockContext::with_answers(&["Allow"]);
        let error = deny_alias_tool
            .call(json!({"name": "Code Review"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }
}
