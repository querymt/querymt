//! Batch execution tool for multi-command + multi-query workflows.
//!
//! Runs multiple shell commands in sequence, indexes all outputs, and
//! optionally runs retrieval queries against the indexed results — all
//! in a single tool call. This dramatically reduces round-trips for
//! research-heavy tasks.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// Maximum commands allowed in a single batch.
const MAX_BATCH_COMMANDS: usize = 10;

/// Maximum queries allowed in a single batch.
const MAX_BATCH_QUERIES: usize = 10;

/// Default preview lines per command output.
const DEFAULT_PREVIEW_LINES: usize = 30;

/// Default max search results per query.
const DEFAULT_MAX_SEARCH_RESULTS: usize = 5;

pub struct BatchExecuteTool;

impl Default for BatchExecuteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchExecuteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for BatchExecuteTool {
    fn name(&self) -> &str {
        "batch_execute"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: concat!(
                    "Run multiple shell commands in sequence, index all outputs, and ",
                    "optionally run retrieval queries against the results — all in one call. ",
                    "This is the preferred primitive for research-heavy tasks: gathering build logs, ",
                    "test results, and file contents, then searching across all of them. ",
                    "Each command's full output is indexed and searchable via `context_search`."
                )
                .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "commands": {
                            "type": "array",
                            "description": "Commands to run in sequence. Each produces an indexed source.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "command": {
                                        "type": "string",
                                        "description": "Shell command to run."
                                    },
                                    "label": {
                                        "type": "string",
                                        "description": "Source label for indexing (defaults to command string)."
                                    },
                                    "workdir": {
                                        "type": "string",
                                        "description": "Working directory for this command."
                                    }
                                },
                                "required": ["command"]
                            },
                            "maxItems": 10
                        },
                        "queries": {
                            "type": "array",
                            "description": "Optional retrieval queries to run after all commands complete. Searches across all indexed outputs.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "query": {
                                        "type": "string",
                                        "description": "Search query text."
                                    },
                                    "source_label": {
                                        "type": "string",
                                        "description": "Optional: limit search to a specific source label."
                                    },
                                    "max_results": {
                                        "type": "integer",
                                        "description": "Maximum results for this query (default: 5).",
                                        "default": 5
                                    }
                                },
                                "required": ["query"]
                            },
                            "maxItems": 10
                        },
                        "preview_lines": {
                            "type": "integer",
                            "description": "Maximum preview lines per command output (default: 30).",
                            "default": 30
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Default working directory for all commands."
                        }
                    },
                    "required": ["commands"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: All command outputs are indexed. Use `context_search` or add \
             queries to the `queries` parameter to search across batch results.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let commands = args
            .get("commands")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidRequest("commands array is required".to_string()))?;

        if commands.is_empty() {
            return Err(ToolError::InvalidRequest(
                "commands array must not be empty".to_string(),
            ));
        }

        if commands.len() > MAX_BATCH_COMMANDS {
            return Err(ToolError::InvalidRequest(format!(
                "maximum {} commands per batch",
                MAX_BATCH_COMMANDS
            )));
        }

        let queries = args
            .get("queries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if queries.len() > MAX_BATCH_QUERIES {
            return Err(ToolError::InvalidRequest(format!(
                "maximum {} queries per batch",
                MAX_BATCH_QUERIES
            )));
        }

        let preview_lines = args
            .get("preview_lines")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_PREVIEW_LINES as u64) as usize;

        let default_workdir = args
            .get("workdir")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?;

        let cancel = context.cancellation_token();

        // Execute commands in sequence
        let mut command_results = Vec::new();

        for cmd_spec in commands {
            // Check for cancellation between commands
            if cancel.is_cancelled() {
                return Err(ToolError::ProviderError("Cancelled by user".to_string()));
            }

            let command = cmd_spec
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::InvalidRequest(
                        "each command must have a 'command' field".to_string(),
                    )
                })?;

            let label = cmd_spec
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(command);

            let workdir = cmd_spec
                .get("workdir")
                .and_then(Value::as_str)
                .map(|s| context.resolve_path(s))
                .transpose()?
                .or_else(|| default_workdir.clone())
                .or_else(|| context.cwd().map(|p| p.to_path_buf()))
                .ok_or_else(|| {
                    ToolError::InvalidRequest("No working directory available".to_string())
                })?;

            let result = run_single_command(command, &workdir, context, label, preview_lines).await;
            command_results.push(result);
        }

        // Run retrieval queries (best-effort, failures reported inline)
        let mut query_results = Vec::new();

        for query_spec in &queries {
            let query = query_spec
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("");

            if query.is_empty() {
                query_results.push(json!({
                    "query": query,
                    "error": "empty query"
                }));
                continue;
            }

            let source_label = query_spec.get("source_label").and_then(Value::as_str);

            let max_results = query_spec
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_SEARCH_RESULTS as u64) as usize;

            match context
                .search_context_content(query, source_label, max_results)
                .await
            {
                Ok(snippets) => {
                    let total_results = snippets.len();
                    query_results.push(json!({
                        "query": query,
                        "source_label": source_label,
                        "results": snippets,
                        "total_results": total_results
                    }));
                }
                Err(e) => {
                    query_results.push(json!({
                        "query": query,
                        "source_label": source_label,
                        "error": e.to_string(),
                        "results": [],
                        "total_results": 0,
                        "hint": "Use `context_search` when retrieval backend is available."
                    }));
                }
            }
        }

        let result = json!({
            "commands_run": command_results.len(),
            "results": command_results,
            "queries": if query_results.is_empty() { Value::Null } else { json!(query_results) },
            "hint": "All outputs are indexed. Use `context_search` to search across results."
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

async fn run_single_command(
    command: &str,
    workdir: &std::path::Path,
    context: &dyn ToolContext,
    label: &str,
    preview_lines: usize,
) -> Value {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-lc", command]);
        cmd
    };

    cmd.current_dir(workdir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(e) => {
            return json!({
                "command": command,
                "label": label,
                "error": format!("failed to spawn: {}", e)
            });
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);

    let full_output = if stderr_str.is_empty() {
        stdout_str.to_string()
    } else {
        format!("{}\n--- stderr ---\n{}", stdout_str, stderr_str)
    };

    let total_lines = full_output.lines().count();
    let total_bytes = full_output.len();

    // Index the output (best-effort)
    let indexed = context
        .index_context_content(label, full_output.clone())
        .await
        .is_ok();

    // Build preview
    let preview = build_preview(&full_output, preview_lines);

    json!({
        "command": command,
        "label": label,
        "exit_code": exit_code,
        "preview": preview,
        "total_lines": total_lines,
        "total_bytes": total_bytes,
        "indexed": indexed
    })
}

/// Build a bounded preview: head + tail with a gap indicator.
fn build_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }

    let head_count = max_lines * 2 / 3;
    let tail_count = max_lines - head_count;
    let omitted = lines.len() - head_count - tail_count;

    let mut preview = lines[..head_count].join("\n");
    preview.push_str(&format!("\n\n... ({} lines omitted) ...\n\n", omitted));
    preview.push_str(&lines[lines.len() - tail_count..].join("\n"));
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_batch_execute_single_command() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = BatchExecuteTool::new();

        let args = json!({
            "commands": [
                { "command": "echo hello", "label": "echo_test" }
            ]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["commands_run"], 1);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results[0]["exit_code"], 0);
        assert!(results[0]["preview"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn test_batch_execute_multiple_commands() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = BatchExecuteTool::new();

        let args = json!({
            "commands": [
                { "command": "echo first" },
                { "command": "echo second" },
                { "command": "echo third" }
            ]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["commands_run"], 3);
        let results = parsed["results"].as_array().unwrap();
        assert!(results[0]["preview"].as_str().unwrap().contains("first"));
        assert!(results[1]["preview"].as_str().unwrap().contains("second"));
        assert!(results[2]["preview"].as_str().unwrap().contains("third"));
    }

    #[tokio::test]
    async fn test_batch_execute_empty_commands() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = BatchExecuteTool::new();

        let args = json!({ "commands": [] });
        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_batch_execute_too_many_commands() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = BatchExecuteTool::new();

        let commands: Vec<Value> = (0..11)
            .map(|i| json!({ "command": format!("echo {}", i) }))
            .collect();
        let args = json!({ "commands": commands });
        let result = tool.call(args, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_batch_execute_with_queries() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = BatchExecuteTool::new();

        let args = json!({
            "commands": [
                { "command": "echo hello" }
            ],
            "queries": [
                { "query": "hello" }
            ]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["commands_run"], 1);
        assert!(parsed["queries"].is_array());

        let queries = parsed["queries"].as_array().unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0]["query"], "hello");
        assert!(queries[0]["error"].is_string());
        assert!(queries[0]["results"].is_array());
        assert_eq!(queries[0]["total_results"], 0);
    }

    #[tokio::test]
    async fn test_batch_execute_with_queries_returns_results_when_retrieval_available() {
        use crate::session::backend::StorageBackend;
        use crate::session::sqlite_storage::SqliteStorage;

        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let store = storage.session_store();
        let session = store.create_session(None, None, None, None).await.unwrap();

        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::new(
            session.public_id,
            Some(temp_dir.path().to_path_buf()),
            None,
            Some(store),
            None,
        );
        let tool = BatchExecuteTool::new();

        let args = json!({
            "commands": [
                { "command": "echo hello", "label": "echo_hello" }
            ],
            "queries": [
                { "query": "hello", "source_label": "echo_hello", "max_results": 3 }
            ]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        let queries = parsed["queries"].as_array().unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0]["query"], "hello");
        assert_eq!(queries[0]["source_label"], "echo_hello");
        assert!(queries[0]["results"].is_array());
        assert!(queries[0]["total_results"].as_u64().unwrap() >= 1);
        assert!(queries[0].get("error").is_none());
    }
}
