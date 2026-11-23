# Design

## Context

See proposal.md (Why). Current state that shapes the approach:

- **Where the wire types live today:**
  - `crates/agent-core/src/events.rs` `AgentEvent`, `AgentEventKind` (103 variants), `EventEnvelope`, `DurableEvent`. Their non-test dependencies are:
    - in agent-core: `protocol` (`ContentBlock`, `StopReason`, `RequestPermissionRequest`), `accounting` (`AttemptStart`, `AttemptSettlement`, `RequestAccounting`, `AccountingReason`), `agent_mode::AgentMode`, `dedup::DuplicateWarning`, `delegation_update`, `elicitation::ElicitationAction`, `hash::RapidHash`, `runtime_status`, `session::domain::*`, `session::projection::SessionWorkspaceView`;
    - in `querymt`: `Usage`, `chat::FinishReason`, `chat::Tool`, `providers::{BillingClass, IncompleteReason, ModelPricing, UsageValuation}`.
  - `config::mcp::McpServerConfig` is used only by `impl From<McpServerConfig> for McpServerInfo`.
  - `crates/agent-core/src/session/projection.rs` `AuditView` sits next to the async storage traits `EventJournal` and `ViewStore` (`async_trait`). `crates/agent-core/src/stream_cursor.rs` `StreamCursor`.
  - `crates/agent-services/src/accounting/snapshot.rs` `AccountingSnapshot` and its scope, budget and cycle parts. `from_reducer` and `from_events` call `AccountingReducer`.
  - `crates/agent-services/src/session/history_window.rs` `LoadWindow`, `HistoryInfo`, `ToolContentPart`, `TruncatedEntry`, `SessionHistorySummary` and its parts. The windowing functions and `WindowRead` stay.
  - `crates/agent-runtime/src/session/load_snapshot.rs` `SessionLoadSnapshot`, `UserPromptRecord`. `crates/agent-runtime/src/control/notifications.rs` `SessionInputStateNotification`, `SessionInputDelivery`, `SessionInputState`.
- **What blocks a browser build:**
  - agent-core depends on `tokio`, `async-trait`, `whatlang`, `rust-stemmers`, `mockall`, and on `agent-client-protocol` 5.4, which pulls in `async-io`, `async-process`, `blocking` and `rustix`.
  - `querymt` with `default-features = false` still depends on `which`, but only `mcp/config.rs` (behind `mcp`) uses it.
- **ACP schema.** `agent-client-protocol` 5.4 re-exports `agent-client-protocol-schema` 1.19 (`agent_client_protocol::v1::schema::*`). The schema crate needs only serde, serde_json, serde_with, strum, derive_more and anyhow. Its v2 module sits behind `unstable_protocol_v2`, which QueryMT does not enable.
- **Compaction, plans, subagents and cursors are standard ACP.** The ACP v1 schema defines `CompactionCapabilities` (client capability `session.compaction`), `CompactionUpdate`, `CompactionSummaryChunk`, `CompactionId` and `CompactionStatus`; in 1.19 they are not behind a feature. Plan operations (`plan_update`, `plan_removed`), subagents (`subagent_update`, `session.subagents`) and session cursors (`cursor_update`, `session.cursors`) ship behind `unstable_plan_operations`, `unstable_subagents` and `unstable_session_cursors`, all part of `unstable`. The agent emits compactions and plans from `crates/agent-runtime/src/acp/shared.rs` `AcpLiveEventTranslator`, gated on the client capability; subagents and cursors come with `adopt-acp-subagents-cursors-usage`.
- **ACP v2 drafts used as the target shape** (not compiled in): `state_update` (foreground work `running` / `requires_action` / `idle` with a stop reason, for prompted and agent-started work alike), the `session/prompt` response returning the inserted user message's `messageId`, entities keyed by type and id with the first update fixing the position, and `session/resume` with `replayFrom`: updates are held until `replay_complete`, running work is split at a cut point, and a current `state_update` follows. The session cursors RFD keeps cursors as positions in history; a point in what a client has received is left open, so the snapshot's stream cursor is not an ACP cursor.
- **How the agent translates events.** Live: each connection's forwarder (`acp/mod.rs`, `websocket.rs`) checks session ownership when it receives an event, then `AcpLiveEventTranslator::translate_notifications`. It tracks streamed messages (a stored message whose deltas were sent is suppressed), structured prompt blocks and `plan_operations`. Replay: `replay_agent_events_materialized`. Both share the stateless projection in `querymt_agent_protocol_types::replay` (task 4.0).
- **Runs.** The agent's turn is a run (`session-history-loading` / Turn boundaries, `agent-services` `session::turns`), from `RunStarted` to `RunCompleted { stop_type }`. Neither event is translated for clients today; only input-state notifications carry a run id. The runtime phase reaches clients as `querymt/session/runtimeChanged` through a `watch` channel, which coalesces transitions (`restructure-agent-turn-state` 6.6). Applied steering emits `UserPromptBlock` and `PromptReceived` inside the running run; scheduled runs emit `RunStarted` with origin `scheduled`.
- **Load ordering.** `LocalAgentHandle::load_session` reads the snapshot (`session_load_snapshot_meta`), and only afterwards does `session/load` subscribe the connection (`attach_rpc_session_locked` → `subscribe_connection`). Events between the two are dropped by the ownership check or, if still queued, delivered on top of the snapshot. Assistant deltas are not journaled, so a connection that attaches mid-message receives only the later deltas, and its translator then suppresses the stored message. For a client without the snapshot capability, replayed `session/update` notifications (marked `_meta.querymtReplay`) come *before* the response (`notify_before_response`); a snapshot-capable client gets no replay (`prefers_snapshot_history`).
- **Older-history response.** `_querymt/session/history` returns `events`, `history`, `truncated` and, for remote sessions, `userPrompts` (`crates/agent-runtime/src/agent/handle/ext_session_history.rs` `SessionHistoryResponse`).
- **Prompt echo and QueryMT `_meta`.** The agent reads `_meta.querymt.client_prompt_id` from `session/prompt` and puts it back on the echoed `user_message_chunk` (`user_prompt_chunk` in `shared.rs`). Reasoning chunks carry `_meta.querymt.reasoning_part_id`; `usage_update` carries the token breakdown in `_meta.querymt.usage`. The v1 prompt response arrives when the run ends and carries only the stop reason (and `usage` with `adopt-acp-subagents-cursors-usage`).
- **Related changes.** `adopt-acp-subagents-cursors-usage` replaces `querymt/session/delegationUpdate` and the snapshot's `delegationUpdates` with `subagent_update` and `subagentUpdates`, and announces `cursor_update`s with ids derived from their anchors (its D1). It also adds `PromptResponse.usage`.
- **Other clients:** querymt-desktop (`session-updates.ts`, `session-snapshot.ts`, `session-conversation.ts`), querymt-mobile (`sessionUpdateReducer.ts`) and querymt-gpui (`src/session.rs`, a native Rust reducer over `serde_json::Value`). They are the behavioural reference for the reducer.
- **Workspace hack.** `.config/hakari.toml` builds `querymt-workspace-hack` (tokio full, rusqlite, libp2p, reqwest) and adds it to every agent-stack crate; `querymt` is in `[final-excludes]` because the wasm32-wasip1 plugins share it. `lint.yml` runs `cargo hakari verify`.
- **Landed changes.** `split-agent-crate` (2026-11-15): its facade paths are final. `restructure-agent-turn-state` and `unify-model-call-path` (2026-11-22): run start ordering (`RunStarted` before the prompt is stored), scheduled runs, steering stored as its own message, retries during compaction.

## Goals / Non-Goals

**Goals:**
- One reducer, written once, that builds natively and for `wasm32-unknown-unknown`, and that a Rust client outside the workspace (querymt-gpui) can depend on directly.
- The reducer reads the agent's real wire types, so a change to them breaks the reducer build, not a client at runtime.
- Updates cost time in proportion to what changed, not to the session size.

**Non-Goals:**
- Rendering (markdown, HTML, diffs). Clients keep it.
- A JS-side state copy. The state lives inside the module.
- ACP v2. The agent additions follow v2's shapes but stay v1 extensions (D8).

## Decisions

### D1. Crate graph

```mermaid
flowchart BT
  qmt["querymt (default-features = false)"]
  acps["agent-client-protocol-schema 1.19 (default-features = false, unstable)"]
  apt["querymt-agent-protocol-types"]
  asc["querymt-agent-session-core"]
  wasm["querymt-agent-session-core-wasm (cdylib, publish = false)"]
  core["querymt-agent-core"]
  svc["querymt-agent-services"]
  rt["querymt-agent-runtime"]
  apt --> qmt
  apt --> acps
  asc --> apt
  wasm --> asc
  core --> apt
  svc --> core
  rt --> svc
  rt -. "dev-dependency (conformance)" .-> asc
```

- `querymt-agent-protocol-types` depends on serde, serde_json, typeshare, time (`serde`, `formatting`, `parsing`), rapidhash, `querymt` with `default-features = false`, and `agent-client-protocol-schema`. All of them are added as workspace dependencies.
  - `agent-client-protocol-schema = { version = "1.19", default-features = false, features = ["unstable"] }` is a new workspace entry. It must be the same major/minor version that `agent-client-protocol` 5.4 re-exports, so the types are identical. A compile-time assertion (`fn same(_: querymt_agent_protocol_types::protocol::SessionNotification) -> agent_client_protocol::v1::schema::SessionNotification`) in agent-core enforces this.
  - The agent's own build still enables the schema's `schemars` feature through `agent-client-protocol`. Feature unification makes that harmless.
- `querymt-agent-session-core` depends only on protocol-types, serde and serde_json.
- **Workspace hack.** `querymt-agent-protocol-types`, `querymt-agent-session-core` and `querymt-agent-session-core-wasm` go under hakari `[final-excludes]`, like `querymt`. They still count for feature unification but never get the `querymt-workspace-hack` dependency, so their closure has no tokio or rusqlite on any target and is the same when built from another workspace.
  - Dev-dependencies, crate-local: `proptest` (new, `default-features = false, features = ["std"]`) and `criterion = "0.8"` (already in the lockfile, without `async_tokio`).
- `querymt-agent-session-core-wasm` depends on session-core and `wasm-bindgen = "=0.2.129"` (the version already locked). The pin keeps the generated JS glue in step with `wasm-bindgen-cli`.
- Provider crates and their `wasm32-wasip1` builds are untouched. The tasks check one provider build anyway, because `querymt` features change.
- *Alternative considered:* mirror types inside session-core (option A). Rejected by the user in favour of a real extraction, so drift becomes a compile error.

### D2. Extraction keeps existing paths
- **What moves.** Types move with their serde and typeshare attributes and with any inherent methods that only use other moved types (for example `LoadWindow::clamped`, `AccountingSnapshot::mark_untransferred_history`, `observe_cursor`).
- **Re-exports.** Each old module re-exports the moved items under the same name: `querymt_agent_core::events::*`, `::hash`, `::stream_cursor`, `::runtime_status`, `::delegation_update`, `::session::domain`, `projection::{AuditView, SessionWorkspaceView}`, services `accounting::snapshot::*` and `session::history_window` payload types, runtime `session::load_snapshot::*` and `control::notifications` input-state types. Callers compile unchanged.
- **Accounting totals.** `SummaryUsage` and `PriorUsage` embed `ScopeTotals`, so `ScopeTotals`, `ComponentSums` and `TokenSums` move to `protocol_types::accounting::totals` with their read-only helpers; the reducer's mutators become private functions in `agent-services` `accounting::reducer`.
- **Shared replay projection.** `protocol_types::replay` holds the stateless event-to-update rules both sides use: `replayable_updates`, `stored_summary_updates`, the todo-list plan, tool kinds, worktree announcements and `CompactionIds` (legacy compaction identity). agent-core re-exports it as `querymt_agent_core::replay`.
  - The todo plan's id (`QUERYMT_TODO_PLAN_ID`, `querymt-todos`) moves here from `acp/shared.rs`, with a helper that projects a `todowrite` as a `plan_update` under that id for plan-operations consumers. The reducer always supports plan operations, so its snapshot projection uses it, and the agent's live translator and replay use it for plan-operations clients. The legacy `plan` stays for the others.
- **Methods that need agent logic become functions in the crate that owns the logic.** Rust forbids inherent impls on foreign types:
  - `AccountingSnapshot::{from_reducer, from_events}` → `querymt_agent_services::accounting::{snapshot_from_reducer, snapshot_from_events}`.
  - `impl From<McpServerConfig> for McpServerInfo` → `McpServerConfig::server_info()`, because the orphan rule allows neither side to hold the impl.
  - Task 2.1 lists every such impl before moving anything.
- **Test samples stay where they are.** For example `accounting::samples` stays behind agent-core `test-support`.
- **`AuditView` and `SessionWorkspaceView`** move into `protocol_types::session::views`. The storage traits stay in `projection.rs`.
- **Typeshare.** `scripts/generate-types.sh` adds `crates/agent-protocol-types` and `crates/agent-session-core`. Declarations for existing types must stay identical. Ordering may change, so they are compared after sorting declarations by name.

### D3. `querymt` builds for wasm32-unknown-unknown without default features
`which` becomes `optional = true` and the `mcp` feature adds `dep:which`. Every other non-optional dependency is already wasm-safe, and `tokio`, `reqwest` and `once_cell` are already behind `cfg(not(target_arch = "wasm32"))`. Task 1.1 is a spike that confirms the whole protocol-types closure checks for `wasm32-unknown-unknown` before any type moves. If something else blocks the build, it is feature-gated the same way. If `time`'s `local-offset` feature (enabled workspace-wide) causes trouble, protocol-types does not use the workspace entry; it declares `time` with only the features it needs.

### D4. Reducer model
- **Not a wire protocol.** `SessionInput` and `ChangeReport` are an in-process API between a client's ACP transport and the reducer. They never go between agent and client. The agent and client still exchange only ACP messages, unchanged.
- **API.** `SessionCore::new(session_id)`, `apply(&mut self, batch: &[SessionInput]) -> ChangeReport`, plus read accessors `header()`, `turn_ids()`, `turn(id)`, `turns(ids)` and `debug_log()`. Every input goes through `apply`, so the module is the single writer. `SessionCore` is `Send + 'static` (no `Rc`, no `RefCell`); a compile-time assertion keeps it so.
- **Borrowed reads (native).** `turn_order() -> impl ExactSizeIterator<Item = &str>`, `turn_ref(id) -> Option<TurnRef<'_>>` (`id()`, `revision()`, `entries() -> impl Iterator<Item = &Entry>`) and `entry(id) -> Option<&Entry>`, backed by an entry-id → slot index kept with the other indexes. They copy no entry content, so reading the last turn after each update does not grow with the size of its tool results. The owned `turn_ids`/`turn`/`turns` are built on `TurnRef` and stay for serialization and the WebAssembly boundary. `TurnRef` is Rust-only (no typeshare).
  - *Alternative considered:* changed entry ids in `ChangeReport`. Not needed natively: comparing revisions over borrowed entries is O(entries in the turn) without copying, and it would change the typeshare and WebAssembly report shape.
- **`SessionInput`** (typeshare, adjacently tagged `type` + `data`):
  - `snapshot_loaded { response }`: the raw load response.
  - `acp_notification { notification, replayed, at_ms }`: any `session/update`, including `subagent_update` and `cursor_update`.
  - `state_update { notification, at_ms }`: the raw `querymt/session/stateUpdate` notification (D8).
  - `history_page { response }`: the raw `_querymt/session/history` response (`events`, `history`, `truncated`, `userPrompts`, `cursorUpdates`).
  - `prompt_optimistic { client_prompt_id, blocks, at_ms }`, `prompt_completed { client_prompt_id, message_id, stop_reason, at_ms }` (`message_id` from the response's `_meta.querymt.messageId`, optional), `prompt_failed { client_prompt_id, message }`. Clients send the same id as `_meta.querymt.client_prompt_id` on `session/prompt`; the agent echoes it on the `user_message_chunk`.
  - `runtime_observed { status }`, `input_state { notification }`, `undo_stack { frames }`, `config_options { options }`, `tool_content { tool_call_id, part, content }`.
  - There is no delegation input: subagents arrive as `acp_notification` and in the snapshot's `subagentUpdates`. The snapshot's `delegationUpdates` key, if present, is ignored.
- **ACP v2 alignment.** Each input stands in for a v2 construct, so moving to v2 changes the transport mapping and not the reducer:

  | Input / rule | v1 + QueryMT today | ACP v2 |
  |---|---|---|
  | `state_update` | `querymt/session/stateUpdate` | `state_update` |
  | run grouping | `_meta.querymt.runId` on updates | `_meta.querymt.runId` (v2 has no turn entity) |
  | `prompt_completed.message_id` | `_meta.querymt.messageId` on the response, at run end | `PromptResponse.messageId`, at insertion |
  | `snapshot_loaded` ends replay | load response | `replay_complete` |
  | entity keys | type + id, ids from the agent | same (Entity IDs) |
  | client prompt match | `_meta.querymt.client_prompt_id` echo | JSON-RPC response id + `messageId` |
  | message content | append-only chunks; the agent sends in-flight text at the cut (D8) | `agent_message` upsert replaces content |
- **Load response and replay.**
  - Replayed notifications (`_meta.querymtReplay`) that arrive before the response are applied like live ones; they are not deduplicated, because they were never applied live.
  - The load response ends replay, like v2 `replay_complete`. The agent guarantees that nothing after it repeats an event at or before the cut and that nothing is missing (D8), so the reducer does no sequence-based deduplication of live updates.
  - `snapshot_loaded` with a snapshot rebuilds the item store from the snapshot, which replaces any replayed items. Pending and failed prompt placeholders are client-local and are carried over. Cursors are discarded and rebuilt from the snapshot's `cursorUpdates`.
  - `snapshot_loaded` without a snapshot keeps the items, and takes config options and modes from the response.
  - Tools without a result in an idle snapshot keep their last status; the run state only stops naming them as the active tool. Clients decide how to present them.
- **Decoding is two-step for tolerance.**
  - Snapshot and history-page events decode first as `{ seq, timestamp, kind: Value }`, then `kind` decodes as `AgentEventKind`, one event at a time. An unknown `type` counts as unknown. Any other decode failure is an error for that event only, reported with its `seq`; the rest of the snapshot or page is applied. A failure of the envelope itself (not an object, `audit.events` not an array) is an error for the whole input.
  - ACP notifications decode the same way, always into the ACP schema's `SessionUpdate`. That includes `compaction_update` and `compaction_summary_chunk` (standard ACP, `CompactionCapabilities`) and `plan_update` / `plan_removed`. The reducer defines no local structs for any session update kind.
  - Compaction follows the ACP contract: the first `compaction_update` for a `compactionId` fixes the compaction's position in the timeline; later updates patch it in place (`summary`, `error` and `_meta`: omitted leaves the value, `null` clears it, `summary: []` clears the summary); `compaction_summary_chunk` appends in receive order and creates an in-progress compaction for a first-seen id; an unknown `status` is kept as other and is not treated as completed. A settled status (completed, failed, cancelled) is final: a later `in_progress` is ignored, but the update's other fields still patch.
  - Plans: `plan` and `plan_update` replace the plan, `plan_removed` clears it. Reasoning chunks with `_meta.querymt.reasoning_part_id` merge per part; chunks without one merge with the last part of the same message. `usage_update` `_meta.querymt.usage` gives the token breakdown; for a windowed snapshot the `summary.usage` totals win.
  - Ignored, unknown and error counts go into the change report (spec: Tolerant input handling).
- **State layout.**
  - An item store (transcript items, tool calls, compactions) keyed by stable id and ordered by history position (`seq`).
  - **A turn is a run.** Live, an update joins the turn of its `_meta.querymt.runId`; without one, the turn of the run the last `state_update` reported running; without either, the last turn. A `state_update` `running` for an unknown run opens its turn. In snapshots and pages, events between `RunStarted` and `RunCompleted` belong to that run. Fallback for agents without run ids (no `runId`, no state updates, no `RunStarted`): a turn starts at a user message or a compaction outside a turn, as before.
  - Each turn exposes ordered **entries** (user message, assistant message, reasoning part, tool call, compaction), each `{ id, revision, … }`. Entries are keyed by entity type and the agent's id: `user-<messageId>`, `assistant-<messageId>`, `reasoning-<messageId>-<partId>`, `tool-<toolCallId>`, `compaction-<compactionId>`. An assistant message and a thought with the same `messageId` are separate entries.
  - **Pending prompts** are not entries of a run. Each is a client-local turn `pending-<clientPromptId>` after the last run turn. When the agent's user message for it arrives (echo carrying the client prompt id, or `prompt_completed.message_id`), the pending turn is removed and the user message is an entry of its run. A failed prompt stays as a pending turn marked failed until the client drops it.
  - Index maps (`tool_call_id → turn`, `message_id → entry`, `run_id → turn`, `client_prompt_id → pending turn`) let one update find its turn in O(1) instead of scanning.
  - **Header** additionally holds `subagents` (upserts of `subagent_update` keyed by child session id; tool calls whose content references a child are linked to it) and `cursors` (keyed by cursor id: first anchor kept, capabilities patched, placed when the anchored entity is in the view, otherwise unplaced).
- **Cost of an update.**
  - A live update rebuilds only the turns it touches, usually the last one. Each turn and each entry carries a `revision: u64` from one view-wide counter that grows on every change. Clients can cache per turn `(id, revision)` (desktop) or per entry (gpui, whose virtual list rows are entries).
  - A history page builds only the new turns and inserts them in front; turn boundaries in pages follow the agent's turn definition (`session-history-loading` / Turn boundaries).
  - Rebuilding everything happens only on `snapshot_loaded`.
- **Turn ids** are `run-<runId>`. Without run ids they come from the opening user message (`turn-<messageId>`), or from the event's history position (`turn-<seq>`) when the message has no id. They never come from client-made ids, so a turn has the same id live and after a reload; `finish_rebuild` then reports only turns whose content changed.
- **Determinism.** No clock and no randomness. Times come from `at_ms` or event `timestamp`. Iteration uses ordered maps or vectors, never `HashMap` iteration order.
- **Replay dedup.** A bounded set of hashes of the newest 2,000 applied live notifications. Inputs with `replayed: true` are dropped when their hash is in the set. Live inputs are never deduplicated.
- **Debug log.** A ring of 1,000 entries plus a dropped count.
- **Run state.** Whether a run is active, its id, `requires_action` and the last stop reason come from `state_update`. Session updates only refine an active run (thinking, streaming, tool running, active tool). `prompt_completed` and `runtime_observed` decide only when no state update has been seen; `runtime_observed` maps every non-idle phase (`starting`, `model`, `tools`, `waiting`, `closing`, `cancel_requested`) to an active run.
- **Usage** is derived as the desktop does today. Cost comes only from `accounting` and `usage_update`.
  - Context and the token breakdown of a load come from the snapshot's `summary`, which every snapshot carries (D8). The reducer does not fold usage from events: the agent's usage fold (`SessionUsageProjection`) is stateful (context invalidated by a later model change, requests counted by position, partial totals) and tied to the accounting reducer, so copying it would bring back a second projection.
  - An `AccountingSnapshot` scope with `billing` `empty` and no priced, excluded, unresolved or incomplete attempts and no unaccounted generations sets no cost. Live, the agent sends no accounting for such a session, so a reload must not show an incomplete zero.
  - **Accounting detail.** `SessionHeader.accounting: Option<AccountingSnapshot>` holds the latest snapshot as received (load snapshot, `_meta.querymt.accounting` on `usage_update` and `session_info_update`), and `cost` is derived from it as before. A native client (gpui's cost panel) needs billing scope, reasons and budget, which `CostView` drops; keeping the agent's DTO avoids a second accounting parser in each client. An empty scope (no model call) leaves both `accounting` and `cost` empty.
  - **Stale context.** The agent treats a measurement taken before a model change as invalid for the new model (`reliable_used`). The reducer follows that rule on both paths: from a snapshot it ignores `summary.usage.context_tokens` when `context_position < summary.provider.position`; live it clears `header.context` on `_meta.querymt.contextInvalidated` (D8). A later `usage_update` sets it again.
  - *Alternative rejected:* narrowing the equivalence scenario to what a snapshot happened to carry ("option B"). That would write a gap into the spec as intended behaviour; the snapshot now carries the summary instead (D8).
- **Schema version.** `SCHEMA_VERSION: u32 = 1` is exported. It is bumped whenever an input or output shape changes in a way old clients cannot read.

### D5. WebAssembly boundary
- **Exported class.** `SessionCore` with `new(sessionId)`, `apply(batchJson: string): string`, `applyBytes(batch: Uint8Array): string`, `header(): string`, `turnIds(): string`, `turns(idsJson: string): string`, `debugLog(): string`, `free()`, plus `schemaVersion()` and `version()`.
- **JSON strings in both directions.** Clients already hold JSON text from the transport, and only changed turns cross the boundary. `serde-wasm-bindgen` is not used.
- **Build.**
  - `cargo build -p querymt-agent-session-core-wasm --release --target wasm32-unknown-unknown`, with a `[profile.wasm-release]` that inherits `release` and sets `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`.
  - Then `wasm-bindgen --target web` and `wasm-opt -Os`.
  - `scripts/session-core-wasm/build.sh` runs all three steps and writes `dist/` with `agent_session_core.js`, `agent_session_core_bg.wasm`, `agent_session_core.d.ts` and `metadata.json` (`version`, `commit`, `schema_version`).
- **Size budget.** The script fails above 600 KiB gzipped and prints the size every time.
- **Toolchain.** `rust-toolchain.toml` gains the `wasm32-unknown-unknown` target. The flake devShell gains `binaryen` and a `wasm-bindgen-cli` built at 0.2.129, because the cli and library versions must match exactly. CI installs the same pinned cli.

### D5a. Native consumers
- **Dependency.** A client outside the workspace (querymt-gpui) declares `querymt-agent-session-core` as a git dependency pinned to a `rev`. Cargo resolves the crate and its path dependencies from that checkout, including the workspace's `[workspace.dependencies]` entries. The client never depends on the wasm crate, so its own `wasm-bindgen` version does not have to match 0.2.129.
- **No workspace hack.** Because the three crates are in hakari `[final-excludes]` (D1), the client's tree has no `querymt-workspace-hack`.
- **Nix.** A client using `rustPlatform.buildRustPackage` adds an `outputHashes` entry for the git source; a rev bump updates it.
- **Threads.** `SessionCore: Send + 'static`, so a client can hydrate a large snapshot on a background executor and move the result to its UI thread.
- **Reading.** A native client uses the borrowed reads (D4). After a change report it walks each listed turn with `turn_ref`, keeps its own projection per `(entry.id, revision)` and re-projects only entries whose revision changed.
- **Out-of-workspace check.** `scripts/session-core-native/check.sh` creates a temporary crate outside the workspace with a path dependency on session-core, runs `cargo check` natively and for `wasm32-unknown-unknown`, and fails if `cargo tree` lists tokio, rusqlite or `querymt-workspace-hack`.
- *Alternative considered:* publish to crates.io. Rejected for now; protocol-types would pull `querymt` into the published set, and a git rev is enough for one consumer.

### D6. Release publishing
- `nightly.yml` and `release.yml` build the module in a new job. The job's artifact is named `qmt-agent-session-core-wasm` so the existing `qmt*-*` download pattern picks it up.
- The job uploads `agent-session-core-wasm-<version>.tar.gz` and its `.sha256`.
- `scripts/release/write-release-json.sh` gains an optional fifth argument, the archive path. It adds `session_core_wasm: { file, sha256, schema_version }` to `release.json`. The fields that querymt-desktop's `resolve-qmtcode.mjs` reads today are unchanged.

### D7. Tests without a live backend
- **Seeded trace generator.** It lives in `agent-session-core` behind a `test-support` feature and builds `AgentEvent` values from `AgentEventKind`, so a changed event shape stops it compiling. It emits lorem-ipsum sessions as the agent journals them: `RunStarted` before the prompt, prompts, reasoning parts, tool calls with large results, the todo-list tool, compactions, delegations, applied steering (with its `UserPromptBlock` and `PromptReceived`), permission waits, scheduled runs without a prompt, errors and `RunCompleted`.
- **Conformance in `querymt-agent-runtime`** (dev-dependency on session-core with `test-support`). Generated events are turned into:
  - a load snapshot, built the way gpui receives it: through the agent's snapshot builder with a load window that covers the session, so `summary` comes from the agent's summarizer and is not set by hand; plus one unwindowed build, which must give the same view;
  - the **live** stream, through `AcpLiveEventTranslator` with gpui's capabilities (plan operations, compaction, notices, subagents, cursors), with streamed deltas and structured prompt blocks generated from the stored messages, plus the state updates (D8);
  - the history replay, through `replay_agent_events_materialized`.
  
  Session-core must produce equal turns (ids included) from all three, with no comparison exceptions: plan id, compaction summary blocks, context, token breakdown and cost are compared as they are. Run state is compared between the snapshot and the live stream's end state; a replay carries no state updates. A mid-run variant loads at a cut inside a streaming message and applies the rest live. This guards drift between the agent's translation and the reducer, without a running agent.
- **Usage in generated traces.** Per model call the generator emits `ProviderChanged` (with a context limit) where the model changes, `LlmAttemptStarted`, `LlmAttemptSettled` and `LlmRequestEnd` with usage and context tokens. One request reports no usage (partial totals), one model change follows the last request (context invalidated), and one seed has no model call at all (no cost).
- **Delegations in generated traces.** The generator emits delegations as the agent journals them: `DelegationRequested`, the child's `SessionForked`, `DelegationStageChanged` (verifying), then `DelegationCompleted`, or `DelegationFailed` with the delegate tool call failed. The conformance test's gpui capabilities include `subagents`. A delegate tool call is linked to its child through `session` content on the live path and through `summary.childSessions` on a load, so the comparison covers subagents (state, stage) and tool-call child links on live, snapshot and replay; the `delegation` and `subagent` golden traces then come from the generator instead of hand-built updates.
- **Proptest** in session-core for chunking invariance, determinism and the "unchanged turns keep id and content" property.
- **Golden traces.** `crates/agent-session-core/tests/traces/*.json` holds inputs plus expected outputs, regenerated by `cargo run -p querymt-agent-session-core --example regen-traces --features test-support`. The native tests compare against them. `scripts/session-core-wasm/conformance.mjs` loads the built module in Node, applies the same traces and diffs the output byte for byte. The desktop reuses these traces.
- **Criterion benchmark** `stream_into_long_session` at 1x and 4x session size, with a second case that reads the last turn by reference after each update. It is informational; the CI gates are the change-report test and the borrowed-read test in the spec.
- **Borrowed-read test.** An integration test binary with a counting `#[global_allocator]` streams chunks into a turn holding a 400 KB tool result, reads the turn by reference after each chunk, and asserts that the bytes allocated per read stay under a small bound that does not depend on the result size.

### D8. Agent wire additions (ACP v1 extensions shaped on v2)
- **`querymt/session/stateUpdate`.** Params `{ sessionId, state, runId, stopReason? }`, `state` one of `running`, `requires_action`, `idle` (v2 `state_update` values). The type lives in protocol-types (typeshare). Emitted from the event stream, not from the coalescing runtime `watch`: `RunStarted` → `running`, `RunCompleted` → `idle` with the ACP stop reason mapped from `stop_type`/outcome, a blocking permission or elicitation wait → `requires_action`, its resolution → `running`. Delivered through the same per-connection path as `session/update`, so it is ordered with the updates of the same run. Not replayed. After a load, one current state is sent first among the released live updates (v2 current-state sync). `querymt/session/runtimeChanged` stays for the detailed phase.
- **`_meta.querymt.runId`.** The live translator stamps the run id of the session's current run on `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update` and `compaction_update`; the replay projection stamps it from the enclosing `RunStarted`. It merges into existing `_meta.querymt` objects (reasoning part id, client prompt id).
- **`_meta.querymt.messageId` on the prompt response.** The message id the run's `PromptReceived` stored. v1 answers at run end, so the echo carrying the client prompt id stays the primary link.
- **Hold and cut on `session/load`.** Order: subscribe the connection with a hold buffer → read the snapshot (cut = its stream cursor; for replay clients, the last replayed event) → answer → release the buffer, dropping events at or before the cut. Ownership is no longer the only filter for that window. Remote-hosted sessions apply the same rule to relayed events.
- **In-flight text at the cut.** Deltas are not journaled, so the agent keeps a per-session accumulator of the text and reasoning streamed for the current message since its last stored event. On release it sends that text as one chunk (message id, reasoning part id) and marks the message streamed in the connection's translator, so the stored message stays suppressed and later deltas append.
- **Context invalidation on model change.** On `ProviderChanged` the live translator sends a `session_info_update` with only `_meta.querymt.contextInvalidated: true`, the carrier accounting-only updates already use. ACP `usage_update` cannot express an unknown measurement, and the config option update is not a reliable carrier: it is sent only once mode, reasoning effort and model are all known. Not replayed: replay already drops a measurement older than the model (`materialize_replay_state`), and the snapshot rule in D4 covers loads.
- **Whole-session summary in every snapshot.** `SessionLoadSnapshot.summary` is filled for loads without a window too, from the same `summarize_history` the windowed path uses (window start: none). gpui and the desktop always request a window; this keeps clients without windowing equal to them.
- **Compaction token counts.** Compaction updates carry `_meta.querymt.compaction { tokensBefore, tokensAfter }` (key defined once in `protocol_types::replay`, merged with `runId`). `tokensBefore` is the last context measurement before the `CompactionStart` (the usage projection's `context_used`), falling back to its `token_estimate`; `tokensAfter` is `CompactionEnd.context_tokens`. The live translator and `materialize_replay_state` read the measurement they already track; session-core's snapshot projection tracks the last `LlmRequestEnd.context_tokens` while replaying events. The reducer keeps the meta on `CompactionEntry.meta` and patches it like other meta, so a later update adds `tokensAfter` without losing `tokensBefore`.
- **Live terminal compaction carries its summary.** `CompactionEnd` already holds the whole summary; the live `completed` `compaction_update` carries it as one block (as `acp-session-updates` / Successful compaction with a final summary requires), so a client that received chunks holds the same content as one that loaded the session.
- **Replay sends the todo plan once.** For plan-operations clients, the replay's history pass sends a `todowrite` as the shared `plan_update` (or skips it, since the materialized plan was already sent); it no longer sends a legacy `plan` after the materialized `plan_update` (`acp-session-updates` / Replay includes a current plan).
- **Cursors in snapshots and pages.** For `session.cursors` clients, `SessionLoadSnapshot` and `SessionHistoryResponse` gain `cursorUpdates`, built with the same cursor helper replay uses (`adopt-acp-subagents-cursors-usage` D1/D2), for the returned events only.
- *Alternative considered:* a client-side seq cursor on live updates. Rejected: v2 forbids relying on entity ids to deduplicate replay against what a client holds, and the agent already knows the cut.

### D9. Subagents and cursors from `adopt-acp-subagents-cursors-usage`
- The reducer reads only `subagent_update`, tool-call session references and `subagentUpdates`; the `delegation_update` input and `delegationUpdates` parsing are removed. Against an agent without that change, the view shows no subagents but everything else works.
- Cursors are opaque to the reducer: it never builds or parses a cursor id. It stores anchor and capabilities per id; gpui offers fork at cursors with the `fork` capability.

## Risks / Trade-offs

- **[Extraction churns the agent facade]** → `split-agent-crate` has landed; moved modules are re-exported at their facade paths, so callers compile unchanged.
- **[Workspace hack leaks into the new crates]** → hakari `[final-excludes]` plus the out-of-workspace check (D5a); `cargo hakari verify` in `lint.yml` catches config drift.
- **[Git-dependency consumers lag]** → gpui pins a rev and moves it deliberately; `SCHEMA_VERSION` signals breaking view changes.
- **[Entry revisions add bookkeeping cost]** → One counter per view and in-place updates; the criterion bench covers it.
- **[ACP schema version skew]** → The compile-time identity assertion (D1) fails the build if `agent-client-protocol` and the direct schema dependency diverge.
- **[WASM size]** → The ACP schema and serde_json dominate. Mitigated by the size budget, `opt-level = "s"` and `wasm-opt`. If the budget fails, decode ACP updates through narrower local structs.
- **[Reducer behaviour differs from today's desktop]** → The desktop change runs a differential test against its current TS path on the same traces before switching. Intentional differences are listed there.
- **[Hidden non-wasm dependency in the extracted closure]** → Spike task 1.1 runs first, and a CI step `cargo check -p querymt-agent-session-core --target wasm32-unknown-unknown` keeps it that way.
- **[Ordering with `adopt-acp-subagents-cursors-usage`]** → The reducer reads subagents and cursors only; the gpui pin requires that change's additive subagent tasks (1.1, 4.1, 4.2, 4.5) on the agent, not its cursors. Tasks 5.4 and 5.5 can land before it, tested with fixture updates, since the shapes come from the ACP schema. The cursor part of 4.5 waits for its cursor helper (its task 3.1).
- **[Hold buffer grows during a slow load]** → The buffer holds only one session's events for the duration of one load; it is bounded by the existing per-connection channel limit, and an overflow fails the load instead of dropping events.
- **[Agents without the additions]** → The reducer keeps the fallbacks (user-message turns, inferred run state, prompt completion); views are then as good as before but ids may differ after a reload.
- **[Desktop and mobile see behaviour changes]** → Additive `_meta` and an extension notification; replayed compactions move to their place and loads stop losing or repeating events. Listed in the CHANGELOG.

## Migration Plan

1. Land protocol-types with re-exports. This is a pure move; typeshare declarations stay identical.
2. Land the agent wire additions (D8) and the reducer's run, identity, subagent and cursor model, then the conformance test against the live translator.
3. Land the wasm crate, the traces and the release job. Nightly then publishes the module.
4. querymt-desktop adopts it (`adopt-agent-session-core`).
5. querymt-gpui adopts it natively (git dependency on a pinned rev) once step 2 and `adopt-acp-subagents-cursors-usage` tasks 1.1, 4.1, 4.2 and 4.5 have landed, without waiting for the release job.
6. Rollback: the desktop pins a module version and gpui pins a rev; the crates can stay in the agent unused. The wire additions are additive and can stay.

## Open Questions

- Should querymt-mobile-ffi expose session-core over FFI for querymt-ios? Deferred; nothing in this change blocks it.
- Does every permission and elicitation wait block the run in a way the agent can report as `requires_action`, or only tool approvals? Task 4.1 records what it finds.
- Should `querymt/session/runtimeChanged` be retired once clients read `stateUpdate`? Not in this change.
