use std::path::Path;

pub const DEFAULT_READ_LIMIT: usize = 2000;

/// Render read output in the canonical XML-like format used by read_tool.
pub async fn render_read_output(
    target: &Path,
    offset: usize,
    limit: usize,
) -> Result<String, String> {
    let metadata = tokio::fs::metadata(target)
        .await
        .map_err(|e| format!("stat failed: {}", e))?;

    if metadata.is_file() {
        let content = tokio::fs::read_to_string(target)
            .await
            .map_err(|e| format!("read failed: {}", e))?;

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
            file_content.push_str(&format!("\n(End of file - total {} lines)\n", total_lines));
        }

        return Ok(format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n{}</content>",
            target.display(),
            file_content
        ));
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

        return Ok(format!(
            "<path>{}</path>\n<type>directory</type>\n<entries>\n{}</entries>",
            target.display(),
            entries_output
        ));
    }

    Err("Target is neither a file nor a directory".to_string())
}
