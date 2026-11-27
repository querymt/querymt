# Proposal

## Why

`add-slash-command-records` records prompt commands and manual compactions. It leaves out the commands whose outcome is a reply:

- `/worktree` replies and announcements carry no record, so a client shows `/worktree list` as a prompt to the model and its output as an assistant reply.
- `/compact` on an empty conversation, or blocked by a hook, reports its outcome as a `notice`. The optimistic `/compact` prompt is left without an answer the client can match.
- A pending `/compact` prompt stays as a prompt bubble next to the compaction row that answers it.

This change finishes that work, from the agent to the app. It was split out of `add-slash-command-records` (tasks 3.4, 3.5, 4.3, 4.4) and `slash-command-transcript` (action command rows, pending command rows) so the prompt-command part could ship first.

## What Changes

- **Agent: records on native command replies.** `/worktree` replies carry the record, and the stable error code when refused or failed. A handoff or archive announcement carries the record when it is the reply to the session's own command.
- **Agent: a reply for `/compact` outcomes without a compaction.** On an empty conversation or when blocked by a hook, `/compact` answers with a transient `SlashCommandReply` instead of a notice.
- **BREAKING (ACP behaviour):** those two `/compact` outcomes arrive as an agent message update, not as a `notice`.
- **Session view: command replies are their own entries.** An agent message that carries a record becomes `EntryKind::CommandReply`, never merged with assistant text. An entry with a record replaces the optimistic prompt it answers.
- **App: command rows.** A command reply is drawn as a command row with its output, a pending prompt that names an action command is a command row in progress, and a manual compaction replaces its `/compact` prompt bubble.
- **Typeshare:** `EntryKind` gains `CommandReply`. The event fields and `SlashCommandReply` already exist (`add-slash-command-records`); this change emits them.

## Non-goals

- Making transient replies durable (`/worktree list`, `status`, `result`, `create`). They stay live-only.
- Structured subcommands in the catalog and palette. Planned separately in `add-palette-subcommands`.
- Searching command rows with session find.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: records on native command replies, the `/compact` reply, and how command replies and commands being sent are shown in the app.
- `session-view-state`: command replies as their own entries and turns, and the optimistic prompt replaced by the entry that answers it.

## Impact

- **Depends on:** `add-slash-command-records` (record types, replay projection, descriptor); `slash-command-actions` tasks 1.1 and 1.2 (`crates/querymt-gpui/src/session/commands.rs`, `CommandTraits::is_action`); `slash-command-transcript` (the user and compaction rows this change extends). Archive after all three, and after `add-agent-session-core`.
- **Packages:**
  - `querymt-agent-runtime`: `execute_prompt_inner` (`/compact` outcomes), `handle/ext_worktree_ops.rs` (records, codes, announcements), `client_prompt_id_from_meta` moves.
  - `querymt-agent-session-core`: `EntryKind::CommandReply`, `command_reply_chunk`, pending prompt replacement, `Store::push`.
  - `querymt-agent`: worktree runtime tests.
  - `querymt-gpui`: `ItemKind::Command`, projection, the command row, activity.
- **ACP wire (additive):** `_meta.querymt.slashCommand` and `_meta.querymt.slashCommandError` on `agent_message_chunk`.
- **ACP behaviour (breaking):** the `/compact` reply, as above.
- **Docs:** `CHANGELOG.md` Unreleased (the `/compact` reply).
- **querymt-desktop follow-up (not planned here):** handle the `command_reply` entry kind; stop reading the `/compact` empty and blocked outcomes from notices.
- **`add-agents-slash-command`:** deliver `/agents` output as `SlashCommandReply` with a record instead of `AgentsCommandOutput`, whichever of the two changes lands second.
