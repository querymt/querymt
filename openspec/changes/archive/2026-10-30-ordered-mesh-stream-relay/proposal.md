# Proposal

## Why

Remote (mesh) provider streams can reach the client in a different order from the one the host sent them in, and some messages can be lost. Since 2026-10-15 the session journal holds 24 structured-mode stream accumulation failures. All 24 happened on mesh-relayed streams (codex 19, anthropic 5), and none happened on more than 14,000 direct requests. Session `01a18993-07fc-7596-89c4-e99aaadcbb66` failed its run after three attempts with `received data after completion` and finally `completed response has unfinished output indexes: [1]`.

The causes are in the relay, not in the providers or the accumulator:

- **Reordering.**
  - The host sends most relay messages without waiting for delivery (`tell().send()`). It only waits for an acknowledgement every 8 batches or every 40 ms.
  - kameo 0.22 carries each message on its own libp2p request stream. The receiving side decodes and delivers each one in its own task, so a large batch can be overtaken by a later, smaller message.
- **Drop when the client is slow.** When the client's 64-slot consumer channel is full, the router puts the message in a replay buffer. That buffer is only drained when a consumer attaches, which happens once per request. The message is lost, and later messages overtake it.
- **Early close.** After delivering a terminal, the router stops delivering for that request. A message that was sent earlier but arrives later is dropped.

The strict item-aware accumulator correctly rejects the result. Before the item-aware work, the same reordering was mostly silent (legacy accumulation tolerated it, or corrupted text without any error). `structured-anthropic-streaming` moves Anthropic onto the strict path, so Anthropic mesh sessions now hit these failures.

## What Changes

- **Relay messages that carry stream data are numbered.** The host gives every data, terminal and stream-failure message a per-request sequence number, starting at 0. Heartbeats and connection-status notices stay unnumbered.
- **The router delivers numbered messages in order.**
  - A message that arrives early is held until every earlier number has arrived.
  - Duplicates are discarded.
  - A terminal is only released after everything before it.
  - Unnumbered messages are delivered immediately, as today.
- **A gap becomes a transport failure.** If a gap stays unfilled past a bounded wait, or too many messages are held, the router ends the stream with a transport failure naming the missing number. The client then sees a retryable transport error instead of an accumulator error.
- **The consumer channel never drops or reorders.** The bounded router-to-client channel becomes unbounded. A slow client no longer loses messages or lets later ones overtake earlier ones. Memory stays bounded by one response.
- **Wire compatibility.**
  - The sequence number is an optional field with a default.
  - A host or router without this change keeps today's behaviour.
  - No item-aware contract version bump is needed.

## Capabilities

### New Capabilities

- `remote-stream-relay`: delivery guarantees for provider streams relayed between mesh peers: ordering, duplicate suppression, terminal handling, gap detection, client backpressure, and mixed-version compatibility.

### Modified Capabilities

None. `item-aware-chat-streaming` already requires remote transports to recognize the canonical terminal. Its requirements are unchanged, and this change makes them hold on reordering transports.

## Impact

- `crates/querymt-remote/src/stream_router_protocol.rs`: `RoutedStreamRelayMessage` gains `seq: Option<u64>`.
- `crates/querymt-remote/src/provider_host_actor.rs`: assigns sequence numbers when queuing messages; resends keep their number.
- `crates/querymt-remote/src/provider_stream_router.rs`: per-request reorder buffer, gap timeout, unbounded consumer channel, and removal of the "consumer full" buffering path.
- `crates/querymt-remote/src/remote_chat_provider.rs`, `mesh_client.rs`, `provider_client_runtime.rs`: consumer channel type and the `prepare_stream_router` signature.
- Tests: router reorder, duplicate and gap tests, an end-to-end shuffled-delivery test using the Anthropic structured fixture, and replacement of `consumer_backpressure_sets_disconnected_and_buffers_message`.
- Mesh rollout: both peers need this change for ordered delivery. Mixed versions keep current behaviour. Release notes should ship this together with `structured-anthropic-streaming`.
- Out of scope:
  - Resending lost messages after a gap: it can be built on the sequence numbers later.
  - Changes to the accumulator, the providers or agent retry classification.
  - The kameo transport itself.
