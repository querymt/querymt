use crate::anchors::edit::{
    AnchorEditOperation, AnchorEditResult, AnchoredDiffLine, AnchoredDiffLineKind,
    BatchAnchorEditResult,
};

#[derive(Debug, Clone)]
pub struct FileEditOutput {
    pub path: String,
    pub hunks: Vec<HunkOutput>,
}

#[derive(Debug, Clone)]
pub struct HunkOutput {
    pub operation: AnchorEditOperation,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<String>,
    pub lines_deleted: usize,
    pub lines_inserted: usize,
}

pub fn build_file_output(path: String, batch: &BatchAnchorEditResult) -> FileEditOutput {
    let hunks = batch.results.iter().map(build_hunk_output).collect();

    FileEditOutput { path, hunks }
}

fn build_hunk_output(result: &AnchorEditResult) -> HunkOutput {
    let lines = result
        .diff_lines
        .iter()
        .map(|line| line.render())
        .collect::<Vec<_>>();
    let old_count = result
        .diff_lines
        .iter()
        .filter(|line| {
            matches!(
                line.kind,
                AnchoredDiffLineKind::Context | AnchoredDiffLineKind::Delete
            )
        })
        .count();
    let new_count = result
        .diff_lines
        .iter()
        .filter(|line| {
            matches!(
                line.kind,
                AnchoredDiffLineKind::Context | AnchoredDiffLineKind::Insert
            )
        })
        .count();
    let old_start = hunk_start(
        &result.diff_lines,
        |line| line.old_line_number(),
        result.old_start_line.saturating_sub(1),
    );
    let new_start = hunk_start(
        &result.diff_lines,
        |line| line.new_line_number(),
        result.new_start_line.saturating_sub(1),
    );

    HunkOutput {
        operation: result.operation,
        old_start,
        old_count,
        new_start,
        new_count,
        lines,
        lines_deleted: result.lines_deleted,
        lines_inserted: result.lines_inserted,
    }
}

fn hunk_start(
    diff_lines: &[AnchoredDiffLine],
    line_number: impl Fn(&AnchoredDiffLine) -> Option<usize>,
    fallback: usize,
) -> usize {
    diff_lines.iter().find_map(line_number).unwrap_or(fallback)
}

pub fn format_output(results: &[FileEditOutput]) -> String {
    let total_paths = results.len();
    let total_edits = results
        .iter()
        .map(|result| result.hunks.len())
        .sum::<usize>();
    let total_added = results
        .iter()
        .flat_map(|result| result.hunks.iter())
        .map(|hunk| hunk.lines_inserted)
        .sum::<usize>();
    let total_deleted = results
        .iter()
        .flat_map(|result| result.hunks.iter())
        .map(|hunk| hunk.lines_deleted)
        .sum::<usize>();

    let mut lines = Vec::new();
    lines.push(format!(
        "OK paths={total_paths} edits={total_edits} added={total_added} deleted={total_deleted} anchors=fresh"
    ));

    for result in results {
        lines.push(format!("P {}", result.path));
        for hunk in &result.hunks {
            let op_str = operation_label(hunk.operation);
            lines.push(format!(
                "H {} old={},{} new={},{}",
                op_str,
                hunk.old_start + 1,
                hunk.old_count,
                hunk.new_start + 1,
                hunk.new_count,
            ));
            lines.extend(hunk.lines.iter().cloned());
        }
    }

    lines.join("\n")
}

fn operation_label(operation: AnchorEditOperation) -> &'static str {
    match operation {
        AnchorEditOperation::Replace => "replace",
        AnchorEditOperation::InsertBefore => "insert_before",
        AnchorEditOperation::InsertAfter => "insert_after",
        AnchorEditOperation::Delete => "delete",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::edit::{AnchorEditRequest, apply_anchor_edit, apply_anchor_edits};
    use crate::anchors::store::clear_anchor_store_for_tests;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn anchor_for_line(session_id: &str, path: &Path, content: &str, line: usize) -> String {
        crate::anchors::store::reconcile_file(session_id, path, content)
            .unwrap()
            .lines[line]
            .anchor
            .clone()
    }

    #[test]
    fn insert_after_uses_anchor_line_for_compact_hunk_header() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "a\nb\nc\nd\ne\n";
        fs::write(&path, content).unwrap();
        let anchor = anchor_for_line("s", &path, content, 1);

        let (_, result) = apply_anchor_edit(
            "s",
            &path,
            content,
            AnchorEditRequest {
                operation: AnchorEditOperation::InsertAfter,
                start_anchor: anchor,
                end_anchor: None,
                new_text: Some("bb".to_string()),
            },
        )
        .unwrap();

        let batch = BatchAnchorEditResult {
            success: true,
            file: path.display().to_string(),
            total_edits: 1,
            results: vec![result],
        };
        let output = format_output(&[build_file_output("file.txt".to_string(), &batch)]);
        assert!(output.starts_with("OK paths=1 edits=1 added=1 deleted=0 anchors=fresh"));

        assert!(output.contains("H insert_after old=1,5 new=1,6"));
    }

    #[test]
    fn delete_keeps_compact_hunk_header_aligned_with_context() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "a\nb\nc\nd\ne\n";
        fs::write(&path, content).unwrap();
        let start_anchor = anchor_for_line("s", &path, content, 1);
        let end_anchor = anchor_for_line("s", &path, content, 2);

        let (_, result) = apply_anchor_edit(
            "s",
            &path,
            content,
            AnchorEditRequest {
                operation: AnchorEditOperation::Delete,
                start_anchor,
                end_anchor: Some(end_anchor),
                new_text: None,
            },
        )
        .unwrap();

        let batch = BatchAnchorEditResult {
            success: true,
            file: path.display().to_string(),
            total_edits: 1,
            results: vec![result],
        };
        let output = format_output(&[build_file_output("file.txt".to_string(), &batch)]);

        assert!(output.contains("H delete old=1,5 new=1,3"));
    }

    #[test]
    fn nearby_multiedit_headers_use_final_positions() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = concat!(
            "before\n",
            "\n",
            "struct DiffRegionLine<'a> {\n",
            "    anchor: &'a str,\n",
            "    text: &'a str,\n",
            "    line_number: usize,\n",
            "}\n",
            "\n",
            "struct DiffRegionLine<'a> {\n",
            "    anchor: &'a str,\n",
            "    text: &'a str,\n",
            "    line_number: usize,\n",
            "}\n",
            "after\n"
        );
        fs::write(&path, content).unwrap();
        let delete_start = anchor_for_line("s", &path, content, 2);
        let delete_end = anchor_for_line("s", &path, content, 6);
        let insert_anchor = anchor_for_line("s", &path, content, 8);

        let (_, batch_result) = apply_anchor_edits(
            "s",
            &path,
            content,
            vec![
                AnchorEditRequest {
                    operation: AnchorEditOperation::Delete,
                    start_anchor: delete_start,
                    end_anchor: Some(delete_end),
                    new_text: None,
                },
                AnchorEditRequest {
                    operation: AnchorEditOperation::InsertBefore,
                    start_anchor: insert_anchor,
                    end_anchor: None,
                    new_text: Some("#[derive(Clone, Copy)]".to_string()),
                },
            ],
        )
        .unwrap();

        let output = format_output(&[build_file_output("file.txt".to_string(), &batch_result)]);
        assert!(output.contains("H delete old=1,10 new=1,5"));
        assert!(output.contains("H insert_before old=1,6 new=1,7"));
    }

    #[test]
    fn session_shaped_nearby_multiedit_output_matches_expected_hunks() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("edit.rs");
        let content = concat!(
            "head\n",
            "keep before\n",
            "\n",
            "struct DiffRegionLine<'a> {\n",
            "    anchor: &'a str,\n",
            "    text: &'a str,\n",
            "    line_number: usize,\n",
            "}\n",
            "\n",
            "struct DiffRegionLine<'a> {\n",
            "    anchor: &'a str,\n",
            "    text: &'a str,\n",
            "    line_number: usize,\n",
            "}\n",
            "tail after\n"
        );
        fs::write(&path, content).unwrap();
        let delete_start = anchor_for_line("s", &path, content, 3);
        let delete_end = anchor_for_line("s", &path, content, 7);
        let insert_anchor = anchor_for_line("s", &path, content, 9);

        let (_, batch_result) = apply_anchor_edits(
            "s",
            &path,
            content,
            vec![
                AnchorEditRequest {
                    operation: AnchorEditOperation::Delete,
                    start_anchor: delete_start,
                    end_anchor: Some(delete_end),
                    new_text: None,
                },
                AnchorEditRequest {
                    operation: AnchorEditOperation::InsertBefore,
                    start_anchor: insert_anchor,
                    end_anchor: None,
                    new_text: Some("#[derive(Clone, Copy)]".to_string()),
                },
            ],
        )
        .unwrap();

        let output = format_output(&[build_file_output("edit.rs".to_string(), &batch_result)]);
        assert!(output.contains("H delete old=1,11 new=1,6"));
        assert!(output.contains("H insert_before old=2,6 new=2,7"));
        assert!(output.contains("#[derive(Clone, Copy)]"));
        assert!(output.contains("struct DiffRegionLine<'a> {"));
    }
}
