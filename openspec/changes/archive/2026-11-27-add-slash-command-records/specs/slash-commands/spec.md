# Spec Delta

## ADDED Requirements

### Requirement: Command descriptor in the catalog
Every entry of an `available_commands_update` SHALL carry `_meta.querymt` with four fields:
- `source`: `builtin`, `global`, `project` or `configured`;
- `effect`: `prompt` when the command becomes a model turn, `action` when the agent runs it itself;
- `input`: `none`, `optional` or `required`;
- `accepts`: the submissions the command runs from, a list drawn from `prompt` and `queue`.

#### Scenario: User-defined command descriptor
- **WHEN** a session's project has `.qmt/commands/explain.md` whose template contains `$ARGUMENTS` and whose frontmatter has no `arguments` key
- **THEN** the catalog entry `explain` carries `source` `project`, `effect` `prompt`, `input` `required` and `accepts` `["prompt", "queue"]`

#### Scenario: Compact descriptor
- **WHEN** a session's catalog is advertised
- **THEN** the entry `compact` carries `source` `builtin`, `effect` `action`, `input` `optional` and `accepts` `["prompt", "queue"]`

#### Scenario: Worktree descriptor
- **WHEN** the catalog of a local session on an agent with the workspace service is advertised
- **THEN** the entry `worktree` carries `source` `builtin`, `effect` `action`, `input` `required` and `accepts` `["prompt"]`

#### Scenario: Overriding command reports its own source
- **WHEN** `explain.md` exists in both `~/.qmt/commands` and the project's `.qmt/commands`
- **THEN** the catalog has one `explain` entry and its `source` is `project`

### Requirement: Declared input need of a user-defined command
A command file MAY declare `arguments: none | optional | required` in its frontmatter. Without the key, the input need SHALL be `required` when the file has an `argument-hint` or its template contains `$ARGUMENTS`, and `none` otherwise. The catalog entry SHALL carry an ACP `input` exactly when the input need is not `none`.

#### Scenario: Arguments key sets the input need
- **WHEN** a command file whose template contains `$ARGUMENTS` declares `arguments: optional`
- **THEN** its catalog entry carries `input` `optional` and an ACP `input` with the file's hint

#### Scenario: Arguments none advertises no input
- **WHEN** a command file whose template contains `$ARGUMENTS` declares `arguments: none`
- **THEN** its catalog entry carries `input` `none` and no ACP `input`

#### Scenario: Input need defaults to none without a placeholder
- **WHEN** a command file has no `arguments` key, no `argument-hint` and no `$ARGUMENTS` in its template
- **THEN** its catalog entry carries `input` `none`

### Requirement: Invalid input need declaration
A command file whose `arguments` value is not `none`, `optional` or `required` SHALL NOT be loaded and SHALL be reported as a diagnostic that names the file and the accepted values.

#### Scenario: Invalid arguments value is a diagnostic
- **WHEN** a command file declares `arguments: sometimes`
- **THEN** the command is not in the catalog
- **AND** discovery reports a diagnostic that names the file and the three accepted values

### Requirement: The input need is advice for clients
The agent SHALL NOT refuse a user-defined command because its arguments are missing, whatever its input need.

#### Scenario: Required input is not enforced
- **WHEN** a command whose input need is `required` is submitted with no arguments
- **THEN** the agent expands it with empty arguments and starts the turn

### Requirement: Invocation record
An invocation record SHALL be `_meta.querymt.slashCommand` with:
- `name` without the slash;
- `arguments`: the trimmed text after the name, an empty string when there is none;
- `source` and `effect` as advertised;
- `clientPromptId` when the prompt carried one.

A record SHALL appear only on the updates of the user message or compaction that delivers the command's outcome.

#### Scenario: Record carries the client prompt id
- **WHEN** a client submits `/explain closures` with `_meta.querymt.client_prompt_id` `c1` and the project defines `explain`
- **THEN** the user message that carries the record has `slashCommand.clientPromptId` `c1`

#### Scenario: Queued command record has no client prompt id
- **WHEN** a client queues `/explain closures` as input and its turn later starts
- **THEN** the turn's user message carries a `slashCommand` without `clientPromptId`

### Requirement: Only catalog commands are recorded
Command names SHALL match a catalog entry exactly, including case. A prompt that names no catalog command SHALL be an ordinary prompt and SHALL produce no record.

#### Scenario: Unknown slash text carries no record
- **WHEN** a client submits the prompt `/tmp/build.log is empty` to a session with no command named `tmp`
- **THEN** the prompt is stored and echoed as an ordinary user message with no `slashCommand`

#### Scenario: Command name case must match
- **WHEN** a client submits the prompt `/COMPACT`
- **THEN** no compaction starts and the prompt is an ordinary user message with no `slashCommand`

### Requirement: Record on a prompt command's user message
For a command whose effect is `prompt`, every `user_message_chunk` of the message the command produced SHALL carry the record. The message content SHALL remain the expanded prompt that the model receives.

#### Scenario: Prompt command user message carries the record
- **WHEN** a client submits `/explain closures` and the project defines `explain`
- **THEN** the `user_message_chunk` for that prompt has the expansion as its content
- **AND** its `slashCommand` is `name` `explain`, `arguments` `closures`, `source` `project`, `effect` `prompt`

#### Scenario: Attachment chunks of a prompt command carry the record
- **WHEN** a client submits `/explain this diagram` with an image block
- **THEN** both the text chunk and the image chunk of the user message carry the same `slashCommand`

### Requirement: Record on a manual compaction
A compaction started by `/compact` SHALL carry the record on its first `compaction_update`, with the command's instructions as `arguments`. A compaction the agent starts on its own SHALL carry no record.

#### Scenario: Manual compaction carries its instructions
- **WHEN** a client submits `/compact keep the API decisions` and the compaction starts
- **THEN** the `in_progress` `compaction_update` carries `slashCommand` with `name` `compact`, `arguments` `keep the API decisions`, `source` `builtin` and `effect` `action`

#### Scenario: Automatic compaction carries no record
- **WHEN** the agent compacts because the context threshold was reached
- **THEN** none of that compaction's updates carries a `slashCommand`

### Requirement: Records in replayed and loaded history
An update that is replayed or served in a load snapshot SHALL carry the same record it carried live. A terminal `compaction_update` that stands for a whole compaction on replay SHALL carry the compaction's record. History written before records existed SHALL replay without records.

#### Scenario: Replayed prompt command keeps its record
- **WHEN** a client that receives history as session updates loads a session in which `/explain closures` was run
- **THEN** the replayed `user_message_chunk` for that prompt carries the same `slashCommand` as it did live

#### Scenario: Load snapshot keeps the record of a prompt command
- **WHEN** a client loads the same session with a snapshot
- **THEN** the snapshot's `prompt_received` event for that prompt carries `slash_command` with `name` `explain` and `arguments` `closures`

#### Scenario: Replayed manual compaction keeps its record
- **WHEN** a session in which `/compact keep the API decisions` completed is replayed
- **THEN** the one terminal `compaction_update` for it carries `slashCommand` with `arguments` `keep the API decisions`

#### Scenario: History without records replays unchanged
- **WHEN** a session whose journal was written before this change is replayed
- **THEN** its updates are the ones replayed before this change and none carries a `slashCommand`

### Requirement: Recognised commands are not steered
A steering submission whose first text block names any command of the session's catalog SHALL be rejected with an invalid-prompt error that names the command and says to queue it. Nothing of that submission SHALL reach the running turn. Text that names no catalog command SHALL be steered as before.

#### Scenario: Steered user-defined command is rejected
- **WHEN** a run is active and a client steers `/explain closures` into it, and the session's catalog has `explain`
- **THEN** the request fails with an invalid-prompt error naming `/explain`
- **AND** the running turn receives no steering input

#### Scenario: Steered unknown slash text is accepted
- **WHEN** a run is active and a client steers `/tmp/build.log is empty` into it
- **THEN** the input is accepted as steering with its text unchanged
