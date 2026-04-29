//! Anchor-based multiedit tool for applying multiple edits across one or more files.
//!
//! All anchors are resolved against original file contents before any file is
//! written. If any edit fails validation, nothing is written. Returns compact
//! line-oriented output with anchored change hunks for LLM follow-up edits.

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::anchors::edit::{AnchorEditRequest, apply_anchor_edits};
use crate::tools::builtins::anchored_edit_output::{
    FileEditOutput, build_file_output, format_output,
};
use crate::tools::builtins::helpers::{display_path, resolve_root};
use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

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
                description: "Apply multiple anchor-based edits across one or more files in a single atomic call. All anchors are validated before any file is written; if any edit fails, nothing is written. Anchors MUST include the § delimiter and line text (e.g. 'xK7mQ2§fn main()'). The line text is validated against the file to prevent stale edits. Successful output includes fresh anchor-delimited hunks for the edited regions; use those anchors for follow-up edits. Do not call read_tool only to verify a successful edit or reacquire anchors for shown regions. Re-read the file only if you need additional context not shown, output was truncated, an anchor is stale, or the file may have changed externally."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "description": "Array of file edit groups. Provide one entry per file, even for a single-file edit.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {
                                        "type": "string",
                                        "description": "Path to the file to modify."
                                    },
                                    "edits": {
                                        "type": "array",
                                        "description": "Anchor edit operations for this file.",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "operation": {
                                                    "type": "string",
                                                    "enum": ["replace", "insert_before", "insert_after", "delete"]
                                                },
                                                "startAnchor": { "type": "string", "description": "Anchor with § delimiter and line text." },
                                                "endAnchor": { "type": "string", "description": "Optional inclusive end anchor." },
                                                "newText": { "type": "string" }
                                            },
                                            "required": ["operation", "startAnchor"]
                                        }
                                    }
                                },
                                "required": ["path", "edits"]
                            }
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory.",
                            "default": "."
                        }
                    },
                    "required": ["paths"]
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
        let _ = resolve_root(&args, context)?; // validates root arg exists
        let file_groups = parse_file_groups(&args)?;

        // Phase 1: Read all files.
        let mut file_contents: HashMap<PathBuf, String> = HashMap::new();
        for group in &file_groups {
            let file_path = context.resolve_path(&group.path)?;
            if file_contents.contains_key(&file_path) {
                return Err(ToolError::InvalidRequest(format!(
                    "Duplicate file path: {}",
                    file_path.display()
                )));
            }
            if group.edits.is_empty() {
                return Err(ToolError::InvalidRequest(format!(
                    "edits array must not be empty for file {}",
                    file_path.display()
                )));
            }
            let content = tokio::fs::read_to_string(&file_path).await.map_err(|e| {
                ToolError::ProviderError(format!("Failed to read {}: {e}", file_path.display()))
            })?;
            file_contents.insert(file_path.clone(), content);
        }

        // Phase 2: Apply edits per file in memory. Any failure = no writes.
        let mut new_contents: HashMap<PathBuf, String> = HashMap::new();
        let mut outputs: Vec<FileEditOutput> = Vec::new();

        for group in &file_groups {
            let file_path = context.resolve_path(&group.path)?;
            let content = &file_contents[&file_path];

            let (new_content, batch_result) = apply_anchor_edits(
                context.session_id(),
                &file_path,
                content,
                group.edits.clone(),
            )
            .map_err(|e| {
                ToolError::ProviderError(format!(
                    "No changes written (validation failed for {}): {e}",
                    file_path.display()
                ))
            })?;

            outputs.push(build_file_output(
                display_path(&file_path, context.cwd()),
                &batch_result,
            ));
            new_contents.insert(file_path.clone(), new_content);
        }

        // Phase 3: Write all files.
        for (file_path, new_content) in &new_contents {
            tokio::fs::write(file_path, new_content)
                .await
                .map_err(|e| {
                    ToolError::ProviderError(format!(
                        "Failed to write {}: {e}",
                        file_path.display()
                    ))
                })?;
        }

        // Phase 4: Format canonical contextual compact output.
        Ok(vec![Content::text(format_output(&outputs))])
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct FileGroup {
    path: String,
    edits: Vec<AnchorEditRequest>,
}

fn parse_file_groups(args: &Value) -> Result<Vec<FileGroup>, ToolError> {
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidRequest("paths array is required".to_string()))?;

    if paths.is_empty() {
        return Err(ToolError::InvalidRequest(
            "paths must include at least one file group".to_string(),
        ));
    }

    paths
        .iter()
        .map(|group| {
            let path = group.get("path").and_then(Value::as_str).ok_or_else(|| {
                ToolError::InvalidRequest("path is required in each group".to_string())
            })?;
            let edits_val = group
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ToolError::InvalidRequest("edits array is required in each group".to_string())
                })?;
            let edits = edits_val
                .iter()
                .map(parse_edit_request)
                .collect::<Result<Vec<_>, _>>()
                .map_err(ToolError::InvalidRequest)?;
            Ok(FileGroup {
                path: path.to_string(),
                edits,
            })
        })
        .collect()
}

fn parse_edit_request(value: &Value) -> Result<AnchorEditRequest, String> {
    let operation = value
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| "operation is required".to_string())
        .and_then(crate::anchors::edit::AnchorEditOperation::parse)?;
    let start_anchor = value
        .get("startAnchor")
        .and_then(Value::as_str)
        .ok_or_else(|| "startAnchor is required".to_string())?
        .to_string();
    let end_anchor = value
        .get("endAnchor")
        .and_then(Value::as_str)
        .map(str::to_string);
    let new_text = value
        .get("newText")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(AnchorEditRequest {
        operation,
        start_anchor,
        end_anchor,
        new_text,
    })
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

    #[test]
    fn schema_requires_paths_without_top_level_one_of() {
        let parameters = MultiEditTool::new().definition().function.parameters;

        assert_eq!(parameters.get("type"), Some(&json!("object")));
        assert_eq!(parameters.get("required"), Some(&json!(["paths"])));
        assert!(parameters.get("oneOf").is_none());
        assert!(parameters.get("path").is_none());
        assert!(parameters.get("edits").is_none());
    }

    #[tokio::test]
    async fn applies_multiple_anchor_edits() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let a = read_anchor(&context, &file_path, "a").await;
        let c = read_anchor(&context, &file_path, "c").await;

        let result = first_text_block(
            MultiEditTool::new()
                .call(
                    json!({
                        "paths": [
                            {
                                "path": file_path.display().to_string(),
                                "edits": [
                                    { "operation": "replace", "startAnchor": a, "newText": "A" },
                                    { "operation": "insert_before", "startAnchor": c, "newText": "bb" }
                                ]
                            }
                        ]
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        assert!(result.starts_with("OK paths=1 edits=2 added=2 deleted=1 anchors=fresh"));
        assert!(result.contains("P test.txt"));
        assert!(result.contains("H replace"));
        assert!(result.contains("H insert_before"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "A\nb\nbb\nc\n");
    }

    #[tokio::test]
    async fn canonical_paths_form() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "alpha\nbeta\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let anchor_a = read_anchor(&context, &file_path, "alpha").await;
        let anchor_b = read_anchor(&context, &file_path, "beta").await;

        let result = first_text_block(
            MultiEditTool::new()
                .call(
                    json!({
                        "paths": [
                            {
                                "path": file_path.display().to_string(),
                                "edits": [
                                    { "operation": "replace", "startAnchor": anchor_a, "newText": "ALPHA" },
                                    { "operation": "replace", "startAnchor": anchor_b, "newText": "BETA" }
                                ]
                            }
                        ]
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        assert!(result.starts_with("OK paths=1 edits=2"));
        assert!(result.contains("P test.txt"));
        assert!(result.contains("+"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "ALPHA\nBETA\n");
    }

    #[tokio::test]
    async fn multi_file_atomic_edits() {
        let temp_dir = TempDir::new().unwrap();
        let file_a = temp_dir.path().join("a.txt");
        let file_b = temp_dir.path().join("b.txt");
        fs::write(&file_a, "alpha\n").unwrap();
        fs::write(&file_b, "beta\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let anchor_a = read_anchor(&context, &file_a, "alpha").await;
        let anchor_b = read_anchor(&context, &file_b, "beta").await;

        let result = first_text_block(
            MultiEditTool::new()
                .call(
                    json!({
                        "paths": [
                            {
                                "path": file_a.display().to_string(),
                                "edits": [{ "operation": "replace", "startAnchor": anchor_a, "newText": "ALPHA" }]
                            },
                            {
                                "path": file_b.display().to_string(),
                                "edits": [{ "operation": "replace", "startAnchor": anchor_b, "newText": "BETA" }]
                            }
                        ]
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        assert!(result.starts_with("OK paths=2 edits=2"));
        assert!(result.contains("P a.txt"));
        assert!(result.contains("P b.txt"));
        assert_eq!(fs::read_to_string(&file_a).unwrap(), "ALPHA\n");
        assert_eq!(fs::read_to_string(&file_b).unwrap(), "BETA\n");
    }

    #[tokio::test]
    async fn resolves_all_anchors_against_original_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let a = read_anchor(&context, &file_path, "a").await;
        let b = read_anchor(&context, &file_path, "b").await;

        MultiEditTool::new()
            .call(
                json!({
                    "paths": [
                        {
                            "path": file_path.display().to_string(),
                            "edits": [
                                { "operation": "insert_before", "startAnchor": a, "newText": "top" },
                                { "operation": "replace", "startAnchor": b, "newText": "B" }
                            ]
                        }
                    ]
                }),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(&file_path).unwrap(), "top\na\nB\nc\n");
    }

    #[tokio::test]
    async fn rejects_overlapping_replace_delete_ranges() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\nd\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let b = read_anchor(&context, &file_path, "b").await;
        let c = read_anchor(&context, &file_path, "c").await;
        let d = read_anchor(&context, &file_path, "d").await;

        let error = MultiEditTool::new()
            .call(
                json!({
                    "paths": [
                        {
                            "path": file_path.display().to_string(),
                            "edits": [
                                { "operation": "replace", "startAnchor": b, "endAnchor": c, "newText": "x" },
                                { "operation": "delete", "startAnchor": c, "endAnchor": d }
                            ]
                        }
                    ]
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{error}").contains("Overlapping replace/delete ranges"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nb\nc\nd\n");
    }

    #[tokio::test]
    async fn invalid_edit_writes_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let error = MultiEditTool::new()
            .call(
                json!({
                    "paths": [
                        {
                            "path": file_path.display().to_string(),
                            "edits": [
                                { "operation": "replace", "startAnchor": "Missing", "newText": "x" }
                            ]
                        }
                    ]
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{error}").contains("No changes written"));
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "a\nb\n");
    }

    #[tokio::test]
    async fn second_file_failure_leaves_first_unchanged() {
        let temp_dir = TempDir::new().unwrap();
        let file_a = temp_dir.path().join("a.txt");
        let file_b = temp_dir.path().join("b.txt");
        fs::write(&file_a, "alpha\n").unwrap();
        fs::write(&file_b, "beta\n").unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let anchor_a = read_anchor(&context, &file_a, "alpha").await;

        let err = MultiEditTool::new()
            .call(
                json!({
                    "paths": [
                        {
                            "path": file_a.display().to_string(),
                            "edits": [{ "operation": "replace", "startAnchor": anchor_a, "newText": "ALPHA" }]
                        },
                        {
                            "path": file_b.display().to_string(),
                            "edits": [{ "operation": "replace", "startAnchor": "bad", "newText": "BETA" }]
                        }
                    ]
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("No changes written"));
        assert_eq!(fs::read_to_string(&file_a).unwrap(), "alpha\n");
        assert_eq!(fs::read_to_string(&file_b).unwrap(), "beta\n");
    }

    #[tokio::test]
    async fn nearby_edits_no_corruption() {
        // Regression test inspired by session 019dd89e where consecutive
        // nearby edits corrupted get_symbol.rs with duplicated function bodies.
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.rs");
        // Simulate a file with function signatures that look similar
        fs::write(
            &file_path,
            "fn foo() {\n    1\n}\nfn bar() {\n    2\n}\nfn baz() {\n    3\n}\n",
        )
        .unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let foo_body = read_anchor(&context, &file_path, "    1").await;
        let bar_body = read_anchor(&context, &file_path, "    2").await;
        let baz_sig = read_anchor(&context, &file_path, "fn baz() {").await;

        let output = first_text_block(
            MultiEditTool::new()
                .call(
                    json!({
                        "paths": [
                            {
                                "path": file_path.display().to_string(),
                                "edits": [
                                    { "operation": "replace", "startAnchor": foo_body, "newText": "    11" },
                                    { "operation": "replace", "startAnchor": bar_body, "newText": "    22" },
                                    { "operation": "insert_before", "startAnchor": baz_sig, "newText": "fn qux() { 4 }" }
                                ]
                            }
                        ]
                    }),
                    &context,
                )
                .await
                .unwrap(),
        );

        let result = fs::read_to_string(&file_path).unwrap();
        assert!(result.contains("fn foo()"));
        assert!(result.contains("    11"));
        assert!(result.contains("fn bar()"));
        assert!(result.contains("    22"));
        assert!(result.contains("fn qux() { 4 }"));
        assert!(result.contains("fn baz()"));
        assert!(result.contains("    3"));
        assert!(output.starts_with("OK paths=1 edits=3 added=3 deleted=2"));
        assert!(output.contains("P test.rs"));
        assert!(output.contains("H replace"));
        assert!(output.contains("H insert_before"));
        assert!(output.contains("+"));

        // Ensure no duplication
        assert_eq!(
            result.matches("fn foo()").count(),
            1,
            "fn foo should appear exactly once"
        );
        assert_eq!(
            result.matches("fn bar()").count(),
            1,
            "fn bar should appear exactly once"
        );
        assert_eq!(
            result.matches("fn baz()").count(),
            1,
            "fn baz should appear exactly once"
        );
    }
}
