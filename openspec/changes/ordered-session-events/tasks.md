# Tasks

## 1. Measure before building

- [x] 1.1 Add a probe test that emits a tool-call start and end back to back through `AgentConfig::emit_event` and asserts their journal `stream_seq` order; record whether it fails today (evidence for "Replay matches live order").
- [x] 1.2 Extend `benches/event_emit.rs` with the split variant (durable ordered, ephemeral inline), WAL + `synchronous=NORMAL`, a batched writer and a paced emitter (realistic token rate, burst at turn end); verify `cargo bench -p querymt-agent --bench event_emit` runs all rows and record a full Criterion run in design.md.
- [x] 1.3 Verify kameo preserves `tell` order from one `EventForwarder` to one `EventRelayActor` with a mesh test that sends 1,000 numbered events and checks arrival order.

## 2. Per-session sequence and ordered emission

- [x] 2.1 Add a per-session emit handle with an atomic `session_seq` counter shared by the session actor, execution task, delegation and scheduler; `emit_event` delegates to it; verify unit tests that concurrent emitters get strictly increasing numbers.
- [x] 2.2 Add `session_seq` to `DurableEvent` and `EphemeralEvent` (serde default for old payloads) and regenerate typeshare types; verify event serialization tests and that old payloads still parse.
- [x] 2.3 Implement the per-session ordered durable writer and the ephemeral reorder buffer (design decision 2); verify the probe from 1.1 passes, fanout order equals `session_seq` order under concurrent emitters, and the paced benchmark meets the thresholds recorded in 1.2.

## 3. Journal contract

- [x] 3.1 Add the `session_seq` column, unique index `(session_id, session_seq)` and backfill migration; verify migration tests on an existing journal and `benches/session_load.rs` load time is not worse.
- [x] 3.2 Persist producer `session_seq` for local events and the host `session_seq` for relayed events; order session replay by `session_seq`; update the `EventJournal` docs ("producers set `session_seq`"); verify replay-order and relay dedup tests.
- [x] 3.3 Apply WAL / batching only if 1.2 showed they meet the thresholds; verify the decision is recorded in design.md.

## 4. Lossless consumers

- [x] 4.1 Fix the WebSocket session-event forwarder exiting on broadcast lag (`acp/websocket.rs` `spawn_event_forwarders`, `Err(_) => break`, deferred from `unify-session-interactions`): on lag, recover the missed durable range from the journal and continue; verify a test that forces lag and asserts later updates still arrive in order.
- [x] 4.2 Add the same lag and gap recovery to the stdio forwarder and the remote event forwarder/relay (gap → fetch from host via the existing backfill path); verify tests for stdio lag and a relayed sequence gap.

## 5. Prompt watermark

- [x] 5.1 Return `event_tip` (last `session_seq` of the run) with the prompt reply, locally and across the mesh; treat a missing tip as "no watermark"; verify actor and remote-ref tests.
- [x] 5.2 Track per-connection, per-session delivered position in the WebSocket and stdio forwarders (advanced for every consumed event) and make `session/prompt` dispatch wait for `event_tip`, released on disconnect, lag-recovery failure, connection close or the safety limit; verify tests for each release path and that a slow second connection does not delay the response.
- [x] 5.3 Enable the ignored `remote_prompt_updates_are_sent_before_prompt_response` test and keep `local_prompt_updates_are_sent_before_prompt_response`; verify both pass 50 consecutive runs.
- [x] 5.4 Document the ordering guarantees, the journal migration/downgrade note and the all-nodes-upgrade requirement in the agent docs/changelog; verify the docs mention `session_seq` and the prompt-ordering guarantee.

## 6. Integration

- [x] 6.1 Run `cargo test -p querymt-agent --all-features`, clippy and fmt, plus the `event_emit` and `session_load` benchmarks; verify green and no benchmark regression beyond the recorded thresholds.

## 7. Review follow-ups

- [x] 7.1 Serve remote pages (`GetEventStreamSince`) in raw `stream_seq` order via `EventJournal::load_session_page`; verify page-order and unsequenced-slot tests.
- [x] 7.2 Limit the `(session_id, session_seq)` unique index and the local counter start to local rows (`source_node_id IS NULL`); verify a relayed host number equal to a local number is stored.
- [x] 7.3 Release a session's event stream when its actor stops, after the writer drains; verify no stream is left behind and numbers are never reused.
- [x] 7.4 Host forwarder sends `RelayLagged` on lag and the relay fetches the durable tail; verify a lost tail is recovered without a later event.
- [x] 7.5 Regenerate typeshare types; verify `session_seq` is in the generated TypeScript.
- [x] 7.6 Resolve the prompt-ordering contradiction in `mesh.md` and document the guard and lag signal in `event-ordering.md`.
- [ ] 7.7 Follow-up change: cross-session batched writer (one writer per database), measured against WAL first.

## Workflow follow-up

- Apply after `unify-session-interactions` is merged.
- Archive after release.
