//! Runtime slash command plugin interfaces.
//!
//! Runtime commands execute Rust code directly instead of asking the main agent
//! loop to interpret a prompt template. The slash-command core stays generic;
//! individual plugins own their own domain dependencies.

use crate::session::provider::SessionHandle;
use crate::slash_commands::types::SlashCommandInvocation;
use async_trait::async_trait;
use std::sync::Arc;

/// What happened after processing a slash command.
#[derive(Debug, Clone)]
pub enum SlashCommandExecution {
    /// Fully handled by runtime code — no LLM turn needed.
    Handled { response: String },
    /// Needs an LLM turn with the given prompt text.
    Prompt { prompt: String },
    /// Runtime code did deterministic setup, then wants an LLM continuation.
    Hybrid {
        response: Option<String>,
        prompt: String,
    },
    /// Not recognised; allow other backends to try.
    NotHandled,
}

/// Static metadata for an advertised runtime command.
#[derive(Debug, Clone)]
pub struct RuntimeCommandDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// Generic host capabilities exposed to runtime slash command plugins.
///
/// This intentionally avoids domain-specific fields such as work-packet stores.
/// Plugins own their own dependencies and use the host only for session-scoped
/// information and access to the current turn's session handle.
pub trait SlashCommandHost: Send + Sync {
    fn session_id(&self) -> &str;
    fn session_handle(&self) -> &SessionHandle;
}

/// A runtime slash command plugin.
///
/// One plugin may advertise and handle multiple commands.
#[async_trait]
pub trait RuntimeSlashCommandPlugin: Send + Sync {
    /// Command descriptors this plugin provides.
    fn descriptors(&self) -> Vec<RuntimeCommandDescriptor>;

    /// Execute a slash command invocation.
    async fn execute(
        &self,
        invocation: &SlashCommandInvocation,
        host: &dyn SlashCommandHost,
    ) -> SlashCommandExecution;
}

/// A registry entry mapping a command name to its plugin and descriptor.
#[derive(Clone)]
pub struct RegisteredRuntimeCommand {
    pub descriptor: RuntimeCommandDescriptor,
    pub plugin: Arc<dyn RuntimeSlashCommandPlugin>,
}
