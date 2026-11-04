# Design

## Context

See proposal.md (Why) for the motivation and specs/ for the required behaviour. The desktop change `speed-up-session-loading` (querymt-desktop repo) holds the client side: windowed-load UI, on-demand history and full-content fetching, and the Tauri IPC work. This document covers the agent only. This section lists the current-state facts that shape the approach.

- **Remote attach downloads history more than once.**
  - `ensure_remote_session_connected` (`agent/handle/remote_connect.rs`) starts `backfill_remote_events` (`agent/remote/event_backfill.rs`). It persists remote events into the local journal from the saved remote sync cursor, 200 events per mesh request (`MAX_EVENT_STREAM_PAGE_SIZE`), as a fire-and-forget task.
  - `build_remote_attach_snapshot` (`agent/handle/model_registry.rs`) separately calls `get_event_stream()`, which pages the whole stream again through `collect_event_stream_pages` (`agent/remote/actor_ref.rs`) and keeps nothing.
  - After `attachSession`, `attach_rpc_session_locked` calls `restore_session_history` (`acp/shared.rs`) for a newly subscribed connection, which calls `get_event_stream()` a third time. `replay_loaded_session` (`acp/stdio.rs`) does the same for clients that don't use snapshots.
- **Persisting remote events publishes them live.** `emit_durable_from_source` (`event_sink.rs`) inserts and then publishes every event to the fanout. `ConnectionDelivery::admit` (`acp/delivery.rs`) passes events older than the live position straight through, so backfilled history reaches connected clients as `session/update`s.
- **Ordering.** `AgentEvent.seq` is the local `stream_seq`, i.e. local insert order. `load_remote_session_stream` (`session/sqlite_storage/event_journal.rs`) returns every row with a source identity regardless of the sync cursor. Clients (the desktop) use `event.seq` as the transcript position.
- **Undo deletes events on the owner** (`cleanup_revert_on_prompt`, `agent/undo.rs`). Today this is invisible to other nodes only because every attach re-downloads the full stream.
- **Attachment reuse.** `connect_remote_session` returns early when an existing attachment is reused (`remote_connect.rs`) and does not start a backfill.
- **Timeouts.** The default remote request timeout is 10 s (`remote_mesh.rs`); the history reply timeout is 60 s (`REMOTE_HISTORY_REPLY_TIMEOUT`, `actor_ref.rs`).
- **Turns.** `launch_prompt` (`session_actor.rs`) emits `RunStarted` (and `QueuedInputStarted`) before the execution emits `UserPromptBlock`s and `PromptReceived`. `PromptReceived` is also emitted inside a running execution (`agent/execution/mod.rs`), so it is not a turn boundary.
- **There are two remote entry points**: `session/load` of a bookmarked session (`agent/handle/core.rs`) and `querymt/remote/attachSession` (`control/remote.rs`). Both call `build_remote_attach_snapshot`, then run `observe_session_runtimes`, `remote_session_profile` (with a 3 s timeout) and `remote_session_config_options` one after another.
- **Local loads** build `querymt/sessionLoadSnapshot.v1` through `load_session_snapshot` / `journal_backed_audit_view` (`session/load_snapshot.rs`) from a full `Vec` of events; journal reads return full vectors. The snapshot already carries a `cursor { local_seq, remote_seq_by_source }`.
- **State clients derive from snapshot events:** token totals, the model fallback, pending inputs and child links. On the agent, `seed_history_from_meta` seeds the live usage projection from the load meta (`acp/shared.rs`).
- **Other snapshot consumers:** the mobile FFI counts `snapshot.audit.events` and installs its own `AcpSessionHooks` (`querymt-mobile-ffi/src/lib.rs`).
- **Tool content.** Results live in `ToolCallEnd { tool_call_id, result: String, .. }`. Clients build file diffs from tool *arguments*; the result is only checked for a success prefix.
- **Mesh:** messages are msgpack over QUIC with a 50 MiB ceiling (`querymt-remote/src/provider_transport.rs`), registered through `remote_msg_impl!` in `agent/remote/remote_actor_impl.rs`.

## Goals / Non-Goals

**Goals:**
- One code path builds every load response (local, remote load, remote attach, attach subscription) from the local journal, so windowing and truncation are implemented once.
- Every change is additive and negotiated. Mixed agent/client versions and mixed mesh versions keep working.

**Non-Goals:**
- Compressing mesh payloads. QUIC traffic stays msgpack-only; reconsider after measuring.
- Truncating tool content inside the journal or on the mesh.
- Changing the shape of the ACP `session/update` replay for clients that don't use snapshots (only its source changes, see D2).
- Client behaviour (scroll-driven loading, full-output UI, IPC framing, WebSocket compression). These belong to the desktop change.

## Decisions

### D1. One owner-side chunk message replaces paging for sync and history
Add `GetEventStreamChunk { after_source_seq?, before_source_seq?, tail, max_events, max_bytes }`, replying `EventStreamChunk { events, latest_source_seq, history_epoch, has_more }`.
- Limits: 5,000 events / 4 MiB, measured by msgpack-encoded size, always at least one event.
- Backward chunks (`tail: true`) end at `before_source_seq` (or the tip), are returned in ascending order, and start at a turn boundary (D4) when one fits.
- `history_epoch` is an owner-side counter incremented whenever journal events are deleted (undo cleanup). See D3.
- The fallback to `GetEventStreamSince` paging triggers on the transport failure an owner returns for an unregistered message type (exact variant pinned by test 1.4). The attaching agent caches "no chunk support" per owner node for the process lifetime. Fallback replies carry no epoch (`history_epoch` is always 0), so rewrite detection is unavailable for owners without chunk support; D3 defines how those owners are kept correct.
- *Alternative considered:* raising `MAX_EVENT_STREAM_PAGE_SIZE`. Rejected: the cap is by count, not size, and it can't fetch tail-first.

### D2. The journal is the single source for every response
`build_remote_attach_snapshot`, `restore_session_history` and `replay_loaded_session` stop calling `get_event_stream()` for remote session refs; they read the local journal after the sync barrier.
1. Connect (or reuse the attachment).
2. Await the **sync barrier**: the requested window (windowed loads) or everything up to the tip observed at sync start (full loads) is covered locally.
3. Build the response with `load_session_snapshot`, exactly as for a local session.

- The barrier is a `watch` on a per-session **sync state** owned by the attaching agent, not by one attachment or one backfill task. A load on a reused attachment triggers an incremental sync chunk (`after = contiguous cursor`) and waits on the same state.
- Transfer budget: 60 s (aligned with `REMOTE_HISTORY_REPLY_TIMEOUT`), configurable. On expiry the load fails with a typed `remote_transfer_failed` error carrying `retryable: true`.
- **Owner unreachable:** if the requested window is already covered locally, the load is served from the journal with `history.stale: true` in the snapshot; otherwise it fails as above.
- **Window barrier:** the load reads the journal's recent window and checks that it lies inside one contiguous covered range that either reaches `source_seq` 1 or is not exhausted by the window. Otherwise it fetches the next older chunk below that range (at most one chunk of events per step) and re-reads, so a window never spans an uncovered gap and `history.complete` is only reported when coverage reaches the start.
- **Partial summary:** a windowed remote load answers once the window is local, while older history may still be filling. The whole-session summary then covers only the events held locally and is reported with `summary.usage.partial = true` (also set when the session has any history that is not yet local). Clients treat partial totals as approximate; `accounting` is partial in the same cases: it covers the locally held events and reports the missing history through the existing completeness metadata (`complete: false`, `completeTotalUsd: null`, reason `untransferredHistory`), so a client never shows a too-low total as complete. A later load after the fill reports exact values. *Alternatives considered:* waiting for the full fill (loses "answer when the window is local"), or an owner-side summary request (a new mesh message; revisit if partial totals prove confusing).
- *Alternative considered:* keep an in-memory snapshot fed by the same transfer. Rejected: it duplicates windowing and truncation and loses incremental reopen.

### D3. Tail-first sync with a range set, persist-only, epoch-checked
- **Coverage:** the sync state tracks covered `source_seq` ranges (a range set persisted next to the existing remote sync cursor). The cursor remains "highest contiguous `source_seq` from 1".
- **First windowed open:** request one backward chunk sized to the window, persist it, mark its range covered, release the barrier; then fill forward from the cursor, skipping covered ranges.
- **On-demand older history:** if the requested range is not covered, fetch exactly the uncovered sub-ranges with backward chunks. Fill and on-demand fetches reserve ranges in the sync state before requesting, so the same range is never transferred twice concurrently.
- **Persist-only:** historical chunks are written through a new `persist_durable_from_source` path that deduplicates by `(source_node, source_seq)` like `emit_durable_from_source` but does **not** publish to the fanout. The live relay and gap repair (`recover_source_gap`) keep publishing.
- **Ordering:** reads of remote-origin rows order by `source_seq` and return only covered ranges. The snapshot's per-event `seq` carries the **history position** (`source_seq` for remote sessions, `stream_seq` for local ones), so client ordering by `seq` is correct. The snapshot `cursor` keeps its current meaning.
- **Rewritten history:** every chunk reply carries `history_epoch`. If it differs from the stored epoch, the attaching agent deletes that source's local rows and coverage, resets the cursor to 0, and resyncs (tail-first if windowed) before releasing the barrier.
- **Owners without chunk support:** their replies cannot reveal a rewrite, so incremental transfer is not safe. Each sync of such an owner's session re-transfers the full stream through paged requests and replaces that source's local rows and coverage (the same reset as an epoch mismatch), which is today's cost and correctness. Incremental reopen, tail-first sync and undo detection by epoch apply only to chunk-capable owners.
- *Alternative considered:* forward-only sync with a barrier at the tip. Rejected: it fails "recent events first" for long sessions.

### D4. Windowing, whole-session summary and truncation in the snapshot builder
`load_session_snapshot` gains `LoadWindow { max_turns: 20, max_bytes: 1 MiB, truncate_bytes: 32 KiB }`.
- **Turns:** a turn starts at `RunStarted` (including any `QueuedInputStarted`, `UserPromptBlock` and `PromptReceived` that follow it) and runs until the next `RunStarted`. For legacy history without run events, a turn starts at a `UserPromptBlock`/`PromptReceived` that is not inside a run. The same function is used by D1 chunk boundaries.
- **Window selection:** a backward, paged journal read by history position, stopping when either limit is reached and always keeping one complete turn. Size is the JSON-encoded size of events after truncation.
- **Streaming reads:** add a paged read (`load_session_stream_page(session, before/after, limit)`) to every `EventJournal` implementation; the builder never materialises the full event vector.
- **Whole-session summary:** one streaming pass over the journal feeds the accounting reducer, `delegation_updates_from_events`, and a new summary reducer producing `summary { usage: { latestRequest, sessionTotals, partial }, provider: { provider, model, node? }, pendingInputs, childSessions }`. The agent seeds its live usage projection (`seed_history_from_meta`) from `summary.usage` when present.
- **Truncation (windowed loads and older-history responses only; a load without `loadWindow` is never truncated):** `ToolCallEnd.result` and string values in tool-call arguments (`ToolCallStart` and related events) longer than 32 KiB are replaced with a UTF-8-safe prefix, and the snapshot gains `truncated: [{ tool_call_id, part: "result" | "arguments", path?, full_bytes }]`. The event schema and journal are unchanged.
- **History position:** the snapshot gains `history: { complete, before?, stale? }`, where `before` is an opaque token encoding the position and the history epoch.
- **Older history:** `querymt/session/history { sessionId, before }` uses the same walker and truncation and returns `{ events, history, truncated }` (no summary). A token whose epoch no longer matches fails with `history_changed`.
- **Full content:** `querymt/session/toolContent { sessionId, toolCallId, part }` returns the complete result or arguments, found by a bounded scan of the session's tool events (benchmarked on 50k events; add an index only if the benchmark fails).
- **Compatibility:** without `loadWindow`, the builder returns exactly today's snapshot. The audit view's other fields (tasks, decisions, intent snapshots, progress entries) stay whole-session. The mobile FFI keeps working because it does not send `loadWindow`.
- *Alternative considered:* windowing in the client after a full load. Rejected: it saves no transfer.

### D5. Requests and capability flags
- **Client request:** `_meta.querymt.loadWindow: { maxTurns: 20, maxBytes: 1048576, truncateBytes: 32768 }` on `session/load` and `remote/attachSession` (extending `AttachRemoteSessionRequest`).
- **Agent:** clamps each value to its own maximum.
- **Capability flags:** `features.session_history_window` covers `loadWindow`, the summary and `session/history`; `features.session_tool_content` covers `querymt/session/toolContent`.
- **Errors:** `remote_transfer_failed` (retryable), `history_changed`, `session_not_found`, `tool_call_not_found`.
- **Typeshare:** the new request, snapshot and error types are exported through typeshare. The desktop regenerates `src/lib/querymt/generated/types.ts` from that output (desktop task).

## Risks / Trade-offs

- **[Sync barrier stalls a load on a slow owner]** → Waits only for the window (D3) within a 60 s budget; fails with a retryable typed error that clients can offer to retry; an already-local window is served stale when the owner is offline.
- **[Out-of-order persistence exposes gaps]** → Reads return covered ranges only, ordered by `source_seq`; the cursor never jumps a gap; tests interrupt between tail chunk and fill.
- **[Epoch resync costs a full transfer]** → Only after a history rewrite on the owner; windowed loads still resync tail-first.
- **[Window walk is expensive on very large journals]** → Paged backward reads with `LIMIT`; streaming summary pass; 50k-event benchmark in the agent tests.
- **[Truncation hides content consumers need]** → Explicit markers; full content available through `toolContent`.
- **[Mixed versions]** → All additions capability-gated; mesh fallback cached per owner. The matrix (task 5.1) covers agent↔owner, stdio/non-snapshot clients and mobile; the desktop change covers desktop↔agent.

## Migration Plan

1. Ship the agent first. Without `loadWindow` it returns full snapshots.
2. The desktop ships next and detects the capability flags.
3. **Rollback:**
   - Agent: the chunk message and new methods are additive; the barrier can be bypassed by restoring the old `build_remote_attach_snapshot`.
   - Journal: the only addition is the coverage/epoch table, which older agents ignore. An older agent reading a journal with uncovered gaps shows them until its own backfill (from the unchanged contiguous cursor) fills them; tested in 5.1.

## Open Questions

- Should full-content requests support byte ranges for multi-hundred-MB results? Deferred; can be added without changing the spec.

## Effect on existing agent specs

- D2 (journal is the single source): replay and snapshot for remote sessions read the local copy, so `acp-session-updates` `Replay semantics` gains the no-second-transfer clause.
- D3 (persist-only, epoch resync): history chunks are deduplicated by source identity and an epoch change replaces the local copy; accounting records are never deleted by undo on the owner, so a resync reintroduces them once (`llm-cost-accounting`).
- D4 (windowed snapshot): only the event list is windowed; accounting remains whole-session (`acp-session-updates`), and is marked incomplete with reason `untransferredHistory` while a remote session's older history is not yet local.
