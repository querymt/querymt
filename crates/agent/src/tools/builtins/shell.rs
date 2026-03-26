//! Shell tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

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

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
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

        // Place the child in its own process group so that on cancellation we
        // can kill the entire tree (sh + any children it spawned) with a single
        // signal rather than leaving orphaned processes behind.
        #[cfg(unix)]
        cmd.process_group(0);

        // Safety net: if the tokio `Child` is dropped (e.g. task abort) send
        // SIGKILL to the direct child automatically.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::ProviderError(format!("command failed to spawn: {}", e)))?;

        // Capture the PID before moving `child` into the spawned task so we can
        // kill the process group from the cancel branch.
        let child_pid = child.id();

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
                // Abort the spawned task — `kill_on_drop(true)` ensures the
                // direct child receives SIGKILL when the `Child` is dropped.
                wait_handle.abort();

                // Kill the entire process group so that any grandchildren
                // (e.g. processes spawned by the shell command) are also
                // terminated.  The negative PID targets the process group
                // we created with `process_group(0)` above.
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    // SAFETY: We send SIGKILL to the process group whose PGID
                    // equals `pid`.  The group was created by us moments ago
                    // via `process_group(0)`.  If the process already exited
                    // the call harmlessly returns ESRCH.
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                }

                return Err(ToolError::ProviderError("Cancelled by user".to_string()));
            }
        };

        let output = std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        };

        let result = json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        });

        serde_json::to_string(&result)
            .map(|s| vec![Content::text(s)])
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_text_block(blocks: Vec<querymt::chat::Content>) -> String {
        blocks
            .into_iter()
            .find_map(|b| match b {
                querymt::chat::Content::Text { text } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }
    use crate::tools::AgentToolContext;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_shell_echo() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ShellTool::new();

        let args = json!({
            "command": "echo hello"
        });

        let result = first_text_block(tool.call(args, &context).await.unwrap());
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

        let result = first_text_block(tool.call(args, &context).await.unwrap());
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert!(parsed["stdout"].as_str().unwrap().contains("hello world"));
    }

    /// Verify that cancelling a running shell command actually kills the
    /// spawned process (and its entire process group on Unix).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cancel_kills_process() {
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let token = CancellationToken::new();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()))
                .with_cancellation_token(token.clone());

        let tool = ShellTool::new();

        // Write a marker PID file so we can verify the process is gone.
        // `sleep 300` will block for 5 minutes — long enough that it can
        // only finish if we kill it.
        let pid_file = temp_dir.path().join("shell.pid");
        let args = json!({
            "command": format!(
                "echo $$ > {} && exec sleep 300",
                pid_file.display()
            )
        });

        // Spawn the tool call in a task so we can cancel from outside.
        let handle = tokio::spawn({
            let context = context;
            async move { tool.call(args, &context).await }
        });

        // Wait for the PID file to appear (process has started).
        let pid: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&pid_file).await {
                    if let Ok(pid) = contents.trim().parse::<i32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("shell process did not write its PID in time");

        // Cancel the token — this should trigger kill.
        token.cancel();

        // The tool call should return an error.
        let result = handle.await.expect("task panicked");
        assert!(result.is_err(), "expected cancellation error");

        // Give the OS a moment to reap the process.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify the process is no longer running.
        // kill(pid, 0) checks existence without sending a signal.
        let alive = unsafe { libc::kill(pid, 0) };
        assert_eq!(
            alive, -1,
            "process {pid} should be dead after cancellation"
        );
    }
}
