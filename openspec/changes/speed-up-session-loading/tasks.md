# Tasks

Agent half of `speed-up-session-loading`. The desktop half (windowed-load UI, raw IPC, regenerated types, end-to-end measurement) is the change of the same name in the `querymt-desktop` repo. Numbering for sections 1-4 matches the original combined task list.

## 0. Baseline and coordination

- [ ] 0.1 Extend the agent load logs (`agent/load_telemetry.rs`) to record snapshot bytes, snapshot events and mesh requests/events per load and attach. Record the agent-side baseline for a local session and for a remote session with about 5,000 events on a throttled link in `baseline.md`.
- [x] 0.2 Review the specs `remote-stream-relay`, `acp-session-updates`, `llm-cost-accounting` and `session-interactions` and carry MODIFIED deltas for any requirement D2-D4 changes (this change's `specs/acp-session-updates` and `specs/llm-cost-accounting`). Verify with `openspec validate speed-up-session-loading --strict`.

## 1. Chunked event transfer

- [x] 1.1 Add `GetEventStreamChunk` / `EventStreamChunk` (with `history_epoch`) to `agent/messages.rs` and register them in `remote/remote_actor_impl.rs`. Verify with msgpack round-trip tests.
- [x] 1.2 Add the owner-side history epoch, incremented when undo cleanup deletes journal events (`agent/undo.rs`). Verify with a test that undo bumps the epoch and appends do not.
- [x] 1.3 Implement the chunk handler in `session_actor.rs`: forward and backward chunks, 5,000-event / 4 MiB limits by encoded size, at least one event, backward chunks starting at a run-based turn boundary (shared with 3.1). Verify with tests for each limit, a single oversized event and backward ordering.
- [x] 1.4 Add `get_event_stream_chunk` to `remote/actor_ref.rs`, falling back to `GetEventStreamSince` on the transport failure an unregistered message produces (pin the variant in the test), and cache "no chunk support" per owner node. Verify that an old owner returns identical events and that a second open sends no chunk request.

## 2. Single-transfer, tail-first remote sync

- [x] 2.1 Add the per-session sync state: covered `source_seq` range set and stored epoch persisted beside the remote sync cursor, plus a `watch` barrier that outlives attachments and backfill tasks. Verify with unit tests for range merging and persistence across restart.
- [x] 2.2 Add `persist_durable_from_source` (dedupe by `(source_node, source_seq)`, no fanout publish) and use it for all history chunks; keep `emit_durable_from_source` for live relay and gap repair. Verify that a connected client receives no `session/update` during a backfill and still receives relayed live events.
- [x] 2.3 Make remote-origin reads order by `source_seq` and return only covered ranges; set the snapshot event `seq` to the history position. Verify with a tail-first persisted journal that reads and snapshots are in owner order with no gaps.
- [x] 2.4 Switch `event_backfill.rs` to chunks and implement tail-first sync for first windowed opens (backward chunk, then forward fill skipping covered ranges, cursor advancing only contiguously). Verify: 3,000 events take one request; interruption between tail and fill resumes with no gaps or duplicates.
- [x] 2.5 On-demand backward fetch of uncovered ranges, reserving ranges so fill and on-demand never fetch the same range. Verify each event crosses the mesh once with concurrent fill and history requests.
- [x] 2.6 Epoch mismatch handling: delete the source's local rows and coverage, reset the cursor, resync before releasing the barrier. Verify that turns undone on the owner disappear from the next load on another node.
- [x] 2.7 Reused attachments: a load triggers an incremental chunk and waits on the sync state. Verify: reopen after 30 new events transfers exactly 30 (with and without attachment reuse); reopen with no changes transfers none.

## 3. Windowed snapshot builder

- [x] 3.1 Add the shared run-based turn function (`RunStarted`-anchored, legacy fallback for history without runs). Verify with tests for steering inside a run, queued input, prompt blocks before `PromptReceived`, and legacy sessions.
- [x] 3.2 Add a paged read to every `EventJournal` implementation. Verify with tests that paging forward and backward returns each event once.
- [x] 3.3 Add `LoadWindow` to `session/load_snapshot.rs`: backward paged walk, 20 turns / 1 MiB of JSON after truncation, at least one complete turn, `history { complete, before, stale? }` with an epoch-bearing token. Verify with long, short and single-oversized-turn tests.
- [x] 3.4 Add the streaming whole-session pass: accounting, delegation and `summary { usage, provider, pendingInputs, childSessions }`. Verify that windowed and full loads report identical totals, model (including a model chosen before the window), pending inputs and child links.
- [x] 3.5 Seed the live usage projection from `summary.usage` in `seed_history_from_meta`. Verify that a request after a windowed load reports whole-session totals.
- [x] 3.6 Truncate `ToolCallEnd.result` and string tool-call arguments above 32 KiB with UTF-8-safe prefixes and `truncated` entries (`part`, `path`, `full_bytes`), leaving the journal untouched. Verify with 500 KiB result, 5 MB argument, 32 KiB boundary, multi-byte boundary and unchanged-journal tests.
- [x] 3.7 Verify that a load without a window produces exactly the previous snapshot (existing `load_snapshot` and `acp::shared` snapshot tests unchanged) and that the mobile FFI tests pass unchanged.
- [x] 3.8 Add a 50k-event benchmark/test with time bounds for the window build and the `toolContent` lookup. Verify it passes in `cargo test -p querymt-agent`.

## 4. One load path for local and remote

- [x] 4.1 Parse `_meta.querymt.loadWindow` (clamped) on `session/load` and on `remote/attachSession` (extend `AttachRemoteSessionRequest`). Verify with parsing tests including out-of-range values.
- [x] 4.2 Replace `build_remote_attach_snapshot`: await the barrier within the 60 s transfer budget, then call `load_session_snapshot`; return `remote_transfer_failed` (retryable) on failure, or a stale snapshot when the owner is unreachable and the window is covered. Verify: remote snapshot equals local for identical events; unreachable owner without local window returns the error and no partial history; with local window returns `stale: true`.
- [x] 4.3 Make `restore_session_history` and `replay_loaded_session` read the local journal for remote session refs. Verify with a request-counting owner that attach plus connection subscription transfers each event once, and that non-snapshot clients receive the same replay as before.
- [x] 4.4 Run snapshot, `observe_session_runtimes`, `remote_session_profile` and `remote_session_config_options` concurrently in `core.rs` and `control/remote.rs`; runtime, profile and config failures don't fail the load. Verify: a 3 s profile delay gives a ~3 s load; config failure and runtime-status failure still load.
- [x] 4.5 Add `querymt/session/history` and `querymt/session/toolContent` (journal-backed, local and remote) with `session_history_window` / `session_tool_content` flags and errors `history_changed`, `session_not_found`, `tool_call_not_found`. Verify: paging to the start returns every event exactly once in order; an undo invalidates an old token; capability assertions in `handle/tests/capabilities.rs`.
- [x] 4.6 Generated types should be updated in the desktop repo: this change only exports the new request, snapshot and error types through typeshare (regenerating and checking `types.ts` is desktop task 4.6). Verify that the typeshare output contains `SessionHistoryRequest`, `SessionHistoryResponse`, `SessionHistorySummary`, `TruncatedEntry` and the new capability flags.

## 5. Integration and verification

- [ ] 5.1 Run the agent side of the mixed-version matrix: new agent with old owner, old agent with new owner, stdio/non-snapshot client, mobile client, and an old agent rolled back onto a journal with uncovered gaps. Verify that load, attach and history succeed in each, and record results in `baseline.md`. (New desktop with old agent and old desktop with new agent are covered in the desktop change.)
- [ ] 5.2 Re-measure the 0.1 agent-side scenarios. Verify that each remote event crosses the mesh once across load and attach, and a reopen transfers only new events; record results in `baseline.md`.
- [ ] 5.3 Add tests asserting the new `llm-cost-accounting` scenarios (chunked delivery, resync after undo).
- [ ] 5.4 Run `cargo test -p querymt-agent --features remote`.

## Workflow follow-up

- Archive this change before the desktop change (the desktop depends on the capabilities defined here), then run `openspec validate` on the archived specs.
