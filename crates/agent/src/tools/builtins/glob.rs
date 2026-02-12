//! Glob tool for fast file pattern matching

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::tools::{CapabilityRequirement, Tool, ToolContext, ToolError};

/// Fast file pattern matching tool
pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self {
        Self
    }

    /// Perform glob search using ignore crate
    fn glob_files(pattern: &str, root: &PathBuf, limit: usize) -> Result<Vec<PathBuf>, ToolError> {
        use glob::Pattern;
        use ignore::WalkBuilder;

        // Parse the glob pattern
        let glob_pattern = Pattern::new(pattern)
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid glob pattern: {}", e)))?;

        let mut matches = Vec::new();
        let mut count = 0;

        // Use ignore crate for gitignore-aware walking
        let walker = WalkBuilder::new(root)
            .hidden(false) // Don't skip hidden files by default
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        for result in walker {
            if count >= limit {
                break;
            }

            let entry = result
                .map_err(|e| ToolError::ProviderError(format!("Error walking directory: {}", e)))?;

            let path = entry.path();

            // Skip directories
            if path.is_dir() {
                continue;
            }

            // Match against pattern
            if let Ok(relative) = path.strip_prefix(root)
                && glob_pattern.matches_path(relative)
            {
                matches.push(path.to_path_buf());
                count += 1;
            }
        }

        // Sort by modification time (most recent first)
        matches.sort_by(|a, b| {
            let a_time = std::fs::metadata(a).and_then(|m| m.modified()).ok();
            let b_time = std::fs::metadata(b).and_then(|m| m.modified()).ok();
            b_time.cmp(&a_time) // Reverse order for most recent first
        });

        Ok(matches)
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "- Fast file pattern matching tool that works with any codebase size\n\
                    - Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"\n\
                    - Returns matching file paths sorted by modification time\n\
                    - Use this tool when you need to find files by name patterns\n\
                    - When you are doing an open-ended search that may require multiple rounds of globbing and grepping, use the Task tool instead\n\
                    - You have the capability to call multiple tools in a single response. It is always better to speculatively perform multiple searches as a batch that are potentially useful."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "The glob pattern to match files against (e.g., \"**/*.rs\", \"src/**/*.txt\")"
                        },
                        "path": {
                            "type": "string",
                            "description": "The directory to search in. Defaults to the current working directory."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of results to return. Defaults to 100.",
                            "default": 100,
                            "minimum": 1
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    fn truncation_hint(&self) -> Option<&'static str> {
        Some(
            "TIP: Many matches found and truncated. Refine your glob pattern \
             or use more specific path filters to narrow results.",
        )
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        // Extract pattern (required)
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("pattern is required".to_string()))?;

        // Extract path (optional, defaults to cwd)
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

        // Extract limit (optional, defaults to 100)
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;

        // Perform glob search
        let matches = Self::glob_files(pattern, &root, limit)?;

        let was_truncated = matches.len() >= limit;

        // Format as simple list with relative paths
        let mut output = String::new();
        for path in &matches {
            if let Ok(relative) = path.strip_prefix(&root) {
                output.push_str(&format!("{}\n", relative.display()));
            } else {
                output.push_str(&format!("{}\n", path.display()));
            }
        }

        // Add count footer
        let count = matches.len();
        if was_truncated {
            output.push_str(&format!("({} matches, truncated)", count));
        } else {
            output.push_str(&format!("({} matches)", count));
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_glob_basic() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create test files
        fs::write(temp_path.join("test.rs"), "content").unwrap();
        fs::write(temp_path.join("test.txt"), "content").unwrap();
        fs::create_dir(temp_path.join("src")).unwrap();
        fs::write(temp_path.join("src/main.rs"), "content").unwrap();

        let context = AgentToolContext::basic("test".to_string(), Some(temp_path.to_path_buf()));
        let tool = GlobTool::new();

        let args = json!({
            "pattern": "**/*.rs"
        });

        let result = tool.call(args, &context).await.unwrap();

        // Check simple list format
        assert!(result.contains("test.rs"));
        assert!(result.contains("src/main.rs") || result.contains("main.rs"));
        assert!(!result.contains("test.txt"));
        assert!(result.contains("matches)"));
    }

    #[tokio::test]
    async fn test_glob_with_limit() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create multiple test files
        for i in 0..10 {
            fs::write(temp_path.join(format!("test{}.rs", i)), "content").unwrap();
        }

        let context = AgentToolContext::basic("test".to_string(), Some(temp_path.to_path_buf()));
        let tool = GlobTool::new();

        let args = json!({
            "pattern": "*.rs",
            "limit": 5
        });

        let result = tool.call(args, &context).await.unwrap();

        // Check truncation message
        assert!(result.contains("(5 matches, truncated)"));

        // Should have exactly 5 matches + 1 count line = 6 lines
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 6);
    }
}
