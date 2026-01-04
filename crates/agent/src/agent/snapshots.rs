//! Snapshot management utilities

use crate::agent::core::{QueryMTAgent, SnapshotPolicy};
use crate::index::merkle::MerkleTree;
use crate::model::MessagePart;
use std::path::Path;

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

impl QueryMTAgent {
    /// Prepares a snapshot configuration if enabled.
    pub(crate) fn prepare_snapshot(&self) -> Option<(std::path::PathBuf, SnapshotPolicy)> {
        if self.snapshot_policy == SnapshotPolicy::None {
            return None;
        }
        let root = self.snapshot_root.clone()?;
        Some((root, self.snapshot_policy))
    }

    /// Determines if a tool should trigger snapshotting.
    pub(crate) fn should_snapshot_tool(&self, tool_name: &str) -> bool {
        if self.mutating_tools.contains(tool_name) {
            return true;
        }
        self.assume_mutating
    }
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
    let part = MessagePart::Snapshot {
        root_hash,
        diff_summary: Some(summary.clone()),
    };
    (part, Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_snapshot_metadata() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let (part, summary) = snapshot_metadata(dir.path());

        assert!(matches!(part, MessagePart::Snapshot { .. }));
        if let MessagePart::Snapshot {
            root_hash,
            diff_summary,
        } = part
        {
            // Hash is always non-zero for non-empty input
            assert_ne!(root_hash.as_u64(), 0);
            assert!(diff_summary.is_some());
            let summary = diff_summary.unwrap();
            assert!(summary.contains("Files: 1"));
            assert!(summary.contains("Bytes: 11"));
        }
        assert!(summary.is_some());
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
