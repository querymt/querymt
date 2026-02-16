use gitpatch::Patch as GitPatch;
/// Shared utilities for patch parsing and handling using patchkit
use patchkit::unified::UnifiedPatch;
use std::path::{Path, PathBuf};

/// Represents a single parsed patch with its target file path
pub struct ParsedPatch {
    pub patch: UnifiedPatch,
    pub file_path: String,
}

/// Split a multi-file patch text into individual patches
/// Returns vector of (patch_text, file_path_hint) tuples
pub fn split_patch_text(patch_text: &str) -> Result<Vec<String>, String> {
    let mut patches = Vec::new();
    let mut current_patch = String::new();
    let mut in_patch = false;

    for line in patch_text.lines() {
        if line.starts_with("--- ") {
            // Start of a new patch
            if in_patch && !current_patch.is_empty() {
                // Save previous patch
                patches.push(current_patch.clone());
                current_patch.clear();
            }
            in_patch = true;
            current_patch.push_str(line);
            current_patch.push('\n');
        } else if in_patch {
            current_patch.push_str(line);
            current_patch.push('\n');
        }
    }

    // Don't forget the last patch
    if in_patch && !current_patch.is_empty() {
        patches.push(current_patch);
    }

    if patches.is_empty() {
        return Err("No valid patches found in input".to_string());
    }

    Ok(patches)
}

/// Parse a single patch text into a UnifiedPatch object
pub fn parse_single_patch(patch_text: &str) -> Result<UnifiedPatch, String> {
    // Convert text to the format patchkit expects: Vec<&[u8]> where each line ends with \n
    let mut lines: Vec<Vec<u8>> = Vec::new();

    for line in patch_text.lines() {
        let mut line_bytes = line.as_bytes().to_vec();
        // Remove any trailing \r (CRLF handling)
        if line_bytes.last() == Some(&b'\r') {
            line_bytes.pop();
        }
        // patchkit expects each line to end with \n
        line_bytes.push(b'\n');
        lines.push(line_bytes);
    }

    // Convert to the slice iterator patchkit wants
    let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();

    UnifiedPatch::parse_patch(line_refs.into_iter()).map_err(|e| match e {
        patchkit::unified::Error::PatchSyntax(msg, _) => {
            format!("Patch syntax error: {}", msg)
        }
        patchkit::unified::Error::BinaryFiles(_, _) => "Binary files not supported".to_string(),
        patchkit::unified::Error::MalformedPatchHeader(msg, _) => {
            format!("Malformed patch header: {}", msg)
        }
        patchkit::unified::Error::MalformedHunkHeader(msg, _) => {
            format!("Malformed hunk header: {}", msg)
        }
    })
}

/// Parse patch text (possibly multi-file) into individual ParsedPatch objects
pub fn parse_patches(patch_text: &str) -> Result<Vec<ParsedPatch>, String> {
    if is_git_diff_blob(patch_text) {
        return parse_git_diff_patches(patch_text);
    }

    let patch_texts = split_patch_text(patch_text)?;
    let mut parsed_patches = Vec::new();

    for text in patch_texts {
        let patch = parse_single_patch(&text)?;

        // Extract file path from patch
        let file_path = String::from_utf8_lossy(&patch.mod_name).to_string();

        parsed_patches.push(ParsedPatch { patch, file_path });
    }

    Ok(parsed_patches)
}

fn is_git_diff_blob(patch_text: &str) -> bool {
    patch_text
        .lines()
        .any(|line| line.starts_with("diff --git "))
}

fn parse_git_diff_patches(patch_text: &str) -> Result<Vec<ParsedPatch>, String> {
    let git_patches =
        GitPatch::from_multiple(patch_text).map_err(|e| format!("Git patch parse error: {}", e))?;

    let mut parsed_patches = Vec::new();

    for git_patch in git_patches {
        let unified_text = format!("{}\n", git_patch);
        let patch = parse_single_patch(&unified_text)?;
        let file_path = String::from_utf8_lossy(&patch.mod_name).to_string();
        parsed_patches.push(ParsedPatch { patch, file_path });
    }

    Ok(parsed_patches)
}

/// Resolve a patch file path to an actual filesystem path
/// Handles workdir, auto-detects git-style prefixes (a/, b/), and validates existence
pub fn resolve_file_path(
    patch_path: &str,
    workdir: Option<&str>,
    strip: usize,
) -> Result<PathBuf, String> {
    let path = Path::new(patch_path);

    // Apply stripping logic
    let stripped = if strip > 0 {
        // Manual strip: remove N path components
        patchkit::strip_prefix(path, strip)
    } else {
        // Auto-detect: try to strip a/ or b/ prefix
        let candidate_without_prefix = if let Ok(without_b) = path.strip_prefix("b/") {
            Some(without_b)
        } else {
            path.strip_prefix("a/").ok()
        };

        // If we found a prefix, check which path exists
        if let Some(without_prefix) = candidate_without_prefix {
            let base_dir = workdir.unwrap_or(".");
            let path_without_prefix = Path::new(base_dir).join(without_prefix);
            let path_with_prefix = Path::new(base_dir).join(path);

            // Try without prefix first (git-style is more common)
            if path_without_prefix.exists() {
                log::debug!(
                    "Auto-detected git-style prefix in '{}', using '{}'",
                    patch_path,
                    without_prefix.display()
                );
                without_prefix
            } else if path_with_prefix.exists() {
                log::debug!(
                    "Path '{}' has literal a/b directory, using as-is",
                    patch_path
                );
                path
            } else {
                // Neither exists, prefer the stripped version (more likely to be correct for new files)
                log::debug!(
                    "Auto-detected potential git-style prefix in '{}', using '{}' (file doesn't exist yet, may be new file)",
                    patch_path,
                    without_prefix.display()
                );
                without_prefix
            }
        } else {
            // No a/ or b/ prefix found
            path
        }
    };

    // Apply workdir if specified
    let final_path = if let Some(dir) = workdir {
        Path::new(dir).join(stripped)
    } else {
        stripped.to_path_buf()
    };

    // Validate existence
    if !final_path.exists() {
        return Err(format!(
            "File '{}' does not exist. Make sure the patch path is correct.",
            final_path.display()
        ));
    }

    Ok(final_path)
}

/// Format patchkit ApplyError into a user-friendly message
pub fn format_apply_error(error: patchkit::ApplyError, file_path: &Path) -> String {
    match error {
        patchkit::ApplyError::Conflict(msg) => {
            format!(
                "Patch cannot be applied to '{}':\n{}\n\nSuggestion: Use read_tool to examine the current content.",
                file_path.display(),
                msg
            )
        }
        patchkit::ApplyError::Unapplyable => {
            format!(
                "Patch is unapplyable to '{}'\n\nSuggestion: Verify the patch format and file content.",
                file_path.display()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_single_file_patch() {
        let patch = "--- test.txt\n+++ test.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let patches = split_patch_text(patch).unwrap();
        assert_eq!(patches.len(), 1);
    }

    #[test]
    fn test_split_multi_file_patch() {
        let patch = "--- file1.txt\n+++ file1.txt\n@@ -1,1 +1,1 @@\n-old1\n+new1\n--- file2.txt\n+++ file2.txt\n@@ -1,1 +1,1 @@\n-old2\n+new2\n";
        let patches = split_patch_text(patch).unwrap();
        assert_eq!(patches.len(), 2);
    }

    #[test]
    fn test_parse_simple_patch() {
        let patch_text = "--- test.txt\n+++ test.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let patch = parse_single_patch(patch_text).unwrap();
        assert_eq!(patch.mod_name, b"test.txt");
    }

    #[test]
    fn test_parse_patches_multi_file() {
        let patch_text = "--- file1.txt\n+++ file1.txt\n@@ -1,1 +1,1 @@\n-old1\n+new1\n--- file2.txt\n+++ file2.txt\n@@ -1,1 +1,1 @@\n-old2\n+new2\n";
        let patches = parse_patches(patch_text).unwrap();
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].file_path, "file1.txt");
        assert_eq!(patches[1].file_path, "file2.txt");
    }

    #[test]
    fn test_parse_patches_git_diff_blob() {
        let patch_text = "\
diff --git a/test.txt b/test.txt
index 1234567..abcdefg 100644
--- a/test.txt
+++ b/test.txt
@@ -1,2 +1,2 @@
-old
+new
 line 2
";
        let patches = parse_patches(patch_text).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].file_path, "b/test.txt");
    }

    #[test]
    fn test_resolve_file_path_auto_strip_b_prefix() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        // Patch path has b/ prefix, should be auto-stripped
        let result = resolve_file_path("b/test.txt", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_auto_strip_a_prefix() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        // Patch path has a/ prefix, should be auto-stripped
        let result = resolve_file_path("a/test.txt", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_no_prefix() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        // Patch path has no prefix, should be used as-is
        let result = resolve_file_path("test.txt", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_manual_strip_ignores_auto() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        fs::create_dir(temp_dir.path().join("bar")).unwrap();
        let file_path = temp_dir.path().join("bar").join("test.txt");
        fs::write(&file_path, "content").unwrap();

        // Manual strip=2 should strip 2 components: "a" and "foo"
        // Result: bar/test.txt
        let result = resolve_file_path(
            "a/foo/bar/test.txt",
            Some(temp_dir.path().to_str().unwrap()),
            2,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_nested_path_with_b_prefix() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        fs::create_dir(temp_dir.path().join("src")).unwrap();
        let file_path = temp_dir.path().join("src").join("lib.rs");
        fs::write(&file_path, "content").unwrap();

        // Patch path has b/ prefix with nested path
        let result = resolve_file_path("b/src/lib.rs", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_literal_b_directory() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Create a literal "b" directory
        fs::create_dir(temp_dir.path().join("b")).unwrap();
        let file_path = temp_dir.path().join("b").join("test.txt");
        fs::write(&file_path, "content").unwrap();

        // Patch path with b/ should use the literal directory
        let result = resolve_file_path("b/test.txt", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), file_path);
    }

    #[test]
    fn test_resolve_file_path_prefers_stripped_over_literal() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        // Create BOTH a file at root and in b/ directory
        let root_file = temp_dir.path().join("test.txt");
        fs::write(&root_file, "root content").unwrap();
        fs::create_dir(temp_dir.path().join("b")).unwrap();
        let b_file = temp_dir.path().join("b").join("test.txt");
        fs::write(&b_file, "b dir content").unwrap();

        // Should prefer the stripped path (git-style is more common)
        let result = resolve_file_path("b/test.txt", Some(temp_dir.path().to_str().unwrap()), 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), root_file);
    }

    #[test]
    fn test_resolve_file_path_file_not_found() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        // File doesn't exist, should return error
        let result = resolve_file_path(
            "nonexistent.txt",
            Some(temp_dir.path().to_str().unwrap()),
            0,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }
}
