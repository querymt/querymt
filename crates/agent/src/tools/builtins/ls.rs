//! List directory contents tool

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

pub struct ListTool;

impl ListTool {
    pub fn new() -> Self {
        Self
    }

    /// Default ignore patterns (matching opencode)
    fn default_ignores() -> Vec<&'static str> {
        vec![
            "node_modules/**",
            ".git/**",
            "dist/**",
            "build/**",
            "out/**",
            "target/**",
            ".next/**",
            ".nuxt/**",
            "vendor/**",
            "__pycache__/**",
            "*.pyc",
            ".venv/**",
            "venv/**",
            "coverage/**",
            ".cache/**",
            "tmp/**",
            "temp/**",
        ]
    }

    /// List directory contents recursively
    fn list_directory(
        root: &Path,
        ignore_patterns: Vec<&str>,
        limit: usize,
    ) -> Result<(Vec<PathBuf>, bool), ToolError> {
        use glob::Pattern;
        use ignore::WalkBuilder;

        // Parse ignore patterns
        let ignore_pats: Result<Vec<Pattern>, _> =
            ignore_patterns.iter().map(|p| Pattern::new(p)).collect();
        let ignore_pats = ignore_pats
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid ignore pattern: {}", e)))?;

        let mut entries = Vec::new();

        let walker = WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .build();

        for result in walker {
            if entries.len() >= limit {
                break;
            }

            let entry = result
                .map_err(|e| ToolError::ProviderError(format!("Error walking directory: {}", e)))?;

            let path = entry.path();

            // Apply ignore patterns
            if let Ok(relative) = path.strip_prefix(root) {
                let should_ignore = ignore_pats.iter().any(|pat| pat.matches_path(relative));
                if should_ignore {
                    continue;
                }
            }

            entries.push(path.to_path_buf());
        }

        let truncated = entries.len() >= limit;

        Ok((entries, truncated))
    }

    /// Format entries as a simple indented tree
    fn format_tree(entries: Vec<PathBuf>, root: &Path, truncated: bool) -> String {
        if entries.is_empty() {
            return format!("{}/\n(0 entries)", root.display());
        }

        let mut output = String::new();
        output.push_str(&format!("{}/\n", root.display()));

        // Convert to relative paths and sort
        let mut relative_entries: Vec<(PathBuf, bool)> = entries
            .iter()
            .filter_map(|entry| {
                entry
                    .strip_prefix(root)
                    .ok()
                    .map(|rel| (rel.to_path_buf(), entry.is_dir()))
            })
            .collect();

        // Sort: directories first, then files, alphabetically within each group
        relative_entries.sort_by(|a, b| match (a.1, b.1) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        });

        // Format each entry with appropriate indentation
        for (relative_path, is_dir) in relative_entries {
            let depth = relative_path.components().count();
            let indent = "  ".repeat(depth);
            let name = relative_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if is_dir {
                output.push_str(&format!("{}{}/\n", indent, name));
            } else {
                output.push_str(&format!("{}{}\n", indent, name));
            }
        }

        let total = entries.len();
        if truncated {
            output.push_str(&format!("({} entries, truncated)\n", total));
        } else {
            output.push_str(&format!("({} entries)\n", total));
        }

        output
    }
}

impl Default for ListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Lists files and directories in a given path. The path parameter must be absolute; omit it to use the current workspace directory. You can optionally provide an array of glob patterns to ignore with the ignore parameter. You should generally prefer the Glob and Grep tools, if you know which directories to search."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path to list. Defaults to current working directory."
                        },
                        "ignore": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Additional glob patterns to ignore (beyond default ignores)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of entries to return. Defaults to 100.",
                            "default": 100,
                            "minimum": 1
                        }
                    },
                    "required": []
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: The file list was truncated. Use search_text or more specific \
             glob patterns to narrow your search.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let root = if let Some(path_str) = args.get("path").and_then(Value::as_str) {
            context.resolve_path(path_str)?
        } else {
            context
                .cwd()
                .ok_or_else(|| {
                    ToolError::InvalidRequest(
                        "No path specified and no working directory set".to_string(),
                    )
                })?
                .to_path_buf()
        };

        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;

        // Combine default ignores with user-provided ones
        let mut ignore_patterns: Vec<String> = Self::default_ignores()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        if let Some(user_ignores) = args.get("ignore").and_then(Value::as_array) {
            let user_strs: Vec<String> = user_ignores
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            ignore_patterns.extend(user_strs);
        }

        let root_for_format = root.clone();
        let (entries, truncated) = tokio::task::spawn_blocking(move || {
            let patterns: Vec<&str> = ignore_patterns.iter().map(|s| s.as_str()).collect();
            Self::list_directory(&root, patterns, limit)
        })
        .await
        .map_err(|e| ToolError::ProviderError(format!("list task failed: {}", e)))??;

        Ok(Self::format_tree(entries, &root_for_format, truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_ls_basic() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::write(temp_path.join("file1.txt"), "content").unwrap();
        fs::create_dir(temp_path.join("subdir")).unwrap();
        fs::write(temp_path.join("subdir/file2.txt"), "content").unwrap();

        let context = AgentToolContext::basic("test".to_string(), Some(temp_path.to_path_buf()));
        let tool = ListTool::new();

        let args = json!({});
        let result = tool.call(args, &context).await.unwrap();

        // Check tree format output
        assert!(result.contains("file1.txt"));
        assert!(result.contains("subdir/"));
        assert!(result.contains("  file2.txt")); // nested with indentation
        assert!(result.contains("entries)"));
    }

    #[tokio::test]
    async fn test_ls_with_ignore() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        fs::write(temp_path.join("keep.txt"), "content").unwrap();
        fs::write(temp_path.join("ignore.log"), "content").unwrap();

        let context = AgentToolContext::basic("test".to_string(), Some(temp_path.to_path_buf()));
        let tool = ListTool::new();

        let args = json!({
            "ignore": ["*.log"]
        });
        let result = tool.call(args, &context).await.unwrap();

        assert!(result.contains("keep.txt"));
        assert!(!result.contains("ignore.log"));
    }
}
