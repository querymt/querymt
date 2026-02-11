//! Shared helper utilities for builtin tools
use crate::config::OverflowStorage;
use std::path::{Path, PathBuf};

/// Maximum lines to return before truncation
pub const MAX_LINES: usize = 2000;

/// Maximum bytes to return before truncation
pub const MAX_BYTES: usize = 51200; // 50 KB

/// Direction for truncation
#[derive(Debug, Clone, Copy)]
pub enum TruncationDirection {
    Head,
    Tail,
}

/// Result of output truncation
pub struct TruncationResult {
    pub content: String,
    pub was_truncated: bool,
    pub original_line_count: usize,
    pub original_byte_count: usize,
}

/// Truncate output based on line count and byte size
pub fn truncate_output(
    content: &str,
    max_lines: usize,
    max_bytes: usize,
    direction: TruncationDirection,
) -> TruncationResult {
    let lines: Vec<&str> = content.lines().collect();
    let original_line_count = lines.len();
    let original_byte_count = content.len();

    let mut was_truncated = false;
    let mut result_lines = lines.clone();

    // Truncate by line count
    if lines.len() > max_lines {
        result_lines = match direction {
            TruncationDirection::Head => lines.iter().take(max_lines).copied().collect(),
            TruncationDirection::Tail => lines
                .iter()
                .skip(lines.len() - max_lines)
                .copied()
                .collect(),
        };
        was_truncated = true;
    }

    // Join and check byte count
    let mut result = result_lines.join("\n");
    if result.len() > max_bytes {
        result = match direction {
            TruncationDirection::Head => result.chars().take(max_bytes).collect::<String>(),
            TruncationDirection::Tail => result.chars().skip(result.len() - max_bytes).collect(),
        };
        was_truncated = true;
    }

    TruncationResult {
        content: result,
        was_truncated,
        original_line_count,
        original_byte_count,
    }
}

/// Format truncation message for user
pub fn format_truncation_message(
    result: &TruncationResult,
    direction: TruncationDirection,
) -> String {
    if !result.was_truncated {
        return String::new();
    }

    let dir_str = match direction {
        TruncationDirection::Head => "first",
        TruncationDirection::Tail => "last",
    };

    format!(
        "\n\n[Output truncated: showing {} {} lines / {} bytes of {} lines / {} bytes total. Use offset/limit parameters to view other sections.]",
        dir_str,
        result.content.lines().count(),
        result.content.len(),
        result.original_line_count,
        result.original_byte_count
    )
}

/// Check if a path is outside the working directory
pub fn is_external_path(path: &Path, cwd: &Path) -> bool {
    // Normalize both paths
    let path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            // If we can't canonicalize (file doesn't exist yet), check parent
            if let Some(parent) = path.parent() {
                match parent.canonicalize() {
                    Ok(p) => p.join(path.file_name().unwrap_or_default()),
                    Err(_) => return true, // Can't verify, assume external
                }
            } else {
                return true;
            }
        }
    };

    let cwd = match cwd.canonicalize() {
        Ok(p) => p,
        Err(_) => return true, // Can't verify cwd, assume external
    };

    !path.starts_with(&cwd)
}

/// Description interpolation helper
pub fn interpolate_description(
    template: &str,
    cwd: &Path,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    template
        .replace("${cwd}", &cwd.display().to_string())
        .replace("${max_lines}", &max_lines.to_string())
        .replace("${max_bytes}", &max_bytes.to_string())
}

// ============================================================================
// Overflow Storage Functions
// ============================================================================

/// Result of saving overflow output
#[derive(Debug, Clone)]
pub struct OverflowSaveResult {
    /// Path where the overflow was saved (if any)
    pub path: Option<PathBuf>,
    /// Error message if save failed (but we don't fail the whole operation)
    pub error: Option<String>,
}

/// Get the directory for overflow storage based on config
pub fn get_overflow_directory(
    storage: &OverflowStorage,
    session_id: &str,
    data_dir: Option<&Path>,
) -> Option<PathBuf> {
    match storage {
        OverflowStorage::Discard => None,
        OverflowStorage::TempDir => {
            let mut path = std::env::temp_dir();
            path.push("qmt-tool-outputs");
            path.push(session_id);
            Some(path)
        }
        OverflowStorage::DataDir => data_dir.map(|d| {
            let mut path = d.to_path_buf();
            path.push("overflow");
            path.push(session_id);
            path
        }),
    }
}

/// Save overflow output to the configured location
///
/// # Arguments
/// * `content` - The full content to save
/// * `storage` - The storage configuration
/// * `session_id` - The session ID (used for directory organization)
/// * `tool_call_id` - The tool call ID (used for filename)
/// * `data_dir` - Optional data directory for DataDir storage
///
/// # Returns
/// An `OverflowSaveResult` with the path where the file was saved, or an error
pub fn save_overflow_output(
    content: &str,
    storage: &OverflowStorage,
    session_id: &str,
    tool_call_id: &str,
    data_dir: Option<&Path>,
) -> OverflowSaveResult {
    let dir = match get_overflow_directory(storage, session_id, data_dir) {
        Some(d) => d,
        None => {
            return OverflowSaveResult {
                path: None,
                error: None,
            };
        }
    };

    // Create the directory if it doesn't exist
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return OverflowSaveResult {
            path: None,
            error: Some(format!("Failed to create overflow directory: {}", e)),
        };
    }

    // Generate a unique filename
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let filename = format!("{}-{}.txt", tool_call_id, timestamp);
    let file_path = dir.join(&filename);

    // Write the content
    match std::fs::write(&file_path, content) {
        Ok(()) => OverflowSaveResult {
            path: Some(file_path),
            error: None,
        },
        Err(e) => OverflowSaveResult {
            path: None,
            error: Some(format!("Failed to write overflow file: {}", e)),
        },
    }
}

/// Format truncation message with optional overflow file path
pub fn format_truncation_message_with_overflow(
    result: &TruncationResult,
    direction: TruncationDirection,
    overflow_result: Option<&OverflowSaveResult>,
    tool_hint: Option<&str>,
) -> String {
    if !result.was_truncated {
        return String::new();
    }

    let dir_str = match direction {
        TruncationDirection::Head => "first",
        TruncationDirection::Tail => "last",
    };

    let overflow_info = match overflow_result {
        Some(OverflowSaveResult { path: Some(p), .. }) => {
            format!(" Full output saved to: {}", p.display())
        }
        Some(OverflowSaveResult { error: Some(e), .. }) => {
            format!(" (Failed to save overflow: {})", e)
        }
        _ => String::new(),
    };

    let hint_str = tool_hint.map(|h| format!("\n\n{}", h)).unwrap_or_default();

    format!(
        "\n\n[Output truncated: showing {} {} lines / {} bytes of {} lines / {} bytes total.{}]{}",
        dir_str,
        result.content.lines().count(),
        result.content.len(),
        result.original_line_count,
        result.original_byte_count,
        overflow_info,
        hint_str
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_output_no_truncation() {
        let content = "line1\nline2\nline3";
        let result = truncate_output(content, 100, 1000, TruncationDirection::Head);

        assert!(!result.was_truncated);
        assert_eq!(result.content, content);
        assert_eq!(result.original_line_count, 3);
    }

    #[test]
    fn test_truncate_output_by_lines_head() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = truncate_output(content, 3, 10000, TruncationDirection::Head);

        assert!(result.was_truncated);
        assert_eq!(result.content, "line1\nline2\nline3");
        assert_eq!(result.original_line_count, 5);
    }

    #[test]
    fn test_truncate_output_by_lines_tail() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = truncate_output(content, 3, 10000, TruncationDirection::Tail);

        assert!(result.was_truncated);
        assert_eq!(result.content, "line3\nline4\nline5");
        assert_eq!(result.original_line_count, 5);
    }

    #[test]
    fn test_truncate_output_by_bytes() {
        let content = "a".repeat(100);
        let result = truncate_output(&content, 1000, 50, TruncationDirection::Head);

        assert!(result.was_truncated);
        assert_eq!(result.content.len(), 50);
        assert_eq!(result.original_byte_count, 100);
    }

    #[test]
    fn test_interpolate_description() {
        let desc = "Current dir: ${cwd}, max lines: ${max_lines}, max bytes: ${max_bytes}";
        let cwd = Path::new("/test/dir");
        let result = interpolate_description(desc, cwd, 100, 2048);

        assert_eq!(
            result,
            "Current dir: /test/dir, max lines: 100, max bytes: 2048"
        );
    }
}
