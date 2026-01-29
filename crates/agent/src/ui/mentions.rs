//! @ mention expansion for file and directory references in prompts.
//!
//! Handles parsing `@{file:path}` and `@{dir:path}` mentions, resolving them
//! to actual files, and building content blocks with attachments.

use crate::index::{FileIndex, FileIndexEntry, WorkspaceIndexManager, resolve_workspace_root};
use agent_client_protocol::{ContentBlock, ImageContent, TextContent};
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Regex for matching file/dir mentions: @{file:path} or @{dir:path}
pub static FILE_MENTION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"@\{(file|dir):([^}]+)\}").unwrap());

/// Build prompt content blocks from text with @ mentions expanded.
/// Returns separate blocks: user text (first), attachments (second if any), images (remaining).
pub async fn build_prompt_blocks(
    workspace_manager: &Arc<WorkspaceIndexManager>,
    cwd: Option<&PathBuf>,
    text: &str,
) -> Vec<ContentBlock> {
    let Some(cwd) = cwd else {
        return vec![ContentBlock::Text(TextContent::new(text.to_string()))];
    };

    let (user_text, attachment_text, image_blocks) =
        expand_prompt_mentions(workspace_manager, cwd, text).await;
    let mut blocks = Vec::new();

    // Block 1: User's message with [file:...] references (clean, for intent snapshots)
    blocks.push(ContentBlock::Text(TextContent::new(user_text)));

    // Block 2: Attachment content (if any) - for LLM context only
    if !attachment_text.is_empty() {
        blocks.push(ContentBlock::Text(TextContent::new(format!(
            "Attachments:\n{}",
            attachment_text
        ))));
    }

    // Blocks 3+: Images
    blocks.extend(image_blocks);

    blocks
}

/// Expand @ mentions in text and return separate components:
/// - user_text: The user's message with [file:...] references (no attachment content)
/// - attachment_text: Joined attachment content (file contents, dir listings, etc.)
/// - image_blocks: Image content blocks
async fn expand_prompt_mentions(
    workspace_manager: &Arc<WorkspaceIndexManager>,
    cwd: &Path,
    text: &str,
) -> (String, String, Vec<ContentBlock>) {
    if !text.contains("@{") {
        return (text.to_string(), String::new(), Vec::new());
    }

    let root = resolve_workspace_root(cwd);
    let index_lookup = build_file_index_lookup(workspace_manager, cwd, &root).await;
    let mut output = String::new();
    let mut attachments = Vec::new();
    let mut blocks = Vec::new();
    let mut seen = HashSet::new();
    let mut last_index = 0;

    for captures in FILE_MENTION_RE.captures_iter(text) {
        let full_match = captures.get(0).unwrap();
        output.push_str(&text[last_index..full_match.start()]);
        last_index = full_match.end();

        let kind = captures.get(1).map(|m| m.as_str()).unwrap_or("file");
        let raw_path = captures.get(2).map(|m| m.as_str()).unwrap_or("").trim();
        if raw_path.is_empty() {
            output.push_str(full_match.as_str());
            continue;
        }

        let expected_is_dir = kind == "dir";
        if let Some(index_lookup) = &index_lookup {
            match index_lookup.get(raw_path) {
                Some(is_dir) if *is_dir == expected_is_dir => {}
                _ => {
                    output.push_str(full_match.as_str());
                    continue;
                }
            }
        }

        let resolved_path = cwd.join(raw_path);
        let resolved_path = match resolved_path.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                output.push_str(full_match.as_str());
                continue;
            }
        };
        if !resolved_path.starts_with(&root) {
            output.push_str(full_match.as_str());
            continue;
        }

        output.push_str(&format!("[{}: {}]", kind, raw_path));

        let seen_key = format!("{}:{}", kind, raw_path);
        if !seen.insert(seen_key) {
            continue;
        }

        if expected_is_dir {
            attachments.push(format_dir_attachment(raw_path, &resolved_path));
            continue;
        }

        let bytes = match std::fs::read(&resolved_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                attachments.push(format!("[file: {}]\n(file could not be read)", raw_path));
                continue;
            }
        };

        if let Some(mime_type) = detect_image_mime(&bytes) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let image = ImageContent::new(encoded, mime_type).uri(raw_path.to_string());
            blocks.push(ContentBlock::Image(image));
            attachments.push(format!("[file: {}]\n(image attached)", raw_path));
            continue;
        }

        match String::from_utf8(bytes) {
            Ok(content) => attachments.push(format!("[file: {}]\n```\n{}\n```", raw_path, content)),
            Err(_) => attachments.push(format!("[file: {}]\n(binary file; not inlined)", raw_path)),
        }
    }

    output.push_str(&text[last_index..]);

    // Don't append attachments to user text - return them separately
    let attachment_content = if !attachments.is_empty() {
        attachments.join("\n\n")
    } else {
        String::new()
    };

    (output, attachment_content, blocks)
}

/// Build a lookup map from the file index for the current working directory.
async fn build_file_index_lookup(
    workspace_manager: &Arc<WorkspaceIndexManager>,
    cwd: &Path,
    root: &Path,
) -> Option<HashMap<String, bool>> {
    let workspace = workspace_manager
        .get_or_create(root.to_path_buf())
        .await
        .ok()?;
    let index = workspace.file_index()?;
    let relative_cwd = cwd.strip_prefix(root).ok()?;
    let entries = filter_index_for_cwd(&index, relative_cwd);
    let mut lookup = HashMap::new();
    for entry in entries {
        lookup.insert(entry.path, entry.is_dir);
    }
    Some(lookup)
}

/// Format a directory listing as an attachment string.
fn format_dir_attachment(display_path: &str, resolved_path: &Path) -> String {
    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(resolved_path) {
        for entry in read_dir.flatten() {
            let file_type = entry.file_type().ok();
            let mut name = entry.file_name().to_string_lossy().to_string();
            if file_type.map(|ft| ft.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            entries.push(name);
        }
    }
    entries.sort();

    if entries.is_empty() {
        return format!("[dir: {}]\n(empty directory)", display_path);
    }

    let listing = entries
        .into_iter()
        .map(|entry| format!("- {}", entry))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[dir: {}]\n{}", display_path, listing)
}

/// Detect if bytes represent a supported image format.
fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    let kind = infer::get(bytes)?;
    match kind.mime_type() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(kind.mime_type()),
        _ => None,
    }
}

/// Filter file index entries to those under the given working directory.
pub fn filter_index_for_cwd(index: &FileIndex, relative_cwd: &Path) -> Vec<FileIndexEntry> {
    if relative_cwd.as_os_str().is_empty() {
        return index.files.clone();
    }

    index
        .files
        .iter()
        .filter_map(|entry| {
            let entry_path = Path::new(&entry.path);
            if !entry_path.starts_with(relative_cwd) {
                return None;
            }

            let relative_path = entry_path.strip_prefix(relative_cwd).ok()?;
            if relative_path.as_os_str().is_empty() {
                return None;
            }

            Some(FileIndexEntry {
                path: relative_path.to_string_lossy().to_string(),
                is_dir: entry.is_dir,
            })
        })
        .collect()
}
