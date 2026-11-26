# Proposal

## Why

The conversation does not show that a slash command ran. Every user message is drawn as a prompt bubble and every agent message as a reply, so:

- a user-defined command (`~/.qmt/commands`, `<root>/.qmt/commands`, configured paths) shows the expanded template ("The user invoked the slash command…") instead of the `/explain closures` the user typed;
- the output of `/worktree list` reads like a model reply, and `/worktree list` itself like a prompt to the model;
- `/compact keep the API decisions` leaves a compaction row with no trace of the instructions;
- the app guesses from text in one place (`is_compact_command`), and the guess ignores case while the agent does not.

querymt now records each invocation on the update that carries its outcome (`add-slash-command-records` in the querymt repository). This change draws the conversation from those records. It treats built-in and user-defined commands by what they are: a prompt to the model, or an action the agent ran.

## What Changes

- **A prompt command shows what was typed.** A user message that carries a command record is drawn as the prompt bubble with `/name arguments`. The expanded prompt the model received is behind a "Show prompt" toggle.
- **An action command is a command row.** A command reply is drawn as a row with `/name arguments`, a command icon and no bubble, with the reply text under it as command output. A refused or failed command is marked and shows its message. It is not drawn as an assistant reply.
- **A manual compaction shows that it was requested.** A compaction that carries a record says it was started by `/compact` and shows the instructions. Automatic compactions are unchanged.
- **A command being sent is a pending command row.** A local prompt that names an advertised action command is drawn as a command row in progress until the agent's entry replaces it, so it does not flash as a prompt bubble.
- **The app stops guessing.** `is_compact_command` is removed. A message without a record is drawn exactly as today.

## Non-goals

- Classifying history that has no records. Sessions written before the querymt change, and agents that send no records, look as they do today.
- Keeping transient replies across a reload. The row of `/worktree list` is gone after a reload, because the agent does not store it.
- Searching command rows with session find. Find covers Markdown views; command output is plain text, like tool output.
- The palette and how commands are submitted (`slash-command-actions`).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: adds how prompt commands, action commands, manual compactions and commands being sent are shown in the conversation.

## Impact

- **Depends on querymt `add-slash-command-records`** (records on entries, the command reply entry kind) and on `slash-command-actions` tasks 1.1 and 1.2 here (the pinned revision and `src/session/commands.rs`).
- **This repository:**
  - `src/session.rs`: `ItemKind::User` gains the command, `ItemKind::Command` is added, `Compaction` gains the instructions; `is_compact_command` is removed.
  - `src/session/projection.rs`: rows from `UserMessage.slash_command`, `CompactionEntry.slash_command` and `EntryKind::CommandReply`.
  - `src/ui/session_view.rs`: the user bubble's command form and toggle, the command row, the compaction row, the activity label.
  - `src/quick/activity.rs`, `src/session/golden.rs`: the new row kind.
  - Tests in `src/session/projection.rs`, `src/ui/session_view.rs`, `src/quick/activity.rs`; golden files.
- **Compatibility:** with no records, nothing changes on screen except the activity label of a `/compact` being sent, which reads "Sending prompt…" until the compaction starts.
