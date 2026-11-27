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

### Requirement: Command replies are their own entries
An `agent_message_chunk` that carries `_meta.querymt.slashCommand` SHALL become a command reply entry holding the record, the text and the error code from `_meta.querymt.slashCommandError` when present. A command reply SHALL NOT be merged into an assistant message, and an assistant chunk SHALL NOT be merged into a command reply. Chunks that carry the same message id SHALL form one command reply.

#### Scenario: Command reply is not an assistant message
- **WHEN** an `agent_message_chunk` with text `[]` carries `slashCommand` `worktree` with `arguments` `list`
- **THEN** the view holds a command reply entry with that record and text, and no assistant message for it

#### Scenario: Command reply does not join the preceding assistant message
- **WHEN** an assistant chunk without a message id is followed by an `agent_message_chunk` without a message id that carries a `slashCommand`
- **THEN** the view holds one assistant message and one command reply, in that order

#### Scenario: Command reply exposes its error code
- **WHEN** an `agent_message_chunk` carries a `slashCommand` and `slashCommandError` `owner_active`
- **THEN** the command reply exposes error code `owner_active`

#### Scenario: Agent message without a record stays an assistant message
- **WHEN** an `agent_message_chunk` carries `_meta.querymt.worktreeCommand` and no `slashCommand`
- **THEN** the view holds an assistant message for it

### Requirement: A record replaces the optimistic prompt it answers
When an entry whose record names a client prompt id is added, the pending prompt with that id SHALL be removed and the entry SHALL stand for it. The prompt's completion SHALL still end its submission: when no run started for it, the run state SHALL return to idle.

#### Scenario: Command reply replaces the pending prompt
- **WHEN** an optimistic prompt `c1` is followed by an `agent_message_chunk` whose `slashCommand.clientPromptId` is `c1`
- **THEN** the view holds the command reply and no pending prompt

#### Scenario: Manual compaction replaces the pending prompt
- **WHEN** an optimistic prompt `c1` is followed by a `compaction_update` whose `slashCommand.clientPromptId` is `c1`
- **THEN** the view holds the compaction and no pending prompt

#### Scenario: Run state returns to idle after a command answered without a run
- **WHEN** the agent's state updates were seen for an earlier run, then an optimistic prompt `c1` is followed by a command reply for `c1` and a prompt completion for `c1` that names no message
- **THEN** the run state is idle and the view holds no pending prompt

### Requirement: Command replies outside a run form their own turn
A command reply that carries no run id while no run is running SHALL start a new turn after the last turn. A command reply that arrives while a run is running SHALL join that run's turn, like other content without a run id.

#### Scenario: Command reply outside a run starts a turn
- **WHEN** a view with two finished turns receives a command reply with no run id
- **THEN** the view holds a third turn whose only entry is the command reply

#### Scenario: Command reply during a run joins the running turn
- **WHEN** the session reports run `r1` running and a command reply with no run id arrives
- **THEN** the command reply is an entry of `r1`'s turn
