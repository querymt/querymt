//! Create task tool implementation using ToolContext

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

pub struct CreateTaskTool;

impl Default for CreateTaskTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CreateTaskTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for CreateTaskTool {
    fn name(&self) -> &str {
        "create_task"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "create_task".to_string(),
                description: "Create a new task for the current session. Use this when the user requests work that should be tracked with clear completion criteria.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["finite", "recurring", "evolving"],
                            "description": "Task kind: 'finite' (one-time with clear end), 'recurring' (repeated), or 'evolving' (open-ended)"
                        },
                        "expected_deliverable": {
                            "type": "string",
                            "description": "What should be produced when this task is complete"
                        },
                        "acceptance_criteria": {
                            "type": "string",
                            "description": "How to determine if the deliverable is satisfactory"
                        }
                    },
                    "required": ["kind", "expected_deliverable"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    async fn call(&self, args: Value, _context: &dyn ToolContext) -> Result<String, ToolError> {
        // Validate arguments
        let kind_str = args["kind"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'kind' field".into()))?;

        if !matches!(kind_str, "finite" | "recurring" | "evolving") {
            return Err(ToolError::InvalidRequest(format!(
                "Invalid task kind '{}'. Must be 'finite', 'recurring', or 'evolving'",
                kind_str
            )));
        }

        let _expected_deliverable = args["expected_deliverable"].as_str().ok_or_else(|| {
            ToolError::InvalidRequest("Missing 'expected_deliverable' field".into())
        })?;

        // The actual task creation is handled by the agent loop via events
        // This tool just validates and returns success
        Ok("Task creation request recorded.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_task_validation() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = CreateTaskTool::new();

        // Test valid request
        let args = json!({
            "kind": "finite",
            "expected_deliverable": "a new feature"
        });
        let result = tool.call(args, &context).await;
        assert!(result.is_ok());

        // Test missing kind
        let args = json!({
            "expected_deliverable": "a new feature"
        });
        let result = tool.call(args, &context).await;
        assert!(result.is_err());

        // Test invalid kind
        let args = json!({
            "kind": "invalid",
            "expected_deliverable": "a new feature"
        });
        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }
}
