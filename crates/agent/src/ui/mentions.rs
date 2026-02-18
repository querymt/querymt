//! Prompt attachment expansion for ACP ResourceLink references.
//!
//! The UI sends one text block plus ResourceLink blocks for file mentions.
//! This module resolves those links and expands text files into synthetic
//! read-style text chunks in the same user turn.

use super::messages::UiPromptBlock;
use crate::index::{
    FileIndex, FileIndexEntry, GetOrCreate, WorkspaceIndexManagerActor, resolve_workspace_root,
};
use crate::tools::builtins::read_shared::{DEFAULT_READ_LIMIT, render_read_output};
use agent_client_protocol::{ContentBlock, ImageContent, TextContent};
use base64::Engine;
use kameo::actor::ActorRef;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Build prompt content blocks from UI prompt blocks.
///
/// Output layout:
/// - First block: original user text
/// - Then, for each unique ResourceLink path:
///   - text or image: resolved resource payload
pub async fn build_prompt_blocks(
    workspace_manager: &ActorRef<WorkspaceIndexManagerActor>,
    cwd: Option<&PathBuf>,
    prompt: &[UiPromptBlock],
) -> Vec<ContentBlock> {
    let user_text = prompt
        .iter()
        .find_map(|block| match block {
            UiPromptBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let Some(cwd) = cwd else {
        return vec![ContentBlock::Text(TextContent::new(user_text))];
    };

    let root = resolve_workspace_root(cwd);
    let index_lookup = build_file_index_lookup(workspace_manager, cwd, &root).await;

    let mut blocks = vec![ContentBlock::Text(TextContent::new(user_text))];
    let mut seen_paths = HashSet::new();

    for block in prompt {
        let UiPromptBlock::ResourceLink { uri, .. } = block else {
            continue;
        };

        let raw_path = uri.trim();
        if raw_path.is_empty() {
            continue;
        }

        let resolved_path = resolve_resource_path(cwd, &root, raw_path);
        let Some(resolved_path) = resolved_path else {
            continue;
        };

        let canonical_key = resolved_path.display().to_string();
        if !seen_paths.insert(canonical_key) {
            continue;
        }

        let metadata = match std::fs::metadata(&resolved_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };

        if let Some(index_lookup) = &index_lookup
            && !metadata.is_dir()
        {
            let normalized = normalize_for_index(raw_path);
            if let Some(is_dir) = index_lookup.get(&normalized)
                && *is_dir
            {
                continue;
            }
        }

        if metadata.is_dir() {
            if let Ok(output) = render_read_output(&resolved_path, 0, DEFAULT_READ_LIMIT).await {
                blocks.push(ContentBlock::Text(TextContent::new(format!(
                    "[dir: {}]\n{}",
                    resolved_path.display(),
                    output
                ))));
            }
            continue;
        }

        let bytes = match std::fs::read(&resolved_path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };

        if let Some(mime_type) = detect_image_mime(&bytes) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let image = ImageContent::new(encoded, mime_type).uri(raw_path.to_string());
            blocks.push(ContentBlock::Image(image));
            continue;
        }

        if String::from_utf8(bytes).is_ok() {
            if let Ok(output) = render_read_output(&resolved_path, 0, DEFAULT_READ_LIMIT).await {
                blocks.push(ContentBlock::Text(TextContent::new(format!(
                    "[file: {}]\n{}",
                    resolved_path.display(),
                    output
                ))));
            }
        } else {
            blocks.push(ContentBlock::Text(TextContent::new(format!(
                "[file: {}]\n(binary file; not inlined)",
                raw_path
            ))));
        }
    }

    blocks
}

fn resolve_resource_path(cwd: &Path, root: &Path, raw_path: &str) -> Option<PathBuf> {
    let candidate = Path::new(raw_path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };

    let resolved = joined.canonicalize().ok()?;
    if !resolved.starts_with(root) {
        return None;
    }

    Some(resolved)
}

fn normalize_for_index(raw_path: &str) -> String {
    raw_path.trim_start_matches("./").to_string()
}

/// Build a lookup map from the file index for the current working directory.
async fn build_file_index_lookup(
    workspace_manager: &ActorRef<WorkspaceIndexManagerActor>,
    cwd: &Path,
    root: &Path,
) -> Option<HashMap<String, bool>> {
    let handle = workspace_manager
        .ask(GetOrCreate {
            root: root.to_path_buf(),
        })
        .await
        .ok()?;
    let index = handle.file_index()?;
    let relative_cwd = cwd.strip_prefix(root).ok()?;
    let entries = filter_index_for_cwd(&index, relative_cwd);
    let mut lookup = HashMap::new();
    for entry in entries {
        lookup.insert(entry.path, entry.is_dir);
    }
    Some(lookup)
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
