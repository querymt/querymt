//! Todo tools for managing agent task lists

use async_trait::async_trait;
use once_cell::sync::Lazy;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::tools::{Tool, ToolContext, ToolError};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoItem {
    id: String,
    content: String,
    status: String,   // "pending", "in_progress", "completed", "cancelled"
    priority: String, // "high", "medium", "low"
}

type TodoStorageType = Arc<Mutex<HashMap<String, Vec<TodoItem>>>>;
/// Global todo storage (session_id -> todos)
static TODO_STORAGE: Lazy<TodoStorageType> = Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

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
                    "properties": {
                        "todos": {
                            "type": "array",
                            "description": "The updated todo list",
                            "items": {
                                "type": "object",
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
            },
        }
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let todos_val = args
            .get("todos")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidRequest("todos array is required".to_string()))?;

        let todos: Vec<TodoItem> = todos_val
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid todo format: {}", e)))?;

        let session_id = context.session_id().to_string();

        // Store todos for this session
        let mut storage = TODO_STORAGE
            .lock()
            .map_err(|e| ToolError::SessionError(format!("Failed to lock todo storage: {}", e)))?;
        storage.insert(session_id, todos.clone());

        let result = json!({
            "success": true,
            "total_todos": todos.len(),
            "pending": todos.iter().filter(|t| t.status == "pending").count(),
            "in_progress": todos.iter().filter(|t| t.status == "in_progress").count(),
            "completed": todos.iter().filter(|t| t.status == "completed").count(),
            "cancelled": todos.iter().filter(|t| t.status == "cancelled").count(),
        });

        serde_json::to_string_pretty(&result)
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

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read the current todo list for this session. Returns all todos with their current status and priority."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        }
    }

    async fn call(&self, _args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let session_id = context.session_id().to_string();

        let storage = TODO_STORAGE
            .lock()
            .map_err(|e| ToolError::SessionError(format!("Failed to lock todo storage: {}", e)))?;

        let todos = storage.get(&session_id).cloned().unwrap_or_default();

        let result = json!({
            "todos": todos,
            "total": todos.len(),
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_todo_write_and_read() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic(
            "test-session".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );

        let write_tool = TodoWriteTool::new();
        let read_tool = TodoReadTool::new();

        // Write todos
        let write_args = json!({
            "todos": [
                {
                    "id": "1",
                    "content": "Task 1",
                    "status": "pending",
                    "priority": "high"
                },
                {
                    "id": "2",
                    "content": "Task 2",
                    "status": "in_progress",
                    "priority": "medium"
                }
            ]
        });

        let write_result = write_tool.call(write_args, &context).await.unwrap();
        assert!(write_result.contains("\"total_todos\": 2"));

        // Read todos
        let read_args = json!({});
        let read_result = read_tool.call(read_args, &context).await.unwrap();
        assert!(read_result.contains("Task 1"));
        assert!(read_result.contains("Task 2"));
    }
}
