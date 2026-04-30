//! Multiedit tool for applying multiple edits to a single file

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde::Deserialize;
use serde_json::{Value, json};

use super::edit::EditTool;
use crate::tools::builtins::edit_output;
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

#[derive(Deserialize)]
struct EditOperation {
    #[serde(rename = "oldString")]
    old_string: String,
    #[serde(rename = "newString")]
    new_string: String,
    #[serde(rename = "replaceAll")]
    #[serde(default)]
    replace_all: bool,
}

/// Multiedit tool for applying multiple edits sequentially
pub struct MultiEditTool;

impl MultiEditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MultiEditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multiedit"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Apply multiple sequential edits to a single file. Each edit is applied in order, so later edits see the results of earlier ones. Returns status for each edit operation."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "The absolute path to the file to modify"
                        },
                        "edits": {
                            "type": "array",
                            "description": "Array of edit operations to apply sequentially",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldString": {
                                        "type": "string",
                                        "description": "The text to replace"
                                    },
                                    "newString": {
                                        "type": "string",
                                        "description": "The text to replace it with"
                                    },
                                    "replaceAll": {
                                        "type": "boolean",
                                        "description": "Replace all occurrences (default false)",
                                        "default": false
                                    }
                                },
                                "required": ["oldString", "newString"]
                            }
                        }
                    },
                    "required": ["filePath", "edits"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        let file_path_str = args
            .get("filePath")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("filePath is required".to_string()))?;

        let edits_val = args
            .get("edits")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidRequest("edits array is required".to_string()))?;

        let edits: Vec<EditOperation> = edits_val
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid edit operation: {}", e)))?;

        let file_path = context.resolve_path(file_path_str)?;

        // Read initial content
        let original_content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {}", e)))?;
        let mut content = original_content.clone();

        // Apply edits sequentially
        for (index, edit) in edits.iter().enumerate() {
            match EditTool::replace(
                &content,
                &edit.old_string,
                &edit.new_string,
                edit.replace_all,
            ) {
                Ok(new_content) => {
                    content = new_content;
                }
                Err(e) => {
                    // Stop on first error
                    return Err(ToolError::ProviderError(format!(
                        "Edit {} failed: {}",
                        index, e
                    )));
                }
            }
        }

        // Write final content
        tokio::fs::write(&file_path, &content)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to write file: {}", e)))?;

        // Build compact line-numbered hunk output from the actual file diff so
        // sequential edits produce precise hunks instead of whole-file replacements.
        let file_output =
            edit_output::build_file_output_from_diff(&file_path, &original_content, &content);
        let output_text = edit_output::format_output(&[file_output]);
        Ok(vec![Content::text(output_text)])
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
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_multiedit() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello world\nrust is good\nrust is fast").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": [
                {
                    "oldString": "hello",
                    "newString": "hi"
                },
                {
                    "oldString": "rust",
                    "newString": "Rust",
                    "replaceAll": true
                }
            ]
        });

        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert!(
            result.contains("OK paths="),
            "expected compact output, got: {}",
            result
        );
        assert!(result.contains("| hi world") || result.contains("| hello world"));

        let new_content = fs::read_to_string(&file_path).unwrap();
        assert!(new_content.contains("hi world"));
        assert!(new_content.contains("Rust is good"));
        assert!(new_content.contains("Rust is fast"));
    }

    #[tokio::test]
    async fn test_multiedit_distant_changes_produce_precise_hunks() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": [
                {
                    "oldString": "b",
                    "newString": "B"
                },
                {
                    "oldString": "j",
                    "newString": "J"
                }
            ]
        });

        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert!(
            result.contains("added=2 deleted=2"),
            "unexpected output: {}",
            result
        );
        assert_eq!(
            result.matches("\nH replace ").count(),
            2,
            "unexpected output: {}",
            result
        );
    }
}
