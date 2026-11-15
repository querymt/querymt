# Proposal

## Why

A session's events do not reach clients in the order the session produced them. Every durable event is persisted from its own spawned task (`AgentConfig::emit_event`), and those tasks race for the single SQLite connection, so publish order and the journal's `stream_seq` reflect lock order, not production order. The `event_emit` benchmark measured 40–630 out-of-order durable events per 100–1,000 emitted in bursts. Nothing ties a prompt response to the updates that precede it either. For remote-hosted sessions the response reliably arrives before the relayed updates, which `unify-session-interactions` surfaced and deliberately left to this change. Consumers also lose events silently: the WebSocket session-event forwarder stops for the rest of the connection after one broadcast lag.

## What Changes

- Each session numbers its events at the point of production (durable and ephemeral) through one ordered path, so live delivery, the journal and replay agree on one order.
- Durable persistence keeps the producer's order and stays off the emitting task's critical path; ephemeral streaming deltas are not delayed by database writes.
- The journal accepts the producer-assigned per-session sequence instead of assigning its own order. **BREAKING** (storage contract and migration; the `EventJournal` rule "producers never set `stream_seq`" changes).
- Relayed events from a remote host keep the host's per-session sequence on the attached node, so one cursor describes a session wherever it is observed.
- The `session/prompt` response is sent only after the requesting connection has delivered every session update the prompt produced, for local and remote sessions alike (event watermark). Prompt only in this change; other mutating requests can adopt it later.
- Live consumers (WebSocket, stdio, remote event forwarder) detect lag and sequence gaps and recover from the journal instead of skipping events.
- **Fix:** the WebSocket session-event forwarder no longer exits on broadcast lag (`acp/websocket.rs`, the `Err(_) => break` in `spawn_event_forwarders`); a lagging connection keeps receiving updates after recovery.
- SQLite settings for the event journal (WAL, `synchronous`) and write batching are evaluated with the `event_emit` benchmark and adopted only if they meet its thresholds.

## Capabilities

### New Capabilities
- `session-event-stream`: ordering, sequencing, delivery and recovery guarantees of a session's event stream from producer to every attached client connection, including the ordering of the prompt response relative to the updates it produced.

### Modified Capabilities
- None. `acp-session-updates` defines which notifications exist, not their ordering or delivery; its requirements are unchanged.

## Impact

- **Code:** `agent/agent_config.rs` (`emit_event`), `event_sink.rs`, `event_fanout.rs`, `events.rs` (`DurableEvent`/`EphemeralEvent` sequence fields), `session/projection.rs` and `session/sqlite_storage/event_journal.rs` (journal contract, migration), `agent/remote/event_relay.rs` and `event_forwarder.rs`, `acp/websocket.rs`, `acp/stdio.rs`, `acp/shared.rs` (prompt dispatch), `agent/session_actor.rs` (prompt reply carries the watermark).
- **Wire:** prompt replies between nodes carry the session sequence (all mesh nodes upgrade together, as in `unify-session-interactions`); the ACP `PromptResponse` gains at most a `_meta` field.
- **Storage:** journal migration for producer-assigned per-session sequences; existing rows keep their order.
- **Tests and benchmarks:** `benches/event_emit.rs` gains split, WAL and paced variants; the ignored remote ordering test from `unify-session-interactions` is enabled.
- **Depends on:** `unify-session-interactions` (sessions hold no client bridge; prompts run through plain `prompt`).
