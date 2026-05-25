use crate::slash_commands::registry::SlashCommandRegistry;
use crate::slash_commands::runtime::{SlashCommandExecution, SlashCommandHost};
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
    let name_end = after_slash
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .unwrap_or(after_slash.len());

    if name_end == 0 {
        return None;
    }

    let name = &after_slash[..name_end];

    if !name.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }

    if registry.get(name).is_none() && !registry.is_runtime(name) {
        return None;
    }

    let arguments = after_slash[name_end..].trim().to_string();
    let original_text = trimmed.to_string();

    Some(SlashCommandInvocation {
        name: name.to_string(),
        arguments,
        original_text,
    })
}

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

pub fn try_expand(text: &str, registry: &SlashCommandRegistry) -> Option<SlashCommandExpansion> {
    let invocation = try_parse_invocation(text, registry)?;
    if registry.is_runtime(&invocation.name) {
        return None;
    }
    expand_invocation(&invocation, registry)
}

pub async fn try_dispatch_runtime(
    text: &str,
    registry: &SlashCommandRegistry,
    host: &dyn SlashCommandHost,
) -> Option<SlashCommandExecution> {
    let invocation = try_parse_invocation(text, registry)?;
    let command = registry.get_runtime(&invocation.name)?;
    let result = command.plugin.execute(&invocation, host).await;
    match result {
        SlashCommandExecution::NotHandled => None,
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unknown_command_returns_none() {
        let reg = SlashCommandRegistry::new();
        assert!(try_parse_invocation("/missing arg", &reg).is_none());
    }
}
