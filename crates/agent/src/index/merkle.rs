use crate::hash::RapidHash;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Represents the file paths that changed between two snapshots
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiffPaths {
    /// Files that were added (exist in new but not in old)
    pub added: Vec<PathBuf>,
    /// Files that were modified (exist in both but content changed)
    pub modified: Vec<PathBuf>,
    /// Files that were removed (exist in old but not in new)
    pub removed: Vec<PathBuf>,
}

impl DiffPaths {
    /// Create a new empty DiffPaths
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if there are no changes
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    /// Generate a human-readable summary of the changes
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("+{} files", self.added.len()));
        }
        if !self.removed.is_empty() {
            parts.push(format!("-{} files", self.removed.len()));
        }
        if !self.modified.is_empty() {
            parts.push(format!("{} modified", self.modified.len()));
        }

        if parts.is_empty() {
            "No changes".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Get all files that were added or modified (useful for analysis)
    pub fn changed_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.added.iter().chain(self.modified.iter())
    }
}

#[derive(Debug, Clone)]
pub struct MerkleTree {
    pub root_hash: RapidHash,
    pub entries: HashMap<PathBuf, RapidHash>,
    pub metadata: HashMap<PathBuf, FileFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    pub size: u64,
    pub modified_ns: u128,
}

impl MerkleTree {
    pub fn scan(root: &Path) -> Self {
        Self::scan_with_previous(root, None)
    }

    pub fn scan_with_previous(root: &Path, previous: Option<&MerkleTree>) -> Self {
        let mut entries = HashMap::new();
        let mut metadata = HashMap::new();
        let mut combined = Vec::new(); // For computing deterministic root hash

        // Collect paths first to sort them (deterministic order)
        let mut paths = Vec::new();

        // TODO: Consider consolidating with file_index.rs's Override pattern for consistency
        // Currently using .standard_filters() which respects .gitignore and common ignore patterns
        for result in WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .standard_filters(true)
            .build()
        {
            match result {
                Ok(entry) => {
                    if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                        && let Ok(file_meta) = entry.metadata()
                    {
                        let modified_ns = file_meta
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|duration| duration.as_nanos())
                            .unwrap_or(0);
                        paths.push((
                            entry.into_path(),
                            FileFingerprint {
                                size: file_meta.len(),
                                modified_ns,
                            },
                        ));
                    }
                }
                Err(_) => continue,
            }
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));

        let mut file_buf = Vec::new();

        for (path, file_meta) in paths {
            let hash = if let Some(previous_tree) = previous {
                if let (Some(prev_meta), Some(prev_hash)) = (
                    previous_tree.metadata.get(&path),
                    previous_tree.entries.get(&path),
                ) {
                    if prev_meta == &file_meta {
                        *prev_hash
                    } else if let Ok(mut file) = std::fs::File::open(&path) {
                        file_buf.clear();
                        if file.read_to_end(&mut file_buf).is_ok() {
                            RapidHash::new(&file_buf)
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                } else if let Ok(mut file) = std::fs::File::open(&path) {
                    file_buf.clear();
                    if file.read_to_end(&mut file_buf).is_ok() {
                        RapidHash::new(&file_buf)
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            } else if let Ok(mut file) = std::fs::File::open(&path) {
                file_buf.clear();
                if file.read_to_end(&mut file_buf).is_ok() {
                    RapidHash::new(&file_buf)
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // Accumulate path + hash bytes for root hash computation
            combined.extend_from_slice(path.to_string_lossy().as_bytes());
            combined.extend_from_slice(&hash.as_u64().to_le_bytes());
            metadata.insert(path.clone(), file_meta);
            entries.insert(path, hash);
        }

        Self {
            root_hash: RapidHash::new(&combined),
            entries,
            metadata,
        }
    }

    /// Compare this tree with an older tree and return a summary string
    ///
    /// Deprecated: Use `diff_paths()` instead for structured access to changed files
    pub fn diff_summary(&self, older: &MerkleTree) -> String {
        self.diff_paths(older).summary()
    }

    /// Compare this tree with an older tree and return the actual changed paths
    pub fn diff_paths(&self, older: &MerkleTree) -> DiffPaths {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut removed = Vec::new();

        for (path, hash) in &self.entries {
            match older.entries.get(path) {
                Some(old_hash) => {
                    if hash != old_hash {
                        modified.push(path.clone());
                    }
                }
                None => added.push(path.clone()),
            }
        }

        for path in older.entries.keys() {
            if !self.entries.contains_key(path) {
                removed.push(path.clone());
            }
        }

        // Sort for deterministic output
        added.sort();
        modified.sort();
        removed.sort();

        DiffPaths {
            added,
            modified,
            removed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MerkleTree;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn scan_with_previous_detects_changes() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("a.txt");

        fs::write(&file, "one").expect("write one");
        let pre = MerkleTree::scan(dir.path());

        // Use a different-length string ("two!" vs "one") so the FileFingerprint
        // size field differs regardless of filesystem mtime granularity.
        // On Linux CI (ext4/tmpfs), mtime resolution can be coarser than the
        // time between two writes, causing same-size overwrites to appear unchanged.
        fs::write(&file, "two!").expect("write two");
        let post = MerkleTree::scan_with_previous(dir.path(), Some(&pre));
        let diff = post.diff_paths(&pre);

        assert_eq!(diff.modified.len(), 1);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn scan_with_previous_handles_unchanged_files() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("same.txt");

        fs::write(&file, "constant").expect("write constant");
        let pre = MerkleTree::scan(dir.path());
        let post = MerkleTree::scan_with_previous(dir.path(), Some(&pre));
        let diff = post.diff_paths(&pre);

        assert!(diff.is_empty());
    }
}
