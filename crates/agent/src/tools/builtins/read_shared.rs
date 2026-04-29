use crate::anchors::{FileAnchorState, reconcile_file, render_anchored_range, resolve_anchor};
use querymt::chat::Content;
use std::path::Path;

pub const DEFAULT_READ_LIMIT: usize = 2000;

/// Maximum file size (bytes) we will read into memory for binary content.
/// Files larger than this get a descriptive error instead.
const MAX_BINARY_READ_BYTES: u64 = 20 * 1024 * 1024; // 20 MiB

/// Detect if bytes represent a supported image format.
pub(crate) fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    let kind = infer::get(bytes)?;
    match kind.mime_type() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(kind.mime_type()),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReadRange {
    pub offset: usize,
    pub limit: usize,
    pub start_anchor: Option<String>,
    pub end_anchor: Option<String>,
    pub before: usize,
    pub after: Option<usize>,
}

/// Render read output for the given path.
///
/// - Text files   -> `[Content::Text]` with anchored XML-like format
/// - Image files  -> `[Content::Image { mime_type, data }]`
/// - Other binary -> `[Content::Text]` with a descriptive error message
/// - Directories  -> `[Content::Text]` with entry listing
pub async fn render_read_output(
    session_id: &str,
    target: &Path,
    range: ReadRange,
) -> Result<Vec<Content>, String> {
    let metadata = tokio::fs::metadata(target)
        .await
        .map_err(|e| format!("stat failed: {}", e))?;

    if metadata.is_file() {
        // Guard against loading enormous binaries into memory.
        if metadata.len() > MAX_BINARY_READ_BYTES {
            return Ok(vec![Content::text(format!(
                "File too large to read inline ({} bytes, limit {} bytes). \
                 Use an external tool to process this file.",
                metadata.len(),
                MAX_BINARY_READ_BYTES,
            ))]);
        }

        let bytes = tokio::fs::read(target)
            .await
            .map_err(|e| format!("read failed: {}", e))?;

        // Binary detection: images get returned as rich content blocks.
        if let Some(mime_type) = detect_image_mime(&bytes) {
            return Ok(vec![Content::image(mime_type, bytes)]);
        }

        // Not a recognised binary format — try to interpret as UTF-8 text.
        match String::from_utf8(bytes) {
            Ok(content) => {
                let state = reconcile_file(session_id, target, &content)?;
                let (offset, limit) =
                    resolve_read_range(session_id, target, &content, &state, &range)?;
                let file_content = render_anchored_range(&content, &state, offset, limit);

                return Ok(vec![Content::text(format!(
                    "<path>{}</path>\n<type>file</type>\n{}",
                    target.display(),
                    file_content
                ))]);
            }
            Err(_) => {
                return Ok(vec![Content::text(format!(
                    "Binary file '{}'; not a supported format (image/text). \
                     Use an external tool to process this file.",
                    target.display(),
                ))]);
            }
        }
    }

    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(target)
            .await
            .map_err(|e| format!("read dir failed: {}", e))?;

        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| format!("read dir entry failed: {}", e))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry
                .file_type()
                .await
                .map_err(|e| format!("stat dir entry failed: {}", e))?
                .is_dir();
            entries.push((name, is_dir));
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let total_entries = entries.len();
        let end_idx = (range.offset + range.limit).min(total_entries);
        let has_more = end_idx < total_entries;

        let mut entries_output = String::new();
        for (name, is_dir) in entries.iter().take(end_idx).skip(range.offset) {
            if *is_dir {
                entries_output.push_str(&format!("{}/\n", name));
            } else {
                entries_output.push_str(&format!("{}\n", name));
            }
        }

        let shown = end_idx.saturating_sub(range.offset.min(total_entries));
        entries_output.push_str(&format!("({} entries)\n", shown));
        if has_more {
            entries_output.push_str("(More entries available. Use a higher offset.)\n");
        }

        return Ok(vec![Content::text(format!(
            "<path>{}</path>\n<type>directory</type>\n<entries>\n{}</entries>",
            target.display(),
            entries_output
        ))]);
    }

    Err("Target is neither a file nor a directory".to_string())
}

fn resolve_read_range(
    session_id: &str,
    target: &Path,
    content: &str,
    state: &FileAnchorState,
    range: &ReadRange,
) -> Result<(usize, usize), String> {
    let Some(start_anchor) = range.start_anchor.as_deref() else {
        return Ok((range.offset, range.limit));
    };

    let anchor_line = resolve_anchor(session_id, target, content, start_anchor)?;
    let start = anchor_line.saturating_sub(range.before);
    let end_exclusive = if let Some(end_anchor) = range.end_anchor.as_deref() {
        let end_line = resolve_anchor(session_id, target, content, end_anchor)?;
        if end_line < start {
            return Err("end_anchor resolves before start_anchor/before range".to_string());
        }
        end_line + 1
    } else if let Some(after) = range.after {
        (anchor_line + after + 1).min(state.line_count)
    } else {
        (start + range.limit).min(state.line_count)
    };

    Ok((start, end_exclusive.saturating_sub(start).max(1)))
}
