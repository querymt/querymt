# slash-commands Specification

## Purpose

Lets the user discover and run the slash commands an agent advertises for the open session (ACP `available_commands_update`), from the command palette and from the session composer.

## Requirements

### Requirement: Advertised commands follow the agent
The app SHALL hold, for the open session, the slash commands of the agent's latest `available_commands_update` for that session. Each update SHALL replace the whole list, and an empty update SHALL clear it. Malformed entries SHALL be skipped without discarding the rest. Commands of one session SHALL NOT appear in another session.

#### Scenario: Agent replaces its commands
- **GIVEN** the open session advertises `/compact` and `/worktree`
- **WHEN** the agent sends an update listing only `/compact`
- **THEN** only `/compact` is offered

#### Scenario: Agent clears its commands
- **WHEN** the agent sends an update with an empty list
- **THEN** no slash commands are offered for the session

#### Scenario: Commands of another session
- **WHEN** an update arrives for a session other than the open one
- **THEN** the commands offered for the open session do not change

### Requirement: Command names with or without a slash
The app SHALL accept advertised names with or without a leading `/`. It SHALL send every command as `/` followed by its name. The session composer's list SHALL show it as `/name`; the command palette, which lists commands rather than completing text, SHALL show the bare name.

#### Scenario: Spec-style name
- **WHEN** the agent advertises a command named `web`
- **THEN** it is shown as `/web` in the composer list, as `web` in the command palette, and sent as `/web`

#### Scenario: Name that already has a slash
- **WHEN** the agent advertises a command named `/compact`
- **THEN** it is shown and sent as `/compact` (shown as `compact` in the command palette), not `//compact`

### Requirement: Commands advertised before the session is shown
Commands the agent advertises for a session between creating it (new session, remote creation, fork) and the app showing it SHALL be offered once the session is shown.

#### Scenario: Commands sent right after creation
- **GIVEN** the user starts a new session
- **WHEN** the agent advertises `/compact` right after it answers the creation request, before the session view opens
- **THEN** `/compact` is offered once the session is open

### Requirement: Loading a session keeps its commands
The `session/load` response, with or without a snapshot, SHALL NOT clear or change the session's commands. Only an `available_commands_update` SHALL change them, whether it arrives before or after the load response.

#### Scenario: Advertisement before the load response
- **GIVEN** the user opens a session
- **WHEN** the agent advertises `/compact` and then answers the load request with a snapshot
- **THEN** `/compact` is offered once the session has loaded

#### Scenario: Large session loaded in the background
- **GIVEN** the user opens a session whose load response is large enough to be decoded off the UI thread
- **WHEN** the agent advertises `/compact` before the response, or while it is being decoded
- **THEN** `/compact` is offered once the session has loaded, exactly as for a small session

#### Scenario: Commands known before the session is opened
- **GIVEN** the agent advertised `/compact` for a session before the user opened it
- **WHEN** the user opens that session and its load is decoded in the background
- **THEN** `/compact` is offered once the session has loaded

#### Scenario: Advertisement after the load response
- **WHEN** the agent answers the load request and then advertises `/compact`
- **THEN** `/compact` is offered

### Requirement: Slash commands in the command palette
While a session is open and its agent advertises commands, the command palette SHALL list them in their own group. Each entry SHALL be a two-line row: the command's name without a `/` on the first line and the agent's description of the command, dimmed, on the second. The search SHALL match the name, the description and the input hint.

#### Scenario: Palette lists the session's commands
- **GIVEN** the open session advertises `/compact` with the description "Summarize the conversation now to free up context"
- **WHEN** the user opens the command palette
- **THEN** the entry shows `compact` with "Summarize the conversation now to free up context" under it

#### Scenario: Palette searches the description
- **GIVEN** the same session
- **WHEN** the user types `free context`
- **THEN** `compact` is listed

#### Scenario: Command without a description
- **WHEN** the agent advertises a command with an empty description
- **THEN** its entry shows the name only

#### Scenario: The best match is the first row
- **GIVEN** the open session advertises `compact`, and the palette also offers the action "Schedule a prompt…"
- **WHEN** the user types `comp`
- **THEN** `compact` is the first row and the one Enter runs, and "Schedule a prompt…" is still listed below it

### Requirement: Palette groups follow the best match
While the command palette has a search, its groups SHALL be ordered by the best match each contains, best first, so the first row is the best match of the whole palette. Groups whose best matches are equal SHALL keep their usual order (Actions, Slash commands, Go to, Sessions). With no search, the usual order applies.

#### Scenario: A command outranks an action
- **GIVEN** a session that advertises `compact`
- **WHEN** the user types `comp`
- **THEN** the Slash commands group is listed before the Actions group

#### Scenario: An action outranks the commands
- **GIVEN** a session that advertises `compact` and `worktree`
- **WHEN** the user types `sidebar`
- **THEN** the Actions group is listed first, and no slash command is listed

#### Scenario: No search
- **WHEN** the command palette opens
- **THEN** the groups are in their usual order

#### Scenario: No session open
- **WHEN** the command palette is opened on the Start page
- **THEN** no slash command group is shown

### Requirement: Running a command from the palette
Picking a command without an input specification SHALL send `/name` to the open session as a prompt, following the same send rules as typed prompts. Picking a command with an input specification SHALL put `/name ` in the session composer, focus the composer, and show the command's hint until arguments are typed.

#### Scenario: Command without input
- **WHEN** the user picks a command that takes no input from the palette
- **THEN** `/name` is sent to the session and the palette closes

#### Scenario: Command with input
- **GIVEN** `/compact` advertises the input hint "[optional instructions…]"
- **WHEN** the user picks `/compact` from the palette
- **THEN** the composer contains `/compact `, has focus with the cursor at the end, and shows the hint

#### Scenario: Palette command during a running turn
- **GIVEN** a turn is running on an agent that can queue input
- **WHEN** the user picks a command without input from the palette
- **THEN** the command is queued, as a typed slash command would be

### Requirement: Command list in the session composer
When the session composer's text starts with `/` and the cursor is still in the first word, the composer SHALL show the session's commands whose names match the text typed after `/`, best match first. Descriptions SHALL NOT be matched here. The list SHALL NOT be shown when the session advertises no commands or nothing matches.

#### Scenario: Typing a slash
- **GIVEN** the open session advertises `/compact` and `/worktree`
- **WHEN** the user types `/` in the empty composer
- **THEN** both commands are listed with their descriptions

#### Scenario: Narrowing the list
- **WHEN** the user types `/wt`
- **THEN** `/worktree` is listed and `/compact` is not

#### Scenario: Slash inside a message
- **WHEN** the composer text is `see a/b`
- **THEN** no command list is shown

### Requirement: Keyboard in the composer command list
While the command list is shown, Up and Down SHALL move the highlighted command. Tab and Enter SHALL replace the first word with `/name ` and keep any arguments already typed, without sending. Esc SHALL close the list without changing the text and without counting toward the double Esc that stops a running turn. Once the list is closed, these keys SHALL behave as they do without it.

#### Scenario: Complete with Tab
- **GIVEN** the list is shown for `/com` with `/compact` highlighted
- **WHEN** the user presses Tab
- **THEN** the composer text is `/compact `, nothing is sent, and the session mode does not change

#### Scenario: Enter completes instead of sending
- **GIVEN** the list is shown for `/wor`
- **WHEN** the user presses Enter
- **THEN** the composer text is `/worktree ` and no prompt is sent

#### Scenario: Esc closes the list only
- **GIVEN** a turn is running and the list is shown
- **WHEN** the user presses Esc once, then Esc again within 700 ms
- **THEN** the first Esc closes the list and the second does not stop the turn

#### Scenario: Sending after completion
- **GIVEN** the composer text is `/compact focus on the API` and the list is closed
- **WHEN** the user presses the send shortcut
- **THEN** `/compact focus on the API` is sent as the prompt

### Requirement: Mouse in the composer command list
Clicking a command in the composer list SHALL complete it as Tab does.

#### Scenario: Click a command
- **WHEN** the user clicks `/worktree` in the list
- **THEN** the composer text is `/worktree ` and the composer keeps focus

### Requirement: Prompt commands in the conversation
A user message that carries a command record SHALL be drawn as a prompt bubble showing what the user typed: `/name` and the arguments, with any attachments. The expanded prompt the model received SHALL be hidden by default and shown by a "Show prompt" toggle on the message. Copying the message SHALL copy what the user typed. It SHALL look the same after the session is reloaded.

#### Scenario: User-defined command shows what was typed
- **GIVEN** the project defines the command `explain`
- **WHEN** the user sends `/explain closures`
- **THEN** the conversation shows a prompt bubble reading `/explain closures`, not the expanded template

#### Scenario: Expanded prompt on demand
- **GIVEN** the same message
- **WHEN** the user picks "Show prompt" on it
- **THEN** the expanded prompt is shown under the bubble and the toggle reads "Hide prompt"

#### Scenario: Reloaded prompt command
- **GIVEN** a session in which `/explain closures` was sent
- **WHEN** the user reopens the session
- **THEN** the message reads `/explain closures` and the toggle is available, as it was live

### Requirement: Manual compactions in the conversation
A compaction that carries a command record SHALL say on its row that it was started by `/compact` and SHALL show the command's instructions when it has any. A compaction without a record SHALL be drawn as before.

#### Scenario: Compact with instructions
- **WHEN** the user runs `/compact keep the API decisions`
- **THEN** the conversation shows one compaction row that names `/compact` and shows "keep the API decisions"

#### Scenario: Compact without instructions
- **WHEN** the user runs `/compact`
- **THEN** the compaction row names `/compact` and shows no instructions

#### Scenario: Automatic compaction
- **WHEN** the agent compacts on its own
- **THEN** the compaction row does not name `/compact`

### Requirement: Conversations without command records
A message that carries no command record SHALL be drawn as before: a user message as a prompt bubble with its content, an agent message as a reply. The app SHALL NOT decide from a message's text that it was a command.

#### Scenario: Agent without records
- **GIVEN** an agent that sends no command records
- **WHEN** the user sends `/worktree list` and the agent answers
- **THEN** the conversation shows the prompt and the answer as a prompt bubble and an assistant reply, as before this change

#### Scenario: Prompt that only starts with a slash
- **WHEN** the user sends `/tmp is full, why?` and the session advertises no `tmp` command
- **THEN** it is shown as an ordinary prompt bubble, while it is being sent and after

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
