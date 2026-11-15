use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

    /// Fold `other` into `self`, keeping each list sorted and free of duplicates.
    pub fn merge(&mut self, other: DiffPaths) {
        self.added.extend(other.added);
        self.modified.extend(other.modified);
        self.removed.extend(other.removed);
        for list in [&mut self.added, &mut self.modified, &mut self.removed] {
            list.sort();
            list.dedup();
        }
    }

    /// Get all files that were added or modified (useful for analysis)
    pub fn changed_files(&self) -> impl Iterator<Item = &PathBuf> {
        self.added.iter().chain(self.modified.iter())
    }
}
