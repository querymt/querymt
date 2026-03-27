//! Search text tool implementation using ToolContext (grep-style)

use async_trait::async_trait;
use glob::Pattern;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch, sinks::Lossy,
};
use ignore::{WalkBuilder, types::TypesBuilder};
use indexmap::IndexMap;
use querymt::chat::{Content, FunctionTool, Tool};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

/// Maximum bytes for a single matched line's text.
/// Lines longer than this are truncated to prevent binary blobs or extremely
/// long lines from bloating tool results.
const MAX_MATCH_TEXT_BYTES: usize = 500;

/// A single match result (internal-only, used during collection)
#[derive(Debug)]
struct Match {
    file: String,
    line: u64,
    text: String,
    context: bool,
}

/// Internal search results (before formatting)
#[derive(Debug)]
struct InternalSearchResults {
    matches: Vec<Match>,
    truncated: bool,
}

/// Options for configuring text search behavior
struct SearchOptions {
    pattern: String,
    root: PathBuf,
    include: Option<String>,
    exclude: Option<Vec<String>>,
    max_results: usize,
    case_insensitive: bool,
    fixed_strings: bool,
    word_match: bool,
    before_context: usize,
    after_context: usize,
    file_type: Option<String>,
}

/// Custom sink implementation that handles context lines
struct ContextSink<'a> {
    matches: &'a mut Vec<Match>,
    path: &'a Path,
    max_results: usize,
}

impl<'a> ContextSink<'a> {
    fn new(matches: &'a mut Vec<Match>, path: &'a Path, max_results: usize) -> Self {
        Self {
            matches,
            path,
            max_results,
        }
    }

    fn add_line(&mut self, line_num: u64, text: &[u8], is_context: bool) -> bool {
        if self.matches.len() >= self.max_results {
            return false;
        }

        let trimmed = String::from_utf8_lossy(text);
        let trimmed = trimmed.trim_end();
        let text_str = if trimmed.len() > MAX_MATCH_TEXT_BYTES {
            let mut truncated = trimmed[..MAX_MATCH_TEXT_BYTES].to_string();
            truncated.push_str("...[truncated]");
            truncated
        } else {
            trimmed.to_string()
        };

        self.matches.push(Match {
            file: self.path.display().to_string(),
            line: line_num,
            text: text_str,
            context: is_context,
        });
        true
    }
}

impl<'a> Sink for ContextSink<'a> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        Ok(self.add_line(mat.line_number().unwrap_or(0), mat.bytes(), false))
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        Ok(self.add_line(ctx.line_number().unwrap_or(0), ctx.bytes(), true))
    }
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
        opts: SearchOptions,
    ) -> Result<InternalSearchResults, Box<dyn std::error::Error + Send>> {
        // Build the regex matcher with specified options
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(opts.case_insensitive)
            .fixed_strings(opts.fixed_strings)
            .word(opts.word_match)
            .build(&opts.pattern)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;

        let mut matches = Vec::new();

        // Parse include pattern
        let include_pattern = if let Some(p) = opts.include {
            Some(Pattern::new(&p).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?)
        } else {
            None
        };

        // Parse exclude patterns
        let exclude_patterns: Result<Vec<Pattern>, glob::PatternError> = opts
            .exclude
            .unwrap_or_default()
            .into_iter()
            .map(|s| Pattern::new(&s))
            .collect();
        let exclude_patterns =
            exclude_patterns.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;

        // Build the directory walker
        let mut walker_builder = WalkBuilder::new(&opts.root);
        walker_builder.hidden(false).git_ignore(true);

        // Add file type filtering if specified
        if let Some(ref ft) = opts.file_type {
            let mut types_builder = TypesBuilder::new();
            types_builder.add_defaults();
            types_builder.select(ft);
            let types = types_builder
                .build()
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;
            walker_builder.types(types);
        }

        let has_context = opts.before_context > 0 || opts.after_context > 0;

        for result in walker_builder.build() {
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
                && let Ok(relative) = path.strip_prefix(&opts.root)
                && !include_pat.matches_path(relative)
            {
                continue;
            }

            // Apply exclude filters
            let should_exclude = exclude_patterns.iter().any(|pat| {
                path.strip_prefix(&opts.root)
                    .ok()
                    .map(|rel| pat.matches_path(rel))
                    .unwrap_or(false)
            });

            if should_exclude {
                continue;
            }

            // Build searcher with context settings
            let mut searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\0'))
                .before_context(opts.before_context)
                .after_context(opts.after_context)
                .build();

            // Use custom sink if context is enabled, otherwise use simple Lossy sink
            if has_context {
                let mut sink = ContextSink::new(&mut matches, path, opts.max_results);
                searcher
                    .search_path(&matcher, path, &mut sink)
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;
            } else {
                searcher
                    .search_path(
                        &matcher,
                        path,
                        Lossy(|lnum, line| {
                            if matches.len() >= opts.max_results {
                                return Ok(false); // Stop searching this file
                            }

                            let trimmed = line.trim_end();
                            let text = if trimmed.len() > MAX_MATCH_TEXT_BYTES {
                                let mut truncated = trimmed[..MAX_MATCH_TEXT_BYTES].to_string();
                                truncated.push_str("...[truncated]");
                                truncated
                            } else {
                                trimmed.to_string()
                            };

                            matches.push(Match {
                                file: path.display().to_string(),
                                line: lnum,
                                text,
                                context: false,
                            });
                            Ok(true)
                        }),
                    )
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send>)?;
            }

            if matches.len() >= opts.max_results {
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

        let truncated = matches.len() >= opts.max_results;

        Ok(InternalSearchResults { matches, truncated })
    }

    /// Format internal search results into compact plain-text output.
    fn format_compact_text(
        results: &InternalSearchResults,
        root: &Path,
        has_context: bool,
    ) -> String {
        let mut file_results: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut actual_match_count = 0;

        // Group matches by file (they're already sorted by file mod time)
        let mut current_file: Option<String> = None;
        let mut prev_line: Option<u64> = None;

        for m in &results.matches {
            // Strip root prefix to get relative path
            let rel_path = Path::new(&m.file)
                .strip_prefix(root)
                .unwrap_or(Path::new(&m.file))
                .display()
                .to_string();

            // If we're switching to a new file, reset prev_line
            if current_file.as_ref() != Some(&rel_path) {
                current_file = Some(rel_path.clone());
                prev_line = None;
            }

            // Get or create the lines vector for this file
            let lines = file_results.entry(rel_path.clone()).or_default();

            // Separate non-contiguous context groups in the same file.
            if has_context && prev_line.is_some_and(|prev| prev.checked_add(1) != Some(m.line)) {
                lines.push("--".to_string());
            }

            // Format the line
            let separator = if m.context { "-" } else { ":" };
            lines.push(format!("{}{}{}", m.line, separator, m.text));

            // Count actual matches (non-context lines)
            if !m.context {
                actual_match_count += 1;
            }

            prev_line = Some(m.line);
        }

        let mut output = String::new();
        let matched_files = file_results.len();

        for (idx, (file, lines)) in file_results.into_iter().enumerate() {
            if idx > 0 {
                output.push('\n');
            }
            output.push_str(&file);
            output.push('\n');
            output.push_str(&lines.join("\n"));
            output.push('\n');
        }

        if results.truncated {
            output.push_str(&format!(
                "({} {}, {} matches, truncated)",
                matched_files,
                if matched_files == 1 { "file" } else { "files" },
                actual_match_count
            ));
        } else {
            output.push_str(&format!(
                "({} {}, {} matches)",
                matched_files,
                if matched_files == 1 { "file" } else { "files" },
                actual_match_count
            ));
        }

        output
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
                    - Searches file contents using regular expressions (or literal strings with fixed_strings)\n\
                    - Supports full regex syntax (eg. \"log.*Error\", \"function\\s+\\w+\", etc.)\n\
                    - Supports case-insensitive search, word boundary matching, and context lines\n\
                    - Filter files by pattern (include/exclude) or by file type (e.g. \"rust\", \"js\")\n\
                    - Returns compact grep-style text grouped by file, sorted by modification time\n\
                    - Use this tool when you need to find files containing specific patterns\n\
                    - When you are doing an open-ended search that may require multiple rounds of globbing and grepping, use the Task tool instead"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for in file contents (or literal string if fixed_strings is true)"
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
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Perform case-insensitive matching. Defaults to false.",
                            "default": false
                        },
                        "fixed_strings": {
                            "type": "boolean",
                            "description": "Treat pattern as a literal string instead of regex. Defaults to false.",
                            "default": false
                        },
                        "word_match": {
                            "type": "boolean",
                            "description": "Only match whole words (word boundaries). Defaults to false.",
                            "default": false
                        },
                        "before_context": {
                            "type": "integer",
                            "description": "Number of lines to include before each match. Defaults to 0.",
                            "default": 0,
                            "minimum": 0
                        },
                        "after_context": {
                            "type": "integer",
                            "description": "Number of lines to include after each match. Defaults to 0.",
                            "default": 0,
                            "minimum": 0
                        },
                        "file_type": {
                            "type": "string",
                            "description": "File type filter (e.g. \"rust\", \"js\", \"python\"). Uses ripgrep's built-in type definitions."
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
            "TIP: Many matches found and truncated. Refine your search pattern, \
             add file type/path filters, or increase specificity to narrow results.",
        )
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<Content>, ToolError> {
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

        let case_insensitive = args
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let fixed_strings = args
            .get("fixed_strings")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let word_match = args
            .get("word_match")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let before_context = args
            .get("before_context")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        let after_context = args
            .get("after_context")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;

        let file_type = args
            .get("file_type")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let has_context = before_context > 0 || after_context > 0;
        let root_for_format = root.clone();

        let opts = SearchOptions {
            pattern,
            root,
            include,
            exclude,
            max_results,
            case_insensitive,
            fixed_strings,
            word_match,
            before_context,
            after_context,
            file_type,
        };

        let internal_results = tokio::task::spawn_blocking(move || Self::grep_search(opts))
            .await
            .map_err(|e| ToolError::ProviderError(format!("search task failed: {}", e)))?
            .map_err(|e| ToolError::ProviderError(format!("search failed: {}", e)))?;

        let compact_output =
            Self::format_compact_text(&internal_results, &root_for_format, has_context);

        Ok(vec![Content::text(compact_output)])
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

    fn file_count(output: &str) -> usize {
        let footer = output.lines().last().unwrap_or_default();
        let file_segment = footer.split(',').next().unwrap_or_default();
        file_segment
            .trim_start_matches('(')
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    }

    fn match_count(output: &str) -> usize {
        let footer = output.lines().last().unwrap_or_default();
        let (_, tail) = footer.split_once(", ").unwrap_or(("", "0 matches"));
        let match_segment = tail.split(',').next().unwrap_or_default();
        match_segment
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    }

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
            "hello world
rust is great",
        )
        .unwrap();

        let args = json!({
            "pattern": "rust"
        });

        let result = first_text_block(tool.call(args, &context).await.unwrap());

        assert!(result.contains("test.txt\n2:rust is great\n"));
        assert_eq!(file_count(&result), 1);
        assert_eq!(match_count(&result), 1);
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

        let result = first_text_block(tool.call(args, &context).await.unwrap());

        assert!(result.contains("test.rs\n1:hello world\n"));
        assert!(!result.contains("test.txt\n1:hello world\n"));
        assert_eq!(file_count(&result), 1);
        assert_eq!(match_count(&result), 1);
    }

    #[tokio::test]
    async fn test_case_insensitive_search() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "Hello World
HELLO WORLD
hello world",
        )
        .unwrap();

        // Case-sensitive (default) - should only match exact case
        let args = json!({
            "pattern": "hello"
        });
        let result = first_text_block(tool.call(args.clone(), &context).await.unwrap());
        assert_eq!(match_count(&result), 1);

        // Case-insensitive - should match all variations
        let args = json!({
            "pattern": "hello",
            "case_insensitive": true
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 3);
    }

    #[tokio::test]
    async fn test_fixed_strings_search() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "foo.bar()
fooXbar()
foo*bar",
        )
        .unwrap();

        // With regex (default) - matches multiple lines due to regex interpretation
        let args = json!({
            "pattern": "foo.bar"
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert!(match_count(&result) >= 2); // Matches foo.bar and fooXbar

        // With fixed strings - only matches literal "foo.bar"
        let args = json!({
            "pattern": "foo.bar",
            "fixed_strings": true
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 1);
        assert!(result.contains("1:foo.bar()"));
    }

    #[tokio::test]
    async fn test_word_match() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "foo foobar barfoo foo_bar",
        )
        .unwrap();

        // Without word match - line should match
        let args = json!({
            "pattern": "foo"
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 1);

        // With word match - only standalone "foo" triggers the line match
        let args = json!({
            "pattern": "foo",
            "word_match": true
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 1);
        assert!(result.contains("foo"));
    }

    #[tokio::test]
    async fn test_context_lines() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "line 1
line 2
MATCH HERE
line 4
line 5",
        )
        .unwrap();

        // Without context
        let args = json!({
            "pattern": "MATCH"
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert!(result.contains("test.txt\n3:MATCH HERE\n"));
        assert_eq!(match_count(&result), 1);

        // With before and after context
        let args = json!({
            "pattern": "MATCH",
            "before_context": 1,
            "after_context": 2
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());

        // Should have only 1 actual match (context lines don't count)
        assert_eq!(match_count(&result), 1);

        // Verify the formatted output contains all lines with correct separators
        assert!(result.contains("2-line 2"));
        assert!(result.contains("3:MATCH HERE"));
        assert!(result.contains("4-line 4"));
        assert!(result.contains("5-line 5"));
    }

    #[tokio::test]
    async fn test_file_type_filter() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(temp_dir.path().join("test.rs"), "fn main() {}").unwrap();
        fs::write(temp_dir.path().join("test.js"), "function main() {}").unwrap();
        fs::write(temp_dir.path().join("test.py"), "def main(): pass").unwrap();

        // Search all files
        let args = json!({
            "pattern": "main"
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 3);
        assert_eq!(file_count(&result), 3);

        // Search only Rust files
        let args = json!({
            "pattern": "main",
            "file_type": "rust"
        });
        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert_eq!(match_count(&result), 1);
        assert_eq!(file_count(&result), 1);
        assert!(result.contains("test.rs\n1:fn main() {}\n"));
    }

    #[tokio::test]
    async fn test_truncated_footer() {
        let temp_dir = TempDir::new().unwrap();
        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = SearchTextTool::new();

        fs::write(
            temp_dir.path().join("test.txt"),
            "match
match again",
        )
        .unwrap();

        let args = json!({
            "pattern": "match",
            "max_results": 1
        });

        let result = first_text_block(tool.call(args, &context).await.unwrap());
        assert!(result.ends_with(", truncated)"));
    }
}
