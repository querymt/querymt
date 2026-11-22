//! The `ToolContext` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::context::*;
use crate::knowledge::{KnowledgeStore, ScopePolicy};
use async_trait::async_trait;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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

    /// Per-session todo lists for the todo tools. `None` in contexts that have
    /// no agent behind them; the todo tools then report todos as unavailable.
    fn todo_store(&self) -> Option<Arc<crate::tools::builtins::todo::TodoStore>> {
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
    fn emit_event(&self, _kind: querymt_agent_core::events::AgentEventKind) {}

    /// ID of the tool call this context was built for, when known. Used to
    /// keep concurrent approval prompts distinct.
    fn tool_call_id(&self) -> Option<&str> {
        None
    }

    /// Get the current working directory, if set.
    fn cwd(&self) -> Option<&Path>;

    /// Resolve a path argument called `path`. See [`Self::resolve_path_arg`].
    fn resolve_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        self.resolve_path_arg("path", path)
    }

    /// Resolve the value of the path argument `arg`.
    ///
    /// A leading `~` or `~/` is the home directory of the user running the
    /// agent process; when none is known the call fails with an error naming
    /// `arg`. Absolute paths are used as written, and a relative path is joined
    /// to the working directory (an error if there is none).
    fn resolve_path_arg(&self, arg: &str, path: &str) -> Result<PathBuf, ToolError> {
        let path = super::path::expand_tilde_arg(arg, path)?;
        if path.is_absolute() {
            Ok(path.into_owned())
        } else {
            self.cwd().map(|cwd| cwd.join(path)).ok_or_else(|| {
                ToolError::InvalidRequest(
                    "Cannot resolve relative path: no working directory set".into(),
                )
            })
        }
    }

    /// Managed worktree workspace service, when the runtime has one.
    fn workspace_service(&self) -> Option<Arc<crate::worktree::WorkspaceService>> {
        None
    }

    /// The session's workspace code index cache, once the workspace index is ready.
    ///
    /// Symbol tools resolve symbols through it so unchanged files are not parsed again.
    /// `None` (the default) means tools parse files directly.
    fn workspace_index(&self) -> Option<querymt_agent_index::CodeCacheClient> {
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
    fn workspace_query_bridge(&self) -> Option<&dyn crate::workspace_query::WorkspaceQueryBridge> {
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
    /// * `options` - Available choices
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
        options: &[QuestionOption],
        multiple: bool,
    ) -> Result<Vec<String>, ToolError> {
        // Fallback to stdin/stdout for CLI mode
        use std::io::{self, Write};

        println!("\n{}", "=".repeat(60));
        println!("{}", header);
        println!("{}", "=".repeat(60));
        println!("{}\n", question);

        for (idx, option) in options.iter().enumerate() {
            println!("{}. {} - {}", idx + 1, option.label, option.description);
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
                answers.push(options[sel - 1].label.clone());
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
