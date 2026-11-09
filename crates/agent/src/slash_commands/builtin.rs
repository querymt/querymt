//! Built-in slash commands handled natively by the agent.
//!
//! Built-ins are matched before registry expansion, so they take priority over
//! user-defined commands with the same name, and they never reach the model as
//! a prompt.

use crate::acp::protocol::{
    AvailableCommand, AvailableCommandInput, ContentBlock, UnstructuredCommandInput,
};

/// Name (with leading slash) of the native compaction command.
pub const COMPACT_COMMAND: &str = "/compact";

/// A parsed `/compact [instructions]` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactCommand {
    /// Optional user focus for the summary (everything after `/compact`).
    pub instructions: Option<String>,
}

/// Detect a native `/compact` invocation.
///
/// Matches only a prompt consisting of a single text block whose first word is
/// exactly `/compact` (so `/compactor` or `/compact/foo` are not caught).
/// Everything after the command name, which may span several lines, is the
/// custom instruction text.
pub fn native_compact_command(prompt: &[ContentBlock]) -> Option<CompactCommand> {
    let [ContentBlock::Text(text)] = prompt else {
        return None;
    };
    let rest = text.text.trim_start().strip_prefix(COMPACT_COMMAND)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some(CompactCommand {
        instructions: (!instructions.is_empty()).then(|| instructions.to_string()),
    })
}

/// ACP catalog entry advertising `/compact`.
pub fn compact_available_command() -> AvailableCommand {
    AvailableCommand::new(
        COMPACT_COMMAND,
        "Summarize the conversation now to free up context",
    )
    .input(AvailableCommandInput::Unstructured(
        UnstructuredCommandInput::new("[optional instructions, e.g. focus on the API changes]"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::TextContent;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(TextContent::new(s))]
    }

    #[test]
    fn bare_compact_has_no_instructions() {
        assert_eq!(
            native_compact_command(&text("/compact")),
            Some(CompactCommand { instructions: None })
        );
        assert_eq!(
            native_compact_command(&text("  /compact  \n")),
            Some(CompactCommand { instructions: None })
        );
    }

    #[test]
    fn compact_carries_custom_instructions() {
        let cmd = native_compact_command(&text(
            "/compact focus on the API changes\nand ignore the test refactoring",
        ))
        .unwrap();
        assert_eq!(
            cmd.instructions.as_deref(),
            Some("focus on the API changes\nand ignore the test refactoring")
        );
    }

    #[test]
    fn similar_names_and_multi_block_prompts_are_not_matched() {
        assert_eq!(native_compact_command(&text("/compactor")), None);
        assert_eq!(native_compact_command(&text("/compact/foo")), None);
        assert_eq!(native_compact_command(&text("please /compact")), None);
        assert_eq!(native_compact_command(&text("compact")), None);
        let mut two = text("/compact");
        two.extend(text("more"));
        assert_eq!(native_compact_command(&two), None);
    }

    #[test]
    fn catalog_entry_is_named_compact() {
        assert_eq!(compact_available_command().name, "/compact");
    }
}
