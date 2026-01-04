use crate::hash::RapidHash;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

    pub fn diff_summary(&self, older: &MerkleTree) -> String {
        let mut added = 0;
        let mut removed = 0;
        let mut modified = 0;

        for (path, hash) in &self.entries {
            match older.entries.get(path) {
                Some(old_hash) => {
                    if hash != old_hash {
                        modified += 1;
                    }
                }
                None => added += 1,
            }
        }

        for path in older.entries.keys() {
            if !self.entries.contains_key(path) {
                removed += 1;
            }
        }

        let mut parts = Vec::new();
        if added > 0 {
            parts.push(format!("+{} files", added));
        }
        if removed > 0 {
            parts.push(format!("-{} files", removed));
        }
        if modified > 0 {
            parts.push(format!("{} modified", modified));
        }

        if parts.is_empty() {
            "No changes".to_string()
        } else {
            parts.join(", ")
        }
    }
}
