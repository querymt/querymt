# Proposal

## Why

Opening a session is slow, and very slow for remote sessions on a poor link. The attach path downloads a remote session's whole history more than once: once to fill the local journal, again (200 events per sequential mesh request) to build the load snapshot, and again when the client connection subscribes. Background-filled history is also pushed to connected clients as live updates. The finished snapshot is one large JSON message that includes every full tool output and argument. Large local sessions pay the same snapshot costs.

This change is the agent half of the work. The desktop half (IPC framing, windowed-load UI, on-demand older history and full tool content) is the `speed-up-session-loading` change in the `querymt-desktop` repo, which depends on this one.

## What Changes

- **Each remote event crosses the mesh once.**
  - The owner serves events in large, size-bounded chunks: one round trip for typical sessions instead of one per 200 events.
  - The background sync becomes the only download; every load, attach and subscription response is built from the local journal, in the owner's order.
  - Filled-in history is stored without being sent to clients as live updates.
  - Reopening a session fetches only the events added since the last sync, even when the attachment is reused.
  - Undo on the owner is detected and the local copy is resynced.
  - Owners that don't support chunks keep working through the existing paged request, remembered per owner.
  - If the owner is unreachable but the requested window is already local, the load succeeds and is marked as possibly out of date; otherwise it fails with a retryable error.
- **Parallel load requests:** runtime status, profile and config options run in parallel, and none of them can fail the load.
- **Recent turns first, for local and remote sessions alike:**
  - A load that asks for a window returns the most recent 20 runs, capped at 1 MiB, plus whole-session state: token totals, cost accounting, delegation state, current model, pending inputs and child-session links.
  - Older history is served by a new `querymt/session/history` request.
  - Loads without a window keep returning the full snapshot.
- **Large tool content is sent truncated:**
  - Tool results and string tool arguments over 32 KiB are sent as a prefix plus a marker.
  - The full content is served by a new `querymt/session/toolContent` request.
  - Stored history is never truncated.

Nothing in this list is a breaking change: every protocol addition is advertised by a capability flag or negotiated, and older peers keep the current behaviour.

## Capabilities

### New Capabilities
- `session-history-loading`: how much of a session's history a load returns (a recent window of runs with whole-session state), how older history and full tool content are served on demand, and the requirement that local and remote sessions behave identically.
- `remote-session-sync`: how a remote session's events are transferred to the attaching agent: once, in bounded chunks, in the owner's order, without live re-delivery, incrementally on reopen, with rewrite detection, concurrent load-time requests and a fallback for older owners.

### Modified Capabilities
- `acp-session-updates`: load snapshots under a load window; replay source for remote sessions.
- `llm-cost-accounting`: idempotency across chunked, out-of-order and resynced remote history.

Reviewed with no requirement change: `remote-stream-relay` (provider stream relay, not session event history) and `session-interactions` (interaction lifecycle is untouched by history transfer).

## Impact

- **Agent (`crates/agent`, crate `querymt-agent`):**
  - `agent/messages.rs`, `agent/event_chunk.rs`, `agent/remote/remote_actor_impl.rs`, `agent/session_actor.rs`, `agent/undo.rs`: chunked event-stream message and history epoch.
  - `agent/remote/actor_ref.rs`, `agent/remote/event_backfill.rs`, `event_sink.rs`, `session/sqlite_storage/event_journal.rs`: per-session sync state, persist-only writes, owner-ordered reads, tail-first sync, paged journal reads.
  - `agent/handle/model_registry.rs`, `agent/handle/core.rs`, `agent/handle/remote_connect.rs`, `control/remote.rs`, `acp/shared.rs`, `acp/stdio.rs`: journal-backed snapshot, attach subscription and replay; parallel load requests; usage seeding from the summary.
  - `session/load_snapshot.rs`: windowed snapshot, whole-session summary, truncation.
  - `control/capabilities.rs`: new feature flags. New extension methods `querymt/session/history` and `querymt/session/toolContent`.
  - `rmp-serde` becomes a direct dependency of `querymt-agent`.
- **Protocol:**
  - Additive `_meta.querymt.loadWindow` on load and attach requests.
  - New snapshot fields `history`, `summary` and `truncated`.
  - Two new extension methods and errors `remote_transfer_failed`, `history_changed`, `session_not_found`, `tool_call_not_found`.
  - Capability flags `session_history_window` and `session_tool_content`.
  - A new mesh message, `GetEventStreamChunk`.
  - The desktop regenerates its TypeScript types from the agent's typeshare output (a desktop task).
- **Other clients:** the mobile FFI and stdio/non-snapshot clients do not send `loadWindow` and keep today's behaviour.
- **Rollout:** ship the agent first, then the desktop. Each half works against an older counterpart through the capability checks.
