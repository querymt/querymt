//! What the app acts on for a slash command (slash-command-actions, design
//! D1): the agent's descriptor with the fallback for fields it did not send,
//! and recognising a command in typed text.
//!
//! No UI types, so the send path (`open.rs`) and the palette share it.

use querymt_agent_session_core::view::{
    CommandInput, CommandSubmission, SlashCommandEffect, SlashCommandSource, SlashCommandView,
};

/// A command as the app treats it. `CommandTraits::of` is the only place that
/// decides what a missing descriptor field means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommandTraits {
    pub(crate) input: CommandInput,
    /// Whether the command may be queued while a turn runs.
    pub(crate) accepts_queue: bool,
    /// `None` when the agent sent no descriptor: neither prompt nor action.
    pub(crate) effect: Option<SlashCommandEffect>,
    pub(crate) source: Option<SlashCommandSource>,
}

impl CommandTraits {
    pub(crate) fn of(command: &SlashCommandView) -> Self {
        let input = command.input.unwrap_or(if command.input_hint.is_some() {
            CommandInput::Required
        } else {
            CommandInput::None
        });
        // `Some(vec![])` is a command that runs from no submission, not a
        // missing descriptor.
        let accepts_queue = command
            .accepts
            .as_ref()
            .is_none_or(|list| list.contains(&CommandSubmission::Queue));
        Self {
            input,
            accepts_queue,
            effect: command.effect,
            source: command.source,
        }
    }

    /// The command becomes a model turn. False for a command without an
    /// effect: do not read `!is_prompt()` as "action".
    pub(crate) fn is_prompt(&self) -> bool {
        self.effect == Some(SlashCommandEffect::Prompt)
    }

    /// The agent runs the command itself. False for a command without an
    /// effect: do not read `!is_action()` as "prompt".
    // Part of the resolver's contract (design D1); its first caller is
    // `add-slash-command-replies`, so only the tests use it for now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_action(&self) -> bool {
        self.effect == Some(SlashCommandEffect::Action)
    }
}

/// The advertised command `text` invokes: leading whitespace skipped, then
/// `/`, then a word equal to an advertised name (exact case) that ends the
/// text or is followed by whitespace. Anything else, a path included, is no
/// command. Follows the agent's own parser.
pub(crate) fn recognise<'a>(
    text: &str,
    commands: &'a [SlashCommandView],
) -> Option<&'a SlashCommandView> {
    let rest = text.trim_start().strip_prefix('/')?;
    let word = rest.split(char::is_whitespace).next().unwrap_or("");
    if word.is_empty() {
        return None;
    }
    commands
        .iter()
        .find(|c| c.name.trim_start_matches('/') == word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, hint: Option<&str>) -> SlashCommandView {
        SlashCommandView {
            name: name.into(),
            description: String::new(),
            input_hint: hint.map(Into::into),
            source: None,
            effect: None,
            input: None,
            accepts: None,
        }
    }

    #[test]
    fn descriptor_states_an_optional_input() {
        let mut c = command("compact", Some("[optional instructions]"));
        c.input = Some(CommandInput::Optional);
        c.effect = Some(SlashCommandEffect::Action);
        c.source = Some(SlashCommandSource::Builtin);
        c.accepts = Some(vec![CommandSubmission::Prompt, CommandSubmission::Queue]);
        let t = CommandTraits::of(&c);
        assert_eq!(t.input, CommandInput::Optional);
        assert!(t.accepts_queue);
        assert!(t.is_action());
        assert!(!t.is_prompt());
        assert_eq!(t.source, Some(SlashCommandSource::Builtin));
    }

    #[test]
    fn command_without_a_descriptor() {
        let hinted = CommandTraits::of(&command("review", Some("[scope]")));
        assert_eq!(hinted.input, CommandInput::Required);
        assert!(hinted.accepts_queue);
        assert!(!hinted.is_prompt());
        assert!(!hinted.is_action());
        assert_eq!(hinted.source, None);
        // Each field falls back on its own.
        assert_eq!(
            CommandTraits::of(&command("plain", None)).input,
            CommandInput::None
        );
        let mut partial = command("p", Some("[x]"));
        partial.input = Some(CommandInput::Optional);
        let t = CommandTraits::of(&partial);
        assert_eq!(t.input, CommandInput::Optional);
        assert!(t.accepts_queue, "no `accepts` falls back to queueing");
    }

    #[test]
    fn empty_accepts_is_not_a_missing_descriptor() {
        let mut c = command("freeze", None);
        c.accepts = Some(vec![]);
        assert!(!CommandTraits::of(&c).accepts_queue);
        c.accepts = Some(vec![CommandSubmission::Prompt]);
        assert!(!CommandTraits::of(&c).accepts_queue);
    }

    #[test]
    fn path_is_not_a_command() {
        let commands = [command("compact", None), command("tmp2", None)];
        assert!(recognise("/tmp/build.log is empty", &commands).is_none());
        assert!(recognise("/compact/x", &commands).is_none());
        assert!(recognise("/compactor", &commands).is_none());
        assert!(recognise("/", &commands).is_none());
        assert!(recognise("compact", &commands).is_none());
        assert!(recognise("see /compact", &commands).is_none());
        // Recognised: bare, with arguments, with leading whitespace.
        assert!(recognise("/compact", &commands).is_some());
        assert!(recognise("/compact keep the plan", &commands).is_some());
        assert!(recognise("  /compact\nmore", &commands).is_some());
    }

    #[test]
    fn name_case_must_match() {
        let commands = [command("compact", None)];
        assert!(recognise("/COMPACT", &commands).is_none());
        assert!(recognise("/Compact now", &commands).is_none());
        assert!(recognise("/compact", &commands).is_some());
    }

    #[test]
    fn a_name_with_a_slash_is_recognised() {
        let commands = [command("/compact", None)];
        assert!(recognise("/compact", &commands).is_some());
    }
}
