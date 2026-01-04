//! Search text tool implementation using ToolContext (grep-style)

use async_trait::async_trait;
use glob::Pattern;
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, sinks::Lossy};
use ignore::WalkBuilder;
use querymt::chat::{FunctionTool, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// A single match result
#[derive(Debug, Serialize, Deserialize)]
struct Match {
    file: String,
    line: u64,
    column: Option<u64>,
    text: String,
}

/// Structured search results
#[derive(Debug, Serialize, Deserialize)]
struct SearchResults {
    matches: Vec<Match>,
    total_files: usize,
    total_matches: usize,
    truncated: bool,
}

pub struct SearchTextTool;

impl Default for SearchTextTool {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchTextTool {
    pub fn new() -> Self {
        Self
    }

    /// Perform grep-style search with include/exclude patterns
    fn grep_search(
        root: &Path,
        pattern: &str,
        include: Option<String>,
        exclude: Option<Vec<String>>,
        max_results: usize,
    ) -> Result<SearchResults, Box<dyn std::error::Error + Send>> {
        let matcher = RegexMatcher::new(pattern)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;
        let mut matches = Vec::new();
        let mut files_searched = 0;

        // Parse include pattern
        let include_pattern = if let Some(p) = include {
            Some(Pattern::new(&p).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?)
        } else {
            None
        };

        // Parse exclude patterns
        let exclude_patterns: Result<Vec<Pattern>, glob::PatternError> = exclude
            .unwrap_or_default()
            .into_iter()
            .map(|s| Pattern::new(&s))
            .collect();
        let exclude_patterns =
            exclude_patterns.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;

        for result in WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();

            // Apply include filter
            if let Some(ref include_pat) = include_pattern
                && let Ok(relative) = path.strip_prefix(root)
                && !include_pat.matches_path(relative)
            {
                continue;
            }

            // Apply exclude filters
            let should_exclude = exclude_patterns.iter().any(|pat| {
                path.strip_prefix(root)
                    .ok()
                    .map(|rel| pat.matches_path(rel))
                    .unwrap_or(false)
            });

            if should_exclude {
                continue;
            }

            files_searched += 1;

            Searcher::new()
                .search_path(
                    &matcher,
                    path,
                    Lossy(|lnum, line| {
                        if matches.len() >= max_results {
                            return Ok(false); // Stop searching this file
                        }

                        matches.push(Match {
                            file: path.display().to_string(),
                            line: lnum,
                            column: None,
                            text: line.trim_end().to_string(),
                        });
                        Ok(true)
                    }),
                )
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;

            if matches.len() >= max_results {
                break;
            }
        }

        // Sort by file modification time (most recent first)
        let mut file_times: HashMap<String, std::time::SystemTime> = HashMap::new();
        for m in &matches {
            if !file_times.contains_key(&m.file)
                && let Ok(metadata) = std::fs::metadata(&m.file)
                && let Ok(modified) = metadata.modified()
            {
                file_times.insert(m.file.clone(), modified);
            }
        }

        matches.sort_by(|a, b| {
            let a_time = file_times.get(&a.file);
            let b_time = file_times.get(&b.file);
            b_time.cmp(&a_time) // Reverse for most recent first
        });

        let total_matches = matches.len();
        let truncated = total_matches >= max_results;

        Ok(SearchResults {
            matches,
            total_files: files_searched,
            total_matches,
            truncated,
        })
    }
}

#[async_trait]
impl ToolTrait for SearchTextTool {
    fn name(&self) -> &str {
        "search_text"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "- Fast content search tool that works with any codebase size\n\
                    - Searches file contents using regular expressions\n\
                    - Supports full regex syntax (eg. \"log.*Error\", \"function\\s+\\w+\", etc.)\n\
                    - Filter files by pattern with the include parameter (eg. \"*.js\", \"*.{ts,tsx}\")\n\
                    - Returns file paths and line numbers with at least one match sorted by modification time\n\
                    - Use this tool when you need to find files containing specific patterns\n\
                    - If you need to identify/count the number of matches within files, use the Bash tool with `rg` (ripgrep) directly. Do NOT use `grep`.\n\
                    - When you are doing an open-ended search that may require multiple rounds of globbing and grepping, use the Task tool instead"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for in file contents"
                        },
                        "path": {
                            "type": "string",
                            "description": "The directory to search in. Defaults to the current working directory."
                        },
                        "include": {
                            "type": "string",
                            "description": "File pattern to include in the search (e.g. \"*.js\", \"*.{ts,tsx}\")"
                        },
                        "exclude": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "File patterns to exclude from the search"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of matches to return. Defaults to 100.",
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

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("pattern is required".to_string()))?
            .to_string();

        let root = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| context.resolve_path(s))
            .transpose()?
            .or_else(|| context.cwd().map(|p| p.to_path_buf()))
            .ok_or_else(|| ToolError::InvalidRequest("No working directory available".into()))?;

        let include = args
            .get("include")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let exclude = args.get("exclude").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });

        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(100) as usize;

        let results = tokio::task::spawn_blocking(move || {
            Self::grep_search(&root, &pattern, include, exclude, max_results)
        })
        .await
        .map_err(|e| ToolError::ProviderError(format!("search task failed: {}", e)))?
        .map_err(|e| ToolError::ProviderError(format!("search failed: {}", e)))?;

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
    async fn test_search_text() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "hello world\nrust is great",
        )
        .unwrap();

        let args = json!({
            "pattern": "rust"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: SearchResults = serde_json::from_str(&result).unwrap();

        assert!(!parsed.matches.is_empty());
        assert!(parsed.matches[0].text.contains("rust"));
        assert_eq!(parsed.matches[0].line, 2);
    }

    #[tokio::test]
    async fn test_search_text_with_include() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(temp_dir.path().join("test.txt"), "hello world").unwrap();
        fs::write(temp_dir.path().join("test.rs"), "hello world").unwrap();

        let args = json!({
            "pattern": "hello",
            "include": "*.rs"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: SearchResults = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed.matches.len(), 1);
        assert!(parsed.matches[0].file.ends_with(".rs"));
    }
}
