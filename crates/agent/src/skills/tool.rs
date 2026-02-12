use crate::skills::permissions::{PermissionLevel, SkillPermissions};
use crate::skills::registry::SkillRegistry;
use crate::tools::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// The skill tool that agents use to load skills on-demand
pub struct SkillTool {
    registry: Arc<Mutex<SkillRegistry>>,
    agent_id: Option<String>,
    permissions: Arc<SkillPermissions>,
}

impl SkillTool {
    pub fn new(
        registry: Arc<Mutex<SkillRegistry>>,
        agent_id: Option<String>,
        permissions: Arc<SkillPermissions>,
    ) -> Self {
        Self {
            registry,
            agent_id,
            permissions,
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
        "skill"
    }

    fn definition(&self) -> querymt::chat::Tool {
        let (skill_list, skill_names) = if let Ok(registry) = self.registry.lock() {
            let list = registry.list_for_description(self.agent_id.as_deref());
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
                name: "skill".to_string(),
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
            },
        }
    }

    async fn call(&self, args: Value, ctx: &dyn ToolContext) -> Result<String, ToolError> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("'name' parameter required".into()))?;

        // Check permissions
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

        // Get skill from registry
        let skill = self
            .registry
            .lock()
            .map_err(|_| ToolError::Other(anyhow::anyhow!("Registry lock poisoned")))?
            .get(name)
            .ok_or_else(|| ToolError::InvalidRequest(format!("Skill '{}' not found", name)))?;

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
        if let Some(policy) = skill.metadata.allowed_tools.as_ref() {
            log::debug!("Skill '{}' has tool policy: {:?}", name, policy);
            // TODO: Apply to session's active tool filter (Phase 5)
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{Skill, SkillMetadata, SkillSource};
    use std::collections::HashMap;
    use std::path::PathBuf;

    struct MockContext {
        session_id: String,
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

    #[tokio::test]
    async fn test_skill_tool_call() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let permissions = Arc::new(SkillPermissions::default());

        {
            let mut reg = registry.lock().unwrap();
            reg.register(create_test_skill("test-skill", "A test skill"));
        }

        let tool = SkillTool::new(registry, None, permissions);
        let ctx = MockContext {
            session_id: "test-session".to_string(),
        };

        let args = json!({"name": "test-skill"});
        let result = tool.call(args, &ctx).await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("test-skill"));
        assert!(output.contains("A test skill"));
        assert!(output.contains("Test content for test-skill"));
    }

    #[tokio::test]
    async fn test_skill_not_found() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let permissions = Arc::new(SkillPermissions::default());

        let tool = SkillTool::new(registry, None, permissions);
        let ctx = MockContext {
            session_id: "test-session".to_string(),
        };

        let args = json!({"name": "nonexistent"});
        let result = tool.call(args, &ctx).await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::InvalidRequest(_)));
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

        let tool = SkillTool::new(registry, None, permissions);
        let ctx = MockContext {
            session_id: "test-session".to_string(),
        };

        let args = json!({"name": "denied-skill"});
        let result = tool.call(args, &ctx).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ToolError::PermissionDenied(_)
        ));
    }

    #[test]
    fn test_skill_tool_definition() {
        let registry = Arc::new(Mutex::new(SkillRegistry::new()));
        let permissions = Arc::new(SkillPermissions::default());

        {
            let mut reg = registry.lock().unwrap();
            reg.register(create_test_skill("skill-a", "First skill"));
            reg.register(create_test_skill("skill-b", "Second skill"));
        }

        let tool = SkillTool::new(registry, None, permissions);
        let def = tool.definition();

        assert_eq!(def.function.name, "skill");
        assert!(!def.function.description.is_empty());
        let desc = &def.function.description;
        assert!(desc.contains("skill-a"));
        assert!(desc.contains("skill-b"));
    }
}
