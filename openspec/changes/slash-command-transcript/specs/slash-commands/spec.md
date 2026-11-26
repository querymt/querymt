# Spec Delta

## ADDED Requirements

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

### Requirement: Action commands in the conversation
A command reply SHALL be drawn as a command row: a command icon, `/name` and its arguments in a monospace face, no prompt bubble, aligned with the agent's column. The reply text SHALL be drawn under the row as command output, not as an assistant reply. Output that is a JSON document SHALL be shown indented. A reply with an error code SHALL mark the row as failed.

#### Scenario: Worktree list and its output
- **WHEN** the user sends `/worktree list` and the agent answers with the list
- **THEN** the conversation shows a command row `/worktree list` with the list under it as output, and neither a prompt bubble nor an assistant reply

#### Scenario: Refused command
- **WHEN** the user sends `/worktree archive` and the agent refuses it with the code `owner_active`
- **THEN** the command row is marked as failed and the refusal message is shown under it

#### Scenario: Reply stored by the agent survives a reload
- **GIVEN** a session in which `/worktree publish` was run
- **WHEN** the user reopens the session
- **THEN** the command row and its output are shown as they were live

#### Scenario: Reply the agent does not store is gone after a reload
- **GIVEN** a session in which `/worktree list` was run
- **WHEN** the user reopens the session
- **THEN** no row is shown for that command

### Requirement: Manual compactions in the conversation
A compaction that carries a command record SHALL say on its row that it was started by `/compact` and SHALL show the command's instructions when it has any. No separate prompt SHALL be shown for the command. A compaction without a record SHALL be drawn as before.

#### Scenario: Compact with instructions
- **WHEN** the user runs `/compact keep the API decisions`
- **THEN** the conversation shows one compaction row that names `/compact` and shows "keep the API decisions", and no prompt bubble

#### Scenario: Compact without instructions
- **WHEN** the user runs `/compact`
- **THEN** the compaction row names `/compact` and shows no instructions

#### Scenario: Automatic compaction
- **WHEN** the agent compacts on its own
- **THEN** the compaction row does not name `/compact`

#### Scenario: Compact on an empty conversation
- **WHEN** the user runs `/compact` in a session with no messages and the agent replies that there is nothing to compact
- **THEN** the conversation shows a command row `/compact` with that reply under it

### Requirement: A command being sent
A prompt the agent has not answered yet SHALL be drawn as a command row in progress when it names an advertised command whose effect is an action, and as a prompt bubble otherwise. When the agent's entry for it arrives, that entry SHALL take its place. If sending fails, the row SHALL be marked as failed and show the error.

#### Scenario: Pending action command
- **WHEN** the user sends `/worktree list` and the agent has not answered yet
- **THEN** the conversation shows a command row `/worktree list` in progress, not a prompt bubble

#### Scenario: Pending prompt command
- **WHEN** the user sends `/explain closures` and the agent has not answered yet
- **THEN** the conversation shows a prompt bubble reading `/explain closures`

#### Scenario: Failed action command
- **WHEN** the user sends `/worktree list` and the request fails
- **THEN** the command row is marked as failed and shows the error

### Requirement: Conversations without command records
A message that carries no command record SHALL be drawn as before: a user message as a prompt bubble with its content, an agent message as a reply. The app SHALL NOT decide from a message's text that it was a command.

#### Scenario: Agent without records
- **GIVEN** an agent that sends no command records
- **WHEN** the user sends `/worktree list` and the agent answers
- **THEN** the conversation shows the prompt and the answer as a prompt bubble and an assistant reply, as before this change

#### Scenario: Prompt that only starts with a slash
- **WHEN** the user sends `/tmp is full, why?` and the session advertises no `tmp` command
- **THEN** it is shown as an ordinary prompt bubble, while it is being sent and after

### Requirement: Command rows start a turn for activity
A command row SHALL count as the start of a turn for the session's activity indicators. Errors and streamed output that precede it SHALL NOT be reported as current.

#### Scenario: Error before a command row is not current
- **GIVEN** a conversation whose last turn ended with an error notice
- **WHEN** the user runs `/worktree list` and its command row is added
- **THEN** the session's activity reports no current error
