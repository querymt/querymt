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
                description: "Apply multiple sequential edits to a single file. Later edits see earlier edits in memory. The operation is all-or-nothing: if any edit fails, no changes are written and the tool returns an error."
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

        if edits.is_empty() {
            return Err(ToolError::InvalidRequest(
                "edits array must not be empty".to_string(),
            ));
        }

        for (index, edit) in edits.iter().enumerate() {
            if edit.old_string.is_empty() {
                return Err(ToolError::InvalidRequest(format!(
                    "edit {} has empty oldString",
                    index + 1
                )));
            }
            if edit.old_string == edit.new_string {
                return Err(ToolError::InvalidRequest(format!(
                    "edit {} has identical oldString and newString",
                    index + 1
                )));
            }
        }

        let file_path = context.resolve_path(file_path_str)?;

        // Apply all edits in memory first so the file is only written once.
        let original_content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {}", e)))?;
        let mut content = original_content.clone();

        for (index, edit) in edits.iter().enumerate() {
            content = EditTool::replace(
                &content,
                &edit.old_string,
                &edit.new_string,
                edit.replace_all,
            )
            .map_err(|e| {
                ToolError::ProviderError(format!(
                    "multiedit failed at edit {} of {}: {}; no changes written",
                    index + 1,
                    edits.len(),
                    e
                ))
            })?;
        }

        // Write final content
        tokio::fs::write(&file_path, &content)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to write file: {}", e)))?;

        // Build compact line-numbered hunk output from the actual file diff so
        // sequential edits produce precise hunks instead of whole-file replacements.
        let file_output =
            edit_output::build_file_output_from_diff(&file_path, &original_content, &content);
        let output_text = edit_output::format_compact_receipt(&[file_output]);
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
        assert!(
            !result.contains("| "),
            "compact receipt should not contain diff lines, got: {}",
            result
        );

        let new_content = fs::read_to_string(&file_path).unwrap();
        assert!(new_content.contains("hi world"));
        assert!(new_content.contains("Rust is good"));
        assert!(new_content.contains("Rust is fast"));
    }

    #[tokio::test]
    async fn test_multiedit_failure_writes_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let original = "hello world\nrust is good\nrust is fast";
        fs::write(&file_path, original).unwrap();

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
                    "oldString": "missing",
                    "newString": "x"
                }
            ]
        });

        let err = tool.call(args, &context).await.unwrap_err().to_string();
        assert!(err.contains("multiedit failed at edit 2 of 2"));
        assert!(err.contains("no changes written"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), original);
    }

    #[tokio::test]
    async fn test_multiedit_later_edit_sees_earlier_edit() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "alpha").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": [
                {
                    "oldString": "alpha",
                    "newString": "beta"
                },
                {
                    "oldString": "beta",
                    "newString": "gamma"
                }
            ]
        });

        tool.call(args, &context).await.unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "gamma");
    }

    #[tokio::test]
    async fn test_multiedit_empty_edits_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": []
        });

        let err = tool.call(args, &context).await.unwrap_err().to_string();
        assert!(err.contains("edits array must not be empty"));
    }

    #[tokio::test]
    async fn test_multiedit_empty_old_string_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": [
                {
                    "oldString": "",
                    "newString": "x"
                }
            ]
        });

        let err = tool.call(args, &context).await.unwrap_err().to_string();
        assert!(err.contains("edit 1 has empty oldString"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_multiedit_identical_old_and_new_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MultiEditTool::new();

        let args = json!({
            "filePath": file_path.display().to_string(),
            "edits": [
                {
                    "oldString": "hello",
                    "newString": "hello"
                }
            ]
        });

        let err = tool.call(args, &context).await.unwrap_err().to_string();
        assert!(err.contains("edit 1 has identical oldString and newString"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello");
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
