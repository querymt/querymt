//! List directory contents tool

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

/// File or directory entry
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    path: String,
    entry_type: String, // "file" or "directory"
    size: Option<u64>,
}

/// List results
#[derive(Debug, Serialize, Deserialize)]
struct ListResults {
    entries: Vec<Entry>,
    total: usize,
    truncated: bool,
}

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
    ) -> Result<ListResults, ToolError> {
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

            let metadata = entry
                .metadata()
                .map_err(|e| ToolError::ProviderError(format!("Failed to get metadata: {}", e)))?;

            let entry_type = if metadata.is_dir() {
                "directory"
            } else {
                "file"
            };

            let size = if metadata.is_file() {
                Some(metadata.len())
            } else {
                None
            };

            entries.push(Entry {
                path: path.display().to_string(),
                entry_type: entry_type.to_string(),
                size,
            });
        }

        let total = entries.len();
        let truncated = total >= limit;

        Ok(ListResults {
            entries,
            total,
            truncated,
        })
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

        let results = tokio::task::spawn_blocking(move || {
            let patterns: Vec<&str> = ignore_patterns.iter().map(|s| s.as_str()).collect();
            Self::list_directory(&root, patterns, limit)
        })
        .await
        .map_err(|e| ToolError::ProviderError(format!("list task failed: {}", e)))??;

        serde_json::to_string_pretty(&results)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
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
        let parsed: ListResults = serde_json::from_str(&result).unwrap();

        assert!(parsed.entries.len() >= 2);
        assert!(parsed.entries.iter().any(|e| e.path.contains("file1.txt")));
        assert!(parsed.entries.iter().any(|e| e.path.contains("subdir")));
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
        let parsed: ListResults = serde_json::from_str(&result).unwrap();

        assert!(parsed.entries.iter().any(|e| e.path.contains("keep.txt")));
        assert!(!parsed.entries.iter().any(|e| e.path.contains("ignore.log")));
    }
}
