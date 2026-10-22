# QueryMT Agent - Session Event Ordering

A session's events reach clients in the order the session produced them, and a `session/prompt` response is sent only after the requesting connection has delivered the updates the prompt produced. This page describes the guarantees, how they work, and what to do when upgrading.

## Guarantees

- **Production-order delivery.** Every attached connection (WebSocket, stdio, relayed remote sessions) receives a session's durable and ephemeral events in the order the session produced them, including events produced concurrently within one session.
- **Replay matches live order.** `session/load` replays durable events in the same order they were delivered live.
- **Prompt response follows its updates.** For local and remote-hosted sessions, the `session/prompt` response is sent after every `session/update` the prompt produced on the requesting connection. A slow second connection never delays the response to the requesting one.
- **Bounded wait.** A response is never withheld indefinitely. If delivery is lost (relay disconnect, connection close, lag that cannot be fully recovered, or the 30 second safety limit) the response is sent and a warning is logged that ordering could not be guaranteed.
- **Lossless durable delivery.** A connection that falls behind the live buffer, or sees a gap in the sequence, recovers the missed durable events from the journal and keeps receiving later events. For remote-hosted sessions, the host's forwarder also tells the attached node when it fell behind, so a loss at the end of a burst is recovered without waiting for a later event. Ephemeral streaming deltas missed while lagging are not recoverable; the recovered durable event (for example the stored assistant message) supersedes them.

Ordering across different sessions (including a parent and its delegated child) is not guaranteed.

## `session_seq`

Each session numbers its events at the point of production with a strictly increasing `session_seq`. Durable and ephemeral events share one counter, so streaming deltas are ordered relative to durable events without waiting for storage.

| Field | Meaning |
|---|---|
| `session_seq` | Per-session production order, assigned by the producer. Identifies an event in the journal, in live delivery and on every node that relays the session. `0` means unsequenced (older payloads and connection annotations). |
| `stream_seq` | Insert-order cursor of the journal, used by global-stream consumers and by remote dedup and backfill. |

Relayed events keep the host's `session_seq` on the attached node, so one cursor describes a session wherever it is observed.

## How it works

1. `SessionEventStream` (one per session) assigns `session_seq` synchronously when an event is emitted.
2. Durable events go through one ordered writer per session, persisted in batches (up to 64 events per transaction) and published only after they are persisted.
3. Ephemeral events publish immediately, unless an earlier durable event is still being persisted; they wait only for events with a lower `session_seq`, never for later ones.
4. Each connection tracks, per session, the highest `session_seq` it has delivered. The prompt reply carries `event_tip` (the last `session_seq` of the run, in `_meta.querymt.eventTip` between nodes). The transport holds the response until the connection has delivered `event_tip`, then strips the internal field before answering the client. A reply without a tip (an older node) is sent immediately.

## Storage

- The event journal gains `session_seq` and a unique index on `(session_id, session_seq)` that covers locally produced rows only. Relayed rows keep the host's numbers, which are a separate sequence deduplicated by source identity, so they never collide with local numbering. Migration `0024_event_journal_session_seq` backfills locally produced rows from their existing order. Relayed legacy rows stay unnumbered, and replay falls back to the previous order for them.
- Pages served to other nodes (backfill and gap recovery) are read in raw journal insert order (`stream_seq`), which the receiving node uses as its cursor.
- File databases now run in WAL mode with `synchronous=NORMAL`. This keeps streaming latency low while batching durable writes. After a power loss the most recent commits can be lost, but the database cannot be corrupted. In-memory databases are unchanged.

## Upgrade notes

- **All mesh nodes must be upgraded together.** The prompt reply and relayed events carry `session_seq` and `event_tip`. A node that does not send `event_tip` is treated as "no watermark" (the response is sent immediately), which is the only compatibility allowance; ordering across mixed versions is not guaranteed.
- **Downgrade requires restoring a backup.** An older binary ignores `session_seq` and assigns journal order itself, so roll back by restoring the pre-migration database together with the previous binary.
- The ACP `PromptResponse` is unchanged for clients: the internal `event_tip` is removed before the response is sent.
