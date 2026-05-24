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
pub mod discovery;
pub mod expander;
pub mod parser;
pub mod registry;
pub mod script;
pub mod types;

pub use discovery::{default_search_paths, discover_all, discover_from_source};
pub use expander::{expand_invocation, try_expand, try_parse_invocation};
pub use parser::parse_command_file;
pub use registry::SlashCommandRegistry;
pub use types::{
    CommandFrontmatter, SlashCommand, SlashCommandDiagnostic, SlashCommandExpansion,
    SlashCommandInvocation, SlashCommandScriptsConfig, SlashCommandSource, is_valid_command_name,
};
