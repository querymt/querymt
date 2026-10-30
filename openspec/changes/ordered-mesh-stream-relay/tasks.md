# Tasks

## 1. Reproduce the failure first

- [x] 1.1 Add a router test helper in `crates/querymt-remote/src/provider_stream_router.rs` tests that loads `crates/providers/anthropic/tests/fixtures/structured_tool_turn.json`, groups it into 16-chunk `ChunkBatch` messages like the host, delivers them to a router with an attached consumer, and accumulates the consumer output with `ChatStreamAccumulator`. Verify: in-order delivery completes and matches direct accumulation of the fixture.
- [x] 1.2 Add failing tests on the current code: (a) two adjacent batches swapped, and (b) the terminal batch delivered before the batch before it. Verify: both fail today with `structured item ... received data after completion`, a missing output index, or `unfinished output indexes`, matching session `01a18993`.

## 2. Wire format

- [x] 2.1 Add `#[serde(default, skip_serializing_if = "Option::is_none")] pub seq: Option<u64>` to `RoutedStreamRelayMessage` (`stream_router_protocol.rs`), and update every constructor (`provider_host_actor.rs`, tests). Verify: `cargo check -p querymt-remote -p querymt-agent --features remote`.
- [x] 2.2 Add `rmp_serde::to_vec_named` round-trip tests between the old shape (a local struct without `seq`) and the new one, in both directions. Verify: an old payload decodes with `seq == None`, and a new payload decodes into the old shape.

## 3. Router ordering

- [x] 3.1 Implement `ReorderBuffer` (`next`, `held: BTreeMap`, `stalled_since`, `admit` returning `Ready`/`Held`/`Duplicate`) in `provider_stream_router.rs`, as in design D2. Verify with unit tests: in-order release, out-of-order hold and release with contiguous successors, duplicates (already delivered and already held), stall clock started on a hold, reset on progress while a gap remains, cleared when the gap closes.
- [x] 3.2 Add `RoutedRequest::route(seq, msg)`: unnumbered messages go straight to `deliver_or_buffer`; numbered ones go through `admit`, and `Ready` messages are delivered in order. Duplicates and held messages update `last_message_at` and are logged at debug level. Wire it into `Message<RoutedStreamRelayMessage>::handle`. Verify: the tests from 1.2 now pass.
- [x] 3.3 Add a seeded shuffle test (no new dependency: a simple LCG or `rand` if it is already a dev-dependency) over at least 200 seeds, with random duplicate injection, using the fixture from 1.1. Verify: every seed completes with output identical to direct accumulation.
- [x] 3.4 Add a heartbeat-during-gap test. Verify: the heartbeat reaches the consumer immediately, and held data is released only when the gap fills.

## 4. Gap detection

- [x] 4.1 Add `gap_timeout` (default 15 s) and `max_held` (default 1024) to `ProviderStreamRouterActor::new`, keeping the existing callers working with defaults. Verify: `cargo check` and the existing router tests.
- [x] 4.2 Add a local `CheckReorderGap { request_id }` message. When `admit` starts or resets the stall clock, the handler spawns a delayed send of it. On a check with held messages, a non-terminal request and a stall at least `gap_timeout` old, the router delivers `TransportFailed { reason: "relay sequence gap: missing seq N (held M)" }` and clears the held messages. Verify: a test with `gap_timeout` of about 50 ms and a missing number gives the consumer exactly one `TransportFailed` naming N, sets the request phase to `Failed`, and delivers no held data.
- [x] 4.3 Fail immediately when `held.len()` exceeds `max_held`. Verify: a test with `max_held = 4` and a missing first message gets `TransportFailed` on the fifth held message.
- [x] 4.4 Verify that a gap filled before the timeout raises nothing: a test where the missing message arrives within the timeout, followed by an elapsed check, delivers all messages in order and no failure.
- [x] 4.5 Verify the client maps a gap failure to a retryable transport error: a `RemoteProviderStreamState`/`poll_stream_message` test that feeds the gap `TransportFailed` yields an `LLMError::Transport` where `is_retryable()` is true, not a stream accumulation error.

## 5. Unbounded consumer channel

- [x] 5.1 Change `consumer_tx` to `mpsc::UnboundedSender<StreamRelayMessage>` in `AttachStreamConsumer`, `RoutedRequest`, `RemoteProviderClientTransport::prepare_stream_router`, `RemoteProviderClientCore::prepare_stream_router`, `mesh_client.rs`, the test transport in `provider_client_runtime.rs`, and `remote_chat_provider.rs:149` (`unbounded_channel`). Verify: `cargo check -p querymt-remote -p querymt-agent --features remote`.
- [x] 5.2 Remove the `TrySendError::Full` branch and its `ConsumerDisconnected` transition. Keep the `Closed` branch. Verify: `cargo clippy -p querymt-remote --all-targets` is clean.
- [x] 5.3 Replace `consumer_backpressure_sets_disconnected_and_buffers_message` with a slow-consumer test: deliver 500 messages without reading, then read. Verify: all 500 arrive in order, the phase is not `ConsumerDisconnected`, and the replay buffer is empty.

## 6. Host sequencing

- [x] 6.1 In `provider_host_actor.rs`, change `buffered` to `VecDeque<(Option<u64>, StreamRelayMessage)>`, with a per-request `next_seq` counter. Number entries in `flush_batch`, for the terminal message, and for the grace-expiry `TransportFailed`. Heartbeats and `TransportDisconnected`/`TransportReconnected` stay `None`. Send `seq` in the outgoing `RoutedStreamRelayMessage`. Verify: a host test with a capture router shows contiguous numbers from 0 on data and terminal messages, and `None` on heartbeats.
- [x] 6.2 Send `seq: Some(0)` with the setup-failure `ProviderError` (`provider_host_actor.rs:344`). Verify: an existing or new setup-failure test still delivers the `ProviderError` to the consumer.
- [ ] 6.3 Confirm that resends after a send error reuse the same number. Verify: a test that forces one send failure, then reconnects, shows the resent message with its original `seq`, and the router delivers it once.
- [x] 6.4 Update `keep_stream_message_buffered` and `should_ack_relay_message` callers for the tuple type, with no behaviour change. Verify: the existing `provider_protocol.rs` tests pass.

## 7. Validation and rollout

- [x] 7.1 Run `cargo nextest run -p querymt-remote` and `cargo nextest run -p querymt-agent`. Verify: all pass, including the remote provider host tests in `crates/agent/src/agent/remote/provider_host_tests.rs`.
- [x] 7.2 Run `cargo clippy --workspace --all-targets --exclude querymt-mobile-ffi`. Verify: no new warnings.
- [ ] 7.3 Run a live mesh check between two peers (for example, this machine and `dagon`), both on this change, with a long Anthropic tool-using turn and a codex turn. Verify: no `stream_accumulation` errors in the journal for those sessions. Record in `notes.md` the counts of `relay sequence gap` failures and of held or duplicate messages from the router debug logs.
- [ ] 7.4 Add the release note: mesh peers need this change on both sides for ordered delivery, and it ships with or before `structured-anthropic-streaming`. Mention the optional host-only interim (always acknowledge data messages) for mixed deployments. Verify: the note is in `notes.md` and in the PR description.
