use crate::skills::discovery;
use crate::skills::permissions::{PermissionLevel, SkillPermissions};
use crate::skills::registry::SkillRegistry;
use crate::skills::types::{Skill, SkillSource};
use crate::tools::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::ToolResultPart;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Discovery function used by registry refreshes. Swappable in tests to
/// inject deterministic discovery failures.
type DiscoverFn = dyn Fn(&[SkillSource], bool) -> anyhow::Result<Vec<Skill>> + Send + Sync;

/// The skill tool that agents use to load skills on-demand
pub struct SkillTool {
    registries: Arc<Mutex<HashMap<PathBuf, SkillRegistry>>>,
    permissions: Arc<SkillPermissions>,
    /// Discovery sources for the fallback workspace. Global and configured
    /// sources remain common; project sources are re-derived for session cwd.
    sources: Vec<SkillSource>,
    fallback_workspace: PathBuf,
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
        Self::new_with_fallback(
            registry,
            permissions,
            sources,
            include_external,
            PathBuf::from("."),
        )
    }

    pub fn new_with_fallback(
        registry: Arc<Mutex<SkillRegistry>>,
        permissions: Arc<SkillPermissions>,
        sources: Vec<SkillSource>,
        include_external: bool,
        fallback_workspace: PathBuf,
    ) -> Self {
        let initial_registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut registries = HashMap::new();
        registries.insert(fallback_workspace.clone(), initial_registry);

        Self {
            registries: Arc::new(Mutex::new(registries)),
            permissions,
            sources,
            fallback_workspace,
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

    fn effective_workspace(&self, cwd: Option<&Path>) -> PathBuf {
        cwd.unwrap_or(&self.fallback_workspace).to_path_buf()
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
        if workspace == self.fallback_workspace {
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
    fn refresh_registry(&self, workspace: &Path) -> anyhow::Result<()> {
        let sources = self.sources_for_workspace(workspace);
        match (self.discovery_fn)(&sources, self.include_external) {
            Ok(skills) => {
                let mut registries = self
                    .registries
                    .lock()
                    .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
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
    async fn refresh_registry_async(&self, workspace: &Path) -> anyhow::Result<()> {
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

        let mut registries = self
            .registries
            .lock()
            .map_err(|_| anyhow::anyhow!("registry lock poisoned"))?;
        let count = registries
            .entry(workspace.clone())
            .or_default()
            .reload_with(skills);
        log::debug!(
            "Skill registry refreshed for {}: {count} skills available",
            workspace.display()
        );
        Ok(())
    }

    /// Look up a skill by callable ID in one workspace snapshot.
    fn lookup_skill(&self, workspace: &Path, id: &str) -> Result<Option<Arc<Skill>>, ToolError> {
        let registries = self
            .registries
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("Registry lock poisoned")))?;
        Ok(registries
            .get(workspace)
            .and_then(|registry| registry.get(id)))
    }

    /// Deterministic not-found error naming the requested callable ID and the
    /// sorted currently available callable IDs (explicitly stating when none
    /// are available).
    fn not_found_error(&self, workspace: &Path, id: &str) -> ToolError {
        let available = self
            .registries
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

    fn definition_for_workspace(&self, cwd: Option<&Path>) -> querymt::chat::Tool {
        let workspace = self.effective_workspace(cwd);

        // Refresh before snapshotting so the model-facing schema reflects the
        // current filesystem state. On refresh failure the previous workspace
        // snapshot is retained.
        let _ = self.refresh_registry(&workspace);

        let (skill_list, skill_names) = if let Ok(registries) = self.registries.lock() {
            if let Some(registry) = registries.get(&workspace) {
                let list = registry.list_for_description();
                let names = registry
                    .names()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                (list, names)
            } else {
                ("No skills available".to_string(), vec![])
            }
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
        self.definition_for_workspace(None)
    }

    fn definition_for_cwd(&self, cwd: Option<&Path>) -> querymt::chat::Tool {
        self.definition_for_workspace(cwd)
    }

    async fn call(
        &self,
        args: Value,
        ctx: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("'name' parameter required".into()))?;
        let workspace = self.effective_workspace(ctx.cwd());

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

        // Get skill from this workspace's registry; on a miss, refresh once and
        // retry so skills added after the last schema snapshot can still load.
        let mut skill = self.lookup_skill(&workspace, name)?;
        if skill.is_none() {
            if let Err(error) = self.refresh_registry_async(&workspace).await {
                log::warn!(
                    "Skill '{}' is not registered for {} and the refresh failed: {error:#}",
                    name,
                    workspace.display()
                );
            }
            skill = self.lookup_skill(&workspace, name)?;
        }

        let Some(skill) = skill else {
            return Err(self.not_found_error(&workspace, name));
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
        cwd: Option<PathBuf>,
        ask_answers: Mutex<Vec<String>>,
    }

    impl MockContext {
        fn new() -> Self {
            Self {
                session_id: "test-session".to_string(),
                cwd: None,
                ask_answers: Mutex::new(vec![]),
            }
        }

        fn with_cwd(cwd: impl Into<PathBuf>) -> Self {
            Self {
                cwd: Some(cwd.into()),
                ..Self::new()
            }
        }

        fn with_answers(answers: &[&str]) -> Self {
            Self {
                session_id: "test-session".to_string(),
                cwd: None,
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
            self.cwd.as_deref()
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

    fn names_from_definition(definition: querymt::chat::Tool) -> Vec<String> {
        definition.function.parameters["properties"]["name"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn enum_of(tool: &SkillTool) -> Vec<String> {
        names_from_definition(tool.definition())
    }

    fn enum_for_cwd(tool: &SkillTool, cwd: &Path) -> Vec<String> {
        names_from_definition(tool.definition_for_cwd(Some(cwd)))
    }

    async fn call_named_with_context(
        tool: &SkillTool,
        name: &str,
        ctx: &dyn ToolContext,
    ) -> Result<String, ToolError> {
        tool.call(json!({"name": name}), ctx)
            .await
            .map(first_text_block)
    }

    async fn call_named(tool: &SkillTool, name: &str) -> Result<String, ToolError> {
        call_named_with_context(tool, name, &MockContext::new()).await
    }

    #[tokio::test]
    async fn test_two_workspaces_advertise_and_call_only_their_own_skills() {
        let fallback = TempDir::new().unwrap();
        let workspace_a = TempDir::new().unwrap();
        let workspace_b = TempDir::new().unwrap();
        write_skill(
            &workspace_a.path().join(".qmt/skills"),
            "only-a",
            "Workspace A",
        );
        write_skill(
            &workspace_b.path().join(".qmt/skills"),
            "only-b",
            "Workspace B",
        );

        let tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(fallback.path().join(".qmt/skills"))],
            true,
            fallback.path().to_path_buf(),
        );

        assert_eq!(enum_for_cwd(&tool, workspace_a.path()), vec!["only-a"]);
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), vec!["only-b"]);

        let context_a = MockContext::with_cwd(workspace_a.path());
        let context_b = MockContext::with_cwd(workspace_b.path());
        let output_a = call_named_with_context(&tool, "only-a", &context_a)
            .await
            .unwrap();
        let output_b = call_named_with_context(&tool, "only-b", &context_b)
            .await
            .unwrap();
        assert!(output_a.contains("Workspace A"));
        assert!(output_b.contains("Workspace B"));

        let error = call_named_with_context(&tool, "only-b", &context_a)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidRequest(message) if message.contains("only-a") && !message.contains("Available skills: only-b"))
        );
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), vec!["only-b"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_simultaneous_workspace_calls_do_not_cross_registry_snapshots() {
        let fallback = TempDir::new().unwrap();
        let workspace_a = TempDir::new().unwrap();
        let workspace_b = TempDir::new().unwrap();
        write_skill(
            &workspace_a.path().join(".qmt/skills"),
            "shared",
            "Content from A",
        );
        write_skill(
            &workspace_b.path().join(".qmt/skills"),
            "shared",
            "Content from B",
        );

        let tool = Arc::new(SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(fallback.path().join(".qmt/skills"))],
            true,
            fallback.path().to_path_buf(),
        ));
        assert_eq!(enum_for_cwd(&tool, workspace_a.path()), vec!["shared"]);
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), vec!["shared"]);

        let mut calls = Vec::new();
        for (workspace, expected, unexpected) in [
            (
                workspace_a.path().to_path_buf(),
                "Content from A",
                "Content from B",
            ),
            (
                workspace_b.path().to_path_buf(),
                "Content from B",
                "Content from A",
            ),
        ] {
            for _ in 0..20 {
                let tool = Arc::clone(&tool);
                let context = MockContext::with_cwd(workspace.clone());
                calls.push(tokio::spawn(async move {
                    let output = call_named_with_context(&tool, "shared", &context)
                        .await
                        .unwrap();
                    assert!(output.contains(expected), "{output}");
                    assert!(!output.contains(unexpected), "{output}");
                }));
            }
        }

        for call in calls {
            call.await.unwrap();
        }
    }

    #[test]
    fn test_no_cwd_definition_uses_builder_workspace_fallback() {
        let fallback = TempDir::new().unwrap();
        let session = TempDir::new().unwrap();
        write_skill(
            &fallback.path().join(".qmt/skills"),
            "builder-skill",
            "Builder fallback",
        );
        write_skill(
            &session.path().join(".qmt/skills"),
            "session-skill",
            "Session workspace",
        );

        let tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(fallback.path().join(".qmt/skills"))],
            true,
            fallback.path().to_path_buf(),
        );

        assert_eq!(enum_of(&tool), vec!["builder-skill"]);
        assert_eq!(enum_for_cwd(&tool, session.path()), vec!["session-skill"]);
        assert_eq!(
            names_from_definition(tool.definition_for_cwd(None)),
            vec!["builder-skill"]
        );
    }

    #[tokio::test]
    async fn test_workspace_precedence_and_configured_source_are_context_aware() {
        let fallback = TempDir::new().unwrap();
        let workspace_a = TempDir::new().unwrap();
        let workspace_b = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        let configured = TempDir::new().unwrap();

        write_skill(global.path(), "shared", "Global version");
        write_skill(
            &workspace_a.path().join(".qmt/skills"),
            "shared",
            "Workspace A version",
        );
        write_skill(
            &workspace_b.path().join(".qmt/skills"),
            "shared",
            "Workspace B version",
        );
        write_skill(configured.path(), "shared", "Configured version");
        write_skill(configured.path(), "configured-only", "Common configured");

        let sources = vec![
            SkillSource::Global(global.path().to_path_buf()),
            SkillSource::Project(fallback.path().join(".qmt/skills")),
            SkillSource::Configured(configured.path().to_path_buf()),
        ];
        let tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            sources,
            true,
            fallback.path().to_path_buf(),
        );

        let expected = vec!["configured-only".to_string(), "shared".to_string()];
        assert_eq!(enum_for_cwd(&tool, workspace_a.path()), expected);
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), expected);

        for workspace in [workspace_a.path(), workspace_b.path()] {
            let context = MockContext::with_cwd(workspace);
            let output = call_named_with_context(&tool, "shared", &context)
                .await
                .unwrap();
            assert!(output.contains("Configured version"));
        }
    }

    #[test]
    fn test_include_external_false_keeps_configured_sources_common() {
        let fallback = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let configured = TempDir::new().unwrap();
        write_skill(
            &workspace.path().join(".qmt/skills"),
            "project-only",
            "Project source",
        );
        write_skill(configured.path(), "configured-only", "Configured source");

        let sources = vec![
            SkillSource::Project(fallback.path().join(".qmt/skills")),
            SkillSource::Configured(configured.path().to_path_buf()),
        ];
        let tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            sources,
            false,
            fallback.path().to_path_buf(),
        );

        assert_eq!(
            enum_for_cwd(&tool, workspace.path()),
            vec!["configured-only"]
        );
    }

    #[test]
    fn test_tool_registry_definitions_are_context_aware() {
        let fallback = TempDir::new().unwrap();
        let workspace_a = TempDir::new().unwrap();
        let workspace_b = TempDir::new().unwrap();
        write_skill(
            &workspace_a.path().join(".qmt/skills"),
            "registry-a",
            "Registry A",
        );
        write_skill(
            &workspace_b.path().join(".qmt/skills"),
            "registry-b",
            "Registry B",
        );

        let skill_tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(fallback.path().join(".qmt/skills"))],
            true,
            fallback.path().to_path_buf(),
        );
        let mut registry = crate::tools::ToolRegistry::new();
        registry.add(Arc::new(skill_tool));

        let definition = registry
            .definition_for_cwd(SkillTool::NAME, Some(workspace_a.path()))
            .unwrap();
        assert_eq!(
            names_from_definition(definition.clone()),
            vec!["registry-a"]
        );
        let validator = jsonschema::validator_for(&definition.function.parameters).unwrap();
        assert!(validator.validate(&json!({"name": "registry-a"})).is_ok());
        assert!(validator.validate(&json!({"name": "registry-b"})).is_err());

        let definitions = registry.definitions_for_cwd(Some(workspace_b.path()));
        let definition = definitions
            .into_iter()
            .find(|definition| definition.function.name == SkillTool::NAME)
            .unwrap();
        assert_eq!(names_from_definition(definition), vec!["registry-b"]);
    }

    #[test]
    fn test_context_aware_schema_hot_reload_is_workspace_local() {
        let fallback = TempDir::new().unwrap();
        let workspace_a = TempDir::new().unwrap();
        let workspace_b = TempDir::new().unwrap();
        write_skill(
            &workspace_a.path().join(".qmt/skills"),
            "a-first",
            "A first",
        );
        write_skill(&workspace_b.path().join(".qmt/skills"), "b-only", "B only");

        let tool = SkillTool::new_with_fallback(
            Arc::new(Mutex::new(SkillRegistry::new())),
            Arc::new(SkillPermissions::default()),
            vec![SkillSource::Project(fallback.path().join(".qmt/skills"))],
            true,
            fallback.path().to_path_buf(),
        );
        assert_eq!(enum_for_cwd(&tool, workspace_a.path()), vec!["a-first"]);
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), vec!["b-only"]);

        write_skill(&workspace_a.path().join(".qmt/skills"), "a-late", "A late");
        assert_eq!(
            enum_for_cwd(&tool, workspace_a.path()),
            vec!["a-first", "a-late"]
        );
        assert_eq!(enum_for_cwd(&tool, workspace_b.path()), vec!["b-only"]);
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
