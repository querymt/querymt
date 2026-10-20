//! Snapshot management utilities

use crate::hash::RapidHash;
use crate::index::merkle::{DiffPaths, MerkleTree};
use crate::model::MessagePart;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Hashes remembered per edited path. The oldest entry (the session's baseline
/// for that file, dirty or clean) is always retained.
const MAX_HASHES_PER_PATH: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    FileEdit,
    FileDelete,
    Other,
}

fn classify(tool_name: &str) -> ToolKind {
    match tool_name {
        "edit" | "multiedit" | "replace_symbol" | "write_file" => ToolKind::FileEdit,
        "delete_file" => ToolKind::FileDelete,
        _ => ToolKind::Other,
    }
}

fn remember(history: &mut Vec<RapidHash>, hash: RapidHash) {
    if history.contains(&hash) {
        return;
    }
    if history.len() >= MAX_HASHES_PER_PATH {
        history.remove(1);
    }
    history.push(hash);
}

/// Observes the Merkle snapshots the executor already takes around tool calls
/// to notice when a file the session edited is changed by something else.
///
/// It needs no Git repository and assumes no clean starting point: the baseline
/// for a file is whatever the first file-tool call saw. Detection is purely
/// observational; nothing is restored and no culprit is named.
///
/// * Between tool calls (`begin`): any change to an edited path is reported,
///   because the agent's own commands cannot have caused it. A return to a hash
///   the session has already seen, or a missing file, is described as a possible
///   reversion; any other change as an outside modification.
/// * During a non-file tool (`finish`): only a return to a known hash or a
///   removal is reported, since commands such as formatters legitimately
///   rewrite files.
///
/// Concurrent tool calls make diffs ambiguous, so checks are skipped while calls
/// overlap and the reference tree is reset afterwards.
#[derive(Default)]
pub(crate) struct MerkleWatch {
    state: Mutex<WatchState>,
}

#[derive(Default)]
struct WatchState {
    root: Option<PathBuf>,
    /// Newest tree observed after the previous tool call.
    last_tree: Option<MerkleTree>,
    /// Hashes seen for each path edited through the file tools.
    history: HashMap<PathBuf, Vec<RapidHash>>,
    in_flight: usize,
    overlapped: bool,
}

/// Held for the duration of one snapshotted tool call. Dropping it without
/// `finish` (cancellation, early error) discards the reference tree.
pub(crate) struct WatchTicket {
    watch: Arc<MerkleWatch>,
    armed: bool,
}

impl MerkleWatch {
    /// Register a tool call and report changes made since the previous one.
    pub(crate) fn begin(
        self: &Arc<Self>,
        root: &Path,
        pre_tree: &MerkleTree,
    ) -> (WatchTicket, Vec<String>) {
        let mut state = self.state.lock();
        let mut warnings = Vec::new();
        if state.in_flight > 0 {
            state.overlapped = true;
        } else {
            if state.root.as_deref() != Some(root) {
                state.last_tree = None;
            }
            if let Some(last_tree) = state.last_tree.take() {
                warnings = state.check_between_calls(root, pre_tree, &last_tree);
            }
        }
        state.root = Some(root.to_path_buf());
        state.in_flight += 1;
        drop(state);
        (
            WatchTicket {
                watch: Arc::clone(self),
                armed: true,
            },
            warnings,
        )
    }

    fn abandon(&self) {
        let mut state = self.state.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_tree = None;
        state.overlapped = state.in_flight > 0;
    }
}

impl WatchTicket {
    /// Record the outcome of the call and report reversions caused during it.
    pub(crate) fn finish(
        mut self,
        tool_name: &str,
        pre_tree: &MerkleTree,
        post_tree: MerkleTree,
        changed: &DiffPaths,
    ) -> Vec<String> {
        self.armed = false;
        let mut state = self.watch.state.lock();
        state.in_flight = state.in_flight.saturating_sub(1);
        let overlapped = state.overlapped;
        let mut warnings = Vec::new();
        match classify(tool_name) {
            ToolKind::FileEdit => {
                for path in changed.added.iter().chain(&changed.modified) {
                    let history = state.history.entry(path.clone()).or_default();
                    if let Some(hash) = pre_tree.entries.get(path) {
                        remember(history, *hash);
                    }
                    if let Some(hash) = post_tree.entries.get(path) {
                        remember(history, *hash);
                    }
                }
            }
            ToolKind::FileDelete => {
                for path in &changed.removed {
                    state.history.remove(path);
                }
            }
            ToolKind::Other => {
                let root = state.root.clone().unwrap_or_default();
                for path in changed.added.iter().chain(&changed.modified) {
                    if let Some(history) = state.history.get_mut(path)
                        && let Some(hash) = post_tree.entries.get(path)
                    {
                        if !overlapped && history.contains(hash) {
                            warnings.push(format!(
                                "Warning: {} was edited earlier in this session and `{tool_name}` returned it to earlier content (possible reversion). {ADVICE}",
                                display(&root, path)
                            ));
                        }
                        remember(history, *hash);
                    }
                }
                if !overlapped {
                    for path in &changed.removed {
                        if state.history.contains_key(path) {
                            warnings.push(format!(
                                "Warning: {} was edited earlier in this session and is now missing after `{tool_name}`. {ADVICE}",
                                display(&root, path)
                            ));
                        }
                    }
                }
            }
        }
        if state.in_flight == 0 {
            state.last_tree = if overlapped { None } else { Some(post_tree) };
            state.overlapped = false;
        }
        warnings
    }
}

impl Drop for WatchTicket {
    fn drop(&mut self) {
        if self.armed {
            self.watch.abandon();
        }
    }
}

const ADVICE: &str =
    "Re-read the file and review the diff before continuing; nothing was restored automatically.";

fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

impl WatchState {
    fn check_between_calls(
        &mut self,
        root: &Path,
        pre_tree: &MerkleTree,
        last_tree: &MerkleTree,
    ) -> Vec<String> {
        let diff = pre_tree.diff_paths(last_tree);
        let mut warnings = Vec::new();
        for path in diff.added.iter().chain(&diff.modified) {
            let (Some(hash), Some(history)) =
                (pre_tree.entries.get(path), self.history.get_mut(path))
            else {
                continue;
            };
            let name = display(root, path);
            warnings.push(if history.contains(hash) {
                format!(
                    "Warning: {name} was edited earlier in this session and returned to earlier content between tool calls (possible external reversion, e.g. checkout, stash, reset or another writer). {ADVICE}"
                )
            } else {
                format!(
                    "Warning: {name} was edited earlier in this session and was modified outside this session's file tools between tool calls. {ADVICE}"
                )
            });
            remember(history, *hash);
        }
        for path in &diff.removed {
            if self.history.contains_key(path) {
                warnings.push(format!(
                    "Warning: {} was edited earlier in this session and is now missing (removed between tool calls). {ADVICE}",
                    display(root, path)
                ));
            }
        }
        warnings
    }
}

/// State of a filesystem snapshot
pub enum SnapshotState {
    None,
    Metadata {
        root: std::path::PathBuf,
    },
    Diff {
        pre_tree: MerkleTree,
        root: std::path::PathBuf,
    },
}

/// Generates metadata snapshot of a directory
pub fn snapshot_metadata(root: &Path) -> (MessagePart, Option<String>) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut latest_mtime = 0i128;

    for result in ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .build()
    {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
            && let Ok(metadata) = entry.metadata()
        {
            files += 1;
            bytes += metadata.len();
            if let Ok(mtime) = metadata.modified()
                && let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH)
            {
                let seconds = duration.as_secs() as i128;
                latest_mtime = latest_mtime.max(seconds);
            }
        }
    }

    let meta_string = format!("files={files},bytes={bytes},mtime={latest_mtime}");
    let root_hash = crate::hash::RapidHash::new(meta_string.as_bytes());

    let summary = format!("Files: {files}, Bytes: {bytes}, Latest mtime: {latest_mtime}");
    // Metadata snapshot doesn't track actual file changes, just aggregate info
    let part = MessagePart::Snapshot {
        root_hash,
        changed_paths: DiffPaths::default(),
    };
    (part, Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const ROOT: &str = "/project";

    fn tree(files: &[(&str, &str)]) -> MerkleTree {
        MerkleTree {
            root_hash: RapidHash::new(b""),
            entries: files
                .iter()
                .map(|(name, content)| {
                    (
                        Path::new(ROOT).join(name),
                        RapidHash::new(content.as_bytes()),
                    )
                })
                .collect(),
            metadata: HashMap::new(),
        }
    }

    /// Run one complete tool call: returns (warnings before, warnings after).
    fn call(
        watch: &Arc<MerkleWatch>,
        tool: &str,
        pre: &MerkleTree,
        post: MerkleTree,
    ) -> (Vec<String>, Vec<String>) {
        let (ticket, before) = watch.begin(Path::new(ROOT), pre);
        let changed = post.diff_paths(pre);
        let after = ticket.finish(tool, pre, post, &changed);
        (before, after)
    }

    #[test]
    fn changes_between_calls_are_reported_for_edited_paths_only() {
        let watch = Arc::new(MerkleWatch::default());
        // The first tree is the baseline even though the file starts dirty.
        let (before, after) = call(
            &watch,
            "edit",
            &tree(&[("a.rs", "dirty"), ("b.rs", "b")]),
            tree(&[("a.rs", "edited"), ("b.rs", "b")]),
        );
        assert!(before.is_empty() && after.is_empty());

        // Return to the starting (dirty) content, with an unrelated change too.
        let (before, _) = call(
            &watch,
            "read_tool",
            &tree(&[("a.rs", "dirty"), ("b.rs", "other")]),
            tree(&[("a.rs", "dirty"), ("b.rs", "other")]),
        );
        assert_eq!(before.len(), 1, "{before:?}");
        assert!(before[0].contains("a.rs") && before[0].contains("possible external reversion"));

        // A never-seen version is an outside modification, not a reversion.
        let unseen = tree(&[("a.rs", "unseen"), ("b.rs", "other")]);
        let (before, _) = call(&watch, "read_tool", &unseen, unseen.clone());
        assert_eq!(before.len(), 1, "{before:?}");
        assert!(before[0].contains("modified outside"));
        assert!(!before[0].contains("possible external reversion"));

        // Nothing changed since the last call: no repeat.
        let (before, _) = call(&watch, "read_tool", &unseen, unseen.clone());
        assert!(before.is_empty());

        // Removal.
        let gone = tree(&[("b.rs", "other")]);
        let (before, _) = call(&watch, "read_tool", &gone, gone.clone());
        assert_eq!(before.len(), 1, "{before:?}");
        assert!(before[0].contains("a.rs") && before[0].contains("missing"));
    }

    #[test]
    fn file_tool_changes_and_unedited_files_never_warn() {
        let watch = Arc::new(MerkleWatch::default());
        let (before, after) = call(
            &watch,
            "write_file",
            &tree(&[("a.rs", "one")]),
            tree(&[("a.rs", "two")]),
        );
        assert!(before.is_empty() && after.is_empty());
        // Restoring through a file tool is intentional.
        let (before, after) = call(
            &watch,
            "write_file",
            &tree(&[("a.rs", "two")]),
            tree(&[("a.rs", "one")]),
        );
        assert!(before.is_empty() && after.is_empty());
        // Files the session never edited are ignored entirely.
        let (before, after) = call(
            &watch,
            "shell",
            &tree(&[("a.rs", "one"), ("c.rs", "1")]),
            tree(&[("a.rs", "one"), ("c.rs", "2")]),
        );
        assert!(before.is_empty() && after.is_empty());
    }

    #[test]
    fn non_file_tools_only_report_returns_to_known_content_or_removal() {
        let watch = Arc::new(MerkleWatch::default());
        call(
            &watch,
            "edit",
            &tree(&[("a.rs", "original")]),
            tree(&[("a.rs", "edited")]),
        );
        // A formatter rewrites the file to new content.
        let (_, after) = call(
            &watch,
            "shell",
            &tree(&[("a.rs", "edited")]),
            tree(&[("a.rs", "formatted")]),
        );
        assert!(after.is_empty(), "{after:?}");
        // A command (stash/checkout) restores the original content.
        let (_, after) = call(
            &watch,
            "shell",
            &tree(&[("a.rs", "formatted")]),
            tree(&[("a.rs", "original")]),
        );
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(after[0].contains("`shell`") && after[0].contains("possible reversion"));
        // A command deletes the file.
        let (_, after) = call(&watch, "shell", &tree(&[("a.rs", "original")]), tree(&[]));
        assert_eq!(after.len(), 1, "{after:?}");
        assert!(after[0].contains("missing"));
    }

    #[test]
    fn deleting_through_delete_file_stops_tracking() {
        let watch = Arc::new(MerkleWatch::default());
        call(
            &watch,
            "edit",
            &tree(&[("a.rs", "1")]),
            tree(&[("a.rs", "2")]),
        );
        let (before, after) = call(&watch, "delete_file", &tree(&[("a.rs", "2")]), tree(&[]));
        assert!(before.is_empty() && after.is_empty());
        let recreated = tree(&[("a.rs", "1")]);
        let (before, _) = call(&watch, "read_tool", &recreated, recreated.clone());
        assert!(before.is_empty(), "{before:?}");
    }

    #[test]
    fn overlapping_and_abandoned_calls_do_not_produce_checks() {
        let watch = Arc::new(MerkleWatch::default());
        call(
            &watch,
            "edit",
            &tree(&[("a.rs", "1")]),
            tree(&[("a.rs", "2")]),
        );

        // Two concurrent calls: neither can attribute changes to the other.
        let pre = tree(&[("a.rs", "2")]);
        let (first, w1) = watch.begin(Path::new(ROOT), &pre);
        let (second, w2) = watch.begin(Path::new(ROOT), &pre);
        assert!(w1.is_empty() && w2.is_empty());
        let reverted = tree(&[("a.rs", "1")]);
        let changed = reverted.diff_paths(&pre);
        assert!(
            first
                .finish("shell", &pre, reverted.clone(), &changed)
                .is_empty()
        );
        assert!(
            second
                .finish("shell", &pre, reverted.clone(), &changed)
                .is_empty()
        );
        // The reference tree was discarded, so the next call only re-baselines.
        let (before, _) = call(&watch, "read_tool", &reverted, reverted.clone());
        assert!(before.is_empty(), "{before:?}");

        // A cancelled call (ticket dropped) also discards the reference tree.
        let (ticket, _) = watch.begin(Path::new(ROOT), &reverted);
        drop(ticket);
        let other = tree(&[("a.rs", "2")]);
        let (before, _) = call(&watch, "read_tool", &other, other.clone());
        assert!(before.is_empty(), "{before:?}");
    }

    #[test]
    fn history_is_bounded_but_keeps_the_initial_baseline() {
        let watch = Arc::new(MerkleWatch::default());
        let mut previous = "baseline".to_string();
        for i in 0..100 {
            let next = format!("edit {i}");
            call(
                &watch,
                "edit",
                &tree(&[("a.rs", previous.as_str())]),
                tree(&[("a.rs", next.as_str())]),
            );
            previous = next;
        }
        let reverted = tree(&[("a.rs", "baseline")]);
        let (before, _) = call(&watch, "read_tool", &reverted, reverted.clone());
        assert!(
            before[0].contains("possible external reversion"),
            "{before:?}"
        );
    }

    #[test]
    fn test_snapshot_metadata() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let (part, summary) = snapshot_metadata(dir.path());

        assert!(matches!(part, MessagePart::Snapshot { .. }));
        if let MessagePart::Snapshot {
            root_hash,
            changed_paths: _,
        } = part
        {
            // Hash is always non-zero for non-empty input
            assert_ne!(root_hash.as_u64(), 0);
        }
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.contains("Files: 1"));
        assert!(summary.contains("Bytes: 11"));
    }

    #[test]
    fn test_snapshot_state_diff() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "initial").unwrap();

        let pre_tree = MerkleTree::scan(dir.path());

        fs::write(&file_path, "modified content").unwrap();

        let post_tree = MerkleTree::scan(dir.path());

        let diff = post_tree.diff_summary(&pre_tree);
        assert!(diff.contains("test.txt") || !diff.is_empty());
    }
}
