# Design

## Context

See proposal.md (Why) for the failure evidence. The current relay path:

1. **Host** (`provider_host_actor.rs`): reads the provider stream and groups chunks into `Chunk`/`ChunkBatch` messages (up to 16 chunks or 25 ms). These go into a `buffered: VecDeque<StreamRelayMessage>` and are sent front-first as `RoutedStreamRelayMessage { request_id, message }` to the client's `ProviderStreamRouterActor`.
   - Most sends use `tell().send()`, which doesn't wait for delivery. `send_ack()` is used every 8 batches, every 40 ms, or for a terminal (`should_ack_relay_message`).
   - On a send error the host marks the receiver disconnected and later resends the same front message.
2. **Transport** (kameo 0.22): each `tell` is its own libp2p request. On the receiving side it is decoded and delivered to the router's mailbox inside a separate `JoinSet` task. So messages aren't guaranteed to reach the router in send order. The acknowledgement fires once the message is in the router's mailbox, not once the router has handled it.
3. **Router** (`provider_stream_router.rs`): `RoutedRequest::deliver_or_buffer` hands each message to the consumer with `try_send` on a bounded `mpsc` channel (capacity 64, `remote_chat_provider.rs:149`).
   - Before a consumer attaches, messages go into a replay buffer (up to 1000, oldest evicted), which is drained once in `AttachStreamConsumer`.
   - If the channel is full, the message is also put in the replay buffer, but nothing drains it again.
   - Once a terminal is delivered, `consumer_tx` is set to `None`.
4. **Client** (`RemoteProviderStreamState`, `poll_stream_message`): splits batches into chunks and passes them to the agent, which feeds them to the item-aware accumulator.

Constraints:
- Mesh peers can run different versions.
- Relay messages are encoded with `rmp_serde::to_vec_named` (map-encoded), so an optional field with `#[serde(default)]` is accepted by old and new decoders alike.
- The kameo transport is out of scope.

## Goals / Non-Goals

**Goals:**
- Fix ordering, duplicates and safe terminal handling in one place: the client-side router, keyed by a sequence number assigned by the host.
- Turn unrecoverable loss into an explicit, retryable transport failure.
- Leave providers, the accumulator, agent retry logic and the item-aware contract version untouched.

**Non-Goals:**
- Resending lost messages after a gap (no host-side retention by acknowledged position).
- Ordering across requests, or for heartbeats and connection notices.
- Replacing the kameo `tell` transport with a single ordered stream.

## Decisions

### D1. Sequence at the host, reorder at the router
- **Where:** `RoutedStreamRelayMessage` gains `#[serde(default, skip_serializing_if = "Option::is_none")] seq: Option<u64>`.
- **What gets a number:** the host assigns 0, 1, 2, … when a message enters `buffered`. That covers chunk batches, the terminal batch, the grace-expiry `TransportFailed`, and the setup-failure `ProviderError` (which is the only message for that request, so it gets 0).
- **What stays unnumbered:** heartbeats and `TransportDisconnected` / `TransportReconnected` notices are informational and not part of the data order.
- **Data structure:** `buffered` becomes `VecDeque<(Option<u64>, StreamRelayMessage)>`. The existing resend-front-on-error path then resends the same number, so a duplicate is recognisable.

Alternatives considered:
- *Always use `send_ack`.* This serializes delivery into the router mailbox at the cost of one round trip per batch. It doesn't fix the client-side drop, the duplicates, or loss detection, and it couples throughput to latency. It is kept only as an optional interim measure (see Migration Plan).
- *Reorder in the client (`RemoteProviderStreamState`).* The router already owns per-request delivery and terminal state, and the consumer-channel defect lives there too. Fixing both in one component keeps the client logic simple.
- *Number individual chunks.* Batch-level numbering is enough because a batch is applied as a whole, and it costs one integer per message.

### D2. `ReorderBuffer` as a small, pure component
Each `RoutedRequest` owns a `ReorderBuffer { next: u64, held: BTreeMap<u64, StreamRelayMessage>, stalled_since: Option<Instant> }` with `admit(seq, msg) -> Admitted::{Ready(Vec<_>), Held, Duplicate}`:
- **Duplicate:** `seq < next`, or `seq` is already held.
- **Held:** `seq > next`. Start the stall clock if it isn't running.
- **Ready:** `seq == next`. Return the message followed by every contiguous held successor. Reset the stall clock if a gap remains, clear it otherwise.

`RoutedRequest::route(seq, msg)`:
- A message with no number goes straight to the existing `deliver_or_buffer`.
- A numbered message goes through `admit`, and each `Ready` message is passed to `deliver_or_buffer` in order.
- `Duplicate` and `Held` produce debug logs and update `last_message_at`.

Because the terminal is numbered, it can only be released after everything before it. The existing rule that the router stops delivering after a terminal therefore becomes correct, and messages arriving after the terminal are discarded as before. `ReorderBuffer` has no I/O, so it can be unit-tested exhaustively.

### D3. Gap detection with a scheduled check message
- **Scheduling:** when `admit` starts or resets the stall clock, the router handler spawns a short task. The task sleeps for `gap_timeout` (default 15 s) and then sends the router a local `CheckReorderGap { request_id }` message.
- **The check:** the handler fails the request if `held` is non-empty, the request is not terminal, and `stalled_since.elapsed() >= gap_timeout`. Checks that arrive after the gap has filled find nothing held and do nothing.
- **Held-message cap:** an `admit` that pushes `held.len()` past `max_held` (default 1024) fails the request immediately.
- **How it fails:**
  - A `TransportFailed { reason: "relay sequence gap: missing seq N (held M)" }` is passed to `deliver_or_buffer`. That message is terminal and maps to `RequestPhase::Failed`.
  - The client already turns it into `LLMError::Transport`, which is retryable.
  - The held messages are discarded.
- `gap_timeout` and `max_held` are constructor parameters of `ProviderStreamRouterActor` (with defaults), so tests don't need real time.

Alternatives considered:
- *Check stalls only when the next message arrives.* A stalled stream may never receive another message: the host may be idle waiting for an acknowledgement, or may be finished.
- *Rely on the client's lease and reconnect timers.* Those only fire on a disconnect. A gap on a healthy connection would hang until the agent's own timeouts.

### D4. Unbounded consumer channel
- **What changes:** `consumer_tx` becomes `mpsc::UnboundedSender<StreamRelayMessage>`. This touches `AttachStreamConsumer`, the `prepare_stream_router` transport trait method and its mesh and test implementations, and the receiver in `remote_chat_provider.rs`.
- **What goes away:** the `TrySendError::Full` branch and the `ConsumerDisconnected` transition on backpressure.
- **What stays:** the `Closed` branch (the consumer was dropped).
- **Why it's safe:** the bounded channel never slowed the host down. Acknowledgements fire on entry into the router's mailbox, so its only effect was dropping or reordering data. Memory is bounded by one provider response plus the reorder buffer's held-message limit.
- **The replay buffer stays,** but only for messages that arrive before the consumer attaches, or after the consumer's channel has closed. Its eviction rule is unchanged.

Alternatives considered:
- *Keep the bounded channel and drain the replay buffer when space frees up.* That needs either waiting sends inside the actor, which would stall all requests on the session router, or a polling drain loop. Both add complexity for no benefit given the acknowledgement semantics.

### D5. Compatibility without a contract version bump
- **Old host to new router:** `seq` is `None`, so messages are delivered as received (today's behaviour). The client still benefits from the lossless consumer channel.
- **New host to old router:** the unknown named field is ignored, so behaviour is unchanged.
- The item-aware contract version and the `GetProviderContractInfo` negotiation are not involved, because neither side depends on the other having the change.

## Risks / Trade-offs

- [Lost messages now fail the stream after `gap_timeout` instead of failing immediately in the accumulator] → Loss is rare on healthy links. The failure is a clear, retryable transport error, and 15 s is well below the agent's stream timeouts.
- [The pre-attach replay buffer still evicts the oldest message past 1000] → Unchanged from today. The consumer attaches before the stream request is sent (`mesh_client.rs:212`), so this path only sees connection notices. Recorded as an existing limit.
- [Duplicates of an already-delivered terminal arrive after the router stopped delivering] → They are discarded as `Duplicate`, or ignored because the request is terminal. No change for the consumer.
- [Unbounded growth if a consumer never reads] → The agent always drains or drops the stream. When the stream is dropped, the channel closes and the `Closed` branch stops delivery.
- [The `seq` counter is per request and restarts at 0 for each attempt] → Every retry is a new `request_id` (`Uuid::now_v7` in `remote_chat_provider.rs:104`), so numbers never mix across attempts.

## Migration Plan

1. Ship the router and host changes together. Each half is safe alone (see D5).
2. Release notes: mesh peers need this change on both sides for ordered delivery. Ship it with or before `structured-anthropic-streaming`, which moves Anthropic onto the strict accumulator.
3. Optional interim on hosts that can't upgrade both sides yet: make `should_ack_relay_message` return `true` for data messages, which serializes mailbox entry. Revert once both peers have sequencing.
4. Rollback: revert the change. Wire compatibility means old and new peers keep interoperating.
