//! Session-scoped symbol digest cache for get_function/get_symbol phases.

use crate::hash::RapidHash;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

static SYMBOL_CACHE: Lazy<RwLock<HashMap<SymbolCacheKey, SymbolReadCacheEntry>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SymbolCacheKey {
    session_id: String,
    path: PathBuf,
    kind: String,
    qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDigest {
    pub hash: RapidHash,
    pub byte_len: usize,
    pub line_count: usize,
}

impl SymbolDigest {
    pub fn new(bytes: &[u8], line_count: usize) -> Self {
        Self {
            hash: RapidHash::new(bytes),
            byte_len: bytes.len(),
            line_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SymbolReadCacheEntry {
    pub path: PathBuf,
    pub qualified_name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    pub digest: SymbolDigest,
    pub last_read_at: SystemTime,
}

pub fn check_symbol_cache(
    session_id: &str,
    path: &Path,
    kind: &str,
    qualified_name: &str,
    digest: &SymbolDigest,
    start_line: usize,
    end_line: usize,
) -> bool {
    let Ok(key) = SymbolCacheKey::new(session_id, path, kind, qualified_name) else {
        return false;
    };

    let cache = SYMBOL_CACHE.read();
    let Some(entry) = cache.get(&key) else {
        return false;
    };

    entry.digest == *digest && entry.start_line == start_line && entry.end_line == end_line
}

pub fn record_symbol_read(
    session_id: &str,
    path: &Path,
    kind: &str,
    qualified_name: &str,
    start_line: usize,
    end_line: usize,
    digest: SymbolDigest,
) {
    let Ok(key) = SymbolCacheKey::new(session_id, path, kind, qualified_name) else {
        return;
    };

    let entry = SymbolReadCacheEntry {
        path: key.path.clone(),
        qualified_name: qualified_name.to_string(),
        kind: kind.to_string(),
        start_line,
        end_line,
        digest,
        last_read_at: SystemTime::now(),
    };
    SYMBOL_CACHE.write().insert(key, entry);
}

impl SymbolCacheKey {
    fn new(
        session_id: &str,
        path: &Path,
        kind: &str,
        qualified_name: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            session_id: session_id.to_string(),
            path: path
                .canonicalize()
                .map_err(|e| format!("canonicalize failed for {}: {e}", path.display()))?,
            kind: kind.to_string(),
            qualified_name: qualified_name.to_string(),
        })
    }
}

#[cfg(test)]
pub(crate) fn clear_symbol_cache_for_tests() {
    SYMBOL_CACHE.write().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn cache_matches_digest_and_range() {
        clear_symbol_cache_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn a() {}\n").unwrap();
        let digest = SymbolDigest::new(b"fn a() {}", 1);

        assert!(!check_symbol_cache(
            "s", &path, "function", "a", &digest, 0, 1
        ));
        record_symbol_read("s", &path, "function", "a", 0, 1, digest.clone());
        assert!(check_symbol_cache(
            "s", &path, "function", "a", &digest, 0, 1
        ));
        assert!(!check_symbol_cache(
            "s", &path, "function", "a", &digest, 1, 2
        ));
    }
}
