//! Create task tool implementation using ToolContext

use crate::tools::{
    CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError, ToolExecutionClass,
};
use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
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
        &[CapabilityRequirement::SessionState]
    }

    fn execution_class(&self) -> ToolExecutionClass {
        ToolExecutionClass::SerialStateful
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        // Validate arguments
        let kind_str = args["kind"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidRequest("Missing 'kind' field".into()))?;

        let kind = super::task::parse_task_kind(kind_str)?;
        let expected_deliverable = args["expected_deliverable"].as_str().ok_or_else(|| {
            ToolError::InvalidRequest("Missing 'expected_deliverable' field".into())
        })?;
        let service = context
            .task_service()
            .ok_or_else(|| ToolError::SessionError("task service is unavailable".to_string()))?;
        let task = service
            .create(
                kind,
                expected_deliverable.to_string(),
                args.get("acceptance_criteria")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
            .await
            .map_err(|error| ToolError::SessionError(error.to_string()))?;
        context.emit_event(crate::events::AgentEventKind::TaskCreated { task: task.clone() });
        Ok(vec![Content::text(
            serde_json::to_string_pretty(&task)
                .map_err(|error| ToolError::SessionError(error.to_string()))?,
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_create_task_validation() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(
            crate::session::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        use crate::session::store::SessionStore;
        storage
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let session = storage.list_sessions().await.unwrap().remove(0);
        let context = AgentToolContext::basic(
            session.public_id.clone(),
            Some(temp_dir.path().to_path_buf()),
        )
        .with_task_service(crate::session::TaskService::new(
            storage,
            session.public_id,
            "test-call".to_string(),
        ));
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
