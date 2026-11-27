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
