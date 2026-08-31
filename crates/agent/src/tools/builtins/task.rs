use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};

use crate::session::domain::{TaskKind, TaskStatus};
use crate::session::store::TaskPatch;
use crate::tools::{
    CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError, ToolExecutionClass,
};

fn service(context: &dyn ToolContext) -> Result<crate::session::TaskService, ToolError> {
    context
        .task_service()
        .ok_or_else(|| ToolError::SessionError("task service is unavailable".to_string()))
}

fn task_content(
    task: &crate::session::domain::Task,
    current: bool,
) -> Result<Vec<Content>, ToolError> {
    let mut value =
        serde_json::to_value(task).map_err(|error| ToolError::SessionError(error.to_string()))?;
    value["current_task"] = Value::Bool(current);
    Ok(vec![Content::text(
        serde_json::to_string_pretty(&value)
            .map_err(|error| ToolError::SessionError(error.to_string()))?,
    )])
}

#[derive(Default)]
pub struct ReadTaskTool;

impl ReadTaskTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ReadTaskTool {
    fn name(&self) -> &str {
        "read_task"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read the current task commitment, or a task in this session by ID."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "task_id": { "type": "string" } }
                }),
            },
        }
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        let requested_id = args.get("task_id").and_then(Value::as_str);
        let task = service(context)?
            .read(requested_id)
            .await
            .map_err(|error| ToolError::SessionError(error.to_string()))?
            .ok_or_else(|| ToolError::SessionError("task not found in this session".to_string()))?;
        task_content(&task, requested_id.is_none())
    }
}

#[derive(Default)]
pub struct UpdateTaskTool;

impl UpdateTaskTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for UpdateTaskTool {
    fn name(&self) -> &str {
        "update_task"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Patch a task commitment using compare-and-swap revision control. Use complete_task for completion.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "expected_revision": { "type": "integer", "minimum": 1 },
                        "reason": { "type": "string" },
                        "expected_deliverable": { "type": "string" },
                        "acceptance_criteria": { "type": "string" },
                        "status": { "type": "string", "enum": ["active", "paused", "cancelled"] },
                        "cancellation_reason": { "type": "string" }
                    },
                    "required": ["task_id", "expected_revision", "reason"]
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
        let task_id = args
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("task_id is required".to_string()))?;
        let expected_revision = args
            .get("expected_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolError::InvalidRequest("expected_revision is required".to_string())
            })?;
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("reason is required".to_string()))?;
        let status = match args.get("status").and_then(Value::as_str) {
            Some("active") => Some(TaskStatus::Active),
            Some("paused") => Some(TaskStatus::Paused),
            Some("cancelled") => Some(TaskStatus::Cancelled),
            Some(other) => {
                return Err(ToolError::InvalidRequest(format!(
                    "invalid task status: {other}"
                )));
            }
            None => None,
        };
        if status == Some(TaskStatus::Cancelled)
            && args
                .get("cancellation_reason")
                .and_then(Value::as_str)
                .is_none()
        {
            return Err(ToolError::InvalidRequest(
                "cancellation_reason is required".to_string(),
            ));
        }
        let task = service(context)?
            .update(
                task_id,
                expected_revision,
                TaskPatch {
                    expected_deliverable: args
                        .get("expected_deliverable")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    acceptance_criteria: args
                        .get("acceptance_criteria")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status,
                    cancellation_reason: args
                        .get("cancellation_reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                reason,
            )
            .await
            .map_err(|error| ToolError::SessionError(error.to_string()))?;
        if task.revision == expected_revision {
            return Ok(vec![Content::text(format!(
                "Task unchanged; no revision was created. Current task state:\n{}",
                serde_json::to_string_pretty(&task)
                    .map_err(|error| ToolError::Other(error.into()))?
            ))]);
        }
        context.emit_event(crate::events::AgentEventKind::TaskUpdated { task: task.clone() });
        task_content(&task, task.status == TaskStatus::Active)
    }
}

#[derive(Default)]
pub struct CompleteTaskTool;

impl CompleteTaskTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for CompleteTaskTool {
    fn name(&self) -> &str {
        "complete_task"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description:
                    "Explicitly complete a finite or evolving task with verification evidence."
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string" },
                        "expected_revision": { "type": "integer", "minimum": 1 },
                        "completion_evidence": { "type": "string" }
                    },
                    "required": ["task_id", "expected_revision", "completion_evidence"]
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
        let task_id = args
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("task_id is required".to_string()))?;
        let expected_revision = args
            .get("expected_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ToolError::InvalidRequest("expected_revision is required".to_string())
            })?;
        let evidence = args
            .get("completion_evidence")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidRequest("completion_evidence is required".to_string())
            })?;
        let task = service(context)?
            .complete(task_id, expected_revision, evidence)
            .await
            .map_err(|error| ToolError::SessionError(error.to_string()))?;
        context.emit_event(crate::events::AgentEventKind::TaskStatusChanged { task: task.clone() });
        task_content(&task, false)
    }
}

pub(crate) fn parse_task_kind(value: &str) -> Result<TaskKind, ToolError> {
    match value {
        "finite" => Ok(TaskKind::Finite),
        "recurring" => Ok(TaskKind::Recurring),
        "evolving" => Ok(TaskKind::Evolving),
        other => Err(ToolError::InvalidRequest(format!(
            "invalid task kind: {other}"
        ))),
    }
}
