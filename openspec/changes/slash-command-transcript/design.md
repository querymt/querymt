# Design

## Context

See proposal.md for the motivation. Paths are relative to `crates/querymt-gpui`, a member of this workspace that depends on `querymt-agent-session-core` by path. The current state:

- **Rows.** `src/session/projection.rs` turns each core entry into an `Item` (`src/session.rs` `ItemKind`):
  - `EntryKind::UserMessage` becomes `ItemKind::User { text, attachments, local }`, where `local` means the agent has not confirmed it;
  - `EntryKind::AssistantMessage` becomes `ItemKind::Agent`;
  - `EntryKind::Compaction` becomes `ItemKind::Compaction` through `compaction_row`;
  - an entry kind the app does not know has no row.
- **Drawing.** `src/ui/session_view.rs` `render_item` draws `User` as the right-aligned accent bubble with a Copy action and, in developer mode, a front-matter toggle. `render_compaction` draws the compaction divider with a "Show summary" toggle.
- **Consumers of `ItemKind::User`:**
  - `src/quick/activity.rs` `observe` stops scanning at the last user row;
  - `src/session/view.rs` `fork_target` uses the user text as the fork's prompt;
  - `src/ui/find.rs` `views_of` indexes the user text;
  - `src/session/golden.rs` records it;
  - `src/ui/session_view.rs` `activity` shows "Sending prompt…" for a local one, or "Compacting context…" when `is_compact_command` matches its text.
- **What `add-slash-command-records` provides** (`querymt-agent-session-core`):
  - `UserMessage.slash_command` and `CompactionEntry.slash_command`: `{ name, arguments, source, effect, client_prompt_id }`;
  - `EntryKind::CommandReply { message_id, slash_command, text, code }`;
  - the entry that carries a record replaces the optimistic prompt it answers, so a command and its result are never both in the view.
- **From `slash-command-actions`:** `src/session/commands.rs` `recognise` and `CommandTraits::of` (is this text an advertised command). `CommandTraits::is_action()` is true only when the agent stated `effect` `action`; a command without a descriptor is neither a prompt nor an action.

## Goals / Non-Goals

**Goals:**
- Rows are built from records. The projection does not look at message text to decide what a row is.
- Existing consumers of the user row keep working without edits where the meaning is unchanged.

**Non-Goals:**
- See proposal.md.

## Decisions

### D1. A prompt command stays a user row; its text is what was typed

- `ItemKind::User` gains `command: Option<UserCommand { name, expansion }>`.
- For a user message with a record, the projection sets `text` to `/name` plus the arguments, and `expansion` to the text of the message's blocks.
- Because `text` is what the user typed, `fork_target`, `find::views_of`, the golden rows and Copy need no change and get the better value.
- The optimistic prompt already holds the typed text, so the row reads the same before and after the agent's echo.
- Alternative considered: a separate `ItemKind::Command` for prompt commands too. Rejected: a prompt command is a message to the model. It belongs with prompts for turn boundaries, forking and find, and the user asked to tell prompts from actions.

### D2. A command reply is a new row kind

- `ItemKind::Command { name, arguments, output, state }` with `CommandState::{ Running, Done, Failed }`.
- From `EntryKind::CommandReply`: `Done`, or `Failed` when `code` is set; `output` is the reply text.
- Output that parses as a JSON document is re-serialized with indentation at projection time. `/worktree` replies are one line of compact JSON today, which is unreadable in a capped block.
- The projection match gains the arm; the existing `_ => {}` arm stays for kinds added later.

### D3. A local prompt that names an action command is a command row in progress

- For a `UserMessage` whose status is pending or failed, the projection asks `commands::recognise(text, &commands)` and `CommandTraits::of`. A command for which `is_action()` holds becomes `ItemKind::Command` with `Running`, or `Failed` with the prompt's error as output.
- The test is `is_action()`, never `!is_prompt()`. A command without a descriptor (an agent from before `add-slash-command-records`) stays a prompt bubble while pending, so it does not show as a command row and then turn into a bubble when the response arrives.
- This uses the catalog at the moment of sending, for a prompt this client just created. It is not history classification: a confirmed message is never recognised from its text (spec: Conversations without command records).
- The row builder needs the command list. The projection already reads it from the core header (`header.available_commands`, the source of its `slash_commands`); the same list is passed to the function that builds a turn's rows.
- With an agent that sends no records, it sends no descriptors either (both come from the same change), so the pending prompt is a bubble throughout. The core marks it sent when the response arrives, and the conversation shows the bubble followed by the assistant reply, as today.
- Alternative considered: leaving the prompt bubble until the reply arrives. Rejected: `/compact` can take many seconds, and the bubble says "this was sent to the model".

### D4. The compaction row shows the request

- `Compaction` gains `requested: Option<String>`: `Some(arguments)` when the entry has a record, with an empty string for no instructions.
- `render_compaction` adds, after the status label, `/compact` in `MONO` and `p.muted`. Instructions follow on the same line, truncated to one line, with the full text in the expanded details above the summary.
- This settles the earlier draft's open question: the instructions are visible on the collapsed row, because they are the only record of what the user asked for.

### D5. The command row's look

- Left-aligned in the agent's column, full width.
- First line:
  - `IconName::SquareSlash` in `p.muted`;
  - `/name` in `p.text` and `kit::MONO`, arguments in `p.muted` and `kit::MONO`, truncated, with the full text in a tooltip (truncated text needs a way to read it);
  - at the end, a spinner for `Running`, nothing for `Done`, `IconName::CircleAlert` in `p.danger` for `Failed`.
- No fill; a 1 px `p.border` outline at the theme radius. It reads as a record of an action, not as a message.
- Output: under the row, in the tool-output block (`code_block`, `clipped_with_more` with `Density::output_lines`), with Copy on hover. A row without output has no block.
- The user bubble for a prompt command: `/name` in `kit::MONO`, the arguments in the bubble's normal face. "Show prompt" / "Hide prompt" sits beside Copy and uses the row's expansion key, like the front-matter toggle.
- Tokens only; both densities use the same row, with the 26 px chip height of tool rows in compact density.

### D6. Consumers of the row kinds

- `src/quick/activity.rs` `observe`: `ItemKind::Command` ends the scan, like `ItemKind::User`.
- `src/ui/session_view.rs`:
  - `activity`: a `Running` command row gives "Running /name…"; `is_compact_command` and its use are removed. An active compaction still gives "Compacting context…" through `active_compaction`.
  - the row hash and the row-kind label next to `ItemKind::User` gain the `Command` arm.
- `src/session/golden.rs`: a `Row::Command` and the `command` of `Row::User`.
- `src/ui/find.rs`: no change. A command row has no Markdown view, like a tool row.

## Risks / Trade-offs

- **A transient reply disappears on reload while a stored one stays.** The conversation after a reload can show `/worktree publish` and not the `/worktree list` before it. → Accepted; it follows the agent's storage, and the spec states it.
- **The compaction row gets longer.** → Instructions are one truncated line; the divider keeps its height.
- **Pretty-printing changes what Copy copies.** → Copy copies the shown, indented text; the content is the same document.
- **`ItemKind::User` gains a field.** Every construction in tests needs `command: None`. → One mechanical task, first.
- **Depends on two other changes.** → Tasks start after `slash-command-actions` tasks 1.1 and 1.2.

## Migration Plan

1. Land `add-slash-command-records`, then `slash-command-actions` tasks 1.1 and 1.2 (`commands.rs`).
2. Ship this change. No stored data changes; golden files are regenerated in task 1.3.
3. Rollback: revert this change. The core still produces command reply entries (path dependency, no pin to move back), and they then have no row: `/worktree` output is not shown. If replies must stay visible, revert the session-view part of `add-slash-command-records` (its group 4) in the same release.
