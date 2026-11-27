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
- **What `add-slash-command-records` provides** (`querymt-agent-session-core`): `UserMessage.slash_command` and `CompactionEntry.slash_command`: `{ name, arguments, source, effect, client_prompt_id }`.

## Goals / Non-Goals

**Goals:**
- Rows are built from records. The projection does not look at message text to decide what a row is.
- Existing consumers of the user row keep working without edits where the meaning is unchanged.

**Non-Goals:**
- See proposal.md. The command row, pending command rows and the `/compact` reply are designed in `add-slash-command-replies` (its D4–D7).

## Decisions

### D1. A prompt command stays a user row; its text is what was typed

- `ItemKind::User` gains `command: Option<UserCommand { name, expansion }>`.
- For a user message with a record, the projection sets `text` to `/name` plus the arguments, and `expansion` to the text of the message's blocks.
- Because `text` is what the user typed, `fork_target`, `find::views_of`, the golden rows and Copy need no change and get the better value.
- The optimistic prompt already holds the typed text, so the row reads the same before and after the agent's echo.
- Alternative considered: a separate `ItemKind::Command` for prompt commands too. Rejected: a prompt command is a message to the model. It belongs with prompts for turn boundaries, forking and find, and the user asked to tell prompts from actions.

### D2. The compaction row shows the request

- `Compaction` gains `requested: Option<String>`: `Some(arguments)` when the entry has a record, with an empty string for no instructions.
- `render_compaction` adds, after the status label, `/compact` in `MONO` and `p.muted`. Instructions follow on the same line, truncated to one line, with the full text in the expanded details above the summary.
- The instructions are visible on the collapsed row, because they are the only record of what the user asked for.

### D3. The user bubble's command form

- `/name` in `kit::MONO`, the arguments in the bubble's normal face.
- "Show prompt" / "Hide prompt" sits beside Copy and uses the row's expansion key, like the front-matter toggle.
- Tokens only; both densities use the same bubble.

### D4. The app stops guessing

- `src/ui/session_view.rs` `activity`: `is_compact_command` and its use are removed. A local `/compact` prompt reads "Sending prompt…" like any other until the compaction starts; an active compaction still gives "Compacting context…" through `active_compaction`.
- `src/session/golden.rs`: the `command` of `Row::User`.
- `src/ui/find.rs`: no change.

## Risks / Trade-offs

- **The `/compact` prompt bubble stays next to the compaction row.** → As today; `add-slash-command-replies` replaces it.
- **The compaction row gets longer.** → Instructions are one truncated line; the divider keeps its height.
- **`ItemKind::User` gains a field.** Every construction in tests needs `command: None`. → One mechanical task, first.

## Migration Plan

1. Land `add-slash-command-records`.
2. Ship this change. No stored data changes; golden files are regenerated in task 1.3.
3. Rollback: revert this change. User messages show the expansion again and compactions lose the instructions; nothing else changes.
