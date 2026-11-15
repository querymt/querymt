//! The `Tool` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::context::*;
use crate::tools::tool_context::ToolContext;
use async_trait::async_trait;
use std::path::Path;

/// Unified tool trait that replaces BuiltInTool
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool definition (schema, description, etc.)
    fn definition(&self) -> querymt::chat::Tool;

    /// Whether the tool leaves workspace files untouched (it only reads, or
    /// only changes session / knowledge state). Such calls take no snapshot.
    /// Default: `false`, so a missing override costs two workspace scans and
    /// may trigger duplicate-code review and re-indexing of changed files.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Approval the tool demands regardless of the agent's `permissions`
    /// lists. The effective need is the stricter of this and the lists
    /// (`always_ask` makes a `Session` default ask on every call). A
    /// `pre_tool_use` hook that allows the call still skips the prompt.
    /// Default: none.
    fn default_approval(&self) -> querymt_agent_core::config::approval::ApprovalNeed {
        querymt_agent_core::config::approval::ApprovalNeed::None
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
