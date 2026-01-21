use crate::hash::RapidHash;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
}

impl MerkleTree {
    pub fn scan(root: &Path) -> Self {
        let mut entries = HashMap::new();
        let mut combined = Vec::new(); // For computing deterministic root hash

        // Collect paths first to sort them (deterministic order)
        let mut paths = Vec::new();

        for result in WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .build()
        {
            match result {
                Ok(entry) => {
                    if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                        paths.push(entry.into_path());
                    }
                }
                Err(_) => continue,
            }
        }
        paths.sort();

        for path in paths {
            if let Ok(content) = std::fs::read(&path) {
                let hash = RapidHash::new(&content);
                // Accumulate path + hash bytes for root hash computation
                combined.extend_from_slice(path.to_string_lossy().as_bytes());
                combined.extend_from_slice(&hash.as_u64().to_le_bytes());
                entries.insert(path, hash);
            }
        }

        Self {
            root_hash: RapidHash::new(&combined),
            entries,
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
