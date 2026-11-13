//! Todo tools for managing agent task lists

use async_trait::async_trait;
use parking_lot::Mutex;
use querymt::chat::{FunctionTool, Tool as ChatTool, ToolResultPart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::tools::{Tool, ToolContext, ToolError};

/// Lifecycle state of a todo item. Serialized as the lowercase strings the
/// `todowrite` tool schema advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

/// Priority of a todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
    pub priority: TodoPriority,
}

/// Per-session todo lists, owned by `AgentConfig` and shared with tools through
/// [`ToolContext::todo_store`]. Lists live as long as the agent, so they
/// survive session actor restarts, and are dropped when the session is deleted.
#[derive(Debug, Default)]
pub struct TodoStore {
    lists: Mutex<HashMap<String, Vec<TodoItem>>>,
}

impl TodoStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the session's list.
    pub fn replace(&self, session_id: &str, todos: Vec<TodoItem>) {
        self.lists.lock().insert(session_id.to_string(), todos);
    }

    /// The session's list, empty if it never wrote one.
    pub fn get(&self, session_id: &str) -> Vec<TodoItem> {
        self.lists
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Discard the session's list.
    pub fn remove(&self, session_id: &str) {
        self.lists.lock().remove(session_id);
    }
}

fn unavailable() -> ToolError {
    ToolError::SessionError("todos are unavailable: this context has no todo store".to_string())
}

/// TodoWrite tool for updating the todo list
pub struct TodoWriteTool;

impl TodoWriteTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todowrite"
    }

    fn is_read_only(&self) -> bool {
        // Touches session or knowledge state, never the workspace files.
        true
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Use this tool to create and manage a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.\n\
                    It also helps the user understand the progress of the task and overall progress of their requests.\n\n\
                    ## When to Use This Tool\n\
                    Use this tool proactively in these scenarios:\n\
                    1. Complex multistep tasks - When a task requires 3 or more distinct steps or actions\n\
                    2. Non-trivial and complex tasks - Tasks that require careful planning or multiple operations\n\
                    3. User explicitly requests todo list - When the user directly asks you to use the todo list\n\
                    4. User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)\n\
                    5. After receiving new instructions - Immediately capture user requirements as todos\n\
                    6. After completing a task - Mark it complete and add any new follow-up tasks\n\
                    7. When you start working on a new task, mark the todo as in_progress\n\n\
                    ## Task States\n\
                    - pending: Task not yet started\n\
                    - in_progress: Currently working on (limit to ONE task at a time)\n\
                    - completed: Task finished successfully\n\
                    - cancelled: Task no longer needed"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "The updated todo list",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "id": {
                                        "type": "string",
                                        "description": "Unique identifier for the todo item"
                                    },
                                    "content": {
                                        "type": "string",
                                        "description": "Brief description of the task"
                                    },
                                    "status": {
                                        "type": "string",
                                        "description": "Current status: pending, in_progress, completed, cancelled",
                                        "enum": ["pending", "in_progress", "completed", "cancelled"]
                                    },
                                    "priority": {
                                        "type": "string",
                                        "description": "Priority level: high, medium, low",
                                        "enum": ["high", "medium", "low"]
                                    }
                                },
                                "required": ["id", "content", "status", "priority"]
                            }
                        }
                    },
                    "required": ["todos"]
                }),
                strict: None,
            },
        }
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let todos_val = args
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidRequest("todos array is required".to_string()))?;

        let todos: Vec<TodoItem> = todos_val
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid todo format: {}", e)))?;

        let store = context.todo_store().ok_or_else(unavailable)?;

        let (mut pending, mut in_progress, mut completed, mut cancelled) = (0, 0, 0, 0);
        for todo in &todos {
            match todo.status {
                TodoStatus::Pending => pending += 1,
                TodoStatus::InProgress => in_progress += 1,
                TodoStatus::Completed => completed += 1,
                TodoStatus::Cancelled => cancelled += 1,
            }
        }
        let total = todos.len();
        store.replace(context.session_id(), todos);

        let result = json!({
            "success": true,
            "total_todos": total,
            "pending": pending,
            "in_progress": in_progress,
            "completed": completed,
            "cancelled": cancelled,
        });

        serde_json::to_string_pretty(&result)
            .map(|s| vec![ToolResultPart::text(s)])
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}

/// TodoRead tool for reading the current todo list
pub struct TodoReadTool;

impl TodoReadTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TodoReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoReadTool {
    fn name(&self) -> &str {
        "todoread"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read the current todo list for this session. Returns all todos with their current status and priority."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {},
                    "required": []
                }),
                strict: None,
            },
        }
    }

    async fn call(
        &self,
        _args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let store = context.todo_store().ok_or_else(unavailable)?;
        let todos = store.get(context.session_id());

        let result = json!({
            "todos": todos,
            "total": todos.len(),
        });

        serde_json::to_string_pretty(&result)
            .map(|s| vec![ToolResultPart::text(s)])
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;

    fn first_text_block(blocks: Vec<querymt::chat::ToolResultPart>) -> String {
        blocks
            .into_iter()
            .find_map(|b| match b {
                querymt::chat::ToolResultPart::Text { text } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn context(session: &str, store: &Arc<TodoStore>) -> AgentToolContext {
        AgentToolContext::basic(session.to_string(), None).with_todo_store(store.clone())
    }

    fn todo(id: &str, status: &str, priority: &str) -> Value {
        json!({"id": id, "content": format!("Task {id}"), "status": status, "priority": priority})
    }

    async fn write(ctx: &AgentToolContext, todos: Vec<Value>) -> Result<Value, ToolError> {
        let out = TodoWriteTool::new()
            .call(json!({ "todos": todos }), ctx)
            .await?;
        Ok(serde_json::from_str(&first_text_block(out)).unwrap())
    }

    async fn read(ctx: &AgentToolContext) -> Value {
        let out = TodoReadTool::new().call(json!({}), ctx).await.unwrap();
        serde_json::from_str(&first_text_block(out)).unwrap()
    }

    #[tokio::test]
    async fn two_sessions_keep_separate_todo_lists() {
        let store = Arc::new(TodoStore::new());
        let a = context("a", &store);
        let b = context("b", &store);
        write(
            &a,
            vec![todo("1", "pending", "high"), todo("2", "completed", "low")],
        )
        .await
        .unwrap();
        write(&b, vec![todo("9", "in_progress", "medium")])
            .await
            .unwrap();

        let ra = read(&a).await;
        let rb = read(&b).await;
        assert_eq!(ra["total"], 2);
        assert_eq!(ra["todos"][0]["id"], "1");
        assert_eq!(ra["todos"][1]["id"], "2");
        assert_eq!(rb["total"], 1);
        assert_eq!(rb["todos"][0]["id"], "9");
    }

    #[tokio::test]
    async fn reading_before_any_write_returns_an_empty_list() {
        let store = Arc::new(TodoStore::new());
        let result = read(&context("fresh", &store)).await;
        assert_eq!(result["todos"], json!([]));
        assert_eq!(result["total"], 0);
    }

    #[tokio::test]
    async fn unknown_status_is_rejected() {
        let store = Arc::new(TodoStore::new());
        let ctx = context("s", &store);
        write(&ctx, vec![todo("1", "pending", "high")])
            .await
            .unwrap();

        let err = write(&ctx, vec![todo("2", "done", "high")])
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidRequest(_)), "{err:?}");
        let err = write(&ctx, vec![todo("2", "pending", "urgent")])
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidRequest(_)), "{err:?}");

        let after = read(&ctx).await;
        assert_eq!(after["total"], 1);
        assert_eq!(after["todos"][0]["id"], "1");
    }

    #[tokio::test]
    async fn stored_values_round_trip_unchanged() {
        let store = Arc::new(TodoStore::new());
        let ctx = context("s", &store);
        write(&ctx, vec![todo("1", "in_progress", "high")])
            .await
            .unwrap();
        let result = read(&ctx).await;
        assert_eq!(result["todos"][0]["status"], "in_progress");
        assert_eq!(result["todos"][0]["priority"], "high");
    }

    #[tokio::test]
    async fn counts_match_the_written_list() {
        let store = Arc::new(TodoStore::new());
        let ctx = context("s", &store);
        let result = write(
            &ctx,
            vec![
                todo("1", "pending", "high"),
                todo("2", "in_progress", "high"),
                todo("3", "in_progress", "low"),
                todo("4", "cancelled", "medium"),
            ],
        )
        .await
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["total_todos"], 4);
        assert_eq!(result["pending"], 1);
        assert_eq!(result["in_progress"], 2);
        assert_eq!(result["completed"], 0);
        assert_eq!(result["cancelled"], 1);
    }

    #[tokio::test]
    async fn context_without_a_store_reports_todos_unavailable() {
        let ctx = AgentToolContext::basic("s".to_string(), None);
        let err = TodoReadTool::new().call(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("unavailable"), "{err}");
    }

    #[tokio::test]
    async fn removing_a_session_discards_its_list() {
        let store = Arc::new(TodoStore::new());
        let ctx = context("s", &store);
        write(&ctx, vec![todo("1", "pending", "high")])
            .await
            .unwrap();
        store.remove("s");
        assert_eq!(read(&ctx).await["total"], 0);
    }
}
