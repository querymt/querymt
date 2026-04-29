use crate::anchors::reconcile::split_lines_preserve_content;
use crate::anchors::store::FileAnchorState;

pub const ANCHOR_DELIMITER: char = '§';

pub fn render_anchored_range(
    content: &str,
    state: &FileAnchorState,
    offset: usize,
    limit: usize,
) -> String {
    let lines = split_lines_preserve_content(content);
    let total_lines = lines.len();
    let start_idx = offset.min(total_lines);
    let end_idx = offset.saturating_add(limit).min(total_lines);

    let mut rendered = String::new();
    rendered.push_str(&format!(
        "<range start_offset=\"{}\" end_offset=\"{}\" total_lines=\"{}\"/>\n",
        start_idx, end_idx, total_lines
    ));
    rendered.push_str("<content>\n");

    for (idx, line) in lines.iter().enumerate().take(end_idx).skip(start_idx) {
        if let Some(anchor) = state.lines.get(idx) {
            rendered.push_str(&anchor.anchor);
            rendered.push(ANCHOR_DELIMITER);
            rendered.push_str(line);
            rendered.push('\n');
        }
    }

    if end_idx < total_lines {
        rendered.push_str(&format!(
            "\n(File has more lines. Use 'offset' parameter to read beyond line {})\n",
            end_idx
        ));
    } else {
        rendered.push_str(&format!("\n(End of file - total {} lines)\n", total_lines));
    }

    rendered.push_str("</content>");
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::store::{clear_anchor_store_for_tests, reconcile_file};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn renders_anchor_delimited_lines_and_range_metadata() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "one\ntwo\nthree\n";
        fs::write(&path, content).unwrap();
        let state = reconcile_file("session", &path, content).unwrap();

        let rendered = render_anchored_range(content, &state, 1, 1);

        assert!(
            rendered.contains("<range start_offset=\"1\" end_offset=\"2\" total_lines=\"3\"/>")
        );
        assert!(rendered.contains(&format!("{}§two", state.lines[1].anchor)));
        assert!(!rendered.contains("00002|"));
    }
}
