//! Built-in slash commands handled natively by the agent.
//!
//! Built-ins are matched before registry expansion, so they never reach the
//! model as a prompt. This module is the single authority for them: the
//! catalog entry, the reserved name, and the one parser every submission path
//! (prompt, queue, steering, scheduled) uses to recognise them.

use crate::acp::protocol::{
    AvailableCommand, AvailableCommandInput, ContentBlock, UnstructuredCommandInput,
};
use crate::agent::turn_control::InputDelivery;

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

/// How a prompt reaches the agent. Which built-ins each path accepts is
/// decided in one place, [`Builtin::accepts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submission {
    /// `session/prompt`, where native commands are intercepted before any turn.
    Prompt,
    /// Input that starts or waits for its own turn (queued or scheduled).
    Turn,
    /// Input appended to a live model run, where it reaches the model as text.
    Steer,
}

impl Submission {
    const fn verb(self) -> &'static str {
        match self {
            Submission::Prompt => "submitted as a prompt",
            Submission::Turn => "queued or scheduled",
            Submission::Steer => "steered into a running turn",
        }
    }
}

impl From<InputDelivery> for Submission {
    fn from(delivery: InputDelivery) -> Self {
        match delivery {
            InputDelivery::Steer => Submission::Steer,
            InputDelivery::Queue => Submission::Turn,
        }
    }
}

/// Why a prompt that names a built-in cannot be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BuiltinError {
    /// A built-in must be the only block of its prompt.
    #[error("{} requires a single text block", .0.name())]
    NotSingleTextBlock(Builtin),
    /// The built-in does not run when submitted this way.
    #[error("{} cannot be {}; {}", .builtin.name(), .via.verb(), .builtin.alternatives())]
    NotAccepted { builtin: Builtin, via: Submission },
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

    /// Whether this built-in runs when submitted via `via`. Exhaustive on
    /// purpose: a new built-in or submission path must be decided here.
    pub const fn accepts(self, via: Submission) -> bool {
        match (self, via) {
            (Builtin::Compact, Submission::Prompt | Submission::Turn) => true,
            (Builtin::Compact, Submission::Steer) => false,
            (Builtin::Worktree, Submission::Prompt) => true,
            (Builtin::Worktree, Submission::Turn | Submission::Steer) => false,
        }
    }

    /// What to do instead, derived from [`Self::accepts`] so it cannot drift.
    const fn alternatives(self) -> &'static str {
        if self.accepts(Submission::Turn) {
            "queue it or submit it with session/prompt"
        } else {
            "submit it with session/prompt"
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
    ///
    /// # Errors
    ///
    /// [`BuiltinError::NotSingleTextBlock`] when a built-in is accompanied by
    /// other blocks.
    pub fn parse(prompt: &[ContentBlock]) -> Result<Option<BuiltinInvocation>, BuiltinError> {
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
                return Err(BuiltinError::NotSingleTextBlock(builtin));
            }
            return Ok(Some(BuiltinInvocation {
                builtin,
                args: rest.trim().to_string(),
            }));
        }
        Ok(None)
    }

    /// [`Self::parse`], then reject a built-in that does not run when
    /// submitted via `via`.
    ///
    /// # Errors
    ///
    /// Everything [`Self::parse`] returns, plus [`BuiltinError::NotAccepted`].
    pub fn parse_for(
        prompt: &[ContentBlock],
        via: Submission,
    ) -> Result<Option<BuiltinInvocation>, BuiltinError> {
        match Self::parse(prompt)? {
            Some(invocation) if !invocation.builtin.accepts(via) => {
                Err(BuiltinError::NotAccepted {
                    builtin: invocation.builtin,
                    via,
                })
            }
            other => Ok(other),
        }
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
        assert_eq!(
            Builtin::parse(&two),
            Err(BuiltinError::NotSingleTextBlock(Builtin::Compact))
        );
        let mut two = text("/worktree list");
        two.extend(text("more"));
        assert_eq!(
            Builtin::parse(&two),
            Err(BuiltinError::NotSingleTextBlock(Builtin::Worktree))
        );
        // Not a built-in: any number of blocks passes through.
        let mut two = text("hello");
        two.extend(text("/compact"));
        assert!(Builtin::parse(&two).unwrap().is_none());
    }

    #[test]
    fn submission_table_is_the_single_source_of_truth() {
        use Submission::{Prompt, Steer, Turn};
        let table = [
            (Builtin::Compact, Prompt, true),
            (Builtin::Compact, Turn, true),
            (Builtin::Compact, Steer, false),
            (Builtin::Worktree, Prompt, true),
            (Builtin::Worktree, Turn, false),
            (Builtin::Worktree, Steer, false),
        ];
        // Every built-in is covered, so a new one fails this test until listed.
        assert_eq!(table.len(), Builtin::ALL.len() * 3);
        for (builtin, via, accepted) in table {
            assert_eq!(builtin.accepts(via), accepted, "{builtin:?} via {via:?}");
            let parsed = Builtin::parse_for(&text(builtin.name()), via);
            if accepted {
                assert_eq!(parsed.unwrap().unwrap().builtin, builtin);
            } else {
                assert_eq!(parsed, Err(BuiltinError::NotAccepted { builtin, via }));
            }
        }
        // Ordinary prompts are never rejected, whatever the path.
        for via in [Prompt, Turn, Steer] {
            assert_eq!(Builtin::parse_for(&text("hello"), via), Ok(None));
        }
    }

    #[test]
    fn delivery_maps_onto_submission() {
        assert_eq!(Submission::from(InputDelivery::Steer), Submission::Steer);
        assert_eq!(Submission::from(InputDelivery::Queue), Submission::Turn);
    }

    #[test]
    fn errors_name_the_builtin_and_the_way_forward() {
        assert_eq!(
            BuiltinError::NotSingleTextBlock(Builtin::Compact).to_string(),
            "/compact requires a single text block"
        );
        assert_eq!(
            BuiltinError::NotAccepted {
                builtin: Builtin::Compact,
                via: Submission::Steer
            }
            .to_string(),
            "/compact cannot be steered into a running turn; queue it or submit it with session/prompt"
        );
        // A built-in that cannot be queued must not suggest queueing it.
        assert_eq!(
            BuiltinError::NotAccepted {
                builtin: Builtin::Worktree,
                via: Submission::Steer
            }
            .to_string(),
            "/worktree cannot be steered into a running turn; submit it with session/prompt"
        );
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
