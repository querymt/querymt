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

    /// Format entries as a depth-prefixed tree.
    ///
    /// Each line (after the root path header) has the format `N name` where N
    /// is the 0-based depth relative to root. Directories get a trailing `/`.
    /// This is compact (low token count) and trivially parsable — just read the
    /// integer prefix to reconstruct the tree.
    ///
    /// Example output:
    /// ```text
    /// /workspace/project/
    /// 0 src/
    /// 1 main.rs
    /// 1 lib.rs
    /// 0 Cargo.toml
    /// (5 entries)
    /// ```
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
                let rel = entry.strip_prefix(root).ok()?;
                // Skip the root itself (empty relative path)
                if rel.as_os_str().is_empty() {
                    return None;
                }
                Some((rel.to_path_buf(), entry.is_dir()))
            })
            .collect();

        // Sort to produce correct depth-first tree order: children are grouped
        // immediately after their parent, with directories before files at each
        // level. We compare path component-by-component; at each level a
        // directory sorts before a file so the tree structure is preserved.
        relative_entries.sort_by(|a, b| {
            use std::cmp::Ordering;
            let a_comps: Vec<_> = a.0.components().collect();
            let b_comps: Vec<_> = b.0.components().collect();

            for i in 0..a_comps.len().min(b_comps.len()) {
                let ac = a_comps[i].as_os_str();
                let bc = b_comps[i].as_os_str();

                // A component is a "directory level" if there are more
                // components after it, or it is the final component and the
                // entry itself is a directory.
                let a_is_dir = i + 1 < a_comps.len() || a.1;
                let b_is_dir = i + 1 < b_comps.len() || b.1;

                if ac != bc {
                    // At the divergence point: dirs before files, then alphabetical
                    match (a_is_dir, b_is_dir) {
                        (true, false) => return Ordering::Less,
                        (false, true) => return Ordering::Greater,
                        _ => return ac.cmp(bc),
                    }
                }

                // Same component name but one is dir and other is file at this
                // level — directory wins.
                if a_is_dir != b_is_dir {
                    return if a_is_dir {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
            }
            // One path is a prefix of the other — parent (shorter) comes first
            a_comps.len().cmp(&b_comps.len())
        });

        // Format each entry with depth prefix
        for (relative_path, is_dir) in &relative_entries {
            // Depth is 0-based: direct children of root are depth 0
            let depth = relative_path.components().count() - 1;
            let name = relative_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if *is_dir {
                output.push_str(&format!("{} {}/\n", depth, name));
            } else {
                output.push_str(&format!("{} {}\n", depth, name));
            }
        }

        let total = relative_entries.len();
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

    #[test]
    fn test_format_tree_depth_prefix_basic() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::create_dir(root.join("subdir")).unwrap();
        fs::write(root.join("file1.txt"), "").unwrap();
        fs::write(root.join("subdir/file2.txt"), "").unwrap();

        let entries = vec![
            root.to_path_buf(),
            root.join("subdir"),
            root.join("file1.txt"),
            root.join("subdir/file2.txt"),
        ];

        let result = ListTool::format_tree(entries, root, false);

        // First line is the root path with trailing /
        let lines: Vec<&str> = result.lines().collect();
        assert!(
            lines[0].ends_with('/'),
            "First line should be root path with /"
        );

        // Each entry line is depth-prefixed: "N name" or "N name/"
        // Depth 0 entries are direct children of root
        assert!(
            lines.iter().any(|l| *l == "0 subdir/"),
            "Should have depth-0 directory 'subdir/': got {:?}",
            lines
        );
        assert!(
            lines.iter().any(|l| *l == "0 file1.txt"),
            "Should have depth-0 file 'file1.txt': got {:?}",
            lines
        );
        // file2.txt is inside subdir, so depth 1
        assert!(
            lines.iter().any(|l| *l == "1 file2.txt"),
            "Should have depth-1 file 'file2.txt': got {:?}",
            lines
        );

        // Footer
        assert!(
            lines.last().unwrap().contains("entries)"),
            "Should end with entry count"
        );
    }

    #[test]
    fn test_format_tree_depth_prefix_nested() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/deep.txt"), "").unwrap();

        let entries = vec![
            root.to_path_buf(),
            root.join("a"),
            root.join("a/b"),
            root.join("a/b/c"),
            root.join("a/b/c/deep.txt"),
        ];

        let result = ListTool::format_tree(entries, root, false);
        let lines: Vec<&str> = result.lines().collect();

        assert!(lines.iter().any(|l| *l == "0 a/"), "depth-0 dir a/");
        assert!(lines.iter().any(|l| *l == "1 b/"), "depth-1 dir b/");
        assert!(lines.iter().any(|l| *l == "2 c/"), "depth-2 dir c/");
        assert!(
            lines.iter().any(|l| *l == "3 deep.txt"),
            "depth-3 file deep.txt"
        );
    }

    #[test]
    fn test_format_tree_depth_prefix_empty() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let result = ListTool::format_tree(vec![], root, false);
        assert!(result.contains("(0 entries)"));
    }

    #[test]
    fn test_format_tree_depth_prefix_truncated() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join("a.txt"), "").unwrap();

        let entries = vec![root.to_path_buf(), root.join("a.txt")];

        let result = ListTool::format_tree(entries, root, true);
        assert!(
            result.contains("entries, truncated)"),
            "Should indicate truncation"
        );
    }

    #[test]
    fn test_format_tree_dirs_sorted_before_files() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::create_dir(root.join("zdir")).unwrap();
        fs::write(root.join("afile.txt"), "").unwrap();

        let entries = vec![
            root.to_path_buf(),
            root.join("afile.txt"),
            root.join("zdir"),
        ];

        let result = ListTool::format_tree(entries, root, false);
        let lines: Vec<&str> = result.lines().collect();

        // Find positions of the dir and file lines
        let dir_pos = lines.iter().position(|l| *l == "0 zdir/").unwrap();
        let file_pos = lines.iter().position(|l| *l == "0 afile.txt").unwrap();
        assert!(
            dir_pos < file_pos,
            "Directories should be sorted before files"
        );
    }

    #[test]
    fn test_format_tree_files_grouped_under_parent() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create:
        //   datasets/
        //     utils/
        //     data_manager.py
        //   hub/
        //     __init__.py
        //   top.txt
        fs::create_dir_all(root.join("datasets/utils")).unwrap();
        fs::write(root.join("datasets/data_manager.py"), "").unwrap();
        fs::create_dir(root.join("hub")).unwrap();
        fs::write(root.join("hub/__init__.py"), "").unwrap();
        fs::write(root.join("top.txt"), "").unwrap();

        let entries = vec![
            root.to_path_buf(),
            root.join("datasets"),
            root.join("datasets/utils"),
            root.join("datasets/data_manager.py"),
            root.join("hub"),
            root.join("hub/__init__.py"),
            root.join("top.txt"),
        ];

        let result = ListTool::format_tree(entries, root, false);
        let lines: Vec<&str> = result.lines().collect();

        // Files must appear directly after their parent directory, not after
        // all directories globally. The correct order is:
        //   /root/
        //   0 datasets/
        //   1 utils/
        //   1 data_manager.py   ← must be BEFORE "0 hub/", not after all dirs
        //   0 hub/
        //   1 __init__.py
        //   0 top.txt

        let datasets_pos = lines.iter().position(|l| *l == "0 datasets/").unwrap();
        let datasets_utils_pos = lines.iter().position(|l| *l == "1 utils/").unwrap();
        let data_manager_pos = lines
            .iter()
            .position(|l| *l == "1 data_manager.py")
            .unwrap();
        let hub_pos = lines.iter().position(|l| *l == "0 hub/").unwrap();
        let init_pos = lines.iter().position(|l| *l == "1 __init__.py").unwrap();
        let top_pos = lines.iter().position(|l| *l == "0 top.txt").unwrap();

        // datasets/ children must come before hub/
        assert!(
            datasets_pos < datasets_utils_pos,
            "datasets/ before its child utils/"
        );
        assert!(
            datasets_utils_pos < data_manager_pos,
            "datasets/utils/ before datasets/data_manager.py"
        );
        assert!(
            data_manager_pos < hub_pos,
            "datasets/data_manager.py (depth 1) must come BEFORE hub/ (depth 0), got lines: {:?}",
            lines
        );

        // hub/ children must come before top.txt
        assert!(hub_pos < init_pos, "hub/ before its child __init__.py");
        assert!(
            init_pos < top_pos,
            "hub/__init__.py must come before top.txt"
        );
    }

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

        // Check depth-prefix format output
        assert!(result.contains("0 file1.txt"), "depth-0 file1.txt");
        assert!(result.contains("0 subdir/"), "depth-0 subdir/");
        assert!(result.contains("1 file2.txt"), "depth-1 file2.txt");
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
