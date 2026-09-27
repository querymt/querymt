# Tasks

## 1. Protocol Surface and Projection Foundation

- [x] 1.1 Confirm the locked `agent-client-protocol` v1 types, feature flags, and exact capability paths for usage, plan operations, notices, and compaction; update `crates/agent/Cargo.toml` only as needed and verify `cargo check -p querymt-agent` succeeds.
- [ ] 1.2 Add a connection-scoped session-update capability snapshot parsed during successful ACP initialization, treating omitted and null Preview capabilities as unsupported; verify unit tests cover every capability combination and isolation between two connections.
- [ ] 1.3 Refactor the shared live translator to return ordered zero-to-many notifications and update both stdio and WebSocket delivery loops; verify existing translation tests pass and new transport tests prove multiple updates retain order.
- [ ] 1.4 Add JSON serialization tests for every targeted ACP v1 discriminator and required field shape, verifying the tests assert `current_mode_update`, `config_option_update`, `session_info_update`, `usage_update`, `plan_update`, `plan_removed`, `notice`, `compaction_update`, and `compaction_summary_chunk` wire output.

## 2. Mode, Configuration, and Session Metadata

- [ ] 2.1 Emit an authoritative post-commit configuration snapshot for effective mode, model, profile, and reasoning-effort transitions while suppressing no-op transitions; verify session-control tests cover each mutation and unchanged values.
- [ ] 2.2 Translate mode snapshots into ordered `current_mode_update` and complete `config_option_update` notifications, and translate other configuration snapshots into complete config replacements; verify translator tests assert current values and ordering.
- [ ] 2.3 Emit metadata patch events only after title or `updated_at` persistence succeeds, including explicit clear semantics where supported; verify store/actor tests prove failed persistence emits nothing and successful persistence matches later session-list output.
- [ ] 2.4 Translate persisted metadata patches to `session_info_update` with RFC 3339 timestamps; verify typed and serialized tests cover title updates, timestamp updates, omissions, and explicit null fields.

## 3. Context Usage Meter

- [ ] 3.1 Track each live session's latest effective model context limit and cumulative USD cost in the connection-local projector, with cleanup on session/connection close; verify unit tests cover model switches, unknown limits, and state cleanup.
- [ ] 3.2 Translate request-completion usage into `usage_update` using current context occupancy and the effective limit, omitting the update when either value is unreliable; verify tests cover cached-token-inclusive usage, cost present/absent, and unknown-size suppression.
- [ ] 3.3 Move or extend compaction completion data so the authoritative post-compaction context-token count can produce a reduced usage snapshot; verify a compaction test observes a lower `usage_update` after completion without estimating from summary length.

## 4. Plan Operations and Notices

- [ ] 4.1 Project non-empty `todowrite` snapshots to capability-gated item-based `plan_update` notifications with one deterministic per-session plan ID while retaining legacy `plan` fallback; verify tests cover capable and legacy clients with complete replacement entries.
- [ ] 4.2 Project an empty or explicitly dismissed announced todo plan to one `plan_removed`, suppress duplicate removals, and keep the legacy empty-plan replacement; verify lifecycle tests cover create, update, remove, repeated empty updates, and recreate.
- [ ] 4.3 Translate `HookNotice` advisories to capability-gated ACP notices with non-empty title, description, and `info`/`error` severity mapping, without converting blocked operations or permissions; verify tests cover capability absence, both severities, and exclusion from replay.

## 5. Compaction Lifecycle

- [ ] 5.1 Extend persisted compaction events with backward-compatible opaque IDs, summary chunks, terminal statuses, optional safe summary/error, and post-compaction usage data; verify event serialization round trips both new records and legacy records lacking IDs.
- [ ] 5.2 Restructure compaction execution to emit one start, one final text summary chunk when displayable content exists, and exactly one matching completed/failed/cancelled terminal event after start; verify execution tests cover success, failure, cancellation, pre-hook blocking, and hidden-summary omission.
- [ ] 5.3 Translate compaction events to capability-gated `compaction_update` and `compaction_summary_chunk` notifications with valid ordering and field constraints; verify tests reject chunk-before-start behavior and assert unsupported clients receive neither variant.

## 6. Materialized Replay and Integration

- [ ] 6.1 Add a replay materialization pass that folds configuration, metadata, latest valid usage, current todo plan, and terminal compactions before applying connection capabilities; verify replay tests use stable IDs and emit only current state.
- [ ] 6.2 Ensure replay omits notices, historical compaction chunks, obsolete in-progress compactions, duplicate summary content, and unsupported Preview variants; verify focused replay fixtures cover each exclusion and legacy persisted compaction events.
- [ ] 6.3 Run `cargo nextest run -p querymt-agent` (or the repository's equivalent package test command) and verify stdio/WebSocket integration fixtures cover mixed-capability clients, stable updates, fallback plans, compaction ordering, and the context-meter regression.
- [ ] 6.4 Update relevant ACP-facing documentation or examples to describe emitted stable updates, Preview capability requirements, fallback behavior, and replay semantics; verify all referenced feature names and JSON examples match serialization tests.
