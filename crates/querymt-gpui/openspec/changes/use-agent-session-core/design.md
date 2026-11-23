# Design

## Context

- **Today.** `Store.session: Option<SessionView>` holds the open session. `SessionView` (`src/session.rs`) is both the reducer and the read model: `apply(update)` for `session/update`, `apply_load_snapshot`, `apply_history_events`/`prepend_events`, `apply_runtime`, `apply_child_links`, `push_local_user` with text-based echo matching (`echo_pending`), `fold_compactions`, `todo_plan`. The UI reads `items: Vec<Item>` (`key`, `rev`, `ItemKind::{User, Agent, Work, Notice, Compaction}`) plus header fields; `ui/session_view.rs` builds row signatures from `Item.key`, `Item.rev` and the `Arc<ToolCall>` pointer, and `turn_ends`/`turn_text`/undo/fork/`turn_range`/`turn_diffs` find turns by scanning for `ItemKind::User`.
- **Routing.** `Store::on_incoming` (`src/store.rs`) dispatches `session/update`, `delegationUpdate`, `runtimeChanged`, `inputState` to the view; `open_session`/load, `load_older_history`, `set_full_tool_content`, undo, config set and `send_prompt`/`finish_prompt` call view methods directly. Steer/queue inputs get a local id `gpui-<ms>`, are re-keyed to the agent id from the submit response, and early notifications wait in `input_notes`.
- **The reducer.** `querymt_agent_session_core::SessionCore`: `apply(&[SessionInput]) -> ChangeReport { added, updated, removed, header_changed, … }`; borrowed reads `turn_order()`, `turn_ref(id) -> TurnRef { id, revision, entries }`, `entry(id)`; `header()` with run state, context, tokens, cost, plan, undo stack, pending inputs, subagents, cursors, history paging. Entries are `UserMessage`, `AssistantMessage`, `Reasoning`, `ToolCall`, `Compaction`, ids from agent ids (`user-<messageId>`, `tool-<toolCallId>`, …); turns are runs (`run-<runId>`) and client-local `pending-<clientPromptId>`. It is `Send + 'static`, has no async runtime, builds for wasm.
- **Constraints.** The crate is one binary (tests live in the bin target; CI runs `cargo llvm-cov nextest --bins` and clippy `-D warnings`). The web build pins `wasm-bindgen = "=0.2.130"`. Nix builds with `buildRustPackage` and `cargoLock.lockFile`.
- **Prerequisites** (QueryMT, see proposal): `header.accounting`, `_meta.querymt.compaction` token numbers, `subagent_update` from the agent.

## Goals / Non-Goals

**Goals:**
- `SessionCore` is the only writer of conversation state; `SessionView` is a read model rebuilt incrementally from change reports.
- The UI files keep their current types, so the switch does not touch rendering beyond turn helpers.
- A deterministic differential test gates the switch.

**Non-Goals:**
- Changing row rendering, the composer, the sidebar session list or the session-list `phase` (still from `runtimeChanged`).
- Session cursors and fork-at-cursor (follow-up once the agent announces cursors).
- Using the reducer for sessions other than the open one (sidebar previews, delegate panels).
- Showing ACP `notice` updates (gpui does not advertise `session.notices`; unchanged).

## Decisions

### D1. Projection, not a new UI model
`SessionView` keeps its public fields and `items: Vec<Item>`, but becomes a projection: `SessionView { core: SessionCore, overlay: Overlay, items, …header fields }`, with all mutation going through one `SessionView::feed(&[SessionInput]) -> ChangeReport` that applies the batch and re-projects.
- *Alternative:* rewrite `ui/session_view.rs` to read the core directly. Rejected for this change: 6,368 lines of row code depend on `Item`, and the differential test needs the old shape to compare against.

### D2. Incremental projection with caches
- A turn projects to a contiguous slice of `items`. The projection keeps `turn id → (revision, Range<usize>)` and replaces only the slices of turns in `added/updated/removed`; on `removed` + `added` after a load, turns whose revision and content are unchanged are not reported by the core, so their items are kept.
- Within a turn: user → `ItemKind::User` (blocks via `prompt_parts`/`UserAttachment::from_block`; a `pending-*` turn is `local: true`, a failed one also gets a notice row); assistant → `ItemKind::Agent`; consecutive reasoning and tool entries → one `ItemKind::Work`, keyed by its first entry id; compaction → `ItemKind::Compaction`.
- `Item.key` is the core entry id (work groups: the first member's id; thoughts keep `"{message}/{part}"` as expand key). `Item.rev` is the entry revision (work groups: the max member revision), which only grows.
- `Arc<ToolCall>` is cached per `(tool entry id, revision)`: the same revision returns the same `Arc`, so row signatures that compare pointers stay equal. `ToolCall` is built from `ToolCallEntry`: title, kind, status, raw input; output text from content and raw output; `truncated_result/args` from `*_full_bytes`; `child_session` from `child_session_id`; `diffs`, `stats`, `facts` computed once per revision by the existing `synthesized_diffs`/`OutputFacts`; `fetch` from the overlay.
- Header fields map: `run_state`/`active_run_id` → `phase`, `prompting`, `running()`, `run_id`; `plan` → `plan`; `context` → `usage`; `tokens` → `breakdown`; `accounting` → `accounting` (parsed with the existing `usage::Accounting` from the core's snapshot); `title`, `current_mode`, `config_options` → `title`, `mode`, `options`; `history` → `history_more`, `history_before`; `undo_stack` → `undo_stack`; `pending_inputs` merged with the overlay → `pending_inputs`.
- *Alternative:* rebuild all items on each change. Rejected: the per-chunk cost would grow with session length, which the row cache exists to avoid.

### D3. Overlay for client-only state
`Overlay` holds what the reducer deliberately leaves out: notices (`text`, `error`, anchored to the turn id that was last when pushed, rendered after that turn's slice), `loading`, `error`/`error_details`, `history_loading`/`history_error`, `modes`, `access`, `after_load_notice`, `pending_options`, `undo_pending`, tool `ContentFetch` by tool call id, and unconfirmed steer/queue inputs. Active time is not in the overlay: it is the core's `header.active_time` (`total_ms` plus `now - active_since_ms` while a run is active).
- **Unconfirmed inputs** keep today's flow: local `gpui-<ms>` id, re-keyed from the submit response, early states buffered in `input_notes`. An overlay input is dropped once the core's `pending_inputs` lists its agent id or its terminal state is known; the projection shows core inputs first, then overlay-only ones.

### D4. Input mapping
| gpui source | `SessionInput` |
|---|---|
| session opened | `SessionCore::new(session_id)` immediately, so replayed updates before the response land in it |
| load response | `SnapshotLoaded { response }` (whole result) |
| `session/update` | `AcpNotification { notification: params, replayed: params._meta.querymtReplay, at_ms }` |
| `querymt/session/stateUpdate` | `StateUpdate { notification: params, at_ms }` |
| `runtimeChanged` | `RuntimeObserved { status: params.runtime }` (sidebar phase unchanged) |
| `inputState` | `InputState { notification }` (also updates overlay inputs) |
| older-history response | `HistoryPage { response }` (raw, including `truncated`) |
| `toolContent` response | `ToolContent { tool_call_id, part, content }` |
| undo stack / undo / redo responses | `UndoStack { frames }` |
| set-option response | `ConfigOptions { options }` |
| `send_prompt` (idle) | `PromptOptimistic { client_prompt_id, blocks, at_ms }`; the request carries `_meta.querymt.client_prompt_id` |
| prompt response | `PromptCompleted { client_prompt_id, message_id: _meta.querymt.messageId, stop_reason, at_ms }`, or `PromptFailed { client_prompt_id, message }` |

`client_prompt_id` is `gpui-<ms>-<n>` (time plus a per-store counter), so two prompts in the same millisecond differ; the same helper replaces the bare `gpui-<ms>` input ids. The `delegationUpdate` branch and `apply_child_links` are removed; child links come from `subagent_update` through the core.

### D5. Capabilities
`initialize` advertises `plan: {}` (plan operations: the todo list arrives as `plan_update` with id `querymt-todos`, as the conformance tests assume) and `subagents: {}` next to `session.compaction` and the load snapshot epoch. `session.notices` and `session.cursors` stay off (non-goals).

### D6. Batching and background hydration
- Inputs are queued in `Store` and flushed once per frame (`cx.on_next_frame`) with one `feed`, so a burst of chunks yields one change report and one `cx.notify()`. Prompt and history actions flush immediately.
- A load response larger than a threshold (256 KiB serialized) is hydrated on `cx.background_executor()`: the `SessionCore` moves to the task, live inputs arriving meanwhile go to a pending queue, and on return the queue is fed in order before the next frame. On native only; the web build hydrates inline (single-threaded).

### D7. Turn helpers on runs
`turn_start`, `turn_key`, `turn_range`, `turn_text`, `turn_ends` (UI) and `fork_point`/`fork_target`/`turn_diffs` use the projection's turn slices instead of scanning for `ItemKind::User`. Undo turns are the run turns with an agent-confirmed user message; the undo target is the turn's first user message id (the agent's undo frames are keyed by prompt message ids, and undoing to the first prompt rolls back the steering messages after it). Fork uses the turn's last agent-identified reply or reasoning, else its first prompt.

### D8. Differential test
> After task 7.1 the old reducer and this test are gone: `session::golden` keeps the ported gpui scenarios as golden files and checks QueryMT's `testing::golden_cases()` (called at test time, not copied) for core errors, empty turns and incremental-vs-rebuilt rows. The fixture copy and its script were dropped.

A test module `session::differential` (bin target) feeds the same inputs to the old `SessionView` reducer (kept behind `#[cfg(test)]` until task 7) and to the new projection, and compares `(key class, kind, text, tool status/output, compaction status/summary, plan)` per item in order. Inputs: QueryMT's golden traces (copied into `tests/fixtures/session-core/` by `scripts/sync-session-core-fixtures.sh` from the pinned rev) and the existing gpui session test fixtures. Allowed differences are listed in one table in the test (run grouping of steering, item keys from agent ids, plan from `plan_update`, compaction position from the agent, child links from subagents) and each must be matched by a rule, not by skipping a fixture.

### D9. Dependency pinning
`querymt-agent-session-core = { git = "https://github.com/querymt/querymt", rev = "<sha>" }`. Local development uses an uncommitted `[patch."https://github.com/querymt/querymt"]` to `../querymt/crates/agent-session-core`. `flake.nix` gets `cargoLock.outputHashes."querymt-agent-session-core-<version>" = "<hash>"`; a rev bump updates `Cargo.lock`, the hash and the dev-dependency's rev together.

## Risks / Trade-offs

- [The pinned rev lacks a prerequisite] → Task 1 checks each one against the rev before the dependency lands; the change does not start wiring until they pass.
- [Agents without subagent support lose delegate child links] → Accepted (decision A in the proposal); noted in the release notes; such agents still show the delegate tool call.
- [Agents without run ids or state updates] → The reducer falls back to user-message turns and inferred run state; the UI keeps working, ids may change on reload there.
- [Row-cache churn if projection keys differ from today's] → Expand state is keyed by item key; keys change once at the switch (old sessions reopen with new keys), not afterwards. Accepted.
- [Overlay notices drift from turn positions after an older page is prepended] → Notices anchor to turn ids, not indexes.
- [Background hydration reorders inputs] → Single pending queue fed before any new input; covered by a test.
- [Undo granularity changes for steered runs] → Undo now rolls back the whole run; spec scenario and test cover it.

## Migration Plan

1. Prerequisites verified on the pinned rev (task group 1).
2. Dependency and fixtures land; nothing wired.
3. Projection, overlay and differential test land; still not wired.
4. Store wiring switches input by input behind the projection; the old reducer stays for the differential test.
5. Old reducer and its tests removed.
- Rollback: revert the wiring commits; the dependency can stay unused.

## Open Questions

- The 256 KiB background-hydration threshold is a starting point; adjust after measuring `session.apply_ms` on real sessions.
