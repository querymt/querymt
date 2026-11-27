# Proposal

## Why

The conversation does not show that a slash command ran. Every user message is drawn as a prompt bubble, so:

- a user-defined command (`~/.qmt/commands`, `<root>/.qmt/commands`, configured paths) shows the expanded template ("The user invoked the slash command…") instead of the `/explain closures` the user typed;
- `/compact keep the API decisions` leaves a compaction row with no trace of the instructions;
- the app guesses from text in one place (`is_compact_command`), and the guess ignores case while the agent does not.

querymt now records prompt commands and manual compactions on the updates that carry them (`add-slash-command-records`, in this repository). This change draws the conversation from those records.

## What Changes

- **A prompt command shows what was typed.** A user message that carries a command record is drawn as the prompt bubble with `/name arguments`. The expanded prompt the model received is behind a "Show prompt" toggle.
- **A manual compaction shows that it was requested.** A compaction that carries a record says it was started by `/compact` and shows the instructions. Automatic compactions are unchanged.
- **The app stops guessing.** `is_compact_command` is removed. A message without a record is drawn exactly as today.

## Non-goals

- Action commands (`/worktree` replies, the `/compact` reply on an empty conversation), command rows for commands being sent, and replacing the `/compact` prompt bubble by its compaction. They need records the agent does not send yet, and are in `add-slash-command-replies`.
- Classifying history that has no records. Sessions written before the querymt change, and agents that send no records, look as they do today.
- The palette and how commands are submitted (`slash-command-actions`).

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: adds how prompt commands and manual compactions are shown in the conversation.

## Impact

- **Depends on `add-slash-command-records`** (`UserMessage.slash_command`, `CompactionEntry.slash_command`). It does not depend on `slash-command-actions`.
- **`crates/querymt-gpui`** (paths below are relative to it):
  - `src/session.rs`: `ItemKind::User` gains the command, `Compaction` gains the instructions; `is_compact_command` is removed.
  - `src/session/projection.rs`: rows from `UserMessage.slash_command` and `CompactionEntry.slash_command`.
  - `src/ui/session_view.rs`: the user bubble's command form and toggle, the compaction row, the activity label.
  - `src/session/golden.rs`: the user row's command, as `Row::UserCommand`.
  - `src/ui/find.rs`: `views_of` indexes a command row's arguments.
  - Tests in `src/session/projection.rs`, `src/ui/session_view.rs`; golden files.
- **Compatibility:** with no records, nothing changes on screen except the activity label of a `/compact` being sent, which reads "Sending prompt…" until the compaction starts. Until `add-slash-command-replies`, a `/compact` prompt bubble stays next to its compaction row, as today.
- **Follow-up:** `add-slash-command-replies` adds the command row and extends the rows of this change.
