use crate::slash_commands::registry::SlashCommandRegistry;
use crate::slash_commands::types::{SlashCommandExpansion, SlashCommandInvocation};

/// Try to parse a slash command invocation from user text.
///
/// Returns `Some(Invocation)` if the text starts with `/<valid-name>` and
/// that name exists in the registry. Returns `None` otherwise (passes through
/// as a normal prompt).
///
/// Rules:
/// - Only triggers on the first text content.
/// - Name must match `[a-zA-Z][a-zA-Z0-9_-]*`.
/// - Unknown commands pass through unchanged (no expansion).
/// - Arguments are everything after the command name (may be multiline).
pub fn try_parse_invocation(
    text: &str,
    registry: &SlashCommandRegistry,
) -> Option<SlashCommandInvocation> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }

    let after_slash = &trimmed[1..];

    // Extract the command name (letters, digits, hyphens, underscores)
    let name_end = after_slash
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .unwrap_or(after_slash.len());

    if name_end == 0 {
        return None;
    }

    let name = &after_slash[..name_end];

    // First char must be alphabetic (prevents /tmp, /123, etc.)
    if !name.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }

    // Must exist in registry
    registry.get(name)?;

    let arguments = after_slash[name_end..].trim().to_string();
    let original_text = trimmed.to_string();

    Some(SlashCommandInvocation {
        name: name.to_string(),
        arguments,
        original_text,
    })
}

/// Expand a slash command invocation into prompt text.
///
/// The expansion wraps the command template with context so the model
/// understands it was invoked as a slash command.
pub fn expand_invocation(
    invocation: &SlashCommandInvocation,
    registry: &SlashCommandRegistry,
) -> Option<SlashCommandExpansion> {
    let cmd = registry.get(&invocation.name)?;

    let rendered_template = cmd.template.replace("$ARGUMENTS", &invocation.arguments);

    let prompt_text = format!(
        "The user invoked the slash command `/{}`.\n\n\
         Command description:\n\
         {}\n\n\
         Command instructions:\n\
         {}\n\n\
         {}",
        invocation.name,
        cmd.description,
        rendered_template,
        if invocation.arguments.is_empty() {
            "Follow the command instructions.".to_string()
        } else {
            "Follow the command instructions using the provided arguments.".to_string()
        },
    );

    Some(SlashCommandExpansion {
        invocation: invocation.clone(),
        prompt_text,
    })
}

/// Convenience: try to detect and expand a slash command from user text.
///
/// Returns `Some(Expansion)` if the text is a recognized command,
/// `None` if it should pass through as a normal prompt.
pub fn try_expand(text: &str, registry: &SlashCommandRegistry) -> Option<SlashCommandExpansion> {
    let invocation = try_parse_invocation(text, registry)?;
    expand_invocation(&invocation, registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_commands::types::SlashCommand;
    use std::path::PathBuf;

    fn make_registry(commands: &[(&str, &str)]) -> SlashCommandRegistry {
        let mut reg = SlashCommandRegistry::new();
        for (name, template) in commands {
            reg.register(SlashCommand {
                name: name.to_string(),
                source: crate::slash_commands::types::SlashCommandSource::Global(PathBuf::from(
                    "/test",
                )),
                path: PathBuf::from(format!("/test/{}.md", name)),
                description: format!("Command: {}", name),
                argument_hint: None,
                tags: vec![],
                template: template.to_string(),
                script: None,
                requires_script: false,
            });
        }
        reg
    }

    #[test]
    fn test_simple_command() {
        let reg = make_registry(&[("review", "Review: $ARGUMENTS")]);
        let inv = try_parse_invocation("/review", &reg).unwrap();
        assert_eq!(inv.name, "review");
        assert_eq!(inv.arguments, "");
    }

    #[test]
    fn test_command_with_args() {
        let reg = make_registry(&[("review", "Review: $ARGUMENTS")]);
        let inv = try_parse_invocation("/review src/foo.rs", &reg).unwrap();
        assert_eq!(inv.name, "review");
        assert_eq!(inv.arguments, "src/foo.rs");
    }

    #[test]
    fn test_command_with_multiline_args() {
        let reg = make_registry(&[("trace", "Analyze: $ARGUMENTS")]);
        let inv = try_parse_invocation("/trace line1\nline2\nline3", &reg).unwrap();
        assert_eq!(inv.name, "trace");
        assert_eq!(inv.arguments, "line1\nline2\nline3");
    }

    #[test]
    fn test_unknown_command_passes_through() {
        let reg = make_registry(&[("review", "Review")]);
        assert!(try_parse_invocation("/unknown", &reg).is_none());
    }

    #[test]
    fn test_path_like_text_passes_through() {
        let reg = make_registry(&[("review", "Review")]);
        // /tmp is NOT registered so it passes through
        assert!(try_parse_invocation("/tmp/file.txt", &reg).is_none());
    }

    #[test]
    fn test_no_slash_passes_through() {
        let reg = make_registry(&[("review", "Review")]);
        assert!(try_parse_invocation("review this", &reg).is_none());
    }

    #[test]
    fn test_bare_slash_passes_through() {
        let reg = make_registry(&[("review", "Review")]);
        assert!(try_parse_invocation("/", &reg).is_none());
    }

    #[test]
    fn test_leading_whitespace_stripped() {
        let reg = make_registry(&[("review", "Review")]);
        let inv = try_parse_invocation("  /review", &reg).unwrap();
        assert_eq!(inv.name, "review");
    }

    #[test]
    fn test_expand_simple() {
        let reg = make_registry(&[("review", "Review: $ARGUMENTS")]);
        let expansion = try_expand("/review src/main.rs", &reg).unwrap();
        assert_eq!(expansion.invocation.name, "review");
        assert!(expansion.prompt_text.contains("slash command `/review`"));
        assert!(expansion.prompt_text.contains("src/main.rs"));
        assert!(expansion.prompt_text.contains("Review: src/main.rs"));
    }

    #[test]
    fn test_expand_no_args() {
        let reg = make_registry(&[("help", "Show help text")]);
        let expansion = try_expand("/help", &reg).unwrap();
        assert!(
            expansion
                .prompt_text
                .contains("Follow the command instructions.")
        );
        assert!(
            !expansion
                .prompt_text
                .contains("using the provided arguments")
        );
    }

    #[test]
    fn test_expand_preserves_description() {
        let reg = make_registry(&[("review", "Review code changes")]);
        let expansion = try_expand("/review", &reg).unwrap();
        assert!(expansion.prompt_text.contains("Command: review"));
        assert!(expansion.prompt_text.contains("Review code changes"));
    }

    #[test]
    fn test_numeric_start_rejected() {
        let reg = make_registry(&[("123abc", "Numeric")]);
        assert!(try_parse_invocation("/123abc", &reg).is_none());
    }

    #[test]
    fn test_hyphenated_command() {
        let reg = make_registry(&[("explain-error", "Explain an error")]);
        let inv = try_parse_invocation("/explain-error cargo test", &reg).unwrap();
        assert_eq!(inv.name, "explain-error");
        assert_eq!(inv.arguments, "cargo test");
    }
}
