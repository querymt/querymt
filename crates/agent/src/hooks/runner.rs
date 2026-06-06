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
        stdin.write_all(stdin_json.as_bytes()).await?;
        stdin.shutdown().await?;
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
