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

/// Render read output for the given path.
///
/// - Text files   → `[Content::Text]` with line-numbered XML-like format
/// - Image files  → `[Content::Image { mime_type, data }]`
/// - Other binary → `[Content::Text]` with a descriptive error message
/// - Directories  → `[Content::Text]` with entry listing
pub async fn render_read_output(
    target: &Path,
    offset: usize,
    limit: usize,
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
                let lines: Vec<&str> = content.lines().collect();
                let total_lines = lines.len();
                let end_idx = (offset + limit).min(total_lines);

                let mut file_content = String::new();
                for (idx, line_content) in lines.iter().enumerate().take(end_idx).skip(offset) {
                    let line_number = idx + 1;
                    file_content.push_str(&format!("{:05}| {}\n", line_number, line_content));
                }

                if end_idx < total_lines {
                    file_content.push_str(&format!(
                        "\n(File has more lines. Use 'offset' parameter to read beyond line {})\n",
                        end_idx
                    ));
                } else {
                    file_content
                        .push_str(&format!("\n(End of file - total {} lines)\n", total_lines));
                }

                return Ok(vec![Content::text(format!(
                    "<path>{}</path>\n<type>file</type>\n<content>\n{}</content>",
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
        let end_idx = (offset + limit).min(total_entries);
        let has_more = end_idx < total_entries;

        let mut entries_output = String::new();
        for (name, is_dir) in entries.iter().take(end_idx).skip(offset) {
            if *is_dir {
                entries_output.push_str(&format!("{}/\n", name));
            } else {
                entries_output.push_str(&format!("{}\n", name));
            }
        }

        let shown = end_idx.saturating_sub(offset.min(total_entries));
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
