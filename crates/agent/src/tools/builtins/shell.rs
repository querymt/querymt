//! Shell tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// Inspect command output for OS-level sandbox denial signatures.
///
/// Returns a human-readable note when all of the following are true:
/// - The session is in read-only mode (Plan/Review)
/// - The command exited with a non-zero code
/// - stderr contains a pattern produced by Landlock (Linux) or Seatbelt (macOS)
///   when a write operation is blocked: "Operation not permitted" (EPERM) or
///   "Permission denied" (EACCES)
///
/// The note is appended to the JSON result as `sandbox_note` so the agent
/// understands why the command failed and knows to switch to Build mode.
/// We surface a note rather than a hard error because:
/// - The command may have produced partial output before the denied op.
/// - "Permission denied" can also arise from ordinary file ownership issues
///   unrelated to the sandbox — callers should see the raw stderr too.
fn detect_sandbox_denial(stderr: &str, exit_code: i32, is_read_only: bool) -> Option<String> {
    if !is_read_only || exit_code == 0 {
        return None;
    }

    // Patterns emitted by the kernel when Landlock (Linux) or Seatbelt (macOS)
    // blocks an operation. Both map to errno EACCES or EPERM respectively.
    let has_denial =
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied");

    if has_denial {
        Some(
            "This command failed with a permission error while the session is in \
             read-only mode (Plan/Review). The OS sandbox may have blocked a \
             write operation. If the command needs filesystem write access, \
             switch to Build mode."
                .to_string(),
        )
    } else {
        None
    }
}

pub struct ShellTool;

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Run a shell command and return stdout/stderr.".to_string(),
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
            "TIP: Pipe command output through grep/head/tail to filter results, \
             or use search_text on the overflow file to find specific content.",
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

        // Pipe stdout/stderr so we can read them after waiting.
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ProviderError(format!("command failed to spawn: {}", e)))?;

        let cancel = context.cancellation_token();

        // Drive the child to completion in a cancellable way.
        //
        // `wait_with_output` cannot be used here because it moves `child` into
        // its future — the cancel branch would then have no way to call `kill`.
        // Instead we spawn the wait+collect as a JoinHandle so both branches can
        // be expressed without ownership conflicts: the handle is abortable via
        // `abort()`, and the child PID is captured as a raw handle for killing.
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

        // Race the child wait against cancellation. Both arms need access to
        // `wait_handle` (normal path to get the result, cancel path to abort),
        // so we pin it and use `&mut` refs in the select branches.
        tokio::pin!(wait_handle);

        let (status, stdout_buf, stderr_buf) = tokio::select! {
            result = &mut wait_handle => {
                result
                    .map_err(|e| ToolError::ProviderError(format!("task join failed: {}", e)))?
                    .map_err(|e| ToolError::ProviderError(format!("command failed: {}", e)))?
            }
            _ = cancel.cancelled() => {
                // Abort the spawned task. On Unix, dropping the tokio `Child`
                // does NOT send SIGKILL, but aborting the task causes the
                // `Child` to be dropped which closes its stdin; the process
                // will likely receive SIGPIPE and exit soon. This is best-effort
                // — the primary goal is unblocking the agent, not guaranteed
                // process termination.
                wait_handle.abort();
                return Err(ToolError::ProviderError("Cancelled by user".to_string()));
            }
        };

        let output = std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        };

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut result = json!({
            "exit_code": exit_code,
            "stdout": stdout_str,
            "stderr": stderr_str,
        });

        if let Some(note) = detect_sandbox_denial(&stderr_str, exit_code, context.is_read_only()) {
            result["sandbox_note"] = Value::String(note);
        }

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_shell_echo() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ShellTool::new();

        let args = json!({
            "command": "echo hello"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_args() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ShellTool::new();

        let args = json!({
            "command": "echo",
            "args": ["hello", "world"]
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["stdout"].as_str().unwrap().contains("hello world"));
    }

    /// Read-only mode must not block commands that don't touch the filesystem.
    #[tokio::test]
    async fn test_shell_read_only_allows_read_commands() {
        let temp_dir = TempDir::new().unwrap();
        let context = AgentToolContext::basic_read_only(
            "test".to_string(),
            Some(temp_dir.path().to_path_buf()),
        );
        let tool = ShellTool::new();

        let result = tool
            .call(json!({ "command": "echo hello" }), &context)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["stdout"].as_str().unwrap().contains("hello"));
        // No sandbox note for a successful, non-mutating command
        assert!(parsed.get("sandbox_note").is_none());
    }

    // ── detect_sandbox_denial unit tests ─────────────────────────────────

    #[test]
    fn test_no_note_when_not_read_only() {
        assert!(
            detect_sandbox_denial("Permission denied", 1, false).is_none(),
            "should not annotate when not in read-only mode"
        );
    }

    #[test]
    fn test_no_note_on_success() {
        assert!(
            detect_sandbox_denial("Permission denied", 0, true).is_none(),
            "should not annotate when exit code is 0"
        );
    }

    #[test]
    fn test_no_note_for_unrelated_failure() {
        assert!(
            detect_sandbox_denial("command not found", 127, true).is_none(),
            "should not annotate for unrelated failures"
        );
    }

    #[test]
    fn test_note_on_eperm_in_read_only() {
        let note = detect_sandbox_denial("touch: file: Operation not permitted", 1, true);
        assert!(note.is_some(), "should annotate EPERM in read-only mode");
        assert!(note.unwrap().contains("Build mode"));
    }

    #[test]
    fn test_note_on_eacces_in_read_only() {
        let note = detect_sandbox_denial("bash: file.txt: Permission denied", 1, true);
        assert!(note.is_some(), "should annotate EACCES in read-only mode");
        assert!(note.unwrap().contains("Build mode"));
    }
}
