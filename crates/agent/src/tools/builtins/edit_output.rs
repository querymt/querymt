//!
//! Output format:
//! ```text
//! OK paths=1 edits=1 added=1 deleted=1
//! P src/file.rs
//! H replace old=1,3 new=1,3
//!  00001| context before
//! -00002| old line
//! +00002| new line
//!  00003| context after
//! ```

use std::ops::Range;
use std::path::Path;

use imara_diff::{Algorithm, Diff, InternedInput};

const CONTEXT_LINES: usize = 3;

/// A single line in a diff hunk.
#[derive(Debug, Clone)]
pub enum DiffLineKind {
    Context,
    Delete,
    Insert,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// 1-based line number in the old (original) file, or None for inserted lines.
    pub old_line: Option<usize>,
    /// 1-based line number in the new file, or None for deleted lines.
    pub new_line: Option<usize>,
}

impl DiffLine {
    pub fn render(&self) -> String {
        let line_num = match self.kind {
            DiffLineKind::Context | DiffLineKind::Delete => {
                format!("{:05}", self.old_line.unwrap_or(0))
            }
            DiffLineKind::Insert => format!("{:05}", self.new_line.unwrap_or(0)),
        };
        let prefix = match self.kind {
            DiffLineKind::Context => ' ',
            DiffLineKind::Delete => '-',
            DiffLineKind::Insert => '+',
        };
        format!("{}{}| {}", prefix, line_num, self.text)
    }
}

#[derive(Debug, Clone)]
pub struct HunkOutput {
    pub operation: &'static str,
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
    pub lines_deleted: usize,
    pub lines_inserted: usize,
}

#[derive(Debug, Clone)]
pub struct FileEditOutput {
    pub path: String,
    pub hunks: Vec<HunkOutput>,
}

#[derive(Debug, Clone)]
struct ChangeRange {
    old_range: Range<usize>,
    new_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct HunkRegion {
    old_range: Range<usize>,
    new_range: Range<usize>,
    changes: Vec<ChangeRange>,
}

/// Build hunks from original and new content, detecting the changed region.
///
/// The old/new span arguments are kept for backward compatibility with callers,
/// but the rendered hunk is computed from an actual line diff so unchanged lines
/// inside a matched replacement block are emitted as context instead of delete/insert.
pub fn build_replace_hunk(
    original: &str,
    new: &str,
    _old_start_line: usize,
    _old_line_count: usize,
    _new_line_count: usize,
) -> HunkOutput {
    build_hunks_from_diff(original, new)
        .into_iter()
        .next()
        .unwrap_or_else(empty_hunk)
}

/// Format one or more file edit results into the compact output string.
pub fn format_output(results: &[FileEditOutput]) -> String {
    let total_paths = results.len();
    let total_edits = results.iter().map(|r| r.hunks.len()).sum::<usize>();
    let total_added = results
        .iter()
        .flat_map(|r| r.hunks.iter())
        .map(|h| h.lines_inserted)
        .sum::<usize>();
    let total_deleted = results
        .iter()
        .flat_map(|r| r.hunks.iter())
        .map(|h| h.lines_deleted)
        .sum::<usize>();

    let mut lines = Vec::new();
    lines.push(format!(
        "OK paths={total_paths} edits={total_edits} added={total_added} deleted={total_deleted}"
    ));

    for result in results {
        lines.push(format!("P {}", result.path));
        for hunk in &result.hunks {
            lines.push(format!(
                "H {} old={},{} new={},{}",
                hunk.operation, hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count,
            ));
            for dl in &hunk.lines {
                lines.push(dl.render());
            }
        }
    }

    lines.join("\n")
}

/// Build a single-file `FileEditOutput` by diffing original and new content.
pub fn build_file_output(
    path: &Path,
    original: &str,
    new: &str,
    _old_start_line: usize,
    _old_line_count: usize,
    _new_line_count: usize,
) -> FileEditOutput {
    build_file_output_from_diff(path, original, new)
}

pub fn build_file_output_from_diff(path: &Path, original: &str, new: &str) -> FileEditOutput {
    FileEditOutput {
        path: path.display().to_string(),
        hunks: build_hunks_from_diff(original, new),
    }
}

fn build_hunks_from_diff(original: &str, new: &str) -> Vec<HunkOutput> {
    let orig_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let changes = diff_changes(original, new);
    let regions = group_changes_into_regions(&changes, orig_lines.len(), new_lines.len());
    regions
        .into_iter()
        .map(|region| build_hunk(&orig_lines, &new_lines, region))
        .collect()
}

fn diff_changes(original: &str, new: &str) -> Vec<ChangeRange> {
    let input = InternedInput::new(original, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    diff.hunks()
        .map(|hunk| ChangeRange {
            old_range: hunk.before.start as usize..hunk.before.end as usize,
            new_range: hunk.after.start as usize..hunk.after.end as usize,
        })
        .collect()
}

fn group_changes_into_regions(
    changes: &[ChangeRange],
    old_len: usize,
    new_len: usize,
) -> Vec<HunkRegion> {
    let mut regions: Vec<HunkRegion> = Vec::new();

    for change in changes {
        let next_old_start = change.old_range.start.saturating_sub(CONTEXT_LINES);
        let next_old_end = (change.old_range.end + CONTEXT_LINES).min(old_len);
        let next_new_start = change.new_range.start.saturating_sub(CONTEXT_LINES);
        let next_new_end = (change.new_range.end + CONTEXT_LINES).min(new_len);

        if let Some(current) = regions.last_mut() {
            let overlaps_old = next_old_start <= current.old_range.end;
            let overlaps_new = next_new_start <= current.new_range.end;
            if overlaps_old || overlaps_new {
                current.old_range.end = current.old_range.end.max(next_old_end);
                current.new_range.end = current.new_range.end.max(next_new_end);
                current.changes.push(change.clone());
                continue;
            }
        }

        regions.push(HunkRegion {
            old_range: next_old_start..next_old_end,
            new_range: next_new_start..next_new_end,
            changes: vec![change.clone()],
        });
    }

    regions
}

fn build_hunk(orig_lines: &[&str], new_lines: &[&str], region: HunkRegion) -> HunkOutput {
    let mut lines = Vec::new();
    let mut old_idx = region.old_range.start;
    let mut new_idx = region.new_range.start;
    let mut deleted = 0usize;
    let mut inserted = 0usize;

    for change in region.changes {
        while old_idx < change.old_range.start && new_idx < change.new_range.start {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                text: orig_lines[old_idx].to_string(),
                old_line: Some(old_idx + 1),
                new_line: Some(new_idx + 1),
            });
            old_idx += 1;
            new_idx += 1;
        }

        while old_idx < change.old_range.end {
            lines.push(DiffLine {
                kind: DiffLineKind::Delete,
                text: orig_lines[old_idx].to_string(),
                old_line: Some(old_idx + 1),
                new_line: None,
            });
            old_idx += 1;
            deleted += 1;
        }

        while new_idx < change.new_range.end {
            lines.push(DiffLine {
                kind: DiffLineKind::Insert,
                text: new_lines[new_idx].to_string(),
                old_line: None,
                new_line: Some(new_idx + 1),
            });
            new_idx += 1;
            inserted += 1;
        }
    }

    while old_idx < region.old_range.end && new_idx < region.new_range.end {
        lines.push(DiffLine {
            kind: DiffLineKind::Context,
            text: orig_lines[old_idx].to_string(),
            old_line: Some(old_idx + 1),
            new_line: Some(new_idx + 1),
        });
        old_idx += 1;
        new_idx += 1;
    }

    HunkOutput {
        operation: "replace",
        old_start: region.old_range.start + 1,
        old_count: context_old_count(&lines),
        new_start: region.new_range.start + 1,
        new_count: context_new_count(&lines),
        lines,
        lines_deleted: deleted,
        lines_inserted: inserted,
    }
}

fn empty_hunk() -> HunkOutput {
    HunkOutput {
        operation: "replace",
        old_start: 1,
        old_count: 0,
        new_start: 1,
        new_count: 0,
        lines: Vec::new(),
        lines_deleted: 0,
        lines_inserted: 0,
    }
}

fn context_old_count(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .filter(|l| matches!(l.kind, DiffLineKind::Context | DiffLineKind::Delete))
        .count()
}

fn context_new_count(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .filter(|l| matches!(l.kind, DiffLineKind::Context | DiffLineKind::Insert))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_hunk_basic() {
        let original = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nX\nY\nd\ne\nf\ng\n";
        let hunk = build_replace_hunk(original, new, 3, 2, 2);

        assert_eq!(hunk.operation, "replace");
        assert_eq!(hunk.lines_deleted, 1);
        assert_eq!(hunk.lines_inserted, 2);
        assert!(
            hunk.lines
                .iter()
                .any(|l| matches!(l.kind, DiffLineKind::Delete))
        );
        assert!(
            hunk.lines
                .iter()
                .any(|l| matches!(l.kind, DiffLineKind::Insert))
        );
    }

    #[test]
    fn test_replace_hunk_context_lines() {
        let original = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\n";
        let new = "line1\nline2\nCHANGED\nline4\nline5\nline6\nline7\nline8\nline9\n";
        let hunk = build_replace_hunk(original, new, 3, 1, 1);

        let context_before = hunk
            .lines
            .iter()
            .take_while(|l| matches!(l.kind, DiffLineKind::Context))
            .count();
        assert!(context_before >= 2);

        for dl in &hunk.lines {
            match dl.kind {
                DiffLineKind::Context => assert_eq!(dl.old_line, dl.new_line),
                DiffLineKind::Delete => {
                    assert!(dl.old_line.is_some());
                    assert!(dl.new_line.is_none());
                }
                DiffLineKind::Insert => {
                    assert!(dl.old_line.is_none());
                    assert!(dl.new_line.is_some());
                }
            }
        }
    }

    #[test]
    fn test_format_output() {
        let original = "a\nb\nc\n";
        let new = "a\nb\nz\n";
        let output = FileEditOutput {
            path: "src/lib.rs".to_string(),
            hunks: vec![build_replace_hunk(original, new, 3, 1, 1)],
        };

        let formatted = format_output(&[output]);
        assert!(formatted.starts_with("OK paths=1"));
        assert!(formatted.contains("P src/lib.rs"));
        assert!(formatted.contains("H replace"));
        assert!(formatted.contains("| "));
    }

    #[test]
    fn test_replace_hunk_line_numbers_coherent() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let new = "a\nb\nX\nY\nf\ng\nh\ni\n";
        let hunk = build_replace_hunk(original, new, 3, 3, 2);

        let mut saw_insert = false;
        for dl in &hunk.lines {
            match dl.kind {
                DiffLineKind::Context => {
                    if saw_insert {
                        assert!(dl.new_line.is_some());
                    }
                }
                DiffLineKind::Insert => saw_insert = true,
                DiffLineKind::Delete => {}
            }
        }
    }

    #[test]
    fn test_no_trailing_newline_in_original() {
        let original = "a\nb\nc";
        let new = "a\nb\nz";
        let hunk = build_replace_hunk(original, new, 3, 1, 1);
        assert!(!hunk.lines.is_empty());
    }

    #[test]
    fn test_replace_at_start_of_file() {
        let original = "old first\nmiddle\nlast\n";
        let new = "new first\nmiddle\nlast\n";
        let hunk = build_replace_hunk(original, new, 1, 1, 1);
        assert!(hunk.lines.iter().any(|l| l.text == "old first"));
        assert!(hunk.lines.iter().any(|l| l.text == "new first"));
    }

    #[test]
    fn test_replace_all_lines() {
        let original = "old1\nold2\nold3\n";
        let new = "new1\nnew2\nnew3\nnew4\n";
        let hunk = build_replace_hunk(original, new, 1, 3, 4);
        assert_eq!(hunk.lines_deleted, 3);
        assert_eq!(hunk.lines_inserted, 4);
    }

    #[test]
    fn test_large_matched_block_shows_single_line_change() {
        let original = [
            "let now = 100;",
            "let input = vec![function(1, true, 0, 100, 7)];",
            "let in_flight = Arc::new(InFlightFunctions::new());",
            "let outcome = scheduler_tick(now, &input, &in_flight, |_| None);",
            "",
            "assert!(outcome.modified_functions.is_empty());",
            "assert_eq!(outcome.queue_submissions.len(), 1);",
            "assert_eq!(outcome.queue_submissions[0].query_id, 1);",
            "assert_eq!(outcome.queue_submissions[0].priority, 7);",
        ]
        .join("\n");
        let new = [
            "let now = 100;",
            "let input = vec![function(1, true, 0, 100, 7)];",
            "let in_flight = Arc::new(InFlightFunctions::new());",
            "let outcome = scheduler_tick(now, &input, &in_flight, |fid| Some(fid));",
            "",
            "assert!(outcome.modified_functions.is_empty());",
            "assert_eq!(outcome.queue_submissions.len(), 1);",
            "assert_eq!(outcome.queue_submissions[0].query_id, 1);",
            "assert_eq!(outcome.queue_submissions[0].priority, 7);",
        ]
        .join("\n");

        let hunk = build_replace_hunk(&original, &new, 1, 9, 9);
        assert_eq!(hunk.lines_deleted, 1);
        assert_eq!(hunk.lines_inserted, 1);
        let context_lines = hunk
            .lines
            .iter()
            .filter(|l| matches!(l.kind, DiffLineKind::Context))
            .count();
        assert!(context_lines >= 3);
    }

    #[test]
    fn test_distant_changes_produce_multiple_hunks() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\nk\nl\n";
        let file_output = build_file_output_from_diff(Path::new("file.txt"), original, new);
        assert_eq!(file_output.hunks.len(), 2);
    }

    #[test]
    fn test_rendered_line_format() {
        let dl = DiffLine {
            kind: DiffLineKind::Context,
            text: "hello".to_string(),
            old_line: Some(5),
            new_line: Some(5),
        };
        let rendered = dl.render();
        assert_eq!(rendered, " 00005| hello");

        let dl = DiffLine {
            kind: DiffLineKind::Delete,
            text: "old".to_string(),
            old_line: Some(10),
            new_line: None,
        };
        let rendered = dl.render();
        assert_eq!(rendered, "-00010| old");

        let dl = DiffLine {
            kind: DiffLineKind::Insert,
            text: "new".to_string(),
            old_line: None,
            new_line: Some(10),
        };
        let rendered = dl.render();
        assert_eq!(rendered, "+00010| new");
    }
}
