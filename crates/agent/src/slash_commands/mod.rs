//! Custom slash command framework for QueryMT.
//!
//! This module implements discovery, registration, and prompt expansion
//! for user-defined slash commands. Commands are defined as markdown files
//! in `~/.qmt/commands` or `<PROJECT_ROOT>/.qmt/commands`.
//!
//! # Command File Format
//!
//! ```markdown
//! ---
//! description: Review the current changes
//! argument-hint: "[scope]"
//! tags: ["review", "code"]
//! ---
//!
//! Review the changes in scope: $ARGUMENTS
//! ```
//!
//! Filename (without `.md`) becomes the command name.
//! The body is the prompt template. `$ARGUMENTS` is substituted
//! with the user's trailing text.
//!
//! # Architecture
//!
//! - [`types`] — domain types (`SlashCommand`, `SlashCommandSource`, etc.)
//! - [`parser`] — parse `.md` files with YAML frontmatter
//! - [`discovery`] — find command files in global/project/configured paths
//! - [`registry`] — merge, deduplicate, and expose command lookup
//! - [`expander`] — detect `/name args` in user text and expand into prompts
//! - [`script`] — future script execution interface (not active yet)
//! - [`acp`] — convert registry entries to ACP `AvailableCommandsUpdate`

pub mod acp;
pub mod builtin_commands;
pub mod discovery;
pub mod expander;
pub mod parser;
pub mod registry;
pub mod script;
pub mod types;

pub use builtin_commands::{
    DelegateMode, DelegateSlashCommand, DelegateTarget, parse_delegate_command,
};
pub use discovery::{default_search_paths, discover_all, discover_from_source};
pub use expander::{expand_invocation, try_expand, try_parse_invocation};
pub use parser::parse_command_file;
pub use registry::SlashCommandRegistry;
pub use types::{
    CommandFrontmatter, SlashCommand, SlashCommandDiagnostic, SlashCommandExpansion,
    SlashCommandInvocation, SlashCommandScriptsConfig, SlashCommandSource, is_valid_command_name,
};

/// Parse the built-in command when the first prompt block is `/delegate`.
pub fn parse_builtin_invocation(
    req: &crate::acp::protocol::PromptRequest,
) -> Option<Result<DelegateSlashCommand, String>> {
    let crate::acp::protocol::ContentBlock::Text(text) = req.prompt.first()? else {
        return None;
    };
    let trimmed = text.text.trim_start();
    let rest = trimmed.strip_prefix("/delegate")?;
    if rest
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some(parse_delegate_command(rest))
}

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use crate::acp::protocol::{ContentBlock, PromptRequest, TextContent};

    fn request(text: &str) -> PromptRequest {
        PromptRequest::new(
            "session".to_string(),
            vec![ContentBlock::Text(TextContent::new(text))],
        )
    }

    #[test]
    fn detects_delegate_prompt() {
        let parsed =
            parse_builtin_invocation(&request("/delegate --async agent:remote-coder run tests"))
                .unwrap()
                .unwrap();
        assert_eq!(parsed.mode, DelegateMode::Async);
        assert_eq!(parsed.target, DelegateTarget::Agent("remote-coder".into()));
    }

    #[test]
    fn does_not_match_similar_command_name() {
        assert!(parse_builtin_invocation(&request("/delegate-more task")).is_none());
        assert!(parse_builtin_invocation(&request("ordinary prompt")).is_none());
    }
}
