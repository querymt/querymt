use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::registry::BuiltInTool;

pub struct ShellTool;

impl ShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl BuiltInTool for ShellTool {
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

    async fn call(&self, args: Value) -> Result<String, LLMError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("command is required".to_string()))?;
        let workdir = args.get("workdir").and_then(Value::as_str);
        let arg_list = args.get("args").and_then(Value::as_array);

        let mut cmd = if let Some(args) = arg_list {
            let mut cmd = Command::new(command);
            cmd.args(args.iter().filter_map(Value::as_str));
            cmd
        } else {
            if cfg!(target_os = "windows") {
                let mut cmd = Command::new("cmd");
                cmd.args(["/C", command]);
                cmd
            } else {
                let mut cmd = Command::new("sh");
                cmd.args(["-lc", command]);
                cmd
            }
        };

        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| LLMError::ProviderError(format!("command failed: {}", e)))?;

        let result = json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        });
        serde_json::to_string(&result)
            .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))
    }
}
