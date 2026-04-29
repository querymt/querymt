use crate::anchors::reconcile::split_lines_preserve_content;
use crate::anchors::store::{reconcile_file, resolve_anchor};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorEditOperation {
    Replace,
    InsertBefore,
    InsertAfter,
    Delete,
}

impl AnchorEditOperation {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "replace" => Ok(Self::Replace),
            "insert_before" => Ok(Self::InsertBefore),
            "insert_after" => Ok(Self::InsertAfter),
            "delete" => Ok(Self::Delete),
            other => Err(format!(
                "Unsupported edit operation '{other}'. Expected replace, insert_before, insert_after, or delete."
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnchorEditRequest {
    pub operation: AnchorEditOperation,
    pub start_anchor: String,
    pub end_anchor: Option<String>,
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchoredDiffLineKind {
    Context,
    Delete,
    Insert,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchoredDiffLine {
    pub kind: AnchoredDiffLineKind,
    pub anchor: String,
    pub text: String,
    #[serde(skip_serializing)]
    old_line: Option<usize>,
    #[serde(skip_serializing)]
    new_line: Option<usize>,
}

impl AnchoredDiffLine {
    pub fn prefix_char(&self) -> char {
        match self.kind {
            AnchoredDiffLineKind::Context => ' ',
            AnchoredDiffLineKind::Delete => '-',
            AnchoredDiffLineKind::Insert => '+',
        }
    }

    pub(crate) fn old_line_number(&self) -> Option<usize> {
        self.old_line
    }

    pub(crate) fn new_line_number(&self) -> Option<usize> {
        self.new_line
    }

    pub fn render(&self) -> String {
        use crate::anchors::render::ANCHOR_DELIMITER;
        format!(
            "{}{}{}{}",
            self.prefix_char(),
            self.anchor,
            ANCHOR_DELIMITER,
            self.text
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchorEditResult {
    pub success: bool,
    pub file: String,
    pub operation: AnchorEditOperation,
    pub lines_deleted: usize,
    pub lines_inserted: usize,
    /// 1-based start line of the replaced/deleted region in the original file.
    pub old_start_line: usize,
    /// Number of lines removed from the original file.
    pub old_line_count: usize,
    /// 1-based start line of the inserted region in the new file.
    pub new_start_line: usize,
    /// Number of lines inserted in the new file.
    pub new_line_count: usize,
    /// Structured contextual hunk showing +/-3 lines around the edit with anchors.
    pub diff_lines: Vec<AnchoredDiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditRange {
    pub start_offset: usize,
    pub end_offset: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAnchorEditResult {
    pub success: bool,
    pub file: String,
    pub total_edits: usize,
    pub results: Vec<AnchorEditResult>,
}

#[derive(Clone, Copy)]
struct DiffBuildContext<'a> {
    edit: &'a ResolvedAnchorEdit,
    old_lines: &'a [&'a str],
    old_state: &'a crate::anchors::store::FileAnchorState,
    new_lines: &'a [&'a str],
    new_state: &'a crate::anchors::store::FileAnchorState,
    new_line_old_numbers: &'a [Option<usize>],
    diff_start: usize,
    diff_end: usize,
    new_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineSource {
    Original(usize),
    Inserted(usize),
}

impl LineSource {
    fn original_line_number(self) -> Option<usize> {
        match self {
            Self::Original(line_number) => Some(line_number),
            Self::Inserted(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedAnchorEdit {
    request: AnchorEditRequest,
    old_range: EditRange,
    insert_at: usize,
    final_insert_at: usize,
    new_lines: Vec<String>,
    original_index: usize,
}

impl ResolvedAnchorEdit {
    fn old_range_len(&self) -> usize {
        self.old_range
            .end_offset
            .saturating_sub(self.old_range.start_offset)
    }
}

pub fn apply_anchor_edit(
    session_id: &str,
    path: &Path,
    content: &str,
    request: AnchorEditRequest,
) -> Result<(String, AnchorEditResult), String> {
    let (new_content, batch_result) = apply_anchor_edits(session_id, path, content, vec![request])?;
    let result = batch_result
        .results
        .into_iter()
        .next()
        .ok_or_else(|| "edit produced no result".to_string())?;
    Ok((new_content, result))
}

pub fn apply_anchor_edits(
    session_id: &str,
    path: &Path,
    content: &str,
    requests: Vec<AnchorEditRequest>,
) -> Result<(String, BatchAnchorEditResult), String> {
    if requests.is_empty() {
        return Err("at least one edit is required".to_string());
    }

    let old_state = reconcile_file(session_id, path, content)?;
    let old_lines: Vec<&str> = split_lines_preserve_content(content);
    let mut line_sources: Vec<LineSource> =
        (0..old_lines.len()).map(LineSource::Original).collect();
    let line_ending = detect_line_ending(content);
    let had_final_newline = content.ends_with('\n');
    let mut lines: Vec<String> = split_lines_preserve_content(content)
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut resolved_edits = requests
        .into_iter()
        .enumerate()
        .map(|(original_index, request)| {
            resolve_edit(session_id, path, content, request, original_index)
        })
        .collect::<Result<Vec<_>, _>>()?;

    reject_overlapping_ranges(&resolved_edits)?;
    resolved_edits.sort_by(|a, b| {
        b.insert_at
            .cmp(&a.insert_at)
            .then_with(|| b.original_index.cmp(&a.original_index))
    });

    for current_idx in 0..resolved_edits.len() {
        let operation = resolved_edits[current_idx].request.operation;
        let old_range = resolved_edits[current_idx].old_range;
        let insert_at = resolved_edits[current_idx].insert_at;
        let inserted_sources = vec![
            LineSource::Inserted(resolved_edits[current_idx].original_index);
            resolved_edits[current_idx].new_lines.len()
        ];
        let delta = line_delta(&resolved_edits[current_idx]);

        match operation {
            AnchorEditOperation::Replace => {
                lines.splice(
                    old_range.start_offset..old_range.end_offset,
                    resolved_edits[current_idx].new_lines.clone(),
                );
                line_sources.splice(
                    old_range.start_offset..old_range.end_offset,
                    inserted_sources,
                );
            }
            AnchorEditOperation::Delete => {
                lines.drain(old_range.start_offset..old_range.end_offset);
                line_sources.drain(old_range.start_offset..old_range.end_offset);
            }
            AnchorEditOperation::InsertBefore | AnchorEditOperation::InsertAfter => {
                lines.splice(
                    insert_at..insert_at,
                    resolved_edits[current_idx].new_lines.clone(),
                );
                line_sources.splice(insert_at..insert_at, inserted_sources);
            }
        }

        for previous_edit in resolved_edits.iter_mut().take(current_idx) {
            if insert_at <= previous_edit.final_insert_at {
                previous_edit.final_insert_at = previous_edit
                    .final_insert_at
                    .checked_add_signed(delta)
                    .unwrap_or(0);
            }
        }
    }

    let new_content = join_lines(&lines, line_ending, had_final_newline);
    let new_state = reconcile_file(session_id, path, &new_content)?;
    let new_lines = split_lines_preserve_content(&new_content);
    let new_line_old_numbers = line_sources
        .iter()
        .map(|source| source.original_line_number())
        .collect::<Vec<_>>();
    let mut indexed_results = resolved_edits
        .iter()
        .map(|edit| {
            (
                edit.original_index,
                build_result(
                    path,
                    edit,
                    &old_lines,
                    &old_state,
                    &new_lines,
                    &new_state,
                    &new_line_old_numbers,
                ),
            )
        })
        .collect::<Vec<_>>();
    indexed_results.sort_by_key(|(original_index, _)| *original_index);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();

    let batch_result = BatchAnchorEditResult {
        success: true,
        file: path.display().to_string(),
        total_edits: results.len(),
        results,
    };

    Ok((new_content, batch_result))
}

fn resolve_edit(
    session_id: &str,
    path: &Path,
    content: &str,
    request: AnchorEditRequest,
    original_index: usize,
) -> Result<ResolvedAnchorEdit, String> {
    let start_line = resolve_anchor(session_id, path, content, &request.start_anchor)?;
    let end_line = match request.operation {
        AnchorEditOperation::Replace | AnchorEditOperation::Delete => {
            if let Some(end_anchor) = request.end_anchor.as_deref() {
                let resolved = resolve_anchor(session_id, path, content, end_anchor)?;
                if resolved < start_line {
                    return Err("endAnchor resolves before startAnchor".to_string());
                }
                resolved
            } else {
                start_line
            }
        }
        AnchorEditOperation::InsertBefore | AnchorEditOperation::InsertAfter => start_line,
    };
    let new_lines = request
        .new_text
        .as_deref()
        .map(split_new_text_lines)
        .unwrap_or_default();

    validate_request(&request, &new_lines)?;

    let old_range = match request.operation {
        AnchorEditOperation::Replace | AnchorEditOperation::Delete => EditRange {
            start_offset: start_line,
            end_offset: end_line + 1,
        },
        AnchorEditOperation::InsertBefore | AnchorEditOperation::InsertAfter => EditRange {
            start_offset: start_line,
            end_offset: start_line,
        },
    };
    let insert_at = match request.operation {
        AnchorEditOperation::Replace | AnchorEditOperation::InsertBefore => start_line,
        AnchorEditOperation::InsertAfter => start_line + 1,
        AnchorEditOperation::Delete => start_line,
    };

    Ok(ResolvedAnchorEdit {
        request,
        old_range,
        insert_at,
        final_insert_at: insert_at,
        new_lines,
        original_index,
    })
}

fn reject_overlapping_ranges(edits: &[ResolvedAnchorEdit]) -> Result<(), String> {
    let mut ranges = edits
        .iter()
        .filter(|edit| {
            matches!(
                edit.request.operation,
                AnchorEditOperation::Replace | AnchorEditOperation::Delete
            )
        })
        .map(|edit| (edit.original_index, edit.old_range))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|(_, range)| (range.start_offset, range.end_offset));

    for pair in ranges.windows(2) {
        let (left_idx, left) = pair[0];
        let (right_idx, right) = pair[1];
        if left.end_offset > right.start_offset {
            return Err(format!(
                "Overlapping replace/delete ranges in edits {left_idx} and {right_idx}"
            ));
        }
    }

    Ok(())
}

fn build_result(
    path: &Path,
    edit: &ResolvedAnchorEdit,
    old_lines: &[&str],
    old_state: &crate::anchors::store::FileAnchorState,
    new_lines: &[&str],
    new_state: &crate::anchors::store::FileAnchorState,
    new_line_old_numbers: &[Option<usize>],
) -> AnchorEditResult {
    let lines_deleted = edit
        .old_range
        .end_offset
        .saturating_sub(edit.old_range.start_offset);
    let lines_inserted = edit.new_lines.len();

    let new_end = match edit.request.operation {
        AnchorEditOperation::Delete => edit.final_insert_at,
        _ => edit.final_insert_at + lines_inserted,
    };

    // Build a contextual diff with +/-3 lines around the edit region,
    // showing the new anchors so the LLM can immediately make follow-up edits.
    const CONTEXT: usize = 3;
    let diff_start = edit.final_insert_at.saturating_sub(CONTEXT);
    let diff_end = (new_end + CONTEXT).min(new_lines.len());

    let diff_lines = build_diff_lines(DiffBuildContext {
        edit,
        old_lines,
        old_state,
        new_lines,
        new_state,
        new_line_old_numbers,
        diff_start,
        diff_end,
        new_end,
    });

    AnchorEditResult {
        success: true,
        file: path.display().to_string(),
        operation: edit.request.operation,
        lines_deleted,
        lines_inserted,
        old_start_line: edit.old_range.start_offset + 1,
        old_line_count: lines_deleted,
        new_start_line: edit.final_insert_at + 1,
        new_line_count: lines_inserted,
        diff_lines,
    }
}

#[derive(Clone, Copy)]
struct DiffRegionLine<'a> {
    anchor: &'a str,
    text: &'a str,
    line_number: usize,
}

fn build_middle_diff_lines(
    old_region: &[DiffRegionLine<'_>],
    new_region: &[DiffRegionLine<'_>],
) -> Vec<AnchoredDiffLine> {
    let old_len = old_region.len();
    let new_len = new_region.len();
    let mut lcs = vec![vec![0usize; new_len + 1]; old_len + 1];

    for old_idx in (0..old_len).rev() {
        for new_idx in (0..new_len).rev() {
            if lines_match(old_region[old_idx], new_region[new_idx]) {
                lcs[old_idx][new_idx] = lcs[old_idx + 1][new_idx + 1] + 1;
            } else {
                lcs[old_idx][new_idx] = lcs[old_idx + 1][new_idx].max(lcs[old_idx][new_idx + 1]);
            }
        }
    }

    let mut diff_lines = Vec::new();
    let mut old_idx = 0;
    let mut new_idx = 0;
    while old_idx < old_len && new_idx < new_len {
        if lines_match(old_region[old_idx], new_region[new_idx]) {
            diff_lines.push(AnchoredDiffLine {
                kind: AnchoredDiffLineKind::Context,
                anchor: new_region[new_idx].anchor.to_string(),
                text: new_region[new_idx].text.to_string(),
                old_line: Some(old_region[old_idx].line_number),
                new_line: Some(new_region[new_idx].line_number),
            });
            old_idx += 1;
            new_idx += 1;
        } else if lcs[old_idx + 1][new_idx] >= lcs[old_idx][new_idx + 1] {
            diff_lines.push(AnchoredDiffLine {
                kind: AnchoredDiffLineKind::Delete,
                anchor: old_region[old_idx].anchor.to_string(),
                text: old_region[old_idx].text.to_string(),
                old_line: Some(old_region[old_idx].line_number),
                new_line: None,
            });
            old_idx += 1;
        } else {
            diff_lines.push(AnchoredDiffLine {
                kind: AnchoredDiffLineKind::Insert,
                anchor: new_region[new_idx].anchor.to_string(),
                text: new_region[new_idx].text.to_string(),
                old_line: None,
                new_line: Some(new_region[new_idx].line_number),
            });
            new_idx += 1;
        }
    }

    while old_idx < old_len {
        diff_lines.push(AnchoredDiffLine {
            kind: AnchoredDiffLineKind::Delete,
            anchor: old_region[old_idx].anchor.to_string(),
            text: old_region[old_idx].text.to_string(),
            old_line: Some(old_region[old_idx].line_number),
            new_line: None,
        });
        old_idx += 1;
    }

    while new_idx < new_len {
        diff_lines.push(AnchoredDiffLine {
            kind: AnchoredDiffLineKind::Insert,
            anchor: new_region[new_idx].anchor.to_string(),
            text: new_region[new_idx].text.to_string(),
            old_line: None,
            new_line: Some(new_region[new_idx].line_number),
        });
        new_idx += 1;
    }

    diff_lines
}

fn lines_match(old_line: DiffRegionLine<'_>, new_line: DiffRegionLine<'_>) -> bool {
    old_line.anchor == new_line.anchor || old_line.text == new_line.text
}

fn build_diff_lines(ctx: DiffBuildContext<'_>) -> Vec<AnchoredDiffLine> {
    let DiffBuildContext {
        edit,
        old_lines,
        old_state,
        new_lines,
        new_state,
        new_line_old_numbers,
        diff_start,
        diff_end,
        new_end,
    } = ctx;
    let mut diff_lines = Vec::new();

    for (i, line) in new_lines
        .iter()
        .enumerate()
        .take(edit.final_insert_at)
        .skip(diff_start)
    {
        if let Some(anchor) = new_state.lines.get(i) {
            diff_lines.push(AnchoredDiffLine {
                kind: AnchoredDiffLineKind::Context,
                anchor: anchor.anchor.clone(),
                text: (*line).to_string(),
                old_line: new_line_old_numbers.get(i).copied().flatten(),
                new_line: Some(i),
            });
        }
    }

    let old_region = old_lines
        .iter()
        .enumerate()
        .take(edit.old_range.end_offset)
        .skip(edit.old_range.start_offset)
        .filter_map(|(i, line)| {
            old_state.lines.get(i).map(|anchor| DiffRegionLine {
                anchor: anchor.anchor.as_str(),
                text: line,
                line_number: i,
            })
        })
        .collect::<Vec<_>>();
    let new_region = new_lines
        .iter()
        .enumerate()
        .take(new_end)
        .skip(edit.final_insert_at)
        .filter_map(|(i, line)| {
            new_state.lines.get(i).map(|anchor| DiffRegionLine {
                anchor: anchor.anchor.as_str(),
                text: line,
                line_number: i,
            })
        })
        .collect::<Vec<_>>();

    diff_lines.extend(build_middle_diff_lines(&old_region, &new_region));

    for (i, line) in new_lines.iter().enumerate().take(diff_end).skip(new_end) {
        if let Some(anchor) = new_state.lines.get(i) {
            diff_lines.push(AnchoredDiffLine {
                kind: AnchoredDiffLineKind::Context,
                anchor: anchor.anchor.clone(),
                text: (*line).to_string(),
                old_line: new_line_old_numbers.get(i).copied().flatten(),
                new_line: Some(i),
            });
        }
    }

    diff_lines
}

fn line_delta(edit: &ResolvedAnchorEdit) -> isize {
    match edit.request.operation {
        AnchorEditOperation::Replace => {
            edit.new_lines.len() as isize - edit.old_range_len() as isize
        }
        AnchorEditOperation::Delete => -(edit.old_range_len() as isize),
        AnchorEditOperation::InsertBefore | AnchorEditOperation::InsertAfter => {
            edit.new_lines.len() as isize
        }
    }
}

fn validate_request(request: &AnchorEditRequest, new_lines: &[String]) -> Result<(), String> {
    match request.operation {
        AnchorEditOperation::Replace
        | AnchorEditOperation::InsertBefore
        | AnchorEditOperation::InsertAfter => {
            if request.new_text.is_none() {
                return Err("newText is required for replace and insert operations".to_string());
            }
        }
        AnchorEditOperation::Delete => {}
    }

    if matches!(request.operation, AnchorEditOperation::Replace)
        && new_lines.len() == 1
        && new_lines[0].is_empty()
    {
        return Err("replace newText must not be empty; use delete instead".to_string());
    }

    Ok(())
}

fn split_new_text_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    split_lines_preserve_content(text)
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn join_lines(lines: &[String], line_ending: &str, final_newline: bool) -> String {
    let mut content = lines.join(line_ending);
    if final_newline && !content.is_empty() {
        content.push_str(line_ending);
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::store::clear_anchor_store_for_tests;
    use std::fs;
    use tempfile::TempDir;

    fn anchor_for_line(session_id: &str, path: &Path, content: &str, line: usize) -> String {
        reconcile_file(session_id, path, content).unwrap().lines[line]
            .anchor
            .clone()
    }

    #[test]
    fn replaces_one_line_and_reconciles() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "a\nb\nc\n";
        fs::write(&path, content).unwrap();
        let anchor = anchor_for_line("s", &path, content, 1);

        let (new_content, result) = apply_anchor_edit(
            "s",
            &path,
            content,
            AnchorEditRequest {
                operation: AnchorEditOperation::Replace,
                start_anchor: anchor,
                end_anchor: None,
                new_text: Some("B".to_string()),
            },
        )
        .unwrap();

        assert_eq!(new_content, "a\nB\nc\n");
        assert_eq!(result.lines_deleted, 1);
        assert_eq!(result.lines_inserted, 1);
        assert!(!result.diff_lines.is_empty());
        assert!(
            result
                .diff_lines
                .iter()
                .any(|line| line.anchor.contains(|c: char| c.is_ascii_alphanumeric()))
        );
    }

    #[test]
    fn preserves_retained_context_order_within_replace_region() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "a\nb\nkeep\nd\ne\n";
        fs::write(&path, content).unwrap();
        let start_anchor = anchor_for_line("s", &path, content, 1);
        let end_anchor = anchor_for_line("s", &path, content, 3);

        let (_, result) = apply_anchor_edit(
            "s",
            &path,
            content,
            AnchorEditRequest {
                operation: AnchorEditOperation::Replace,
                start_anchor,
                end_anchor: Some(end_anchor),
                new_text: Some("B\nkeep\nD".to_string()),
            },
        )
        .unwrap();

        let rendered = result
            .diff_lines
            .iter()
            .map(|line| format!("{}{}", line.prefix_char(), line.text))
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                " a".to_string(),
                "-b".to_string(),
                "+B".to_string(),
                " keep".to_string(),
                "-d".to_string(),
                "+D".to_string(),
                " e".to_string(),
            ]
        );
    }

    #[test]
    fn preserves_crlf_and_final_newline() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        let content = "a\r\nb\r\n";
        fs::write(&path, content).unwrap();
        let anchor = anchor_for_line("s", &path, content, 1);

        let (new_content, _) = apply_anchor_edit(
            "s",
            &path,
            content,
            AnchorEditRequest {
                operation: AnchorEditOperation::InsertAfter,
                start_anchor: anchor,
                end_anchor: None,
                new_text: Some("c".to_string()),
            },
        )
        .unwrap();

        assert_eq!(new_content, "a\r\nb\r\nc\r\n");
    }

    #[test]
    fn batch_results_track_final_positions_for_nearby_edits() {
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

        let (new_content, batch_result) = apply_anchor_edits(
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

        assert_eq!(
            new_content,
            concat!(
                "before\n",
                "\n",
                "\n",
                "#[derive(Clone, Copy)]\n",
                "struct DiffRegionLine<'a> {\n",
                "    anchor: &'a str,\n",
                "    text: &'a str,\n",
                "    line_number: usize,\n",
                "}\n",
                "after\n"
            )
        );
        assert_eq!(batch_result.results[1].new_start_line, 4);
        let rendered = batch_result.results[1]
            .diff_lines
            .iter()
            .map(|line| format!("{}{}", line.prefix_char(), line.text))
            .collect::<Vec<_>>();
        assert!(rendered.contains(&"+#[derive(Clone, Copy)]".to_string()));
        assert!(rendered.contains(&" struct DiffRegionLine<'a> {".to_string()));
    }

    #[test]
    fn session_shaped_nearby_delete_and_insert_before_stay_locally_coherent() {
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

        let delete_rendered = batch_result.results[0]
            .diff_lines
            .iter()
            .map(|line| format!("{}{}", line.prefix_char(), line.text))
            .collect::<Vec<_>>();
        assert_eq!(
            delete_rendered,
            vec![
                " head".to_string(),
                " keep before".to_string(),
                " ".to_string(),
                "-struct DiffRegionLine<'a> {".to_string(),
                "-    anchor: &'a str,".to_string(),
                "-    text: &'a str,".to_string(),
                "-    line_number: usize,".to_string(),
                "-}".to_string(),
                " ".to_string(),
                " #[derive(Clone, Copy)]".to_string(),
                " struct DiffRegionLine<'a> {".to_string(),
            ]
        );

        let insert_rendered = batch_result.results[1]
            .diff_lines
            .iter()
            .map(|line| format!("{}{}", line.prefix_char(), line.text))
            .collect::<Vec<_>>();
        assert_eq!(
            insert_rendered,
            vec![
                " keep before".to_string(),
                " ".to_string(),
                " ".to_string(),
                "+#[derive(Clone, Copy)]".to_string(),
                " struct DiffRegionLine<'a> {".to_string(),
                "     anchor: &'a str,".to_string(),
                "     text: &'a str,".to_string(),
            ]
        );
    }
}
