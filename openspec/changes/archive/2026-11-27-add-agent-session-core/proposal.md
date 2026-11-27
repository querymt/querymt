# Proposal

> **Scope at archive:** the WebAssembly module (`querymt-agent-session-core-wasm`), its build scripts, CI job and release artifacts, and the querymt-desktop follow-up were descoped, because querymt-desktop was replaced by querymt-gpui, which uses session-core natively. The sections below keep their original text; read the items marked "Descoped" as not implemented.

## Why

Every QueryMT client turns the same agent output (the `querymt/sessionLoadSnapshot.v1` load snapshot plus ACP `session/update` notifications) into a conversation view, and each one does it separately: querymt-desktop in about 3,000 lines of TypeScript (`session-updates.ts`, `session-snapshot.ts`, `session-conversation.ts`), querymt-mobile in its own `sessionUpdateReducer.ts`, and querymt-gpui in about 3,000 lines of Rust over untyped JSON (`src/session.rs`, including a hand-copied version of the agent's replay projection). The desktop version copies the whole session on most updates and rebuilds every turn on each change, so it slows down as sessions get longer. All three drift from the agent's wire types, because they re-declare them by hand. One pure, deterministic Rust reducer that is built from the agent's own types and runs natively (as a plain Rust dependency, for querymt-gpui) and as WebAssembly (for querymt-desktop) fixes both problems at the source.

A review of the first reducer against the agent's call paths found that today's wire cannot give one view the same shape live and after a reload:
- **No run boundaries.** The agent groups history into runs (`session-history-loading` / Turn boundaries), but run start and end never reach the client, so a reducer guesses turns from user messages. An applied steer or a mid-run compaction splits one run into several turns.
- **No stable prompt identity.** The client only learns its prompt's message id from the echo, so an optimistic prompt keeps a client-made id that differs after a reload.
- **A load can lose or repeat events.** The connection subscribes to live events only after the snapshot is read, so events in between are lost or delivered on top of the snapshot. A client that attaches while a message streams keeps only the tail of it.

ACP v2 (draft) has a construct for each: `state_update` for foreground work, a prompt response with the inserted `messageId`, and replay held until `replay_complete` with a cut point. QueryMT speaks ACP v1, so this change adds the same shapes as QueryMT extensions on v1, and the reducer is built around them so a later move to v2 renames inputs instead of changing the design.

The wire types the reducer needs (`AgentEvent`, `AgentEventKind`, the load-snapshot payload and their closure) currently live in `querymt-agent-core`, `querymt-agent-services` and `querymt-agent-runtime`. These crates pull in tokio, async-trait, rusqlite contracts, the ACP runtime and more, so a browser build cannot depend on them. Those types move into a light crate that both the agent and the reducer depend on, so drift becomes a compile error instead of a runtime bug.

## What Changes

- **ACP stays the protocol.** `agent-session-core` is a client-side consumer of ACP v1. Agents and clients keep talking ACP; the additions below use ACP's own extension points (`_meta` and an extension notification) and add no session update kind.
  - Live `session/update` notifications are decoded with the official ACP schema types. That includes context compaction (`compaction_update`, `compaction_summary_chunk`, which a client opts into through the standard `session.compaction` capability, `CompactionCapabilities`), plan operations (`plan_update`, `plan_removed`), subagents (`subagent_update`, `session.subagents`) and session cursors (`cursor_update`, `session.cursors`).
  - QueryMT extensions the reducer reads: the `_meta["querymt/sessionLoadSnapshot.v1"]` load snapshot, and the run-identity additions below. Without them (an older agent), the reducer falls back to the standard replayed `session/update` notifications and to turns that start at user messages.
  - The reducer's input list (`snapshot_loaded`, `acp_notification`, `state_update`, `prompt_optimistic`, …) is an in-process API between a client's transport and the reducer, not a wire protocol. Each input is shaped on the ACP v2 construct it stands in for (design D4, ACP v2 alignment).
- **Agent wire additions (ACP v1 extensions shaped on ACP v2):**
  - A `querymt/session/stateUpdate` extension notification, shaped like v2 `state_update` (`running`, `requires_action`, `idle` with the stop reason) plus the run id. It is sent for every run transition, for prompted and scheduled runs alike, and is not coalesced.
  - `_meta.querymt.runId` on message, thought, tool call and compaction updates, live and replayed, so a client groups content by run.
  - `_meta.querymt.messageId` on the `session/prompt` response: the id of the user message the prompt inserted (v2 returns it as `messageId`).
  - Gap-free loads: on `session/load` the agent holds live updates from before it reads the history until it has answered. It then drops the held updates at or before the load's cut point, sends the text of a message still streaming at the cut, and only then sends the rest (v2's hold-until-`replay_complete` and cut point).
  - Load snapshots and older-history pages carry the `cursor_update`s of the entities they return, for clients that advertise `session.cursors`.
  - Every load snapshot carries the whole-session summary (usage, context, model), not only windowed ones.
  - A model change tells live clients that the last context measurement no longer applies (`_meta.querymt.contextInvalidated` on a `session_info_update`), so no path shows a measurement taken for the previous model.
  - Two fixes where the agent did not meet `acp-session-updates`: the live `completed` compaction update carries its summary, and replay sends the todo plan once as a `plan_update` for plan-operations clients.
- **Shared replay projection.** The agent's history replay and the reducer project journal events to ACP updates with one function set in `querymt-agent-protocol-types` (`replay`), so a loaded and a replayed session cannot differ. The agent's replay sends each finished compaction where it happened instead of before the whole conversation.
- **New package `querymt-agent-protocol-types`** (`crates/agent-protocol-types`). It holds the plain wire types clients consume:
  - agent events and their leaf types,
  - the load-snapshot payload and its parts (audit view, stream cursor, accounting snapshot, history window info, summary, truncation entries, delegation updates, user prompt records, runtime status),
  - session input-state notifications.
  
  It is serde and typeshare only, has no async runtime, and builds for `wasm32-unknown-unknown`.
- **New package `querymt-agent-session-core`** (`crates/agent-session-core`). It is a pure reducer: session events in, a session view (header plus ordered conversation turns, each made of ordered entries with stable ids and revisions) and a change report out. It covers:
  - snapshot hydration, live ACP updates (including replayed updates that arrive before the load response), older-history pages, optimistic prompts, run state, runtime and input-state observations, undo stacks, subagents, session cursors, config options, plans, reasoning parts and the token-usage breakdown,
  - turns that are agent runs, keyed by run id, and entry ids taken from agent identities, so one session has the same ids live and after a reload,
  - deterministic output with no clock or randomness,
  - tolerant decoding, per event inside snapshots and history pages,
  - a bounded debug event log (newest 1,000 entries).
  - `SessionCore` is `Send + 'static`, so native clients can hydrate large snapshots off their UI thread.
  - Native clients read turns and entries by reference: reading a turn copies no entry content, so reading the last turn after every update costs the same however large its tool results are. The owned `turn`/`turns` accessors stay for serialization and the WebAssembly boundary.
- **Descoped: new package `querymt-agent-session-core-wasm`** (`crates/agent-session-core-wasm`, `publish = false`). These are wasm-bindgen bindings that keep the state inside the module and exchange JSON batches and change reports.
- **Native Rust consumers.** A client outside this workspace (querymt-gpui) depends on `querymt-agent-session-core` through a git dependency pinned to a revision. The three new crates are listed under hakari `[final-excludes]`, so they never depend on `querymt-workspace-hack` and their dependency closure is the same inside and outside the workspace.
- **Descoped: release artifacts.** Nightly and stable releases publish `agent-session-core-wasm-<version>.tar.gz` plus its `.sha256`, listed in `release.json`.
- **Toolchain.** `rust-toolchain.toml` gains the `wasm32-unknown-unknown` target. The devShell gains `wasm-bindgen-cli` (matching the locked `wasm-bindgen` 0.2.130) and `binaryen`.
- **`querymt` features.** The `which` dependency becomes optional and is enabled by the `mcp` feature, which is its only user. As a result, `querymt` with `default-features = false` builds for `wasm32-unknown-unknown`.
- **Typeshare.** `scripts/generate-types.sh` reads the two new crates. Declarations of moved types are unchanged. The new session event and view types are added.
- **BREAKING (Rust API):**
  - `impl From<McpServerConfig> for McpServerInfo` is replaced by `McpServerConfig::server_info()` (orphan rule after the move).
  - Inherent constructors on moved types that need agent logic become functions in the owning crate. For example, `AccountingSnapshot::from_reducer` / `from_events` become `querymt_agent_services::accounting::snapshot_from_reducer` / `snapshot_from_events`.
  - Module paths under `querymt_agent_core` keep resolving through re-exports, so other paths do not change.
- No change to provider wire shapes, the session DB schema, config, hooks, middleware, the Extism contract, Python bindings or the log targets of the agent crates. The ACP changes are the additions listed above; no existing ACP message changes shape.

## Non-goals

- Replacing ACP or adding QueryMT-specific session update kinds. Compaction, plans, subagents and cursors stay on the standard ACP updates; QueryMT additions use `_meta` and one extension notification.
- Moving QueryMT to ACP v2. The additions are shaped so that a later move maps them one to one (design D4, ACP v2 alignment).
- Switching the agent runtime, querymt-mobile-ffi, querymt-mobile or querymt-ios to the new reducer. They can adopt it later.
- Markdown or HTML rendering, diff building, or tool presentation. These stay in clients.
- Client-local notices (connection errors, "Stopped.", failed undo). Clients render them outside the reducer's view.
- Reading the removed `querymt/session/delegationUpdate` notification or `delegationUpdates` snapshot key. The reducer reads subagents only (`adopt-acp-subagents-cursors-usage`).
- Publishing the crates to crates.io or an npm package. Native consumers use a pinned git dependency.
- Splitting `querymt-agent-core` further than the wire-type extraction.

## Capabilities

### New Capabilities
- `session-view-state`: deterministic reduction of agent session output (load snapshot, ACP session updates, history pages and client observations) into a client conversation view and change reports, available as a native Rust library (the published WebAssembly module was descoped).

### Modified Capabilities
- `acp-session-updates`: adds run-state notifications, run identity on session updates, the inserted message id on prompt responses, gap-free loads (hold and cut, including a message still streaming at the cut), cursors in load snapshots and history pages, a whole-session summary in every load snapshot, and context invalidation on a model change.

## Impact

**Packages and features:**

| Package | Status | Features whose code changes |
|---|---|---|
| `querymt-agent-protocol-types` | New | none |
| `querymt-agent-session-core` | New | none |
| `querymt-agent-session-core-wasm` | Descoped, not created | none |
| `querymt` | `which` becomes optional | `mcp` gains `dep:which` |
| `querymt-agent-core` | Moved modules become re-exports; `McpServerConfig::server_info()` | none |
| `querymt-agent-services` | Accounting snapshot and history-window payload types move out; constructors become functions | none |
| `querymt-agent-runtime` | `SessionLoadSnapshot`, `UserPromptRecord` and input-state notification types move out; ACP translator and load path gain the wire additions above; replay uses the shared projection | none |
| `querymt-agent`, `querymt-mobile-ffi`, `querymt-agent-py` | Downstream; no source change expected | none |

**ACP wire (additive):** `querymt/session/stateUpdate`, `_meta.querymt.runId` on session updates, `_meta.querymt.messageId` on prompt responses, `cursorUpdates` in load snapshots and history pages, `summary` in unwindowed load snapshots, `_meta.querymt.contextInvalidated` on `session_info_update`. Clients that ignore unknown extension notifications and `_meta` keys see no change, except that a load no longer loses or repeats events and replayed compactions arrive in place.

**Typeshare:** the set of declarations for existing types is unchanged. New declarations cover the session event, view and change-report types and the state-update notification.

**Workspace config:** `.config/hakari.toml` lists the two new crates that exist (protocol-types and session-core) under `[final-excludes]`; `cargo hakari verify` stays green.

**Coordination:**
- `split-agent-crate` has landed (archived 2026-11-15), so moved modules re-export through the final facade paths.
- `restructure-agent-turn-state` and `unify-model-call-path` (archived 2026-11-22) changed run start ordering, scheduled runs and the runtime phase channel; this change builds on them.
- `adopt-acp-subagents-cursors-usage` supplies `subagent_update`, `subagentUpdates` and `cursor_update`. The reducer reads only those. The gpui pin needs that change's additive subagent support (tasks 1.1, 4.1, 4.2 and 4.5) on the agent it talks to; cursors are not needed for gpui, only this change's task 4.5b waits for that change's cursor helper (3.1).

**Descoped: querymt-desktop follow-up (UI, not planned here):** the desktop change `adopt-agent-session-core` vendors the WASM artifact, regenerates `src/lib/querymt/generated/types.ts`, and replaces its TypeScript reducer and conversation builder with this module. Its `history_page` mapping passes the raw older-history response.

**querymt-gpui follow-up (not planned here):** gpui depends on `querymt-agent-session-core` natively (git dependency on a pinned revision) and replaces the reducer in `src/session.rs`; diff synthesis, tool labels and notices stay in gpui.
