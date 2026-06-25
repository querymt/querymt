use crate::acp::protocol::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, SessionId,
    SessionNotification, SessionUpdate, UnstructuredCommandInput,
};
use crate::slash_commands::registry::SlashCommandRegistry;

/// Convert the registry into an ACP `AvailableCommandsUpdate`.
pub fn registry_to_acp_update(registry: &SlashCommandRegistry) -> AvailableCommandsUpdate {
    let commands: Vec<AvailableCommand> = registry
        .all()
        .map(|cmd| {
            let mut acp_cmd = AvailableCommand::new(format!("/{}", cmd.name), &cmd.description);

            if cmd.argument_hint.is_some() || cmd.template.contains("$ARGUMENTS") {
                acp_cmd = acp_cmd.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(cmd.argument_hint.clone().unwrap_or_default()),
                ));
            }

            acp_cmd
        })
        .collect();

    AvailableCommandsUpdate::new(commands)
}

/// Build a `SessionNotification` that advertises available commands.
pub fn build_commands_notification(
    session_id: &str,
    registry: &SlashCommandRegistry,
) -> SessionNotification {
    let update = registry_to_acp_update(registry);
    SessionNotification::new(
        SessionId::from(session_id.to_string()),
        SessionUpdate::AvailableCommandsUpdate(update),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_commands::types::{SlashCommand, SlashCommandSource};
    use std::path::PathBuf;

    fn make_command(name: &str, description: &str, argument_hint: Option<&str>) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            source: SlashCommandSource::Global(PathBuf::from("/test")),
            path: PathBuf::from(format!("/test/{}.md", name)),
            description: description.to_string(),
            argument_hint: argument_hint.map(|s| s.to_string()),
            tags: vec![],
            template: if argument_hint.is_some() {
                format!("{}: $ARGUMENTS", description)
            } else {
                description.to_string()
            },
            script: None,
            requires_script: false,
        }
    }

    #[test]
    fn test_empty_registry() {
        let registry = SlashCommandRegistry::new();
        let update = registry_to_acp_update(&registry);
        assert!(update.available_commands.is_empty());
    }

    #[test]
    fn test_command_with_hint() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("review", "Review changes", Some("[scope]")));

        let update = registry_to_acp_update(&registry);
        assert_eq!(update.available_commands.len(), 1);

        let cmd = &update.available_commands[0];
        assert_eq!(cmd.name, "/review");
        assert_eq!(cmd.description, "Review changes");
        assert!(cmd.input.is_some());
    }

    #[test]
    fn test_command_without_hint() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("help", "Show help", None));

        let update = registry_to_acp_update(&registry);
        assert_eq!(update.available_commands.len(), 1);
        assert_eq!(update.available_commands[0].name, "/help");
        // No argument hint and no $ARGUMENTS in template
        assert!(update.available_commands[0].input.is_none());
    }

    #[test]
    fn test_build_notification() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("test", "Test command", None));

        let notif = build_commands_notification("session-1", &registry);
        assert_eq!(notif.session_id.to_string(), "session-1");
    }
}
