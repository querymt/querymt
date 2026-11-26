# Proposal

## Why

gpui builds the open session's transcript with its own reducer in `src/session.rs` (about 2,000 of its 3,609 lines: `apply*`, `fold_compactions`, `replay_updates`, `apply_history_events`). It re-declares the agent's wire shapes by hand and guesses where it lacks information: turns start at every user message, a prompt is matched to its echo by text (`echo_pending`), run state comes from the coalescing `runtimeChanged` phase, and a reload rebuilds every item with new keys. QueryMT now ships `querymt-agent-session-core` (change `add-agent-session-core` in the querymt repo), a native reducer built from the agent's own types and tested for equality against the agent's live stream, history replay and load snapshot. Using it removes the duplicated reducer and gives gpui the same view live and after a reload.

## What Changes

- Add `querymt-agent-session-core` as a git dependency pinned to a QueryMT revision, with the matching `outputHashes` entry in `flake.nix`.
- The open session's conversation state is owned by one `SessionCore`. Every agent input for the session goes through `SessionCore::apply` in order: the load response, `session/update` (including replayed ones), `querymt/session/stateUpdate`, `runtimeChanged`, `inputState`, older-history pages, full tool content, undo stacks, config option responses, and prompt submission, completion and failure.
- `SessionView` stays the type the UI reads, but it becomes a projection of the core's turns plus a small gpui-owned overlay (client notices, load and fetch state, modes list, access, unconfirmed steer/queue inputs, timers). Projected items are cached per turn and entry revision, so unchanged rows keep their keys, revisions and `Arc<ToolCall>` pointers.
- A turn is one agent run. A steering message stays in the run's turn; undo, fork, turn diff and "copy turn" work on these turns.
- `session/prompt` sends `_meta.querymt.client_prompt_id`; the prompt response's `_meta.querymt.messageId` links the optimistic prompt. Text-based echo matching is removed.
- `initialize` advertises plan operations (`plan`) and subagents (`subagents`) in addition to compaction and the load snapshot, so the agent sends the shapes the reducer is tested against.
- Delegate tool calls are linked to their child sessions through ACP `subagent_update` and tool-call session references; the `querymt/session/delegationUpdate` handling is removed. **BREAKING** for agents without subagent support: delegate rows show no child link there.
- Remove the old reducer code and its tests; keep `Diff`, `synthesized_diffs`, the `ToolCall` presentation, `UserAttachment`, `UndoFrame`, `SelectOption` and `SessionAccess`.
- A differential test runs QueryMT's golden traces and gpui's existing session fixtures through the old reducer and the new projection before the switch, and records the intended differences.

Prerequisites in QueryMT (tracked there, not implemented here):
- `add-agent-session-core` groups 1–6, pushed; plus two additions: the reducer keeps the latest accounting snapshot in its header, and compaction updates carry `_meta.querymt.compaction` with the tokens before and after.
- `adopt-acp-subagents-cursors-usage` subagent support: the agent emits `subagent_update`, tool-call session references and the snapshot's `subagentUpdates`.

## Capabilities

### New Capabilities
- `session-transcript`: how gpui holds and shows the open session's conversation: one reducer as the source, identity that survives reloads, prompts and their echo, runs and run state, compactions, plan, usage and cost, older history, full tool content, and the client-only overlay.
- `session-turn-actions`: what undo, redo, fork, turn diff and turn copy operate on once a turn is an agent run.

### Modified Capabilities
None. gpui has no specs yet.

## Impact

- **Code:** `src/session.rs` (reducer removed, presentation types kept), `src/store.rs` (session open/load, notification routing, prompting, steering, history paging, tool content, undo, config), a new `src/session/` projection module, `src/ui/session_view.rs` turn helpers (`turn_ends`, `turn_text`), `src/store/delegates.rs` (child links).
- **Dependencies:** `querymt-agent-session-core` (git, pinned rev; no tokio, rusqlite or workspace-hack in its tree; builds for `wasm32-unknown-unknown`). Its transitive `typeshare`/`chrono` reach the binary; `wasm-bindgen` stays `=0.2.130`.
- **Build:** `flake.nix` gains `cargoLock.outputHashes` for the git source; every rev bump changes it. `web/build.sh` is unchanged.
- **Wire:** gpui sends `client_prompt_id` on prompts and advertises `plan` and `subagents`; it reads `querymt/session/stateUpdate`. It no longer reads `querymt/session/delegationUpdate`.
- **Telemetry:** `session.apply_ms` now measures `SessionCore::apply` plus projection.
