//! Layer identity and source provenance for `.agents` Protocol documents.

use std::path::{Path, PathBuf};

/// Which protocol layer a document was discovered in.
///
/// Layers are resolved in this order: explicit QueryMT base configuration,
/// global protocol layer, workspace protocol layer. Later layers override
/// earlier ones for the same key or singleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum DotagentsLayer {
    /// The user-controlled global layer (`~/.agents/`).
    #[default]
    Global,
    /// The workspace/repository-controlled layer (`<workspace>/.agents/`).
    Workspace,
}

impl DotagentsLayer {
    /// Stable lowercase identifier used in diagnostics and source keys.
    pub fn as_str(self) -> &'static str {
        match self {
            DotagentsLayer::Global => "global",
            DotagentsLayer::Workspace => "workspace",
        }
    }

    /// Precedence rank; higher wins.
    pub fn precedence(self) -> u8 {
        match self {
            DotagentsLayer::Global => 0,
            DotagentsLayer::Workspace => 1,
        }
    }
}

impl std::fmt::Display for DotagentsLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lexical location a document or entry was read from.
///
/// Source references never carry resolved secret values; they only identify
/// which layer and path contributed effective content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsSource {
    /// Which layer contributed the content.
    pub layer: DotagentsLayer,
    /// The lexical path as constructed from the layer root and protocol
    /// layout (not canonicalized).
    pub lexical_path: PathBuf,
    /// The canonical path after symlink resolution, when it could be resolved.
    pub canonical_path: Option<PathBuf>,
    /// Optional normalized entry ID (for collection entries).
    pub entry_id: Option<String>,
}

impl DotagentsSource {
    /// Construct a source reference for a singleton document.
    pub fn singleton(layer: DotagentsLayer, lexical_path: impl Into<PathBuf>) -> Self {
        Self {
            layer,
            lexical_path: lexical_path.into(),
            canonical_path: None,
            entry_id: None,
        }
    }

    /// Construct a source reference for a collection entry.
    pub fn entry(
        layer: DotagentsLayer,
        lexical_path: impl Into<PathBuf>,
        entry_id: impl Into<String>,
    ) -> Self {
        Self {
            layer,
            lexical_path: lexical_path.into(),
            canonical_path: None,
            entry_id: Some(entry_id.into()),
        }
    }

    /// Attach the canonical path.
    pub fn with_canonical(mut self, canonical: impl Into<PathBuf>) -> Self {
        self.canonical_path = Some(canonical.into());
        self
    }

    /// The lexical path.
    pub fn path(&self) -> &Path {
        &self.lexical_path
    }
}

impl std::fmt::Display for DotagentsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.layer, self.lexical_path.display())?;
        if let Some(id) = &self.entry_id {
            write!(f, " (entry {})", id)?;
        }
        Ok(())
    }
}

/// A resolved layer root and whether it was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsSourceRef {
    /// The layer this root belongs to.
    pub layer: DotagentsLayer,
    /// The root path (lexical).
    pub root: PathBuf,
    /// Whether the root directory exists on disk.
    pub exists: bool,
}

impl DotagentsSourceRef {
    /// Construct a new source reference.
    pub fn new(layer: DotagentsLayer, root: impl Into<PathBuf>, exists: bool) -> Self {
        Self {
            layer,
            root: root.into(),
            exists,
        }
    }
}
