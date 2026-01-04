use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use querymt::error::LLMError;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::registry::BuiltInTool;

pub struct ApplyPatchTool;

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl BuiltInTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Apply a unified diff patch using the system patch utility."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "Unified diff patch to apply."
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Working directory for the patch."
                        },
                        "strip": {
                            "type": "integer",
                            "description": "Number of leading path components to strip.",
                            "default": 0
                        }
                    },
                    "required": ["patch"]
                }),
            },
        }
    }

    async fn call(&self, args: Value) -> Result<String, LLMError> {
        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| LLMError::InvalidRequest("patch is required".to_string()))?;
        let workdir = args.get("workdir").and_then(Value::as_str);
        let strip = args.get("strip").and_then(Value::as_u64).unwrap_or(0);

        let mut cmd = Command::new("patch");
        cmd.arg(format!("-p{}", strip));
        if let Some(dir) = workdir {
            cmd.current_dir(dir);
        }
        cmd.arg("--batch");
        cmd.arg("--forward");
        cmd.arg("-");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| LLMError::ProviderError(format!("spawn failed: {}", e)))?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(patch.as_bytes())
                .await
                .map_err(|e| LLMError::ProviderError(format!("stdin failed: {}", e)))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| LLMError::ProviderError(format!("patch failed: {}", e)))?;

        let result = json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        });
        serde_json::to_string(&result)
            .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))
    }
}
