//! Tool context and error types for unified tool interface

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;

use crate::tools::read_scope::ReadScope;
pub use crate::tools::tool::Tool;
pub use crate::tools::tool_context::ToolContext;

/// Capability requirements that tools may need
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapabilityRequirement {
    /// Requires filesystem access (cwd must be set).
    Filesystem,
    /// Mutates durable session control-plane state.
    SessionState,
}

/// One selectable choice of a structured question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Text shown for the choice; also the value returned when it is picked.
    pub label: String,
    /// Explanation shown next to the label.
    pub description: String,
}

impl QuestionOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
        }
    }
}

/// Unified error type for all tools
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Provider error: {0}")]
    ProviderError(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Session error: {0}")]
    SessionError(String),
    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionClass {
    ParallelSafe,
    SerialStateful,
    ClarificationBoundary,
}

/// File-system access a single tool call is going to perform.
///
/// Used by the tool-batch scheduler to serialize calls that would otherwise
/// race on the same file, and by middleware to detect redundant same-file
/// mutations. Paths are expected to be normalised with
/// [`crate::tools::normalize_tool_path`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FileAccess {
    /// The call does not touch files (or does not declare it). Default.
    #[default]
    None,
    /// The call reads the listed scopes and/or writes the listed paths.
    Paths {
        /// What the call observes. A scope may be a directory narrowed by the
        /// same include/exclude/file-type filter the tool applies.
        reads: Vec<ReadScope>,
        /// Files the call creates, modifies or deletes.
        writes: Vec<PathBuf>,
        /// Directories the call removes or replaces as a whole. Unlike a file
        /// write, this changes every entry beneath the directory, so read
        /// filters do not apply to it.
        tree_writes: Vec<PathBuf>,
    },
    /// The call may touch any file (e.g. a shell command).
    Unknown,
    /// The call must be treated as reading and writing every file. Conflicts
    /// with every other call that declares file access (reads, writes and
    /// `Unknown`). Used when a call's access could not be determined at all,
    /// e.g. because the tool's `file_access` panicked.
    Any,
}

impl FileAccess {
    /// Access that only writes the given paths.
    pub fn writes(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self::Paths {
            reads: Vec::new(),
            writes: paths.into_iter().collect(),
            tree_writes: Vec::new(),
        }
    }

    /// Access that removes or replaces a whole directory tree.
    pub fn writes_tree(dir: PathBuf) -> Self {
        Self::Paths {
            reads: Vec::new(),
            writes: Vec::new(),
            tree_writes: vec![dir],
        }
    }

    /// Access that only reads everything at or beneath the given paths.
    pub fn reads(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self::read_scopes(paths.into_iter().map(ReadScope::all))
    }

    /// Access that only reads the given (possibly filtered) scopes.
    pub fn read_scopes(scopes: impl IntoIterator<Item = ReadScope>) -> Self {
        Self::Paths {
            reads: scopes.into_iter().collect(),
            writes: Vec::new(),
            tree_writes: Vec::new(),
        }
    }

    /// Files written by this call (empty for `None` and `Unknown`). Directory
    /// tree writes are not included; see [`Self::written_trees`].
    pub fn written_paths(&self) -> &[PathBuf] {
        match self {
            Self::Paths { writes, .. } => writes,
            Self::None | Self::Unknown | Self::Any => &[],
        }
    }

    /// Scopes read by this call (empty for `None` and `Unknown`).
    pub fn read_scopes_slice(&self) -> &[ReadScope] {
        match self {
            Self::Paths { reads, .. } => reads,
            Self::None | Self::Unknown | Self::Any => &[],
        }
    }

    /// Directory trees removed or replaced by this call.
    pub fn written_trees(&self) -> &[PathBuf] {
        match self {
            Self::Paths { tree_writes, .. } => tree_writes,
            Self::None | Self::Unknown | Self::Any => &[],
        }
    }

    /// Whether the call declares at least one written file or tree.
    pub fn has_writes(&self) -> bool {
        !self.written_paths().is_empty() || !self.written_trees().is_empty()
    }
}

/// Run a mutating tool operation under the session's checkout-scoped
/// cooperative mutation gate when one is configured; otherwise run it
/// unchanged.
///
/// Mutating built-ins wrap their write phase in this helper so a concurrent
/// baseline capture (which holds the gate) never observes a half-applied
/// edit, and vice versa. Opaque operations (shell commands, MCP tools) are
/// deliberately *not* gated: their filesystem writes are invisible to
/// QueryMT, and capture validates observed content around the window
/// instead.
pub async fn run_under_mutation_gate<T, F>(
    context: &dyn ToolContext,
    fut: F,
) -> Result<T, ToolError>
where
    F: Future<Output = Result<T, ToolError>>,
{
    match context.checkout_mutation_gate() {
        Some(gate) => match gate.run(fut).await {
            Ok(inner) => inner,
            Err(err) => Err(ToolError::Other(err.into())),
        },
        None => fut.await,
    }
}

/// Where an [`ArgAlias`] applies within a call's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasScope {
    /// Keys of the top-level argument object.
    TopLevel,
    /// Keys of each object item of the named top-level array property.
    ArrayItems(&'static str),
}

/// A compatibility spelling accepted for a canonical parameter name.
///
/// `to` must name a property of the tool's advertised (snake_case) schema.
/// Aliases are never advertised to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgAlias {
    pub scope: AliasScope,
    pub from: &'static str,
    pub to: &'static str,
}

/// Allows a single item to be supplied without its wrapping array.
///
/// When the top-level `array` property is absent and every name in `required`
/// is present at the top level (after item aliases), the top-level `fields`
/// are moved into one object that becomes the only element of `array`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapRule {
    pub array: &'static str,
    pub fields: &'static [&'static str],
    pub required: &'static [&'static str],
}

/// Conversion from old LLMError to ToolError for migration
impl From<querymt::error::LLMError> for ToolError {
    fn from(error: querymt::error::LLMError) -> Self {
        match error {
            querymt::error::LLMError::InvalidRequest(msg) => ToolError::InvalidRequest(msg),
            querymt::error::LLMError::ProviderError(msg) => ToolError::ProviderError(msg),
            _ => ToolError::ProviderError(error.to_string()),
        }
    }
}
