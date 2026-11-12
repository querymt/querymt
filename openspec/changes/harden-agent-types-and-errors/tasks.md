# Tasks

Package for every group unless stated: `querymt-agent`. Focused test runs use `cargo nextest run -p querymt-agent --features remote -E 'test(/<name>/)'`. Full CI feature checks (`dashboard,oauth,zbus-secret-service,systemd-credentials,remote`) need `QMT_UI_DIST` set to the minimal fixture from `.github/workflows/test-agent.yml`. The zbus/systemd features only take effect on Linux, so on macOS they stay unverified.

## 1. Remove `tasks::run_verification` (D10)

- [ ] 1.1 Delete `run_verification`, `extract_verification_commands` and their tests from `crates/agent/src/tasks.rs`, keeping `TaskWatcher`. Verify that `cargo check -p querymt-agent --features remote` passes and that searching `crates/` for `extract_verification_commands` finds nothing.
- [ ] 1.2 Add a BREAKING entry for the removal under "Unreleased" in `CHANGELOG.md`. Verify by reading the entry.

## 2. Poison-free storage and one blocking helper (D1, D2)

- [ ] 2.1 Switch every `Arc<std::sync::Mutex<rusqlite::Connection>>` in `session/`, `knowledge/sqlite.rs`, `dotagents/{tasks,automation}.rs` and `worktree/{lifecycle,allocate,repo_workspace}.rs` to `parking_lot::Mutex`, and drop the `.lock().unwrap()` calls. Verify with `cargo check -p querymt-agent --features remote` and by searching for `lock().unwrap()` on connections, which should find no remaining hits.
- [ ] 2.2 Add `session/sqlite_storage/blocking.rs` with `with_conn` and its `rusqlite::Result` sibling. Replace the nine `run_blocking` copies and the inline `spawn_blocking` blocks in `event_journal.rs` with it. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/sqlite_storage|repo_/)'`.
- [ ] 2.3 Change `SessionError` in `session/error.rs`:
  - add `#[non_exhaustive]`
  - add `TaskFailed(#[from] JoinError)`
  - replace `DatabaseError(String)` with `Database(#[from] rusqlite::Error)`, and re-home the non-rusqlite `DatabaseError(format!…)` sites
  - add `is_transient()` for `DatabaseBusy`/`DatabaseLocked`

  Verify with `cargo check -p querymt-agent --features remote`.
- [ ] 2.4 Make `From<SessionError> for LLMError` render the full source chain (D3). Covers `session-storage-reliability / Storage error converted for a provider caller keeps its source`; test `storage_error_converted_for_a_provider_caller_keeps_its_source`, which checks that the rusqlite message appears in the LLM error text and that `SessionError::source()` is the rusqlite error.
- [ ] 2.5 Add tests in `session/sqlite_storage/tests.rs` that inject a panicking closure and a failing SQL statement through `with_conn`, then run a session load, an event append and a session list. Covers:
  - `session-storage-reliability / Storage keeps working after an operation panics` (`storage_keeps_working_after_an_operation_panics`)
  - `/ Storage keeps working after an operation returns an error` (`storage_keeps_working_after_an_operation_returns_an_error`)
  - `/ Panicked storage task is reported as a task failure` (`panicked_storage_task_is_reported_as_a_task_failure`, asserting `SessionError::TaskFailed`)

  Run them with the nextest filter `test(/storage_keeps_working|panicked_storage_task/)`.

## 3. Typed delegation claim retry (D9 row)

- [ ] 3.1 Change `delegation/core.rs` `claim_delegation` to return `SessionResult<DelegationClaim>` and retry only when `is_transient()`, up to three attempts. Add tests with a store double that counts attempts. Covers:
  - `session-storage-reliability / Busy database claim is retried` (`busy_database_claim_is_retried`)
  - `/ Non-transient claim failure is not retried` (`non_transient_claim_failure_is_not_retried`)
  - `/ Missing delegation is a claim outcome, not a failure` (`missing_delegation_is_a_claim_outcome_not_a_failure`)

  Run them with the nextest filter `test(/claim_/)`.

## 4. Async event stream base (D4)

- [ ] 4.1 Add `EventJournal::max_session_seq` (async, through `with_conn`) and remove `max_session_seq_blocking`, along with its overrides in `accounting/recorder.rs`, `agent/execution/wait.rs` and `session/projection.rs`. Verify with `cargo check -p querymt-agent --features remote`.
- [ ] 4.2 Make `EventSink::session_stream` create the stream with an unresolved base. The writer task resolves `max(retired_tip, N)` and then publishes the pending events in order, renumbered to absolute values.
  - List every `emit_ephemeral_seq` and absolute-`session_seq` caller.
  - Move each one onto an awaited base watch. Use `block_in_place` only if a caller is truly synchronous; record any such caller in the design.

  Verify with the existing `session_event_stream` and `event_sink` tests: `cargo nextest run -p querymt-agent --features remote -E 'test(/session_event_stream|event_sink/)'`.
- [ ] 4.3 Add tests. Cover `session-storage-reliability / Stream opens while the connection is busy` with `stream_opens_while_the_connection_is_busy`: a `current_thread` runtime, a journal holding the connection, and a sibling task that must complete before the hold ends. Cover `/ Reopened stream continues numbering` with `reopened_stream_continues_numbering`: a fresh `EventSink` over a journal with N events emits N+1. Run them with the nextest filter `test(/stream_opens_while|reopened_stream_continues/)`.

## 5. Typed event kind tags (D6)

- [ ] 5.1 Add `AgentEventKind::tag()` (exhaustive match) and tag consts to `events.rs`. Add a test, `event_kind_tag_matches_serde_tag`, that checks `tag()` against the serde `"type"` for every variant. Run it with the nextest filter `test(/event_kind_tag_matches_serde_tag/)`.
- [ ] 5.2 Switch `event_journal.rs` `insert_event` to `kind.tag()`, and replace the Rust-side tag literals in `event_journal.rs`, `session_store.rs` and `view_store.rs` with the consts. Leave `migrations.rs` untouched. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/event_journal|view_store|legacy_baseline_upgrade_matches_fresh_install/)'`.
- [ ] 5.3 Rewrite the forked-session filter in `session_store.rs` to deserialize `AgentEventKind` into a named `JournalRow`. Verify that the existing fork tests in `session/sqlite_storage/tests.rs` pass (`test(/fork/)`).

## 6. One run id end to end (D5)

- [ ] 6.1 Change `turn_control.rs` (`ActiveRun`, `SteeringInbox`, `TurnControlError`), `agent/messages.rs`, `agent/objective.rs` and `remote/actor_ref.rs` to carry `TurnId`. Parse incoming `expected_run_id` with `TurnId::from_stored`. Remove the parallel `run_id` strings in `session_actor.rs`. Wire structs (`events.rs` run events, `SubmitInputResult`) still take `to_string()`. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/steer|turn_control|session_actor/)'`.
- [ ] 6.2 Remove `ExecutionContext::turn_id_string` and add `require_turn_id()`. Hook and event builders take `TurnId`. Native `/compact` and maintenance compaction call `with_turn_id(TurnId::new())`. Remove `impl Default for TurnId`. Verify with `cargo check -p querymt-agent --features remote` and by searching for `turn_id_string`, which should find nothing.
- [ ] 6.3 Add tests in `agent/session_actor.rs`:
  - `run_events_hooks_and_snapshot_records_share_the_run_id`: one run with a command hook capturing input, a file edit and a snapshot backend. Covers `session-run-lifecycle / Run events, hooks and snapshot records share the run id`.
  - `steering_reports_the_active_runs_id`. Covers `/ Steering reports the active run's id`.
  - `hook_fired_inside_a_run_gets_the_runs_id`, using a stop hook. Covers `/ Hook fired inside a run gets the run's id`.
  - `hook_fired_outside_a_run_gets_a_fresh_id`, using native `/compact` with a compaction hook. Covers `/ Hook fired outside a run gets a fresh id`.

  Run them with the nextest filter `test(/share_the_run_id|steering_reports_the_active|hook_fired_/)`.
- [ ] 6.4 Document the `turn_id` guarantee in `docs/docs/agent/hooks.md` and add a BREAKING CHANGELOG entry. Verify by reading both.

## 7. Per-session todos with typed states (D7, D8)

- [ ] 7.1 Add `TodoStatus`, `TodoPriority` and `TodoStore` in `tools/builtins/todo.rs`, delete `TODO_STORAGE`, add `ToolContext::todo_store()`, and wire the store through `AgentConfig` and `AgentToolContext`. Verify with `cargo check -p querymt-agent --features remote`.
- [ ] 7.2 Call `todo_store.remove` from `api/sessions.rs` `Sessions::delete`. Verify with `cargo check -p querymt-agent --features remote`.
- [ ] 7.3 Add tests in `todo.rs`, and in `api/` for the delete case:
  - `two_sessions_keep_separate_todo_lists`
  - `reading_before_any_write_returns_an_empty_list`
  - `unknown_status_is_rejected`
  - `stored_values_round_trip_unchanged`
  - `counts_match_the_written_list`
  - `recreated_session_id_starts_with_an_empty_list`

  These cover every `session-todos` scenario. Run them with the nextest filter `test(/todo|separate_todo|empty_list|unknown_status|round_trip_unchanged|counts_match/)`.

## 8. Typed states at the remaining boundaries (D7)

- [ ] 8.1 Add `#[typeshare]` to `ScheduleState` and type `ScheduleInfo.state` as `ScheduleState`. Add `InputDelivery`/`PendingInputState` to `PendingInputSummary`, a private `AttemptState` in `export/atif.rs`, and a private `ContextMode` in `get_symbol.rs`. Add a test, `typed_state_fields_serialize_unchanged`, that checks the JSON strings stay the same. Run it with the nextest filter `test(/typed_state_fields_serialize_unchanged|atif|get_symbol|schedule/)`.
- [ ] 8.2 Note in the CHANGELOG that querymt-desktop should regenerate typeshare for `ScheduleInfo.state` and `PendingInputSummary` (UI follow-up, no UI work here). Verify by reading the entry.

## 9. Typed errors at index, hooks and admission (D9)

- [ ] 9.1 Add `IndexError` and use it in the `workspace_actor.rs` `Rebuild` reply, the `workspace_manager_actor.rs` `GetOrCreate` reply, `FunctionIndex::build` and `index/search.rs`. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/index/)'`.
- [ ] 9.2 Add `HookError` and `HookConfigError`, and use them in `HookEngine::new`, `run_stop` and `HooksConfig::validate`. Update callers (`agent/execution/mod.rs`, `config/tests.rs`). Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/hook/)'`.
- [ ] 9.3 Add a serializable `AdmissionRejection` for the `AdmissionRequest` reply (feature `remote`), mapped to `AdmissionError::Rejected` on the client. Add a mesh-wire BREAKING CHANGELOG entry. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/admission/)'`.

## 10. Small cleanups

- [ ] 10.1 Replace the 4-tuple `records` in `agent/undo.rs` `resolve_turn_patches` with a named struct. Verify with `cargo nextest run -p querymt-agent --features remote -E 'test(/undo/)'`.

## 11. Integration checks

- [ ] 11.1 Run `cargo fmt -p querymt-agent -- --check`, then `cargo clippy -p querymt-agent --all-targets --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -- -D warnings` with `QMT_UI_DIST` set. Both must pass.
- [ ] 11.2 Run the full `cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote` with `QMT_UI_DIST` set, and `cargo test -p querymt-agent --doc`. Both must pass.
- [ ] 11.3 Build the downstream packages from `cargo tree -i querymt-agent`: run `cargo check -p querymt-mobile-ffi`, and `cargo check -p querymt-agent-py` with `PYO3_PYTHON` set to Python 3.10 or newer. Both must pass.
- [ ] 11.4 Run `openspec validate harden-agent-types-and-errors --strict`. It must pass.

## Workflow follow-up

- Report the querymt-desktop typeshare regeneration follow-up.
- Archive the change after review.
