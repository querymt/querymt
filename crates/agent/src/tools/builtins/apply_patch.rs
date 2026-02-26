//! Apply patch tool implementation using ToolContext

use async_trait::async_trait;
use patchkit::ContentPatch;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};
use std::fs;

use super::patch_utils;
use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct ApplyPatchTool;

impl Default for ApplyPatchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplyPatchTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Apply a unified diff patch to files. Uses pure Rust implementation for cross-platform compatibility."
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
                        },
                        "skip_validation": {
                            "type": "boolean",
                            "description": "Skip patch validation (not recommended). Use only if you're certain the patch is correct.",
                            "default": false
                        }
                    },
                    "required": ["patch"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        if context.is_read_only() {
            return Err(ToolError::PermissionDenied(
                "Session is in read-only mode — applying patches is not allowed".to_string(),
            ));
        }

        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("patch is required".to_string()))?
            .to_string();

        let workdir = args
            .get("workdir")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?
            .or_else(|| context.cwd().map(|p| p.to_path_buf()))
            .ok_or_else(|| ToolError::InvalidRequest("No working directory available".into()))?;

        let strip = args.get("strip").and_then(Value::as_u64).unwrap_or(0) as usize;
        let skip_validation = args
            .get("skip_validation")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        tokio::task::spawn_blocking(move || {
            if !skip_validation {
                patch_utils::parse_patches(&patch)
                    .map_err(|e| ToolError::InvalidRequest(format!("Invalid patch: {}", e)))?;
            }

            let patches = patch_utils::parse_patches(&patch)
                .map_err(|e| ToolError::InvalidRequest(format!("Failed to parse patch: {}", e)))?;

            let mut files_modified = Vec::new();

            for parsed in patches {
                let file_path = Self::apply_single_patch(&parsed, Some(&workdir), strip)?;
                files_modified.push(file_path);
            }

            Ok(json!({
                "success": true,
                "files": files_modified,
            })
            .to_string())
        })
        .await
        .map_err(|e| ToolError::ProviderError(format!("patch task failed: {}", e)))?
    }
}

impl ApplyPatchTool {
    fn apply_single_patch(
        parsed: &patch_utils::ParsedPatch,
        workdir: Option<&std::path::Path>,
        strip: usize,
    ) -> Result<String, ToolError> {
        let file_path = patch_utils::resolve_file_path(
            &parsed.file_path,
            workdir.and_then(|p| p.to_str()),
            strip,
        )
        .map_err(ToolError::InvalidRequest)?;

        let original = fs::read(&file_path).map_err(|e| {
            ToolError::InvalidRequest(format!("Cannot read '{}': {}", file_path.display(), e))
        })?;

        let modified = parsed.patch.apply_exact(&original).map_err(|e| {
            ToolError::InvalidRequest(patch_utils::format_apply_error(e, &file_path))
        })?;

        fs::write(&file_path, &modified).map_err(|e| {
            ToolError::ProviderError(format!("Cannot write '{}': {}", file_path.display(), e))
        })?;

        Ok(file_path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_file(dir: &TempDir, filename: &str, content: &str) -> std::path::PathBuf {
        let file_path = dir.path().join(filename);
        let mut file = fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    #[test]
    fn test_tool_name() {
        let tool = ApplyPatchTool::new();
        assert_eq!(tool.name(), "apply_patch");
    }

    #[test]
    fn test_tool_definition() {
        let tool = ApplyPatchTool::new();
        let definition = tool.definition();

        assert_eq!(definition.tool_type, "function");
        assert_eq!(definition.function.name, "apply_patch");
        assert!(definition.function.description.contains("patch"));

        // Check required parameters
        let params = &definition.function.parameters;
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["patch"].is_object());
        assert!(params["properties"]["workdir"].is_object());
        assert!(params["properties"]["strip"].is_object());
        assert!(params["properties"]["skip_validation"].is_object());
        assert_eq!(params["required"][0], "patch");
    }

    #[tokio::test]
    async fn test_call_missing_patch_parameter() {
        let tool = ApplyPatchTool::new();
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let args = json!({});

        let result = tool.call(args, &context).await;
        assert!(result.is_err());

        if let Err(ToolError::InvalidRequest(msg)) = result {
            assert!(msg.contains("patch is required"));
        } else {
            panic!("Expected InvalidRequest error");
        }
    }

    #[tokio::test]
    async fn test_call_validation_failure() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_content = "line 1\nline 2\nline 3\n";
        create_test_file(&temp_dir, "test.txt", file_content);

        let tool = ApplyPatchTool::new();

        // Patch with incorrect context
        let patch = "--- test.txt\n+++ test.txt\n@@ -1,3 +1,3 @@\n line 1\n-wrong line\n+line 2 modified\n line 3\n";

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap()
        });

        let result = tool.call(args, &context).await;
        assert!(
            result.is_err(),
            "Patch application should have failed due to incorrect context"
        );

        if let Err(ToolError::InvalidRequest(msg)) = result {
            assert!(msg.contains("Patch cannot be applied") || msg.contains("Conflict"));
        } else {
            panic!(
                "Expected InvalidRequest error for validation failure, got {:?}",
                result
            );
        }
    }

    #[tokio::test]
    async fn test_call_successful_patch_application() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let original_content = "line 1\nline 2\nline 3\n";
        let file_path = create_test_file(&temp_dir, "test.txt", original_content);

        let tool = ApplyPatchTool::new();

        let patch = "--- test.txt\n+++ test.txt\n@@ -1,3 +1,3 @@\n line 1\n-line 2\n+line 2 modified\n line 3\n";

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap()
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Patch application failed: {:?}", result);

        let output = result.unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();

        // Check response format
        assert_eq!(parsed["success"], true);
        assert!(parsed["files"].is_array());
        assert_eq!(parsed["files"].as_array().unwrap().len(), 1);

        // Verify file was actually modified
        let modified_content = fs::read_to_string(&file_path).unwrap();
        assert!(modified_content.contains("line 2 modified"));
        assert!(!modified_content.contains("line 2\n"));
    }

    #[tokio::test]
    async fn test_call_skip_validation() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let file_content = "line 1\nline 2\nline 3\n";
        create_test_file(&temp_dir, "test.txt", file_content);

        let tool = ApplyPatchTool::new();

        // Patch with incorrect context, but skip_validation enabled
        let patch = "--- test.txt\n+++ test.txt\n@@ -1,3 +1,3 @@\n line 1\n-wrong line\n+line 2 modified\n line 3\n";

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap(),
            "skip_validation": true
        });

        let result = tool.call(args, &context).await;
        // Should still fail during apply_exact, not during validation
        assert!(
            result.is_err(),
            "Patch application should have failed even with skip_validation"
        );
    }

    #[tokio::test]
    async fn test_call_with_strip_parameter() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let original_content = "line 1\nline 2\n";
        create_test_file(&temp_dir, "test.txt", original_content);

        let tool = ApplyPatchTool::new();

        // Patch with path prefix that needs stripping
        let patch = r#"--- a/test.txt
+++ b/test.txt
@@ -1,2 +1,2 @@
-line 1
-+line 1 modified
 line 2
"#;

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap(),
            "strip": 1
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Patch with strip failed: {:?}", result);
    }

    #[tokio::test]
    async fn test_call_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        create_test_file(&temp_dir, "file1.txt", "content 1\n");
        create_test_file(&temp_dir, "file2.txt", "content 2\n");

        let tool = ApplyPatchTool::new();

        let patch = "--- file1.txt\n+++ file1.txt\n@@ -1,1 +1,1 @@\n-content 1\n+content 1 modified\n--- file2.txt\n+++ file2.txt\n@@ -1,1 +1,1 @@\n-content 2\n+content 2 modified\n";

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap()
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "Multi-file patch failed: {:?}", result);

        let output = result.unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();

        // Should have modified 2 files
        assert_eq!(parsed["files"].as_array().unwrap().len(), 2);

        // Verify both files were modified
        let content1 = fs::read_to_string(temp_dir.path().join("file1.txt")).unwrap();
        let content2 = fs::read_to_string(temp_dir.path().join("file2.txt")).unwrap();

        assert!(content1.contains("content 1 modified"));
        assert!(content2.contains("content 2 modified"));
    }

    #[tokio::test]
    async fn test_call_patch_file_not_found_validation() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        // Don't create the file

        let tool = ApplyPatchTool::new();

        let patch = r#"--- nonexistent.txt
+++ nonexistent.txt
@@ -1,1 +1,1 @@
-line 1
-+line 1 modified
"#;

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap()
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_err());

        if let Err(ToolError::InvalidRequest(msg)) = result {
            assert!(msg.contains("Cannot read") || msg.contains("does not exist"));
        } else {
            panic!(
                "Expected InvalidRequest error for missing file, got {:?}",
                result
            );
        }
    }

    #[tokio::test]
    async fn test_call_with_crlf_line_endings() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        create_test_file(&temp_dir, "test.txt", "line 1\nline 2\n");

        let tool = ApplyPatchTool::new();

        // Patch with CRLF line endings (Windows style)
        let patch = "--- test.txt\r\n+++ test.txt\r\n@@ -1,2 +1,2 @@\r\n-line 1\r\n+line 1 modified\r\n line 2\r\n";

        let args = json!({
            "patch": patch,
            "workdir": temp_dir.path().to_str().unwrap()
        });

        let result = tool.call(args, &context).await;
        assert!(result.is_ok(), "CRLF patch failed: {:?}", result);
    }
}
