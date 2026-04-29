//! Read tool implementation using ToolContext

use async_trait::async_trait;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

use super::read_shared::{DEFAULT_READ_LIMIT, ReadRange, render_read_output};

pub struct ReadTool;

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ReadTool {
    fn name(&self) -> &str {
        "read_tool"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Read a file or directory under the workspace. For text files, returns XML-like output with <path>, <type>, one <range>, and <content> containing anchor-delimited lines like A00001§content. Supports numeric offset/limit and anchor range queries. For image files (PNG, JPEG, GIF, WebP), returns the image content directly. For other binary files, returns a descriptive error. Directories support non-recursive offset/limit pagination."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the target to read, relative to the workspace root or absolute."
                        },
                        "root": {
                            "type": "string",
                            "description": "Workspace root directory to resolve relative paths against.",
                            "default": "."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "0-based pagination offset. For files, this is a line offset. For directories, this is an entry offset.",
                            "default": 0,
                            "minimum": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines (files) or entries (directories) to return. Defaults to 2000.",
                            "default": 2000,
                            "minimum": 1
                        },
                        "start_anchor": {
                            "type": "string",
                            "description": "Anchor to start from for text-file reads. If provided, offset is ignored."
                        },
                        "end_anchor": {
                            "type": "string",
                            "description": "Anchor to read through, inclusive, for text-file reads. Requires start_anchor."
                        },
                        "before": {
                            "type": "integer",
                            "description": "Number of lines before start_anchor to include.",
                            "default": 0,
                            "minimum": 0
                        },
                        "after": {
                            "type": "integer",
                            "description": "Number of lines after start_anchor to include when end_anchor is absent.",
                            "minimum": 0
                        }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: Use offset/limit to page through large files or directories, and use search_text when you only need specific content.",
        )
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;

        let root = args
            .get("root")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?
            .or_else(|| context.cwd().map(|p| p.to_path_buf()))
            .ok_or_else(|| ToolError::InvalidRequest("No working directory available".into()))?;

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_LIMIT as u64) as usize;
        let before = args.get("before").and_then(Value::as_u64).unwrap_or(0) as usize;
        let after = args
            .get("after")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let start_anchor = args
            .get("start_anchor")
            .and_then(Value::as_str)
            .map(str::to_string);
        let end_anchor = args
            .get("end_anchor")
            .and_then(Value::as_str)
            .map(str::to_string);

        if limit == 0 {
            return Err(ToolError::InvalidRequest("limit must be >= 1".to_string()));
        }
        if end_anchor.is_some() && start_anchor.is_none() {
            return Err(ToolError::InvalidRequest(
                "end_anchor requires start_anchor".to_string(),
            ));
        }

        let path = context.resolve_path(path)?;
        let target = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };

        let range = ReadRange {
            offset,
            limit,
            start_anchor,
            end_anchor,
            before,
            after,
        };

        render_read_output(context.session_id(), &target, range)
            .await
            .map_err(ToolError::ProviderError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use querymt::chat::Content;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // ── helpers ──────────────────────────────────────────────────────────────

    async fn create_test_file(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let file_path = dir.path().join(name);
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    /// Extract the text from the first Content::Text block, panicking if absent.
    fn first_text(blocks: &[Content]) -> &str {
        for b in blocks {
            if let Content::Text { text } = b {
                return text.as_str();
            }
        }
        panic!("no Content::Text block found in result: {:?}", blocks);
    }

    fn anchor_for_line(text: &str, expected_line: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_suffix(expected_line))
            .and_then(|prefix| prefix.strip_suffix('§'))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("missing anchored line for {expected_line:?} in {text}"))
    }

    // ── existing text / directory tests (updated for Vec<Content>) ───────────

    #[tokio::test]
    async fn test_read_file_full() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3").await;

        let tool = ReadTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });

        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("<type>file</type>"));
        assert!(text.contains("<range start_offset=\"0\" end_offset=\"3\" total_lines=\"3\"/>"));
        assert!(text.contains("<content>"));
        assert!(text.contains("§line 1"));
        assert!(text.contains("§line 3"));
        assert!(!text.contains("00001| line 1"));
        assert!(text.contains("(End of file - total 3 lines)"));
    }

    #[tokio::test]
    async fn test_read_file_with_offset_limit() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path =
            create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3\nline 4").await;

        let tool = ReadTool::new();
        let args = json!({
            "path": file_path.to_str().unwrap(),
            "offset": 1,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("<range start_offset=\"1\" end_offset=\"3\" total_lines=\"4\"/>"));
        assert!(text.contains("§line 2"));
        assert!(text.contains("§line 3"));
        assert!(!text.contains("§line 1"));
        assert!(text.contains("Use 'offset' parameter to read beyond line 3"));
    }

    #[tokio::test]
    async fn test_read_file_with_anchor_after_range() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path =
            create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3\nline 4").await;

        let tool = ReadTool::new();
        let first = tool
            .call(json!({ "path": file_path.to_str().unwrap() }), &context)
            .await
            .unwrap();
        let line_2_anchor = anchor_for_line(first_text(&first), "line 2");

        let result = tool
            .call(
                json!({
                    "path": file_path.to_str().unwrap(),
                    "start_anchor": line_2_anchor,
                    "before": 1,
                    "after": 1
                }),
                &context,
            )
            .await
            .unwrap();
        let text = first_text(&result);

        assert!(text.contains("<range start_offset=\"0\" end_offset=\"3\" total_lines=\"4\"/>"));
        assert!(text.contains("§line 1"));
        assert!(text.contains("§line 2"));
        assert!(text.contains("§line 3"));
        assert!(!text.contains("§line 4"));
    }

    #[tokio::test]
    async fn test_read_file_with_anchor_end_range() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path =
            create_test_file(&temp_dir, "test.txt", "line 1\nline 2\nline 3\nline 4").await;

        let tool = ReadTool::new();
        let first = tool
            .call(json!({ "path": file_path.to_str().unwrap() }), &context)
            .await
            .unwrap();
        let text = first_text(&first);
        let line_2_anchor = anchor_for_line(text, "line 2");
        let line_4_anchor = anchor_for_line(text, "line 4");

        let result = tool
            .call(
                json!({
                    "path": file_path.to_str().unwrap(),
                    "start_anchor": line_2_anchor,
                    "end_anchor": line_4_anchor
                }),
                &context,
            )
            .await
            .unwrap();
        let text = first_text(&result);

        assert!(text.contains("<range start_offset=\"1\" end_offset=\"4\" total_lines=\"4\"/>"));
        assert!(!text.contains("§line 1"));
        assert!(text.contains("§line 2"));
        assert!(text.contains("§line 3"));
        assert!(text.contains("§line 4"));
    }

    #[tokio::test]
    async fn test_read_file_missing_anchor_errors() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_path = create_test_file(&temp_dir, "test.txt", "line 1\nline 2").await;

        let tool = ReadTool::new();
        let error = tool
            .call(
                json!({
                    "path": file_path.to_str().unwrap(),
                    "start_anchor": "Missing"
                }),
                &context,
            )
            .await
            .unwrap_err();

        assert!(format!("{error}").contains("missing or stale"));
    }

    #[tokio::test]
    async fn test_read_directory_non_recursive_with_pagination() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
        fs::create_dir(temp_dir.path().join("src")).unwrap();
        fs::write(temp_dir.path().join("src").join("nested.txt"), "nested").unwrap();

        let tool = ReadTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "offset": 1,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("<type>directory</type>"));
        assert!(text.contains("<entries>"));
        assert!(!text.contains("nested.txt"));
        assert!(text.contains("(2 entries)"));
    }

    #[tokio::test]
    async fn test_read_directory_truncation_hint() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
        fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
        fs::write(temp_dir.path().join("c.txt"), "c").unwrap();

        let tool = ReadTool::new();
        let args = json!({
            "path": temp_dir.path().to_str().unwrap(),
            "offset": 0,
            "limit": 2
        });

        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(text.contains("(2 entries)"));
        assert!(text.contains("(More entries available. Use a higher offset.)"));
    }

    // ── new image / unsupported-binary tests ─────────────────────────────────

    /// Minimal valid 1×1 red PNG (67 bytes).
    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR length + type
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1×1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, // 8-bit RGB, CRC
        0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, // IDAT length + type
        0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, // IDAT data
        0x00, 0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC, // IDAT CRC
        0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, // IEND length + type
        0x44, 0xAE, 0x42, 0x60, 0x82, // IEND CRC
    ];

    #[tokio::test]
    async fn test_read_png_returns_image_content() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        let file_path = temp_dir.path().join("image.png");
        fs::write(&file_path, MINIMAL_PNG).unwrap();

        let tool = ReadTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });

        let result = tool.call(args, &context).await.unwrap();

        assert_eq!(result.len(), 1, "expected exactly one content block");
        match &result[0] {
            Content::Image { mime_type, data } => {
                assert_eq!(mime_type, "image/png");
                assert_eq!(data, MINIMAL_PNG);
            }
            other => panic!("expected Content::Image, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_read_jpeg_returns_image_content() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Minimal JPEG: SOI marker + EOI marker
        let jpeg_bytes: &[u8] = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        let file_path = temp_dir.path().join("image.jpg");
        fs::write(&file_path, jpeg_bytes).unwrap();

        let tool = ReadTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });

        let result = tool.call(args, &context).await.unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            Content::Image { mime_type, .. } => assert_eq!(mime_type, "image/jpeg"),
            other => panic!("expected Content::Image, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_read_unsupported_binary_returns_error_text() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));

        // Random binary bytes that are not a valid image.
        let binary: &[u8] = &[0x00, 0x01, 0x02, 0x03, 0xFF, 0xFE, 0xFD, 0xFC];
        let file_path = temp_dir.path().join("random.bin");
        fs::write(&file_path, binary).unwrap();

        let tool = ReadTool::new();
        let args = json!({ "path": file_path.to_str().unwrap() });

        let result = tool.call(args, &context).await.unwrap();
        let text = first_text(&result);

        assert!(
            text.contains("Binary file"),
            "expected a 'Binary file' message, got: {}",
            text
        );
    }
}
