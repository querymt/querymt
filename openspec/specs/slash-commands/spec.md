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
The app SHALL accept advertised names with or without a leading `/`. It SHALL send every command as `/` followed by its name. The session composer's list SHALL show it as `/name`; the command palette, which lists commands rather than completing text, SHALL show the bare name, followed by `…` when the command's input is required.

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

### Requirement: Project commands follow the session's folder
For a local session, the agent SHALL read project commands (`<root>/.qmt/commands`) from the session's project root, and SHALL advertise and expand a session's commands from that one registry. A session that runs in a managed worktree SHALL keep using its checkout. Global commands, configured paths, the scripts policy and the discovery switches SHALL follow the agent's configuration.

#### Scenario: Session in a plain folder sees its project commands
- **GIVEN** an agent started without a working directory and a folder `~/proj` that holds `.qmt/commands/explain.md`
- **WHEN** a session opens in `~/proj`
- **THEN** `explain` is in that session's advertised commands, with source `project`
- **AND** a prompt `/explain closures` in that session expands the folder's `explain.md`

#### Scenario: Sessions in different folders differ
- **GIVEN** two folders that define `deploy` with different templates
- **WHEN** a session is opened in each and `/deploy` is sent
- **THEN** each session expands its own folder's template

#### Scenario: Idle session reports the same commands as a live one
- **GIVEN** a session whose actor is not running
- **WHEN** its command catalog is requested
- **THEN** it lists the same user commands the session lists once its actor runs

#### Scenario: Agent-level commands do not leak into other folders
- **GIVEN** an agent whose startup working directory holds `.qmt/commands/agent-only.md`
- **WHEN** a session opens in a different folder that has no such file
- **THEN** `agent-only` is not advertised for that session and `/agent-only` is not expanded

#### Scenario: Managed worktree session unchanged
- **GIVEN** a session bound to an isolated checkout that holds `.qmt/commands/deploy.md` different from its source checkout's
- **WHEN** `/deploy` is sent
- **THEN** the isolated checkout's template is expanded

### Requirement: The session's project root
The project root of a session SHALL be the root of the Git checkout that contains the session's working directory, or the working directory itself when it is not inside a Git repository. The agent SHALL NOT search folders above that root.

#### Scenario: Session in a subfolder of a repository
- **GIVEN** a Git repository whose root holds `.qmt/commands/deploy.md`
- **WHEN** a session opens in `<root>/crates/api`
- **THEN** `deploy` is advertised and expands from the repository root's file

#### Scenario: Folder outside Git does not search parents
- **GIVEN** a folder `~/work` that holds `.qmt/commands/lint.md` and is not a Git repository
- **WHEN** a session opens in `~/work/scratch`
- **THEN** `lint` is not advertised and `/lint` is not expanded

### Requirement: Sessions that keep the agent-level commands
A session without a working directory, an agent with command discovery off or unconfigured, and a session hosted on another mesh node SHALL keep the agent-level commands, or none.

#### Scenario: Session without a working directory keeps the agent-level commands
- **GIVEN** an agent with startup commands
- **WHEN** a session has no working directory
- **THEN** its catalog and expansion use the agent-level commands

#### Scenario: Discovery switched off
- **GIVEN** an agent with command discovery disabled
- **WHEN** a session opens in a folder that holds `.qmt/commands/explain.md`
- **THEN** `explain` is not advertised and `/explain` is not expanded

### Requirement: Command files are read without a restart
When a command file in the user's global folder (`~/.qmt/commands`) or in a live session's `<root>/.qmt/commands` is added, edited or removed, the agent SHALL use the new set for that session's next prompt. It SHALL also send the session's connected clients an `available_commands_update` with the full catalog, built-ins included, without a restart or reopen. A running turn SHALL keep the commands it started with.

#### Scenario: New global command appears in an open session
- **GIVEN** an open session and an existing `~/.qmt/commands` folder without `standup.md`
- **WHEN** `~/.qmt/commands/standup.md` is created
- **THEN** the session's clients receive an `available_commands_update` that lists `standup` with source `global`, together with the built-ins
- **AND** `/standup` in that session expands the new file

#### Scenario: New project command appears in an open session
- **GIVEN** an open session in `~/proj` whose `.qmt/commands` is not ignored by Git
- **WHEN** `~/proj/.qmt/commands/explain.md` is created
- **THEN** the session's clients receive an `available_commands_update` that lists `explain` with source `project`
- **AND** `/explain closures` in that session expands the new file

#### Scenario: Edited template is used by the next prompt
- **GIVEN** an open session that has run `/explain` from `.qmt/commands/explain.md`
- **WHEN** the file's template is changed and `/explain` is sent again
- **THEN** the new template is expanded

#### Scenario: Deleted command disappears
- **GIVEN** an open session that advertises the project command `explain`
- **WHEN** `.qmt/commands/explain.md` is deleted
- **THEN** the session's clients receive an `available_commands_update` without `explain`
- **AND** `/explain` is sent to the model as ordinary text

#### Scenario: Session without a working directory sees global edits
- **GIVEN** an open session without a working directory
- **WHEN** a file in `~/.qmt/commands` is created
- **THEN** that session's catalog and expansion include it

#### Scenario: Unrelated file changes send no update
- **GIVEN** an open session in `~/proj`
- **WHEN** a file outside `.qmt/commands`, or a non-`.md` file inside it, changes
- **THEN** no `available_commands_update` is sent

#### Scenario: Configured paths are read at startup only
- **GIVEN** an agent with a configured `slash_commands.paths` entry
- **WHEN** a file in that path is created while a session is open
- **THEN** that change alone sends no update, and the session lists the file once it next opens or its commands are next reloaded

#### Scenario: Ignored commands folder is read on open
- **GIVEN** a repository whose `.gitignore` ignores `.qmt/`, and an open session in it
- **WHEN** `.qmt/commands/explain.md` is created
- **THEN** no update is sent, and `explain` is listed when the session next opens, loads or resumes

### Requirement: Slash commands in the command palette
While a session is open and its agent advertises commands, the command palette SHALL list them in their own group. Each entry SHALL be a two-line row: the command's name without a `/` on the first line and, dimmed on the second, the agent's description of the command or the reason the session cannot take it now. The search SHALL match the name, the description, the input hint, the source and, for a prompt command, the word `prompt`.

#### Scenario: Palette lists the session's commands
- **GIVEN** the open session advertises `/compact` with the description "Summarize the conversation now to free up context"
- **WHEN** the user opens the command palette
- **THEN** the entry shows `compact` with "Summarize the conversation now to free up context" under it

#### Scenario: Palette searches the description
- **GIVEN** the same session
- **WHEN** the user types `free context`
- **THEN** `compact` is listed

#### Scenario: Palette searches the source
- **GIVEN** the session advertises the built-in `compact` and the project command `explain`
- **WHEN** the user types `project`
- **THEN** `explain…` is listed and `compact` is not

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
Picking an available command whose input is `none` or `optional` SHALL submit `/name` to the open session and close the palette. Picking one whose input is `required` SHALL put it in the session composer, focus the composer with the cursor at the end, and show the command's hint until arguments are typed. A draft already in the composer SHALL be kept as the command's arguments.

#### Scenario: Command without input
- **WHEN** the user picks a command that takes no input from the palette
- **THEN** `/name` is sent to the session and the palette closes

#### Scenario: Command with optional input
- **GIVEN** `compact` advertises the input hint "[optional instructions…]" and the input need `optional`
- **WHEN** the user picks `compact` from the palette with Enter or a click
- **THEN** `/compact` is sent at once and the palette closes

#### Scenario: Command with input
- **GIVEN** `explain` requires input and advertises the hint "[topic]"
- **WHEN** the user picks `explain…` from the palette
- **THEN** the composer contains `/explain `, has focus with the cursor at the end, and shows the hint

#### Scenario: Composer draft becomes the arguments
- **GIVEN** the composer holds the draft `closures in Rust`
- **WHEN** the user picks `explain…` from the palette
- **THEN** the composer contains `/explain closures in Rust` with the cursor at the end

#### Scenario: Draft that starts with a path
- **GIVEN** the composer holds the draft `/tmp/build.log is empty`
- **WHEN** the user picks `explain…` from the palette
- **THEN** the composer contains `/explain /tmp/build.log is empty` with the cursor at the end

#### Scenario: Palette command during a running turn
- **GIVEN** a turn is running on an agent that can queue input
- **WHEN** the user picks a command without input that accepts queueing
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

### Requirement: Command descriptor from the agent
For each advertised command the app SHALL read the agent's descriptor: its source, its effect (a model prompt or an action the agent runs), its input need (`none`, `optional`, `required`) and the submissions it accepts (prompt, queue). A list of accepted submissions that is present but empty SHALL mean the command accepts no submission.

#### Scenario: Descriptor states an optional input
- **GIVEN** the agent advertises `compact` with an input hint and the input need `optional`
- **THEN** the app treats `/compact` as runnable without arguments

#### Scenario: Empty list of accepted submissions
- **GIVEN** the agent advertises `freeze` with the accepted submissions `[]`
- **THEN** the app treats `/freeze` as accepting neither prompt nor queue, not as a command without a descriptor

### Requirement: Fields missing from the descriptor
Each descriptor field the agent did not send SHALL fall back on its own. Without an input need, an input hint SHALL mean required input and no hint no input. Without accepted submissions, the command SHALL accept prompt and queue. Without a source or effect, none SHALL be shown, and the command SHALL be treated as neither a model prompt nor an action.

#### Scenario: Command without a descriptor
- **GIVEN** an agent advertises `review` with the input hint "[scope]" and no descriptor
- **THEN** the app treats `/review` as requiring input and as accepting prompt and queue, as before this change
- **AND** the app treats it as neither a model prompt nor an action

### Requirement: Recognising a command in typed text
Text SHALL be treated as a slash command only when its first word is `/` followed by the exact name of a command the session advertises, including case, and the name ends the text or is followed by whitespace. Any other text SHALL be an ordinary message, whether or not it starts with `/`.

#### Scenario: Path is not a command
- **GIVEN** a turn is running, the composer's delivery choice is steer, and the session advertises no `tmp` command
- **WHEN** the user sends `/tmp/build.log is empty`
- **THEN** the message is steered into the running turn

#### Scenario: Name case must match
- **GIVEN** the session advertises `compact`
- **WHEN** the user sends `/COMPACT`
- **THEN** it is sent as an ordinary message

### Requirement: Submitting a command
A recognised command SHALL be sent as a prompt while no turn runs. While a turn runs it SHALL be queued, never steered, when it accepts queueing. A command that does not accept queueing SHALL NOT be sent while a turn runs: the composer SHALL keep its text and SHALL show why under it. The same rule SHALL apply to a command picked in the palette and to one typed in the composer.

#### Scenario: Typed command is queued, not steered
- **GIVEN** a turn is running and the composer's delivery choice is steer
- **WHEN** the user sends `/compact keep the plan`
- **THEN** the command is queued

#### Scenario: Typed prompt-only command during a turn
- **GIVEN** a turn is running and `worktree` accepts only the prompt submission
- **WHEN** the user sends `/worktree list`
- **THEN** nothing is sent, the composer still holds `/worktree list`, and "Available when the turn ends" is shown under it

#### Scenario: Typed prompt-only command when idle
- **GIVEN** no turn is running and the same `worktree`
- **WHEN** the user sends `/worktree list`
- **THEN** it is sent as a prompt

### Requirement: Palette shows what picking a command does
In the command palette, a command whose input is required SHALL end its name with `…`. A command whose effect is a model prompt SHALL be marked `Prompt` at the end of its row. A command with no descriptor SHALL show neither a source nor the mark.

#### Scenario: Required and optional input side by side
- **GIVEN** the session advertises `compact` (input optional) and `explain` (input required)
- **WHEN** the user opens the command palette
- **THEN** the rows read `compact` and `explain…`

#### Scenario: Prompt command is marked
- **GIVEN** the session advertises the built-in `compact` (an action) and the project command `explain` (a prompt)
- **WHEN** the user opens the command palette
- **THEN** the `explain…` row is marked `Prompt` and the `compact` row is not

### Requirement: Palette commands the session cannot take
When the open session cannot take a command now, the palette SHALL show the command dimmed, with the reason as its second line in place of the description. Picking it SHALL send nothing and SHALL keep the palette open. The row SHALL keep its place in the list, so the first row stays the best match. The reasons are:
- "Agent not connected";
- "Session is loading";
- "Session could not be loaded";
- "Available when the turn ends".

#### Scenario: Session not connected
- **GIVEN** the open session's agent is disconnected
- **WHEN** the user opens the command palette
- **THEN** each slash command is dimmed and shows "Agent not connected"

#### Scenario: Prompt-only command during a turn
- **GIVEN** a turn is running, `worktree` accepts only the prompt submission and `compact` accepts queueing
- **WHEN** the user opens the command palette
- **THEN** `worktree…` is dimmed with "Available when the turn ends" and `compact` is not dimmed

#### Scenario: Picking an unavailable command
- **GIVEN** the same session and palette
- **WHEN** the user types `worktree` and presses Enter
- **THEN** nothing is sent, the composer is unchanged and the palette stays open with `worktree…` as its first row

### Requirement: Adding arguments from the palette
Tab on a highlighted, available command that takes input SHALL put it in the composer instead of submitting it, and SHALL close the palette. Tab on an available subcommand row SHALL do what picking it does. Tab on any other row SHALL do nothing. While an available command with optional input is highlighted, the palette SHALL show below the list that Enter runs it and Tab adds arguments.

#### Scenario: Adding arguments to an optional-input command
- **GIVEN** `compact` advertises the input hint "[optional instructions…]" and the input need `optional`
- **WHEN** the user highlights `compact` in the palette and presses Tab
- **THEN** the palette closes, nothing is sent, and the composer contains `/compact `, has focus with the cursor at the end, and shows the hint

#### Scenario: Key hint for an optional-input command
- **GIVEN** the same `compact`
- **WHEN** `compact` is the highlighted row
- **THEN** the palette shows "Enter to run · Tab to add arguments" below the list
- **AND** the hint is not shown while a row of another group is highlighted

#### Scenario: Tab on a command without input
- **WHEN** the user highlights a command that takes no input and presses Tab
- **THEN** nothing is sent, the composer is unchanged and the palette stays open

### Requirement: Subcommands in the catalog
A catalog entry MAY carry `_meta.querymt.subcommands`: a list of objects, each with a one-word `name` and a one-line `description`. An entry without subcommands SHALL omit the field. The `worktree` entry SHALL list `create`, `handoff`, `list`, `status`, `result`, `publish`, `merge`, `archive`, `remove`, `recover`, in that order. The agent SHALL accept every advertised name as the first word of the command's arguments.

#### Scenario: Worktree subcommands are advertised
- **WHEN** the catalog of a local session on an agent with the workspace service is advertised
- **THEN** the entry `worktree` carries `subcommands` with the ten names above, in that order, each with a non-empty description

#### Scenario: Advertised subcommands are accepted
- **WHEN** `/worktree` is run with any advertised subcommand name as its first argument, followed by the arguments that subcommand needs
- **THEN** the agent does not answer with the `/worktree` usage error

#### Scenario: Command without subcommands omits the field
- **WHEN** a session's catalog is advertised
- **THEN** the entry `compact` and every user-defined command carry no `subcommands`

### Requirement: Subcommands from the agent
For each advertised command, the app SHALL read the subcommands from `_meta.querymt.subcommands`. An element without a non-empty `name`, or whose name contains whitespace, SHALL be dropped. When two elements share a name, the first SHALL be kept. A missing `description` SHALL be read as empty. A command without the field, or with a value that is not a list, SHALL have no subcommands.

#### Scenario: Malformed subcommand entries are dropped
- **GIVEN** the agent advertises `worktree` with the subcommands `list`, an entry with no name, `two words` and `list` again
- **THEN** the app treats `worktree` as having the single subcommand `list`

#### Scenario: Agent without subcommands
- **GIVEN** an agent advertises `worktree` without `subcommands`
- **THEN** the app treats `worktree` as having no subcommands, and the palette shows only the `worktree…` row

### Requirement: Subcommands in the command palette
The palette SHALL show one row per subcommand, labelled `<command> <subcommand>…`, with the subcommand's description (or the parent's unavailable reason) as its second line. With no search the rows SHALL follow the command's row in advertised order; with a search they rank like any row. The search SHALL also match the subcommand's description and the parent's source.

#### Scenario: Palette lists worktree subcommands
- **GIVEN** the open session advertises `worktree` with its ten subcommands
- **WHEN** the user opens the command palette
- **THEN** the Slash commands group shows `worktree…` followed by `worktree create…` through `worktree recover…` in the advertised order, each with its description under it

#### Scenario: Searching for a subcommand
- **GIVEN** the same session
- **WHEN** the user types `worktree status`
- **THEN** `worktree status…` is the first row

#### Scenario: Searching a subcommand's description
- **GIVEN** the same session
- **WHEN** the user types the words of the `recover` description
- **THEN** `worktree recover…` is listed

### Requirement: Picking a subcommand from the palette
Picking an available subcommand row with Enter, Tab or a click SHALL close the palette and put `/<command> <subcommand> ` in the composer, focused with the cursor at the end. Nothing SHALL be sent. A composer draft SHALL be kept as it is when a command is picked. While the session cannot take the parent command, its subcommand rows SHALL be dimmed with the parent's reason, and picking one SHALL change nothing and keep the palette open.

#### Scenario: Picking a subcommand prefills it
- **GIVEN** the same session, no turn running and an empty composer
- **WHEN** the user picks `worktree list…` with Enter
- **THEN** the palette closes, nothing is sent, and the composer contains `/worktree list ` with focus and the cursor at the end

#### Scenario: Tab on a subcommand prefills it
- **GIVEN** the same session
- **WHEN** the user highlights `worktree status…` and presses Tab
- **THEN** the palette closes, nothing is sent, and the composer contains `/worktree status `

#### Scenario: Subcommand keeps the composer draft
- **GIVEN** the composer holds the draft `feature-x`
- **WHEN** the user picks `worktree create…`
- **THEN** the composer contains `/worktree create feature-x` with the cursor at the end

#### Scenario: Subcommand during a turn
- **GIVEN** a turn is running and `worktree` accepts only the prompt submission
- **WHEN** the user opens the command palette
- **THEN** every `worktree` subcommand row is dimmed with "Available when the turn ends"
- **AND** picking one sends nothing, leaves the composer unchanged and keeps the palette open
