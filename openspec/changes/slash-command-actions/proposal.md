# Proposal

## Why

The command palette cannot tell what picking a slash command will do. The app knows a name, a description and a free-text input hint, so:

- every command with a hint only fills the composer, even when its input is optional (`compact`);
- a user-defined command that sends a prompt to the model looks the same as a built-in the agent runs itself;
- a command the session cannot take now closes the palette and does nothing;
- while a turn runs, any text that starts with `/` is queued, including a message that starts with a path, and a command the agent only takes as a prompt (`/worktree`) is queued and then rejected.

querymt now states these facts per command (`add-slash-command-records`, in this repository): source, effect, input need and accepted submissions. This change makes the app use them. It covers built-in and user-defined commands (`~/.qmt/commands`, `<root>/.qmt/commands`, configured paths) with the same rules.

This change replaces the earlier draft of `slash-command-actions`. The conversation part of that draft moved to `slash-command-transcript`.

## What Changes

- **The app reads the command descriptor.** Each command's source, effect, input need and accepted submissions come from the agent. A command without a descriptor (an older or different agent) behaves as today.
- **The palette runs commands that need no arguments.** Enter on a command whose input is `none` or `optional` sends it. A command whose input is `required` is put in the composer, and its row ends in `…`. Tab puts any command that takes input in the composer. **BREAKING** for the spec scenario "Command with input", which requires `/compact` to fill the composer.
- **The palette says what a command is.** A command that becomes a model prompt is marked `Prompt`. The source and the mark are searchable.
- **The palette shows commands the session cannot take.** Such a row is dimmed and shows the reason. Picking it does nothing and the palette stays open. Today the palette closes and nothing happens.
- **One rule decides how a command is submitted**, for the palette and the composer:
  - text is a command only when its first word is `/` plus an advertised name, exactly;
  - a command is queued while a turn runs, never steered;
  - a command that does not accept queueing is not sent while a turn runs: the palette row is unavailable, and a typed one stays in the composer with the reason shown;
  - any other text that starts with `/` is an ordinary message and follows the composer's delivery choice. **BREAKING** for such messages: today they are always queued.

## Non-goals

- How commands look in the conversation (`slash-command-transcript`, `add-slash-command-replies`).
- Subcommands and destructive commands (`slash-subcommands`, which builds on this change).
- Markers in the composer's own command list. Completing there never sends, so "runs or asks for more" does not apply.
- Sending a prompt-only command (`/worktree`) while a turn runs. The agent would accept it; the app waits for the turn to end.
- A confirmation step before `compact`. Enter runs it, as it does for every command without required input.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`:
  - the app reads the agent's descriptor for each command;
  - running from the palette depends on the command's input need;
  - palette rows show required input, prompt commands and why a command is unavailable;
  - submission of a typed or picked command follows the command's accepted submissions.

## Impact

- **Depends on `add-slash-command-records`** (catalog descriptor and `SlashCommandView` fields). That change must land first. `querymt-gpui` uses `querymt-agent-session-core` by path, so there is no revision to pin.
- **`crates/querymt-gpui`** (paths below are relative to it):
  - `src/session/commands.rs` (new): the descriptor fallback and command recognition.
  - `src/ui/slash.rs`: command recognition, readiness, `run_slash_command`.
  - `src/ui/keys.rs`: palette targets, row content, Tab, the key hint footer.
  - `src/ui/session_view.rs`: `session_ready` and `send_session_prompt` use the shared readiness; the composer shows the reason.
  - `src/session/open.rs`: `OpenSession::send` takes the delivery decision instead of testing for a leading `/`.
  - Tests in `src/ui/keys.rs`, `src/ui/slash.rs`, `src/session/open.rs`.
- **Compatibility:** with an agent that sends no descriptor, commands behave as before this change, except that text starting with `/` that names no command is no longer forced to queue.
- **Related:** `slash-subcommands` depends on this change's input need, accepted submissions and unavailable rows. `add-slash-command-replies` builds on tasks 1.1 and 1.2 of this change (the descriptor check and `src/session/commands.rs`, for pending command rows). `slash-command-transcript` does not depend on this change.
