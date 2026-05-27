//! Runtime slash command plugin interfaces.
//!
//! Runtime commands execute Rust code directly instead of asking the main agent
//! loop to interpret a prompt template. The slash-command core stays generic;
//! individual plugins own their own domain dependencies.

use crate::session::provider::SessionHandle;
use crate::slash_commands::types::SlashCommandInvocation;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use typeshare::typeshare;

#[derive(Debug, Clone)]
pub enum PostTurnAction {
    CreatePlanPacket {
        command_id: String,
        command_name: String,
        objective: Option<String>,
    },
}

impl PostTurnAction {
    pub fn command_id(&self) -> &str {
        match self {
            Self::CreatePlanPacket { command_id, .. } => command_id,
        }
    }

    pub fn command_name(&self) -> &str {
        match self {
            Self::CreatePlanPacket { command_name, .. } => command_name,
        }
    }
}

// ── Command output ────────────────────────────────────────────────────────────

/// Severity level of command output.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutputLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// How the command output body should be rendered.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutputDisplay {
    Text,
    Markdown,
}

/// Generic structured output from a runtime slash command.
///
/// This is intentionally domain-agnostic: work packets, git, MCP, debug, etc.
/// all return the same displayable output shape. Domain-specific effects
/// (e.g. work packet creation) persist through their own stores/events;
/// this struct is only for UI display.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    /// Optional title shown above the body (e.g. "Active packet").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Main content.
    pub body: String,
    /// Severity level.
    pub level: CommandOutputLevel,
    /// How to render the body.
    pub display: CommandOutputDisplay,
}

impl CommandOutput {
    /// Convenience constructor for plain text output.
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            title: None,
            body: body.into(),
            level: CommandOutputLevel::Info,
            display: CommandOutputDisplay::Text,
        }
    }

    /// Convenience constructor for markdown output.
    pub fn markdown(body: impl Into<String>) -> Self {
        Self {
            title: None,
            body: body.into(),
            level: CommandOutputLevel::Info,
            display: CommandOutputDisplay::Markdown,
        }
    }

    /// Convenience constructor for success output.
    pub fn success(body: impl Into<String>) -> Self {
        Self {
            title: None,
            body: body.into(),
            level: CommandOutputLevel::Success,
            display: CommandOutputDisplay::Markdown,
        }
    }

    /// Convenience constructor for error output.
    pub fn error(body: impl Into<String>) -> Self {
        Self {
            title: None,
            body: body.into(),
            level: CommandOutputLevel::Error,
            display: CommandOutputDisplay::Text,
        }
    }

    /// Builder: set title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

// ── Slash command execution result ────────────────────────────────────────────

/// What happened after processing a slash command.
#[derive(Debug, Clone)]
pub enum SlashCommandExecution {
    /// Fully handled by runtime code — no LLM turn needed.
    Handled { output: CommandOutput },
    /// Needs an LLM turn with the given prompt text.
    Prompt {
        prompt: String,
        post_turn_action: Option<PostTurnAction>,
    },
    /// Runtime code did deterministic setup, then wants an LLM continuation.
    Hybrid {
        output: Option<CommandOutput>,
        prompt: String,
        post_turn_action: Option<PostTurnAction>,
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
