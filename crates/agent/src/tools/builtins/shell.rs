//! Shell tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct ShellTool;

#[cfg(unix)]
fn detach_session() -> std::io::Result<()> {
    // SAFETY: `setsid` only changes process-local session state.
    if unsafe { libc::setsid() } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
struct KillProcessGroup(Option<u32>);

#[cfg(unix)]
impl Drop for KillProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: `setsid` made the child the leader of this process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

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

        // Pipe stdout/stderr so we can read them after waiting, and detach
        // stdin so commands that read from it receive EOF instead of the terminal.
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());

        // Detach from the controlling tty. `setsid` also creates the process
        // group used for cancellation below.
        #[cfg(unix)]
        {
            // SAFETY: the callback only performs the async-signal-safe `setsid` syscall.
            unsafe { cmd.pre_exec(detach_session) };
        }

        // Safety net: if the tokio `Child` is dropped (e.g. task abort) send
        // SIGKILL to the direct child automatically.
        cmd.kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::ProviderError(format!("command failed to spawn: {}", e)))?;

        #[cfg(unix)]
        let mut group_guard = KillProcessGroup(child.id());

        let cancel = context.cancellation_token();
        let output = tokio::select! {
            result = child.wait_with_output() => {
                let output = result
                    .map_err(|e| ToolError::ProviderError(format!("command failed: {}", e)))?;
                #[cfg(unix)]
                {
                    group_guard.0 = None;
                }
                output
            }
            _ = cancel.cancelled() => {
                return Err(ToolError::ProviderError("Cancelled by user".to_string()));
            }
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

    #[cfg(unix)]
    async fn wait_for_pid(path: &std::path::Path) -> i32 {
        use std::time::Duration;

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(path).await
                    && let Ok(pid) = contents.trim().parse::<i32>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("process did not write its PID in time")
    }

    #[cfg(unix)]
    async fn assert_process_exits(pid: i32) {
        use std::time::Duration;

        tokio::time::timeout(Duration::from_secs(5), async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("process {pid} did not exit"));
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_stdin_defaults_to_eof() {
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = ShellTool::new();

        let args = json!({
            "command": "cat 2>&1 | tail -5"
        });

        let result = tokio::time::timeout(Duration::from_secs(2), tool.call(args, &context))
            .await
            .expect("stdin-reading command should complete with EOF")
            .unwrap();
        let parsed: Value = serde_json::from_str(&first_text_block(result)).unwrap();

        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["stdout"], "");
        assert_eq!(parsed["stderr"], "");
    }

    /// Verify that cancelling a running shell command actually kills the
    /// spawned process (and its entire process group on Unix).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_cancel_kills_process() {
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

        let pid = wait_for_pid(&pid_file).await;

        // SAFETY: `getsid` only queries process metadata.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);

        // Cancel the token — this should trigger kill.
        token.cancel();

        // The tool call should return an error.
        let result = handle.await.expect("task panicked");
        assert!(result.is_err(), "expected cancellation error");

        assert_process_exits(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_dropping_tool_future_kills_process_group() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let pid_file = temp_dir.path().join("child.pid");
        let args = json!({
            "command": format!("sleep 300 & echo $! > {}; wait", pid_file.display())
        });

        let handle = tokio::spawn(async move { ShellTool::new().call(args, &context).await });
        let pid = wait_for_pid(&pid_file).await;

        handle.abort();
        let _ = handle.await;

        assert_process_exits(pid).await;
    }
}
