//! Tool context and error types for unified tool interface

use crate::knowledge::{KnowledgeStore, ScopePolicy};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::tools::read_scope::ReadScope;

/// Capability requirements that tools may need
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapabilityRequirement {
    /// Requires filesystem access (cwd must be set).
    Filesystem,
    /// Mutates durable session control-plane state.
    SessionState,
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

/// Core context trait that all tools receive during execution
#[async_trait]
pub trait ToolContext: Send + Sync {
    /// Get the current session ID
    fn session_id(&self) -> &str;

    /// Get the current session public ID (defaults to `session_id`).
    fn session_public_id(&self) -> Option<String> {
        Some(self.session_id().to_string())
    }

    /// Optional session-scoped task lifecycle access for task tools.
    fn task_service(&self) -> Option<crate::session::TaskService> {
        None
    }

    fn knowledge_store(&self) -> Option<Arc<dyn KnowledgeStore>> {
        None
    }

    /// Portable repository knowledge for this session's checkout, when the
    /// host enabled it. Resolution is lazy and uses only [`cwd`](Self::cwd);
    /// callers cannot select another checkout.
    fn repository_knowledge(
        &self,
    ) -> Option<Arc<crate::knowledge::repository::RepositoryKnowledgeRegistry>> {
        None
    }

    /// Scope policy for knowledge tools. Defaults to permissive (allow all scopes).
    fn scope_policy(&self) -> Arc<dyn ScopePolicy> {
        Arc::new(crate::knowledge::PermissiveScopePolicy)
    }

    /// Emit an agent event (durable or ephemeral).
    ///
    /// Knowledge tools use this to emit `KnowledgeIngested` / `KnowledgeConsolidated`
    /// events so the scheduler can react to them. The default implementation is a
    /// no-op — only `AgentToolContext` (which carries an `EventSink`) actually emits.
    fn emit_event(&self, _kind: crate::events::AgentEventKind) {}

    /// Get the current working directory, if set.
    fn cwd(&self) -> Option<&Path>;

    /// Resolve a path. Returns error if cwd is None and path is relative.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        let path = Path::new(path);
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            self.cwd().map(|cwd| cwd.join(path)).ok_or_else(|| {
                ToolError::InvalidRequest(
                    "Cannot resolve relative path: no working directory set".into(),
                )
            })
        }
    }

    /// Get access to the agent registry for delegation tools
    fn agent_registry(&self) -> Option<Arc<dyn crate::delegation::AgentRegistry>> {
        None
    }

    /// Managed worktree workspace service, when the runtime has one.
    fn workspace_service(&self) -> Option<Arc<crate::worktree::WorkspaceService>> {
        None
    }

    /// Durable session store, for tools that resolve persisted records.
    fn session_store(&self) -> Option<Arc<dyn crate::session::store::SessionStore>> {
        None
    }

    /// Record progress for long-running operations
    async fn record_progress(
        &self,
        kind: &str,
        content: String,
        metadata: Option<serde_json::Value>,
    ) -> Result<String, ToolError>;

    /// Access to tool-specific context extensions
    fn as_any(&self) -> &dyn Any;

    /// Access the workspace query bridge for language intelligence queries.
    ///
    /// Returns `Some` when connected to a client that supports workspace queries
    /// (e.g., VS Code extension). Returns `None` in CLI mode or when the client
    /// does not advertise workspace query support.
    fn workspace_query_bridge(&self) -> Option<&crate::acp::client_bridge::ClientBridgeSender> {
        None
    }

    /// A cancellation token that is cancelled when the session is cancelled.
    ///
    /// Tools should poll this to abort long-running work cooperatively. The
    /// default returns a token that is never cancelled (no-op for tools that
    /// don't need it).
    fn cancellation_token(&self) -> CancellationToken {
        CancellationToken::new()
    }

    /// The session's checkout-scoped cooperative mutation gate, when the
    /// session participates in worktree-aware coordination.
    ///
    /// Mutating built-ins wrap their file writes in this gate so baseline
    /// capture never observes a half-applied edit. The default is `None`
    /// (plain local execution, no gating). Opaque operations (shell, MCP)
    /// are deliberately not gated; capture validates observed content
    /// instead.
    fn checkout_mutation_gate(&self) -> Option<Arc<crate::worktree::gate::CheckoutMutationGate>> {
        None
    }

    /// Ask the user a structured question and wait for a response.
    /// Returns the selected answer labels, or an error if the question cannot be delivered.
    ///
    /// # Arguments
    /// * `question_id` - Unique identifier for this question
    /// * `question` - The question text to display
    /// * `header` - Short header label (max 12 chars)
    /// * `options` - Available choices as (label, description) pairs
    /// * `multiple` - Whether multiple selections are allowed
    ///
    /// # Returns
    /// A vector of selected answer labels (option labels chosen by the user)
    ///
    /// # Default Implementation
    /// Falls back to stdin/stdout for CLI mode when no question channel is available.
    async fn ask_question(
        &self,
        _question_id: &str,
        question: &str,
        header: &str,
        options: &[(String, String)],
        multiple: bool,
    ) -> Result<Vec<String>, ToolError> {
        // Fallback to stdin/stdout for CLI mode
        use std::io::{self, Write};

        println!("\n{}", "=".repeat(60));
        println!("{}", header);
        println!("{}", "=".repeat(60));
        println!("{}\n", question);

        for (idx, (label, description)) in options.iter().enumerate() {
            println!("{}. {} - {}", idx + 1, label, description);
        }

        if multiple {
            println!(
                "\nEnter your choices (comma-separated numbers, or 'other' for custom input): "
            );
        } else {
            println!("\nEnter your choice (number, or 'other' for custom input): ");
        }

        print!("> ");
        io::stdout()
            .flush()
            .map_err(|e| ToolError::Other(e.into()))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| ToolError::Other(e.into()))?;
        let input = input.trim();

        if input.to_lowercase() == "other" {
            println!("Enter your custom response: ");
            print!("> ");
            io::stdout()
                .flush()
                .map_err(|e| ToolError::Other(e.into()))?;

            let mut custom = String::new();
            io::stdin()
                .read_line(&mut custom)
                .map_err(|e| ToolError::Other(e.into()))?;
            return Ok(vec![custom.trim().to_string()]);
        }

        let selections: Vec<usize> = input
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .collect();

        let mut answers = Vec::new();
        for sel in selections {
            if sel > 0 && sel <= options.len() {
                answers.push(options[sel - 1].0.clone());
            }
        }

        if answers.is_empty() {
            Err(ToolError::InvalidRequest(
                "No valid selections made".to_string(),
            ))
        } else {
            Ok(answers)
        }
    }
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

/// Unified tool trait that replaces BuiltInTool
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool definition (schema, description, etc.)
    fn definition(&self) -> querymt::chat::Tool;

    /// Get the tool definition for an effective workspace.
    ///
    /// Most tools have static schemas and use [`Self::definition`]. Dynamic
    /// tools can override this method so schema advertisement and validation
    /// reflect the current session working directory.
    fn definition_for_cwd(&self, _cwd: Option<&Path>) -> querymt::chat::Tool {
        self.definition()
    }

    /// Capabilities this tool requires. Default: empty.
    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    fn execution_class(&self) -> ToolExecutionClass {
        ToolExecutionClass::ParallelSafe
    }

    /// Compatibility aliases accepted for this tool's parameters. Builtin
    /// tools only; the target of every alias is a canonical schema name.
    /// Default: none.
    fn arg_aliases(&self) -> &'static [ArgAlias] {
        &[]
    }

    /// Optional rule allowing one item to be supplied without its wrapping
    /// array. Builtin tools only. Default: none.
    fn wrap_single_item(&self) -> Option<WrapRule> {
        None
    }

    /// Declare the file access a call with these arguments will perform.
    ///
    /// The default is [`FileAccess::None`], which keeps the call fully
    /// parallel. Implementations must return [`FileAccess::None`] when the
    /// arguments cannot be parsed: the call will fail its own validation and
    /// cannot write anything.
    fn file_access(&self, _args: &serde_json::Value, _cwd: Option<&Path>) -> FileAccess {
        FileAccess::None
    }

    /// Execute the tool with given arguments and context
    ///
    /// Returns canonical bounded [`querymt::chat::ToolResultPart`] values so the
    /// tool-result boundary never passes through the legacy recursive content
    /// representation.
    async fn call(
        &self,
        args: serde_json::Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<querymt::chat::ToolResultPart>, ToolError>;

    /// Returns a context-aware hint to show when this tool's output is truncated.
    ///
    /// This method is called when the tool's output exceeds the configured limits
    /// and is truncated. The hint is appended to the standard truncation message
    /// to provide tool-specific guidance on how to access the full content.
    ///
    /// # Returns
    /// * `Some(hint)` - A hint string to append (e.g., "TIP: Use search_text...")
    /// * `None` - Use only the standard truncation message (default)
    ///
    /// # Example
    /// ```ignore
    /// fn truncation_hint(&self) -> Option<&'static str> {
    ///     Some("TIP: Use search_text to find specific content in the full output.")
    /// }
    /// ```
    fn truncation_hint(&self) -> Option<&'static str> {
        None
    }

    /// Lifecycle hook: called when tool is initialized (optional)
    async fn initialize(&mut self, _context: &dyn ToolContext) -> Result<(), ToolError> {
        Ok(())
    }

    /// Lifecycle hook: called when tool is cleaned up (optional)
    async fn cleanup(&mut self) -> Result<(), ToolError> {
        Ok(())
    }
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
