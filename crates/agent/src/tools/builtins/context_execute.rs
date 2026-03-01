//! Context-safe shell command execution tool.
//!
//! Like `shell`, but stores full output in the retrieval index and returns
//! only a bounded preview to model context. This prevents large command
//! outputs from flooding the conversation while keeping the data searchable.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// Default maximum preview lines returned to model context.
const DEFAULT_PREVIEW_LINES: usize = 50;

pub struct ContextExecuteTool;

impl Default for ContextExecuteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextExecuteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ContextExecuteTool {
    fn name(&self) -> &str {
        "context_execute"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: concat!(
                    "Run a shell command, index the full output for later retrieval, ",
                    "and return only a bounded preview. Use this instead of `shell` ",
                    "when you expect large output (build logs, test results, file listings). ",
                    "The full output is searchable via `context_search`."
                )
                .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Command to run. If args is omitted, this is passed to the shell."
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Arguments for the command."
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Working directory."
                        },
                        "preview_lines": {
                            "type": "integer",
                            "description": "Maximum lines in the returned preview (default: 50). Full output is always indexed.",
                            "default": 50
                        },
                        "source_label": {
                            "type": "string",
                            "description": "Optional label for the indexed source (for scoped retrieval). Defaults to the command string."
                        }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The full output is indexed and searchable. Use `context_search` \
             to find specific content without loading the entire output into context.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("command is required".to_string()))?;

        let workdir = args
            .get("workdir")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?;

        let preview_lines = args
            .get("preview_lines")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_PREVIEW_LINES as u64) as usize;

        let source_label = args
            .get("source_label")
            .and_then(Value::as_str)
            .unwrap_or(command);

        let arg_list = args.get("args").and_then(Value::as_array);

        let mut cmd = if let Some(args) = arg_list {
            let mut cmd = Command::new(command);
            cmd.args(args.iter().filter_map(Value::as_str));
            cmd
        } else if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", command]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-lc", command]);
            cmd
        };

        let dir = workdir
            .or_else(|| context.cwd().map(|p| p.to_path_buf()))
            .ok_or_else(|| ToolError::InvalidRequest("No working directory available".into()))?;
        cmd.current_dir(dir);

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ProviderError(format!("command failed to spawn: {}", e)))?;

        let cancel = context.cancellation_token();

        let wait_handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let (_, _) = tokio::join!(
                async {
                    if let Some(ref mut s) = stdout {
                        let _ = s.read_to_end(&mut stdout_buf).await;
                    }
                },
                async {
                    if let Some(ref mut s) = stderr {
                        let _ = s.read_to_end(&mut stderr_buf).await;
                    }
                },
            );
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, stdout_buf, stderr_buf))
        });

        tokio::pin!(wait_handle);

        let (status, stdout_buf, stderr_buf) = tokio::select! {
            result = &mut wait_handle => {
                result
                    .map_err(|e| ToolError::ProviderError(format!("task join failed: {}", e)))?
                    .map_err(|e| ToolError::ProviderError(format!("command failed: {}", e)))?
            }
            _ = cancel.cancelled() => {
                wait_handle.abort();
                return Err(ToolError::ProviderError("Cancelled by user".to_string()));
            }
        };

        let exit_code = status.code().unwrap_or(-1);
        let stdout_str = String::from_utf8_lossy(&stdout_buf);
        let stderr_str = String::from_utf8_lossy(&stderr_buf);

        // Combine stdout + stderr for indexing
        let full_output = if stderr_str.is_empty() {
            stdout_str.to_string()
        } else {
            format!("{}\n--- stderr ---\n{}", stdout_str, stderr_str)
        };

        let total_lines = full_output.lines().count();
        let total_bytes = full_output.len();

        // Index the full output (best-effort — do not fail the tool call)
        let indexed = index_output(context, source_label, &full_output).await;

        // Build bounded preview
        let preview = build_preview(&full_output, preview_lines);

        let result = json!({
            "exit_code": exit_code,
            "preview": preview,
            "total_lines": total_lines,
            "total_bytes": total_bytes,
            "indexed": indexed,
            "source_label": source_label,
            "hint": if total_lines > preview_lines {
                "Output was large. Use `context_search` to find specific content."
            } else {
                "Full output shown."
            }
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
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

/// Best-effort indexing of tool output into session retrieval store.
///
/// Returns `true` if indexing succeeded, `false` otherwise.
/// Failures are logged but never propagate as tool errors.
async fn index_output(context: &dyn ToolContext, source_label: &str, content: &str) -> bool {
    match context
        .index_context_content(source_label, content.to_string())
        .await
    {
        Ok(_) => true,
        Err(e) => {
            log::debug!("context_execute: failed to index output: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[test]
    fn test_build_preview_short() {
        let content = "line 1\nline 2\nline 3";
        let preview = build_preview(content, 50);
        assert_eq!(preview, content);
    }

    #[test]
    fn test_build_preview_long() {
        let content = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = build_preview(&content, 10);
        // Should contain head + gap + tail
        assert!(preview.contains("line 0"));
        assert!(preview.contains("omitted"));
        assert!(preview.contains("line 99"));
        // Should not contain middle lines
        assert!(!preview.contains("line 50"));
    }

    #[tokio::test]
    async fn test_context_execute_echo() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteTool::new();

        let args = json!({
            "command": "echo hello world"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["preview"].as_str().unwrap().contains("hello world"));
        assert_eq!(parsed["hint"], "Full output shown.");
    }

    #[tokio::test]
    async fn test_context_execute_with_source_label() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ContextExecuteTool::new();

        let args = json!({
            "command": "echo test",
            "source_label": "build_log"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["source_label"], "build_log");
    }
}
