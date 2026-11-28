# Spec Delta

## ADDED Requirements

### Requirement: Command descriptors in the view
Each command of the session's command list SHALL expose the descriptor the agent advertised for it: source, effect, input need and accepted submissions. A field the agent did not send, or sent with a value the view does not know, SHALL be exposed as absent; the command SHALL still be listed.

#### Scenario: Descriptor is read from the catalog
- **WHEN** an `available_commands_update` entry `compact` carries `_meta.querymt` with `source` `builtin`, `effect` `action`, `input` `optional` and `accepts` `["prompt", "queue"]`
- **THEN** the view's `compact` command exposes those four values

#### Scenario: Catalog without descriptors
- **WHEN** an `available_commands_update` from an agent that sends no `_meta.querymt` is applied
- **THEN** every command is listed with its name, description and input hint, and its four descriptor fields are absent

#### Scenario: Unknown descriptor value is absent
- **WHEN** a catalog entry carries `effect` `workflow` and `source` `project`
- **THEN** the command is listed with source `project` and no effect

### Requirement: Invocation records on entries
A user message or a compaction whose update carried `_meta.querymt.slashCommand` SHALL expose that record: name, arguments, source, effect and client prompt id. Once set, a record SHALL stay on its entry when later updates for the same entry do not repeat it. An entry built from a load snapshot SHALL expose the same record as the one built from live updates.

#### Scenario: User message exposes its record
- **WHEN** a `user_message_chunk` carries `slashCommand` with `name` `explain` and `arguments` `closures`
- **THEN** the view's user message exposes that record and keeps the chunk's content as its blocks

#### Scenario: Compaction keeps its record through later updates
- **WHEN** an `in_progress` `compaction_update` carries a `slashCommand` and the `completed` update for the same id carries none
- **THEN** the completed compaction still exposes the record

#### Scenario: Loaded prompt command exposes its record
- **WHEN** a load snapshot holds a `prompt_received` event with `slash_command` `explain` and the prompt record of that message
- **THEN** the view's user message has the prompt record's blocks and exposes the record
