use crate::acp::protocol::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, Meta, SessionId,
    SessionNotification, SessionUpdate, UnstructuredCommandInput,
};
use crate::slash_commands::registry::SlashCommandRegistry;
use crate::slash_commands::types::ArgumentsNeed;
use querymt_agent_core::events::{SlashCommandEffect, SlashCommandSource};

/// The submissions a catalog entry can be run from, as `accepts` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptedSubmission {
    /// `session/prompt`.
    Prompt,
    /// Queued or scheduled input.
    Queue,
}

impl AcceptedSubmission {
    fn as_str(self) -> &'static str {
        match self {
            AcceptedSubmission::Prompt => "prompt",
            AcceptedSubmission::Queue => "queue",
        }
    }
}

/// One operation a command offers as its first argument, advertised as an
/// element of `_meta.querymt.subcommands`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subcommand {
    /// One word, as typed after the command name.
    pub name: &'static str,
    /// One line for people.
    pub description: &'static str,
}

/// The `_meta` of a catalog entry: `_meta.querymt` with the command's
/// `source`, `effect`, `input` and `accepts`, and its `subcommands` when it
/// has any. Built-in and user-defined commands share these fields.
pub fn descriptor_meta(
    source: SlashCommandSource,
    effect: SlashCommandEffect,
    input: ArgumentsNeed,
    accepts: &[AcceptedSubmission],
    subcommands: &[Subcommand],
) -> Meta {
    let accepts: Vec<&str> = accepts.iter().map(|via| via.as_str()).collect();
    let mut descriptor = serde_json::json!({
        "source": source,
        "effect": effect,
        "input": input.as_str(),
        "accepts": accepts,
    });
    if !subcommands.is_empty() {
        descriptor["subcommands"] = subcommands
            .iter()
            .map(|sub| serde_json::json!({ "name": sub.name, "description": sub.description }))
            .collect();
    }
    Meta::from_iter([(
        crate::acp::shared::QUERYMT_META_NAMESPACE.to_owned(),
        descriptor,
    )])
}

/// Convert the registry into an ACP `AvailableCommandsUpdate`. Names are
/// advertised without the leading `/`, as ACP specifies; clients add it when
/// they show or send the command.
pub fn registry_to_acp_update(registry: &SlashCommandRegistry) -> AvailableCommandsUpdate {
    let commands: Vec<AvailableCommand> = registry
        .all()
        .map(|cmd| {
            let mut acp_cmd = AvailableCommand::new(cmd.name.clone(), &cmd.description);

            let input = cmd.input_need();
            if input != ArgumentsNeed::None {
                acp_cmd = acp_cmd.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(cmd.argument_hint.clone().unwrap_or_default()),
                ));
            }

            acp_cmd.meta(descriptor_meta(
                cmd.source.wire(),
                SlashCommandEffect::Prompt,
                input,
                &[AcceptedSubmission::Prompt, AcceptedSubmission::Queue],
                &[],
            ))
        })
        .collect();

    AvailableCommandsUpdate::new(commands)
}

/// Append the built-ins `host` can run to `update`.
pub fn append_builtins(
    update: &mut AvailableCommandsUpdate,
    host: crate::slash_commands::builtin::BuiltinHost,
) {
    use crate::slash_commands::builtin::Builtin;
    update.available_commands.extend(
        Builtin::ALL
            .into_iter()
            .filter(|builtin| builtin.available(host))
            .map(Builtin::entry),
    );
}

/// The complete catalog a local session advertises: its user commands
/// (never a reserved built-in name) followed by the built-ins `host` can run.
pub fn catalog_update(
    registry: &SlashCommandRegistry,
    host: crate::slash_commands::builtin::BuiltinHost,
) -> AvailableCommandsUpdate {
    let mut update = registry_to_acp_update(registry);
    update.available_commands.retain(|cmd| {
        !crate::slash_commands::builtin::Builtin::is_reserved(cmd.name.trim_start_matches('/'))
    });
    append_builtins(&mut update, host);
    update
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
            arguments: None,
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
        assert_eq!(cmd.name, "review");
        assert_eq!(cmd.description, "Review changes");
        assert!(cmd.input.is_some());
    }

    fn descriptor_input(cmd: &AvailableCommand) -> serde_json::Value {
        serde_json::to_value(cmd).unwrap()["_meta"]["querymt"]["input"].clone()
    }

    #[test]
    fn arguments_key_sets_the_input_need_in_the_catalog() {
        let mut command = make_command("explain", "Explain", Some("[topic]"));
        command.arguments = Some(ArgumentsNeed::Optional);
        let mut registry = SlashCommandRegistry::new();
        registry.register(command);

        let update = registry_to_acp_update(&registry);
        let cmd = &update.available_commands[0];
        assert_eq!(descriptor_input(cmd), "optional");
        let Some(AvailableCommandInput::Unstructured(input)) = &cmd.input else {
            panic!("an ACP input with the hint is advertised: {cmd:?}");
        };
        assert_eq!(input.hint, "[topic]");
    }

    #[test]
    fn arguments_none_advertises_no_input() {
        // The template contains `$ARGUMENTS`, which alone would make it required.
        let mut command = make_command("explain", "Explain", Some("[topic]"));
        command.arguments = Some(ArgumentsNeed::None);
        let mut registry = SlashCommandRegistry::new();
        registry.register(command);

        let update = registry_to_acp_update(&registry);
        let cmd = &update.available_commands[0];
        assert_eq!(descriptor_input(cmd), "none");
        assert!(cmd.input.is_none());
    }

    #[test]
    fn input_need_defaults_to_none_without_a_placeholder_in_the_catalog() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("help", "Show help", None));
        let update = registry_to_acp_update(&registry);
        assert_eq!(descriptor_input(&update.available_commands[0]), "none");
    }

    #[test]
    fn test_command_without_hint() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("help", "Show help", None));

        let update = registry_to_acp_update(&registry);
        assert_eq!(update.available_commands.len(), 1);
        assert_eq!(update.available_commands[0].name, "help");
        // No argument hint and no $ARGUMENTS in template
        assert!(update.available_commands[0].input.is_none());
    }

    #[test]
    fn command_without_subcommands_omits_the_field() {
        use crate::slash_commands::builtin::Builtin;
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("review", "Review changes", Some("[scope]")));
        registry.register(make_command("help", "Show help", None));
        let update = registry_to_acp_update(&registry);
        let mut commands = update.available_commands;
        commands.push(Builtin::Compact.entry());
        assert_eq!(commands.len(), 3);
        for cmd in &commands {
            let value = serde_json::to_value(cmd).unwrap();
            let querymt = value["_meta"]["querymt"].as_object().expect("descriptor");
            assert!(
                !querymt.contains_key("subcommands"),
                "{} must omit subcommands: {querymt:?}",
                cmd.name
            );
        }
    }

    #[test]
    fn descriptor_writes_subcommands_when_given() {
        let meta = descriptor_meta(
            querymt_agent_core::events::SlashCommandSource::Builtin,
            SlashCommandEffect::Action,
            ArgumentsNeed::Required,
            &[AcceptedSubmission::Prompt],
            &[Subcommand {
                name: "list",
                description: "List things",
            }],
        );
        let value = serde_json::to_value(meta).unwrap();
        assert_eq!(
            value["querymt"]["subcommands"],
            serde_json::json!([{ "name": "list", "description": "List things" }])
        );
    }

    #[test]
    fn test_build_notification() {
        let mut registry = SlashCommandRegistry::new();
        registry.register(make_command("test", "Test command", None));

        let notif = build_commands_notification("session-1", &registry);
        assert_eq!(notif.session_id.to_string(), "session-1");
    }
}
