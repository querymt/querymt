//! Built-in slash commands handled natively by the agent.
//!
//! Built-ins are matched before registry expansion, so they never reach the
//! model as a prompt. This module is the single authority for them: the
//! catalog entry, the reserved name, and the one parser every submission path
//! (prompt, queue, steering, scheduled) uses to recognise them.

use crate::acp::protocol::{
    AvailableCommand, AvailableCommandInput, ContentBlock, Error, UnstructuredCommandInput,
};

/// A natively handled slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `/compact [instructions]`: summarize the conversation now. Always
    /// available; it cannot be turned off.
    Compact,
    /// `/worktree ...`: managed workspace administration. Runs only for local
    /// sessions on an agent with the workspace service enabled.
    Worktree,
}

/// A parsed built-in invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinInvocation {
    pub builtin: Builtin,
    /// Everything after the command name, trimmed. May span several lines.
    pub args: String,
}

/// What a session's host can run, which decides which built-ins it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinHost {
    /// The session executes on this node (not on a mesh peer).
    pub local: bool,
    /// The managed workspace service is enabled on this agent.
    pub workspace_service: bool,
}

impl Builtin {
    pub const ALL: [Builtin; 2] = [Builtin::Compact, Builtin::Worktree];

    /// Command name with its leading slash.
    pub const fn name(self) -> &'static str {
        match self {
            Builtin::Compact => "/compact",
            Builtin::Worktree => "/worktree",
        }
    }

    /// ACP catalog entry advertising this command.
    pub fn entry(self) -> AvailableCommand {
        match self {
            Builtin::Compact => AvailableCommand::new(
                self.name(),
                "Summarize the conversation now to free up context",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new(
                    "[optional instructions, e.g. focus on the API changes]",
                ),
            )),
            Builtin::Worktree => AvailableCommand::new(
                self.name(),
                "Create, hand off, archive and inspect isolated sessions; conservative removal, no automatic integration",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new(
                    "create [ref] [name] | create --include-local-changes [name] | list | status [id] | result [id] | publish [id] | handoff [name] [--summary] | archive [id] | remove [id] [--archive] | recover <id>",
                ),
            )),
        }
    }

    /// Whether a session on `host` can run this command. A command is only
    /// advertised when this is true, so the catalog matches what executes.
    pub const fn available(self, host: BuiltinHost) -> bool {
        match self {
            Builtin::Compact => true,
            Builtin::Worktree => host.local && host.workspace_service,
        }
    }

    /// Whether a user command file may not use `name` (without the slash).
    pub fn is_reserved(name: &str) -> bool {
        Self::ALL
            .iter()
            .any(|builtin| builtin.name().trim_start_matches('/') == name)
    }

    /// Recognise a built-in invocation.
    ///
    /// A built-in is a prompt whose first text block starts with the exact
    /// command name (so `/compactor` or `/compact/foo` are not caught). It must
    /// be the only block; anything else is an error rather than silently
    /// reaching the model. Returns `Ok(None)` for every other prompt.
    pub fn parse(prompt: &[ContentBlock]) -> Result<Option<BuiltinInvocation>, Error> {
        let Some(ContentBlock::Text(text)) = prompt.first() else {
            return Ok(None);
        };
        let trimmed = text.text.trim_start();
        for builtin in Self::ALL {
            let Some(rest) = trimmed.strip_prefix(builtin.name()) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
                continue;
            }
            if prompt.len() != 1 {
                return Err(Error::invalid_params()
                    .data(format!("{} requires a single text block", builtin.name())));
            }
            return Ok(Some(BuiltinInvocation {
                builtin,
                args: rest.trim().to_string(),
            }));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::TextContent;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextContent::new(s))]
    }

    fn parse(s: &str) -> Option<BuiltinInvocation> {
        Builtin::parse(&text(s)).expect("single block parses")
    }

    #[test]
    fn bare_builtin_has_no_arguments() {
        let inv = parse("/compact").unwrap();
        assert_eq!(inv.builtin, Builtin::Compact);
        assert_eq!(inv.args, "");
        assert_eq!(parse("  /compact  \n").unwrap().args, "");
        assert_eq!(parse("/worktree").unwrap().builtin, Builtin::Worktree);
    }

    #[test]
    fn arguments_are_everything_after_the_name_and_may_span_lines() {
        let inv =
            parse("/compact focus on the API changes\nand ignore the test refactoring").unwrap();
        assert_eq!(
            inv.args,
            "focus on the API changes\nand ignore the test refactoring"
        );
        let inv = parse("/worktree create HEAD w2").unwrap();
        assert_eq!(inv.builtin, Builtin::Worktree);
        assert_eq!(inv.args, "create HEAD w2");
    }

    #[test]
    fn similar_names_and_non_leading_names_are_not_matched() {
        assert!(parse("/compactor").is_none());
        assert!(parse("/compact/foo").is_none());
        assert!(parse("/worktrees").is_none());
        assert!(parse("please /compact").is_none());
        assert!(parse("compact").is_none());
        assert!(parse("/review src").is_none());
        assert!(Builtin::parse(&[]).unwrap().is_none());
    }

    #[test]
    fn a_builtin_must_be_the_only_block() {
        let mut two = text("/compact");
        two.extend(text("more"));
        assert!(Builtin::parse(&two).is_err());
        let mut two = text("/worktree list");
        two.extend(text("more"));
        assert!(Builtin::parse(&two).is_err());
        // Not a built-in: any number of blocks passes through.
        let mut two = text("hello");
        two.extend(text("/compact"));
        assert!(Builtin::parse(&two).unwrap().is_none());
    }

    #[test]
    fn compact_is_always_available_and_worktree_needs_a_local_workspace_host() {
        let host = |local, workspace_service| BuiltinHost {
            local,
            workspace_service,
        };
        for local in [true, false] {
            for ws in [true, false] {
                assert!(Builtin::Compact.available(host(local, ws)));
            }
        }
        assert!(Builtin::Worktree.available(host(true, true)));
        assert!(!Builtin::Worktree.available(host(true, false)));
        assert!(!Builtin::Worktree.available(host(false, true)));
        assert!(!Builtin::Worktree.available(host(false, false)));
    }

    #[test]
    fn every_builtin_has_a_matching_catalog_entry_and_is_reserved() {
        for builtin in Builtin::ALL {
            assert_eq!(builtin.entry().name, builtin.name());
            assert!(builtin.name().starts_with('/'));
            assert!(Builtin::is_reserved(builtin.name().trim_start_matches('/')));
        }
        assert!(!Builtin::is_reserved("review"));
    }
}
