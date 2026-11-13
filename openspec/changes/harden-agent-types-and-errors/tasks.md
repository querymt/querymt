# Tasks

Package for every group unless stated: `querymt-agent`. Focused test runs use `cargo nextest run -p querymt-agent --features remote -E 'test(/<name>/)'`. Full CI feature checks (`dashboard,oauth,zbus-secret-service,systemd-credentials,remote`) need `QMT_UI_DIST` set to the minimal fixture from `.github/workflows/test-agent.yml`. The zbus/systemd features only take effect on Linux, so on macOS they stay unverified.

## 1. Remove `tasks::run_verification` (D10)

- [x] 1.1 Delete `run_verification`, `extract_verification_commands` and their tests from `crates/agent/src/tasks.rs`, keeping `TaskWatcher`. Verify that `cargo check -p querymt-agent --features remote` passes and that searching `crates/` for `extract_verification_commands` finds nothing.
  - Ran `cargo check -p querymt-agent --features remote`: passed (5 pre-existing warnings in `auth/` and `acp/websocket.rs`, none from this change). Search for `extract_verification_commands` in `crates/`: no hits.
- [x] 1.2 Add a BREAKING entry for the removal under "Unreleased" in `CHANGELOG.md`. Verify by reading the entry.
  - Entry added as the first bullet under "Unreleased / Behavior changes" and re-read.

## 2. Poison-free storage and one blocking helper (D1, D2)

- [x] 2.1 Switch every `Arc<std::sync::Mutex<rusqlite::Connection>>` in `session/`, `knowledge/sqlite.rs`, `dotagents/{tasks,automation}.rs` and `worktree/{lifecycle,allocate,repo_workspace}.rs` to `parking_lot::Mutex`, and drop the `.lock().unwrap()` calls. Verify with `cargo check -p querymt-agent --features remote` and by searching for `lock().unwrap()` on connections, which should find no remaining hits.
- [x] 2.2 Add `session/sqlite_storage/blocking.rs` with `with_conn` and its `rusqlite::Result` sibling. Replace the nine `run_blocking` copies and the inline `spawn_blocking` blocks in `event_journal.rs` with it. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/sqlite_storage|repo_/)'`.
- [x] 2.3 Change `SessionError` in `session/error.rs`:
  - add `#[non_exhaustive]`
  - add `TaskFailed(#[from] JoinError)`
  - replace `DatabaseError(String)` with `Database(#[from] rusqlite::Error)`, and re-home the non-rusqlite `DatabaseError(format!…)` sites
  - add `is_transient()` for `DatabaseBusy`/`DatabaseLocked`

  Verify with `cargo check -p querymt-agent --features remote`.
- [x] 2.4 Make `From<SessionError> for LLMError` render the full source chain (D3). Covers `session-storage-reliability / Storage error converted for a provider caller keeps its source`; test `storage_error_converted_for_a_provider_caller_keeps_its_source`. Acceptance is checked on the converted `LLMError`: its text must contain the rusqlite message. The test also checks that `SessionError::source()` is the rusqlite error before conversion, which supports `Storage failures keep their cause`.
- [x] 2.5 Add tests in `session/sqlite_storage/tests.rs` that inject a panicking closure and a failing SQL statement through `with_conn`, then run a session load, an event append and a session list. Covers:
  - `session-storage-reliability / Storage keeps working after an operation panics` (`storage_keeps_working_after_an_operation_panics`)
  - `/ Storage keeps working after an operation returns an error` (`storage_keeps_working_after_an_operation_returns_an_error`)
  - `/ Panicked storage task is reported as a task failure` (`panicked_storage_task_is_reported_as_a_task_failure`, asserting `SessionError::TaskFailed`)

  Run them with the nextest filter `test(/storage_keeps_working|panicked_storage_task/)`.

  Ran `cargo nextest run -p querymt-agent --features remote -E 'test(/storage_keeps_working|panicked_storage_task|storage_error_converted|is_transient_only/)'`: 5 passed, 0 failed. Group 2 earlier: `test(/sqlite_storage|repo_/)` 188 passed.

## 3. Typed delegation claim retry (D9 row)

- [x] 3.1 Change `delegation/core.rs` `claim_delegation` to return `SessionResult<DelegationClaim>` and retry only when `is_transient()`, up to three attempts. Add tests with a store double that counts attempts. Covers:
  - `session-storage-reliability / Busy database claim is retried` (`busy_database_claim_is_retried`)
  - `/ Non-transient claim failure is not retried` (`non_transient_claim_failure_is_not_retried`)
  - `/ Missing delegation is a claim outcome, not a failure` (`missing_delegation_is_a_claim_outcome_not_a_failure`)

  Run them with the nextest filter `test(/claim_/)`.

  Ran `cargo nextest run -p querymt-agent --features remote -E 'test(/claim_/)'`: 12 passed, 0 failed (includes an extra test, `busy_database_claim_gives_up_after_three_attempts`).

## 4. Async event stream base (D4)

- [x] 4.1 Add `EventJournal::max_session_seq` (async, through `with_conn`) and remove `max_session_seq_blocking`, along with its overrides in `accounting/recorder.rs`, `agent/execution/wait.rs` and `session/projection.rs`. Verify with `cargo check -p querymt-agent --features remote`.
- [x] 4.2 Make `EventSink::session_stream` create the stream with an unresolved base. The writer task resolves `max(retired_tip, N)` and then publishes the pending events in order, renumbered to absolute values.
  - List every `emit_ephemeral_seq` and absolute-`session_seq` caller.
  - Move each one onto an awaited base watch. Use `block_in_place` only if a caller is truly synchronous; record any such caller in the design.

  Verify with the existing `session_event_stream` and `event_sink` tests: `cargo nextest run -p querymt-agent --features remote -E 'test(/session_event_stream|event_sink/)'`.
- [x] 4.3 Add tests. Cover `session-storage-reliability / Stream opens while the connection is busy` with `stream_opens_while_the_connection_is_busy`: a `current_thread` runtime, a journal holding the connection, and a sibling task that must complete before the hold ends. Cover `/ Reopened stream continues numbering` with `reopened_stream_continues_numbering`: a fresh `EventSink` over a journal with N events emits N+1. Run them with the nextest filter `test(/stream_opens_while|reopened_stream_continues/)`.
  - 4.1-4.3 results: `cargo check ... --all-targets` passes; `test(/session_event_stream|event_sink/)` 26 passed; `test(/stream_opens_while|reopened_stream_continues/)` 2 passed.
  - Audit: the only absolute-`session_seq` callers were `emit_ephemeral_event` (`ext_worktree_ops.rs`, already async) and the prompt-reply `tip()` in `session_actor.rs` (inside a spawned task). Both now await the base (`emit_ephemeral_seq` is async, `resolved_tip()` added). The MCP progress sink ignored the returned number and uses `emit_ephemeral`. No `block_in_place` needed. The existing `ephemeral_without_pending_durable_publishes_inline` test now awaits `resolved_tip()` first, since a fresh stream holds events until its base resolves (D4).

## 5. Typed event kind tags (D6)

- [x] 5.1 Add `AgentEventKind::tag()` (exhaustive match) and tag consts to `events.rs`. Add a test, `event_kind_tag_matches_serde_tag`, that checks `tag()` against the serde `"type"` for every variant. Run it with the nextest filter `test(/event_kind_tag_matches_serde_tag/)`.
- [x] 5.2 Switch `event_journal.rs` `insert_event` to `kind.tag()`, and replace the Rust-side tag literals in `event_journal.rs`, `session_store.rs` and `view_store.rs` with the consts. Leave `migrations.rs` untouched. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/event_journal|view_store|legacy_baseline_upgrade_matches_fresh_install/)'`.
- [x] 5.3 Rewrite the forked-session filter in `session_store.rs` to deserialize `AgentEventKind` into a named `JournalRow`. Verify that the existing fork tests in `session/sqlite_storage/tests.rs` pass (`test(/fork/)`).

Group 5 results: `cargo nextest run -p querymt-agent --features remote -E 'test(/event_journal|view_store|legacy_baseline_upgrade_matches_fresh_install|fork|event_kind_tag/)'`: 59 passed, 0 failed. `migrations.rs` untouched.

## 6. One run id end to end (D5)

- [x] 6.1 Change `turn_control.rs` (`ActiveRun`, `SteeringInbox`, `TurnControlError`), `agent/messages.rs`, `agent/objective.rs` and `remote/actor_ref.rs` to carry `TurnId`. Parse incoming `expected_run_id` with `TurnId::from_stored`. Remove the parallel `run_id` strings in `session_actor.rs`. Wire structs (`events.rs` run events, `SubmitInputResult`) still take `to_string()`. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/steer|turn_control|session_actor/)'`.
- [x] 6.2 Remove `ExecutionContext::turn_id_string` and add `require_turn_id()`. Hook and event builders take `TurnId`. Native `/compact` and maintenance compaction call `with_turn_id(TurnId::new())`. (Superseded by 6.5: the id is now a constructor argument and `require_turn_id` / `with_turn_id` no longer exist.) Remove `impl Default for TurnId`. Verify with `cargo check -p querymt-agent --features remote` and by searching for `turn_id_string`, which should find nothing.
- [x] 6.3 Add tests in `agent/session_actor.rs`:
  - `run_events_hooks_and_snapshot_records_share_the_run_id`: one run with a command hook capturing input, a file edit and a snapshot backend. Covers `session-run-lifecycle / Run events, hooks and snapshot records share the run id`.
  - `steering_reports_the_active_runs_id`. Covers `/ Steering reports the active run's id`.
  - `hook_fired_inside_a_run_gets_the_runs_id`, using a stop hook. Covers `/ Hook fired inside a run gets the run's id`.
  - `native_compact_hook_gets_that_prompts_run_id` (renamed from `hook_fired_outside_a_run_gets_a_fresh_id` in 6.6), using native `/compact` with a compaction hook. Covers `/ Native compact hook gets its prompt's run id`.

  Run them with the nextest filter `test(/share_the_run_id|steering_reports_the_active|hook_fired_|native_compact_hook/)`.

  Ran `cargo nextest run -p querymt-agent --features remote -E 'test(/share_the_run_id|steering_reports_the_active|hook_fired_|native_compact_hook/)'`: 4 passed, 0 failed. Also ran `test(/steer|turn_control|session_actor/)` for 6.1: 110 passed. Added `UndoTestFixture::with_provider_and_hooks` to drive real runs with hooks. Delegation hooks in `delegation/core.rs` also mint a `TurnId` instead of sending `""`.
- [x] 6.4 Document the `turn_id` guarantee in `docs/docs/agent/hooks.md` and add a BREAKING CHANGELOG entry. Verify by reading both.

  Read both after editing: hooks.md states `turn_id` is always a UUID, the run's id inside a run and a fresh one outside; CHANGELOG has the BREAKING entry under Unreleased.

- [x] 6.5 Make the execution context's run id required (CodeRabbit review). `ExecutionContext::new` takes the `TurnId`, the field is private with a `turn_id()` accessor, and `with_turn_id`, `require_turn_id` and the debug-assert / mint-a-fresh-id fallback are removed. Update D5, the `session-run-lifecycle` spec, `hooks.md` and the CHANGELOG, which wrongly described `/compact` as running outside a run. Verify with `cargo check -p querymt-agent --features remote --all-targets` and by searching for `require_turn_id` and `with_turn_id`, which should find no hits.

  Check is clean and the search finds no hits. The only production construction (`execute_prompt_inner`) passes the run's `turn_id`; ~16 test constructions pass `TurnId::new()`. `run_events_hooks_and_snapshot_records_share_the_run_id` now also captures `post_tool_use`, so both hooks of one tool call are checked against the run id. Delegation hooks (`delegation/core.rs`) still mint their own id; that is out of scope and now documented.
- [x] 6.6 Remove the race in the run-id tests (CodeRabbit review). The prompt reply is sent before the actor records `RunCompleted`, so reading the journal right after `prompt` could miss it. Add `UndoTestFixture::prompt_to_completion`, which subscribes first, takes the run id from the prompt's `RunStarted`, and waits (10s timeout, lag reported as an error) for that run's `RunCompleted`. Use it for every model or `/compact` prompt in the 6.3 tests that reads the journal. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/share_the_run_id|steering_reports_the_active|hook_fired_|native_compact_hook/)'`.

  4 passed. With a temporary 300 ms delay before `PromptFinished` (reverted, no `TEMP` left in the tree), the old plain `prompt()` version of `run_events_hooks_and_snapshot_records_share_the_run_id` failed and the `prompt_to_completion` versions passed. The `/compact` test was renamed to `native_compact_hook_gets_that_prompts_run_id` and now asserts the hook id equals that prompt's own `RunStarted` id (it runs inside a run). Also fixed the test hook command so the newline goes to the capture file, not into the hook's stdout JSON.

  Side fix found while verifying: `configured_default_baseline_applies_only_to_unpinned_isolation` hung (about 50% of runs on this branch, 0% on the baseline). The test thread was stuck dropping `FileIndexWatcher`: the notify thread was blocked in `event_tx.blocking_send` on a full 256-slot channel whose consumer task is no longer polled once the runtime shuts down, so `FsEventWatcher::stop` never returned. `index/file_index.rs` now uses an unbounded channel (the consumer already buffers events in an unbounded `Vec`). 24/24 runs pass afterwards.

## 7. Per-session todos with typed states (D7, D8)

- [x] 7.1 Add `TodoStatus`, `TodoPriority` and `TodoStore` in `tools/builtins/todo.rs`, delete `TODO_STORAGE`, add `ToolContext::todo_store()`, and wire the store through `AgentConfig` and `AgentToolContext`. Verify with `cargo check -p querymt-agent --features remote`.
- [x] 7.2 Call `todo_store.remove` from `api/sessions.rs` `Sessions::delete`. Verify with `cargo check -p querymt-agent --features remote`.
- [x] 7.3 Add tests in `todo.rs`, and in `api/` for the delete case:
  - `two_sessions_keep_separate_todo_lists`
  - `reading_before_any_write_returns_an_empty_list`
  - `unknown_status_is_rejected`
  - `stored_values_round_trip_unchanged`
  - `counts_match_the_written_list`
  - `recreated_session_id_starts_with_an_empty_list`

  These cover every `session-todos` scenario. Run them with the nextest filter `test(/todo|separate_todo|empty_list|unknown_status|round_trip_unchanged|counts_match/)`.

  Ran that filter: 18 passed, 0 failed. `TodoStore` is owned by `AgentConfig` and exposed through `ToolContext::todo_store()`; `TODO_STORAGE` is gone. Both delete paths (`Sessions::delete` and `send_agent_lifecycle` delete) call `todo_store.remove`. Contexts with no store return a `SessionError` saying todos are unavailable (extra test `context_without_a_store_reports_todos_unavailable`). Profile-bound sessions keep their list in the profile runtime's own store, so both delete paths now call `LocalAgentHandle::discard_session_todos`, which clears the main and the bound runtime's store (test `deleting_a_profile_bound_session_discards_its_todo_list`). `TestAgent::execution_context` now sets a turn id, because `require_turn_id` debug-asserts (found via `timeout_applies_to_every_call_needs_with_empty_lists`).

## 8. Typed states at the remaining boundaries (D7)

- [x] 8.1 Add `#[typeshare]` to `ScheduleState` and type `ScheduleInfo.state` as `ScheduleState`. Add `InputDelivery`/`PendingInputState` to `PendingInputSummary`, a private `AttemptState` in `export/atif.rs`, and a private `ContextMode` in `get_symbol.rs`. Add a test, `typed_state_fields_serialize_unchanged`, that checks the JSON strings stay the same. Run it with the nextest filter `test(/typed_state_fields_serialize_unchanged|atif|get_symbol|schedule/)`.

  Ran that filter: 112 passed, 0 failed. One `typed_state_fields_serialize_unchanged` test per module (schedules, history_window, atif, get_symbol). `ContextMode` keeps unrecognised values behaving like `lines`. Test fixtures that build an `ExecutionContext` now set a turn id (needed by `require_turn_id`).
- [x] 8.2 Note in the CHANGELOG that querymt-desktop should regenerate typeshare for `ScheduleInfo.state` and `PendingInputSummary` (UI follow-up, no UI work here). Verify by reading the entry.

  Read the entry in CHANGELOG.md under Unreleased.

## 9. Typed errors at index, hooks and admission (D9)

- [x] 9.1 Add `IndexError` and use it in the `workspace_actor.rs` `Rebuild` reply, the `workspace_manager_actor.rs` `GetOrCreate` reply, `FunctionIndex::build` and `index/search.rs`. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/index/)'`.

  Ran `test(/index/)`: 153 passed. New `index/error.rs` has `IndexError` (`FileIndex`, `Regex`, `Search`, `TaskFailed`, `ThreadPool`); `WorkspaceIndexError` is removed. `collect_source_files` could never fail, so it now returns a plain `Vec`.
- [x] 9.2 Add `HookError` and `HookConfigError`, and use them in `HookEngine::new`, `run_stop` and `HooksConfig::validate`. Update callers (`agent/execution/mod.rs`, `config/tests.rs`). Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/hook/)'`.

  Ran `test(/hook/)`: 101 passed. New `hooks/error.rs` has `HookConfigError` (one variant per validation rule) and `HookError` (`Config`, `Spawn`, `Timeout`, `Input`, `Failed`, `Terminated`, MCP variants). `HooksConfig::validate`, `Hooks::new`, `run_stop`, `run_command_hook` and `run_stop_hook` use them. Other `run_*` methods keep `anyhow` where it never crosses a typed boundary (spec allows it). Added tests `invalid_matcher_keeps_its_regex_source` and `empty_group_is_a_typed_config_error`.
- [x] 9.3 Add a serializable `AdmissionRejection` for the `AdmissionRequest` reply (feature `remote`), mapped to `AdmissionError::Rejected` on the client. Add a mesh-wire BREAKING CHANGELOG entry. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/admission/)'`.

  Ran `test(/admission/)`: 10 passed. `AdmissionRejection` (`NoMeshHandle`, `NoInviteStore`, `InvalidPeerId`, `PeerIdMismatch`, `NotAMember`, `Denied(String)`) is the reply's error arm; the client maps `RemoteSendError::HandlerError` to `AdmissionError::Rejected`. Added `rejection_round_trips_over_msgpack_and_maps_to_rejected`; the two node-manager tests that matched `AdmissionResponse::Rejected` now match the handler error. `AdmissionResponse::Rejected` remains in the enum (unused by the host now) to keep the response shape.

## 10. Small cleanups

- [x] 10.1 Replace the 4-tuple `records` in `agent/undo.rs` `resolve_turn_patches` with a named struct. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/undo/)'`.

  Ran `test(/undo/)`: 61 passed. The tuple is now a private `HistoryRecord { message_idx, part, turn_id, record }`.

## 11. Integration checks

- [x] 11.1 Run `cargo fmt -p querymt-agent -- --check`, then `cargo clippy -p querymt-agent --all-targets --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -- -D warnings` with `QMT_UI_DIST` set. Both must pass.

  Ran fmt `--check` (clean) and clippy with `-D warnings` and `QMT_UI_DIST` set (clean after removing an unused import, an unused `ExecutionContext::turn_id`, five needless borrows in `event_journal.rs`, and moving one `allow(too_many_arguments)` in `get_symbol.rs`).
- [x] 11.2 Run the full `cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote` with `QMT_UI_DIST` set, and `cargo test -p querymt-agent --doc`. Both must pass.

  Full nextest with all features: 4342 run, 4341 passed. The one failure, `connect_budget_defaults_are_bounded_and_reason_specific`, is unrelated: it fails identically on the stashed baseline because this shell exports `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS=20000` and the test does not guard that variable; it passes with the variable unset. Also fixed `suspended_live_progress_does_not_wait_for_stalled_recovery`, which now awaits `resolved_tip()` first (unresolved-base stream). Doc tests: 20 passed, 6 ignored.
- [x] 11.3 Build the downstream packages from `cargo tree -i querymt-agent`: run `cargo check -p querymt-mobile-ffi`, and `cargo check -p querymt-agent-py` with `PYO3_PYTHON` set to Python 3.10 or newer. Both must pass.

  `cargo check -p querymt-mobile-ffi` passed; `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py` passed.
- [x] 11.4 Run `openspec validate harden-agent-types-and-errors --strict`. It must pass.

  `openspec validate harden-agent-types-and-errors --strict`: valid.

## Workflow follow-up

- Report the querymt-desktop typeshare regeneration follow-up.
- Archive the change after review.
