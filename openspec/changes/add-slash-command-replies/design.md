# Design

## Context

See proposal.md for the motivation. Agent paths are under `crates/`; app paths are relative to `crates/querymt-gpui`.

- **From `add-slash-command-records`:**
  - `SlashCommandRecord`, and `slash_command: Option<SlashCommandRecord>` on `WorktreeCommandOutput`, `WorktreeCommandCompleted`, `WorktreeOwnershipTransferred` and `WorktreeArchived` (always `None` so far), plus `code` on `WorktreeCommandOutput`;
  - `AgentEventKind::SlashCommandReply { slash_command, text, code }`, ephemeral, never emitted so far;
  - `replay.rs` `stamp_slash_command(update, record, error)`, already called by `replayable_updates` for those five events;
  - `UserMessage.slash_command`, `CompactionEntry.slash_command` and the descriptor in `SlashCommandView` (`effect: Option<SlashCommandEffect>`).
- **`/compact`:** `session_actor.rs` `execute_prompt_inner` handles it before the prompt is stored. `CompactionOutcome::NothingToCompact` and `CompactionOutcome::Blocked` emit only `AgentEventKind::HookNotice`.
- **`/worktree`:** `handle/ext_worktree_ops.rs` `handle_native_worktree_prompt` runs before the session actor. Its outcome is the transient `WorktreeCommandOutput`, the durable `WorktreeCommandCompleted` (`NativeCommand::durable_reply`), or an announcement written by `announce_handoff` or `announce_archived`.
- **Session view:** `updates.rs` `assistant_chunk` turns every `agent_message_chunk` into an `AssistantMessage`. `client.rs` `prompt_completed` settles a prompt the agent never echoed (`settle_unechoed_prompt`). `store.rs` `Store::push` lets only user messages and compactions open a turn outside a run.
- **App, after `slash-command-transcript`:** `ItemKind::User` has `command`, `Compaction` has `requested`, and `is_compact_command` is gone. `src/session/commands.rs` (`slash-command-actions`) has `recognise` and `CommandTraits::is_action`.

## Goals / Non-Goals

**Goals:**
- Every recognised command leaves a record on the update that delivers its outcome.
- A client never shows a command's reply as model output, and never keeps a prompt bubble for a command that was answered.

**Non-Goals:**
- See proposal.md.

## Decisions

### D1. `SlashCommandReply` covers `/compact` outcomes without a compaction

- `execute_prompt_inner` emits `SlashCommandReply` instead of `HookNotice` for `CompactionOutcome::NothingToCompact` (no code) and `CompactionOutcome::Blocked` (code `blocked_by_hook`).
- It is emitted through the session's ordered stream, so it precedes the prompt response like the worktree output does.
- Every other `/compact` outcome already has a compaction to carry the record (`Completed`, `PriceLimit`, failed, cancelled) or fails the request (provider not buildable before the start).
- Alternative considered: leaving the notices. Rejected: the client would keep an unanswered `/compact` prompt with no record, and would have to recognise it by its text.

### D2. Worktree replies and announcements carry the record

- `client_prompt_id_from_meta` moves from `session_actor.rs` to a place both the actor and the worktree handler can call.
- `handle_native_worktree_prompt` builds the record (`source` `builtin`, `effect` `action`) and sets it, and `code`, on `WorktreeCommandOutput` and `WorktreeCommandCompleted`.
- `run_native_command` passes the record down to `announce_handoff` and `announce_archived`. Callers that are not a native command of the announced session pass `None`:
  - the ACP extension methods;
  - `announce_recovered_handoff`;
  - an archive issued from another session.
- In the last case the issuing session gets `WorktreeCommandCompleted` with the record; the owner's announcement has none.
- Alternative considered: sending a second, empty reply that only carries the record. Rejected: an older client would show an empty message.

### D3. Session view: command reply entries and the pending prompt

- `entry.rs`: new `EntryKind::CommandReply(CommandReply { message_id, slash_command, text, code })`. `EntryKind` is `#[non_exhaustive]`.
- `updates.rs`: `apply_update_inner` routes an `AgentMessageChunk` with `slashCommand` to a new `command_reply_chunk`. It has its own id index, so it never merges with `assistant_chunk`'s "newest id-less assistant message" rule.
- **Pending prompt.** When an entry with `client_prompt_id` is created, the pending prompt is removed (`client.rs` `drop_unsent_prompt`) and `idx.prompt` points at the new entry. `prompt_completed` treats "the prompt's entry is a command reply or compaction created outside a run" as answered without a run, so `RunState::Submitting` still ends. Today that signal comes from `settle_unechoed_prompt` finding the prompt pending, which no longer happens.
- **Turns.** `store.rs` `Store::push` adds `EntryKind::CommandReply` to `opens_turn`. With a running run it joins that run's turn through the existing `active_run` rule.
- `snapshot.rs` keeps skipping prompts settled without a run on reload, for agents that send no records.

### D4. App: a command reply is a new row kind

- `ItemKind::Command { name, arguments, output, state }` with `CommandState::{ Running, Done, Failed }`.
- From `EntryKind::CommandReply`: `Done`, or `Failed` when `code` is set; `output` is the reply text.
- Output that parses as a JSON document is re-serialized with indentation at projection time. `/worktree` replies are one line of compact JSON today, which is unreadable in a capped block.
- The projection match gains the arm; the existing `_ => {}` arm stays for kinds added later.

### D5. App: a local prompt that names an action command is a command row in progress

- For a `UserMessage` whose status is pending or failed, the projection asks `commands::recognise(text, &commands)` and `CommandTraits::of`. A command for which `is_action()` holds becomes `ItemKind::Command` with `Running`, or `Failed` with the prompt's error as output.
- The test is `is_action()`, never `!is_prompt()`. A command without a descriptor stays a prompt bubble while pending, so it does not show as a command row and then turn into a bubble when the response arrives.
- This uses the catalog at the moment of sending, for a prompt this client just created. It is not history classification: a confirmed message is never recognised from its text.
- The row builder needs the command list. The projection already reads it from the core header (`header.available_commands`); the same list is passed to the function that builds a turn's rows.
- A pending `/compact` becomes a command row in progress, and the compaction that carries its record replaces it (D3), so no `/compact` prompt bubble is left next to the compaction row.
- Alternative considered: leaving the prompt bubble until the reply arrives. Rejected: `/compact` can take many seconds, and the bubble says "this was sent to the model".

### D6. App: the command row's look

- Left-aligned in the agent's column, full width.
- First line:
  - `IconName::SquareSlash` in `p.muted`;
  - `/name` in `p.text` and `kit::MONO`, arguments in `p.muted` and `kit::MONO`, truncated, with the full text in a tooltip;
  - at the end, a spinner for `Running`, nothing for `Done`, `IconName::CircleAlert` in `p.danger` for `Failed`.
- No fill; a 1 px `p.border` outline at the theme radius. It reads as a record of an action, not as a message.
- Output: under the row, in the tool-output block (`code_block`, `clipped_with_more` with `Density::output_lines`), with Copy on hover. A row without output has no block.
- Tokens only; both densities use the same row, with the 26 px chip height of tool rows in compact density.

### D7. App: consumers of the new row kind

- `src/quick/activity.rs` `observe`: `ItemKind::Command` ends the scan, like `ItemKind::User`.
- `src/ui/session_view.rs`: `activity` gives "Running /name…" for a `Running` command row; the row hash and the row-kind label gain the `Command` arm.
- `src/session/golden.rs`: a `Row::Command`.
- `src/ui/find.rs`: no change. A command row has no Markdown view, like a tool row.

## Risks / Trade-offs

- **A reply during a run lands inside the running turn.** → Accepted; it is the existing rule for content without a run id, and `/worktree` is the only command that can do it.
- **A transient reply disappears on reload while a stored one stays.** → Accepted; it follows the agent's storage, and the spec states it.
- **The `/compact` notice becomes a message (breaking for clients that read the notice).** → Listed in CHANGELOG; querymt-desktop is a listed follow-up.
- **Pretty-printing changes what Copy copies.** → Copy copies the shown, indented text; the content is the same document.

## Migration Plan

1. Land after `add-slash-command-records`, `slash-command-actions` tasks 1.1–1.2 and `slash-command-transcript`.
2. No stored data changes: the event fields exist already. Journals written before this change replay without records on replies.
3. Rollback: revert this change. Worktree replies return to assistant messages and the `/compact` outcomes to notices.
