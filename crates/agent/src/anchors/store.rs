use crate::anchors::reconcile::{file_salt, line_hashes, reconcile_lines};
use crate::hash::RapidHash;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

static ANCHOR_STORE: Lazy<RwLock<HashMap<AnchorKey, FileAnchorState>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AnchorKey {
    pub session_id: String,
    pub path: PathBuf,
}

impl AnchorKey {
    pub fn new(session_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            session_id: session_id.into(),
            path: path.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileAnchorState {
    pub file_digest: RapidHash,
    pub line_count: usize,
    pub lines: Vec<LineAnchor>,
    pub anchor_to_line: HashMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAnchor {
    pub anchor: String,
    pub line_hash: RapidHash,
}

pub fn reconcile_file(
    session_id: &str,
    path: &Path,
    content: &str,
) -> Result<FileAnchorState, String> {
    let canonical_path = canonicalize_for_key(path)?;
    let salt = file_salt(session_id, &canonical_path);
    let key = AnchorKey::new(session_id, canonical_path);
    let file_digest = RapidHash::new(content.as_bytes());

    let mut store = ANCHOR_STORE.write();
    if let Some(existing) = store.get(&key)
        && existing.file_digest == file_digest
    {
        return Ok(existing.clone());
    }

    let hashes = line_hashes(content);
    let previous_lines = store.get(&key).map(|state| state.lines.as_slice());
    let lines = reconcile_lines(salt, previous_lines, &hashes);
    let state = FileAnchorState::new(file_digest, lines);
    store.insert(key, state.clone());
    Ok(state)
}

pub fn resolve_anchor(
    session_id: &str,
    path: &Path,
    content: &str,
    anchor: &str,
) -> Result<usize, String> {
    let (anchor_id, provided_text) = crate::anchors::split_anchor(anchor);
    let state = reconcile_file(session_id, path, content)?;
    let line_idx = state
        .anchor_to_line
        .get(anchor_id)
        .copied()
        .ok_or_else(|| {
            format!(
                "Anchor '{anchor_id}' is missing or stale for {}. \
                 Re-read the file to get current anchors.",
                path.display()
            )
        })?;

    // Dirac-style content validation: if the caller included the §text
    // portion, verify it matches the actual file content.
    if let Some(expected) = provided_text {
        let lines = crate::anchors::reconcile::split_lines_preserve_content(content);
        let actual = lines.get(line_idx).unwrap_or(&"");
        if expected != *actual {
            return Err(format!(
                "Anchor '{anchor_id}' exists but the content doesn't match. \
                 Expected: \"{actual}\", Provided: \"{expected}\". \
                 Re-read the file to get current content.",
            ));
        }
    }

    Ok(line_idx)
}

impl FileAnchorState {
    fn new(file_digest: RapidHash, lines: Vec<LineAnchor>) -> Self {
        let anchor_to_line = lines
            .iter()
            .enumerate()
            .map(|(idx, line)| (line.anchor.clone(), idx))
            .collect();

        Self {
            file_digest,
            line_count: lines.len(),
            lines,
            anchor_to_line,
        }
    }
}

fn canonicalize_for_key(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|e| format!("canonicalize failed for {}: {e}", path.display()))
}

#[cfg(test)]
pub(crate) fn clear_anchor_store_for_tests() {
    ANCHOR_STORE.write().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn unchanged_file_keeps_anchors_stable() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let first = reconcile_file("session", &path, &content).unwrap();
        let second = reconcile_file("session", &path, &content).unwrap();

        assert_eq!(first.file_digest, second.file_digest);
        assert_eq!(first.line_count, 3);
        assert_eq!(
            first.iter_anchors().collect::<Vec<_>>(),
            second.iter_anchors().collect::<Vec<_>>()
        );
    }

    #[test]
    fn sessions_have_independent_anchor_state() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "same\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let first = reconcile_file("a", &path, &content).unwrap();
        let second = reconcile_file("b", &path, &content).unwrap();

        assert_ne!(first.lines[0].anchor, second.lines[0].anchor);
    }

    impl FileAnchorState {
        fn iter_anchors(&self) -> impl Iterator<Item = &str> {
            self.lines.iter().map(|line| line.anchor.as_str())
        }
    }

    #[test]
    fn anchor_with_section_text_resolves() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\nworld\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let state = reconcile_file("s", &path, &content).unwrap();
        let anchor_id = state.lines[0].anchor.as_str();
        let full_anchor = format!("{anchor_id}§hello");

        let idx = resolve_anchor("s", &path, &content, &full_anchor).unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn bare_anchor_still_resolves() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\nworld\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let state = reconcile_file("s", &path, &content).unwrap();
        let anchor_id = state.lines[1].anchor.clone();

        let idx = resolve_anchor("s", &path, &content, &anchor_id).unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn anchor_with_wrong_text_fails() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\nworld\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let state = reconcile_file("s", &path, &content).unwrap();
        let anchor_id = state.lines[0].anchor.as_str();
        let bad_anchor = format!("{anchor_id}§wrong_text");

        let err = resolve_anchor("s", &path, &content, &bad_anchor).unwrap_err();
        assert!(
            err.contains("content doesn't match"),
            "expected content mismatch error, got: {err}"
        );
    }

    #[test]
    fn anchor_with_newline_in_text_fails() {
        clear_anchor_store_for_tests();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "hello\nworld\n").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        let state = reconcile_file("s", &path, &content).unwrap();
        let anchor_id = state.lines[0].anchor.as_str();
        // The §text portion contains a newline — should still match the single line
        // since split_lines_preserve_content returns individual lines.
        let bad_anchor = format!("{anchor_id}§hello\nextra");

        let err = resolve_anchor("s", &path, &content, &bad_anchor).unwrap_err();
        assert!(
            err.contains("content doesn't match"),
            "expected content mismatch error, got: {err}"
        );
    }
}
