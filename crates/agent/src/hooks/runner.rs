use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CommandHookSpec {
    pub command: String,
    pub timeout: Duration,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run_command_hook(
    spec: &CommandHookSpec,
    cwd: &Path,
    stdin_json: &str,
) -> anyhow::Result<CommandOutput> {
    let mut command = if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(&spec.command);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&spec.command);
        cmd
    };

    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(spec.env.clone());

    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let result = async {
            stdin.write_all(stdin_json.as_bytes()).await?;
            stdin.shutdown().await
        }
        .await;
        // Hooks may produce a valid response without consuming their input.
        if let Err(err) = result
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(err.into());
        }
    }

    let output = tokio::time::timeout(spec.timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("hook timed out after {}s", spec.timeout.as_secs()))??;

    Ok(CommandOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn early_stdin_close_preserves_output_and_exit_status() {
        // Exceed the pipe capacity so the write cannot finish before stdin closes.
        let input = "x".repeat(4 * 1024 * 1024);
        let cwd = tempfile::tempdir().unwrap();
        for exit_code in [0, 2] {
            let spec = CommandHookSpec {
                command: format!(
                    "exec 0<&-; printf '%s' 'response'; printf '%s' 'diagnostic' >&2; exit {exit_code}"
                ),
                timeout: Duration::from_secs(5),
                env: BTreeMap::new(),
            };
            let output = tokio::time::timeout(
                Duration::from_secs(10),
                run_command_hook(&spec, cwd.path(), &input),
            )
            .await
            .expect("hook should finish after closing stdin")
            .unwrap();
            assert_eq!(output.exit_code, Some(exit_code));
            assert_eq!(output.stdout, "response");
            assert_eq!(output.stderr, "diagnostic");
        }
    }

    #[tokio::test]
    async fn hook_receives_complete_input_and_eof() {
        let spec = CommandHookSpec {
            command: "cat".into(),
            timeout: Duration::from_secs(5),
            env: BTreeMap::new(),
        };
        let cwd = tempfile::tempdir().unwrap();
        let input = r#"{"session_id":"test"}"#;
        let output = run_command_hook(&spec, cwd.path(), input).await.unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, input);
        assert!(output.stderr.is_empty());
    }
}
