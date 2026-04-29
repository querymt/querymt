use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};

use crate::anchors::edit::{
    AnchorEditOperation, AnchorEditRequest, BatchAnchorEditResult, apply_anchor_edit,
};
use crate::tools::builtins::anchored_edit_output::{build_file_output, format_output};
use crate::tools::builtins::helpers::display_path;
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

pub struct EditTool;

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Edit a text file using anchors returned by read_tool or prior successful edit/multiedit output. Anchors MUST include the § delimiter and the line text (e.g. 'xK7mQ2§fn main()'). The line text is validated against the file to prevent stale edits. Successful output includes fresh anchor-delimited hunks for the edited region; use those anchors for follow-up edits. Do not call read_tool only to verify a successful edit or reacquire anchors for shown regions. Re-read the file only if you need additional context not shown, output was truncated, an anchor is stale, or the file may have changed externally."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "filePath": {
                            "type": "string",
                            "description": "Path to the file to modify, relative to the workspace root or absolute."
                        },
                        "operation": {
                            "type": "string",
                            "enum": ["replace", "insert_before", "insert_after", "delete"],
                            "description": "Anchor edit operation to apply."
                        },
                        "startAnchor": {
                            "type": "string",
                            "description": "Anchor for the first affected line, or the insertion reference line. Must include § delimiter and line text (e.g. 'xK7mQ2§fn main()')."
                        },
                        "endAnchor": {
                            "type": "string",
                            "description": "Optional inclusive end anchor for replace/delete. Defaults to startAnchor. Must include § delimiter and line text."
                        },
                        "newText": {
                            "type": "string"
                        }
                    },
                    "required": ["filePath", "operation", "startAnchor"]
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
        let operation = args
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("operation is required".to_string()))
            .and_then(|value| {
                AnchorEditOperation::parse(value).map_err(ToolError::InvalidRequest)
            })?;
        let start_anchor = args
            .get("startAnchor")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("startAnchor is required".to_string()))?
            .to_string();
        let end_anchor = args
            .get("endAnchor")
            .and_then(Value::as_str)
            .map(str::to_string);
        let new_text = args
            .get("newText")
            .and_then(Value::as_str)
            .map(str::to_string);

        let file_path = context.resolve_path(file_path_str)?;
        let content = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to read file: {e}")))?;

        let request = AnchorEditRequest {
            operation,
            start_anchor,
            end_anchor,
            new_text,
        };
        let (new_content, result) =
            apply_anchor_edit(context.session_id(), &file_path, &content, request)
                .map_err(ToolError::ProviderError)?;

        tokio::fs::write(&file_path, &new_content)
            .await
            .map_err(|e| ToolError::ProviderError(format!("Failed to write file: {e}")))?;

        let batch_result = BatchAnchorEditResult {
            success: true,
            file: result.file.clone(),
            total_edits: 1,
            results: vec![result],
        };
        let file_output = build_file_output(display_path(&file_path, context.cwd()), &batch_result);

        Ok(vec![Content::text(format_output(&[file_output]))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use crate::tools::builtins::read_tool::ReadTool;
    use std::fs;
    use tempfile::TempDir;

    fn first_text_block(blocks: Vec<querymt::chat::Content>) -> String {
        blocks
            .into_iter()
            .find_map(|b| match b {
                querymt::chat::Content::Text { text } => Some(text),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn anchor_for_line(text: &str, expected_line: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_suffix(expected_line))
            .and_then(|prefix| prefix.strip_suffix('§'))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("missing anchored line for {expected_line:?} in {text}"))
    }

    fn anchor_for_compact_line(text: &str, expected_line: &str) -> String {
        text.lines()
            .filter_map(|line| line.strip_prefix('+').or_else(|| line.strip_prefix(' ')))
            .find_map(|line| line.strip_suffix(expected_line))
            .and_then(|prefix| prefix.strip_suffix('§'))
            .map(str::to_string)
            .unwrap_or_else(|| {
                panic!("missing compact anchored line for {expected_line:?} in {text}")
            })
    }

    async fn read_anchor(
        context: &AgentToolContext,
        file_path: &std::path::Path,
        line: &str,
    ) -> String {
        let read_tool = ReadTool::new();
        let text = first_text_block(
            read_tool
                .call(json!({ "path": file_path.display().to_string() }), context)
                .await
                .unwrap(),
        );
        anchor_for_line(&text, line)
    }

    #[tokio::test]
    async fn replace_single_line_by_anchor() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "hello\nold\nbye\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let anchor = read_anchor(&context, &file_path, "old").await;

        let result = first_text_block(
            EditTool::new()
                .call(
                    json!({
                        "filePath": file_path.display().to_string(),
                        "operation": "replace",
                        "startAnchor": anchor,
                        "newText": "new"
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        assert!(result.starts_with("OK paths=1 edits=1"));
        assert!(result.contains("P test.txt"));
        assert!(result.contains("H replace"));
        assert!(result.contains("+"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello\nnew\nbye\n");
    }

    #[tokio::test]
    async fn follow_up_edit_can_use_anchor_from_compact_output_without_reread() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let b_anchor = read_anchor(&context, &file_path, "b").await;

        let first_output = first_text_block(
            EditTool::new()
                .call(
                    json!({
                        "filePath": file_path.display().to_string(),
                        "operation": "replace",
                        "startAnchor": b_anchor,
                        "newText": "B"
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        let fresh_anchor = anchor_for_compact_line(&first_output, "B");
        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "insert_after",
                    "startAnchor": fresh_anchor,
                    "newText": "bb"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nB\nbb\nc\n");
    }

    #[tokio::test]
    async fn replace_multi_line_by_anchor_range() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\nd\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let start = read_anchor(&context, &file_path, "b").await;
        let end = read_anchor(&context, &file_path, "c").await;

        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "replace",
                    "startAnchor": start,
                    "endAnchor": end,
                    "newText": "x\ny"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nx\ny\nd\n");
    }

    #[tokio::test]
    async fn insert_before_and_after_by_anchor() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nc\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let c_anchor = read_anchor(&context, &file_path, "c").await;

        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "insert_before",
                    "startAnchor": c_anchor,
                    "newText": "b"
                }),
                &context,
            )
            .await
            .unwrap();

        let b_anchor = read_anchor(&context, &file_path, "b").await;
        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "insert_after",
                    "startAnchor": b_anchor,
                    "newText": "bb"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nb\nbb\nc\n");
    }

    #[tokio::test]
    async fn delete_anchor_range() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\nd\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let start = read_anchor(&context, &file_path, "b").await;
        let end = read_anchor(&context, &file_path, "c").await;

        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "delete",
                    "startAnchor": start,
                    "endAnchor": end
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nd\n");
    }

    #[tokio::test]
    async fn missing_anchor_returns_actionable_error() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let error = EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "delete",
                    "startAnchor": "Missing"
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{error}").contains("missing or stale"));
    }

    #[tokio::test]
    async fn preserves_crlf() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\r\nb\r\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let anchor = read_anchor(&context, &file_path, "b").await;

        EditTool::new()
            .call(
                json!({
                    "filePath": file_path.display().to_string(),
                    "operation": "insert_after",
                    "startAnchor": anchor,
                    "newText": "c"
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\r\nb\r\nc\r\n");
    }
}
