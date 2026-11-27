# Proposal

## Why

A client cannot tell what a slash command is or what it did. The catalog gives a name, a description and an input hint, so a client cannot know whether a command goes to the model, where it comes from, whether it needs input, or how it may be submitted. After a command runs, nothing in the session says a command ran:

- A user-defined command (`~/.qmt/commands`, `<root>/.qmt/commands`, configured paths) is replaced by its expansion before it is stored. The conversation shows "The user invoked the slash command…" and the whole template instead of `/explain x`.
- `/compact` stores no user message. Its instructions are used for the summary and then lost.
- `/worktree` replies arrive as assistant messages.

Clients fill the gap by guessing from text, and the guesses disagree with the agent (a leading `/` is treated as a command; `/COMPACT` is treated as `/compact`). A third built-in, `/agents`, is planned (`add-agents-slash-command`), so the gap grows with every command.

## What Changes

- **Catalog descriptor.** Every advertised command carries `_meta.querymt` with `source`, `effect`, `input` and `accepts`. Built-in and user-defined commands use the same fields.
- **Invocation record.** When the agent recognises a slash command, the session update that delivers the command's outcome carries `_meta.querymt.slashCommand` with the command's name, arguments, source, effect and the client prompt id. Exactly one update per invocation carries it:
  - a user-defined command: its user message (the content stays the expansion);
  - `/compact`: its compaction;
  - `/worktree`: its reply, or the ownership or archive announcement when that is the reply.
- **Records are kept wherever the outcome is kept.** A replayed or loaded session carries the same record on the same update. Transient replies stay transient.
- **A reply for outcomes that have no update today.** `/compact` on an empty conversation, or blocked by a hook, answers with a transient command reply instead of a notice.
- **Declared input need for user commands.** New optional frontmatter key `arguments: none | optional | required`.
- **BREAKING (ACP behaviour):** a user-defined command submitted as steering is rejected, like a built-in. Today it reaches the model unexpanded.
- **BREAKING (ACP behaviour):** the two `/compact` outcomes above are delivered as an agent message update, not as a `notice`.
- **BREAKING (slash commands):** a command file whose `arguments` frontmatter value is not one of the three is not loaded and is reported as a diagnostic. Today the key is ignored.
- **Session view (`querymt-agent-session-core`).** The view reads the descriptor into the command list and the record into the entry that carries it. A command reply becomes its own entry kind instead of an assistant message. An entry with a record replaces the optimistic prompt it answers.
- **Typeshare:**
  - `AgentEventKind::PromptReceived`, `UserPromptBlock`, `CompactionStart`, `WorktreeCommandOutput`, `WorktreeCommandCompleted`, `WorktreeOwnershipTransferred` and `WorktreeArchived` gain an optional `slash_command`.
  - `WorktreeCommandOutput` gains an optional `code`.
  - New `AgentEventKind::SlashCommandReply`.
  - New `SlashCommandRecord`.
  - `SlashCommandView`, `UserMessage` and `CompactionEntry` gain fields; `EntryKind` gains `CommandReply`.

## Non-goals

- Making transient replies durable (`/worktree list`, `status`, `result`, `create`). They stay live-only.
- Enforcing `arguments: required`. The agent still expands a command sent without arguments; the key tells clients what to ask for.
- Structured subcommands in the catalog. That is a separate change; it adds `subcommands` next to the descriptor fields.
- Script-backed commands. Scripts stay declared and not executed.
- Changing how a restricted mesh turn treats a slash command (it is not expanded).
- `/agents`. `add-agents-slash-command` should deliver its output as a command reply with a record; see design.md, Open Questions.
- A client capability for the new metadata. It rides on existing update kinds, so a client that ignores it sees what it sees today.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `slash-commands`: adds what the agent advertises about each slash command, how it records an invocation on the session's updates, and which submissions it accepts. The spec already holds the querymt-gpui requirements (moved into this repository's `openspec/`); the added requirements are about the agent and name it as their subject.
- `session-view-state`: the view exposes command descriptors and invocation records, holds command replies as their own entries, and replaces an optimistic prompt by the entry that answers it. This capability is introduced by `add-agent-session-core`, which must be archived before this change.

## Impact

- **Packages:**
  - `querymt-agent-protocol-types`: event fields, the new event, `SlashCommandRecord`, replay projection. Feature `test-support` (fixtures).
  - `querymt-agent-runtime`: catalog, frontmatter, record emission, steering rejection. Features `remote`, `oauth`, `plugin-loaders`, `api`.
  - `querymt-agent-session-core`: view types and reducer. Feature `test-support`.
  - `querymt-agent-services`: event matches that list the changed variants (`event_sink`, `session/turns`, `export`).
  - `querymt-agent`: facade tests and examples that construct the changed events.
  - `querymt-gpui` (`crates/querymt-gpui`, path dependency on `querymt-agent-session-core`): test constructions of `SlashCommandView` only; no behaviour change.
- **ACP wire (additive):** `_meta.querymt` on `available_commands_update` entries; `_meta.querymt.slashCommand` and `_meta.querymt.slashCommandError` on `user_message_chunk`, `compaction_update` and `agent_message_chunk`.
- **ACP behaviour (breaking):** steering rejection and the `/compact` reply, as listed above.
- **Session journal:** new optional fields on existing durable events. No migration. A journal written before this change has no records and replays as before.
- **Config:** none.
- **Docs:** `docs/docs/agent/slash-commands.md` (the `arguments` key, steering), `CHANGELOG.md` Unreleased.
- **Downstream:** `querymt-mobile-ffi` and `querymt-agent-py` build against the changed event types.
- **querymt-desktop UI follow-up (not planned here):**
  - regenerate typeshare types;
  - handle the `command_reply` entry kind;
  - render user messages and compactions that carry a record;
  - stop reading the `/compact` empty and blocked outcomes from notices.
- **querymt-gpui follow-up:** the changes `slash-command-actions`, `slash-command-transcript` and `slash-subcommands` (in this repository's `openspec/changes`, code in `crates/querymt-gpui`) consume this change.
