# Tasks

Verification uses the agent package aliases from `.cargo/config.toml` (same feature sets as CI):

- `cargo t-core`, `cargo t-sqlite`, `cargo t-runtime` (`remote,oauth,plugin-loaders,api`), `cargo t-agent` (`dashboard,oauth,zbus-secret-service,systemd-credentials,remote`)
- `cargo cl-<pkg>` for clippy with `-D warnings`

`t-agent` / `cl-agent` build the dashboard, so prepare the UI fixture once per shell:

```sh
export QMT_UI_DIST="$(mktemp -d)/dashboard-ui"; mkdir -p "$QMT_UI_DIST"
printf '<!doctype html><title>QueryMT</title>\n' > "$QMT_UI_DIST/index.html"
printf '{"revision":"ci","target":"embedded","acpWebSocketPath":"/acp/ws"}\n' > "$QMT_UI_DIST/querymt-ui.json"
```

The zbus/systemd features only take effect on Linux, so on macOS those paths stay unverified. Never open the user's `~/.qmt/sessions.db` for writing; plan and timing checks against real data use an online backup copy made through a read-only connection.

## 1. Prompt record type and store method in core

- [x] 1.1 Before any change, run `scripts/generate-types.sh` and keep `target/generated/types.ts` as the baseline (if the `typeshare` CLI is not installed, install it with `cargo install typeshare-cli` or record that the check was skipped).
- [x] 1.2 Move `UserPromptRecord`, `user_prompt_records` and `filter_prompt_records` from `crates/agent-runtime/src/session/load_snapshot.rs` to a new `crates/agent-core/src/session/prompt_records.rs` (declare `pub mod prompt_records;` in `agent-core/src/session/mod.rs`). Keep derives, serde and typeshare attributes unchanged; the two functions become `pub`. Move the existing projection unit tests that only exercise `user_prompt_records` with them. In `load_snapshot.rs`, re-export `UserPromptRecord` (`pub use`) and the two functions (`pub(crate) use`) so every existing `crate::session::load_snapshot::…` path compiles unchanged (design decision 1). Verify with `cargo check -p querymt-agent-core --all-targets` and `cargo check -p querymt-agent-runtime --all-targets --features remote,oauth,plugin-loaders,api`.
- [x] 1.3 Add `SessionStore::get_user_prompts(session_id, message_ids: Option<&[String]>)` to `crates/agent-core/src/session/session_store.rs`, next to `get_effective_history`, with the default implementation from design decision 2 and fully qualified paths (no new imports above the trait). Verify with `cargo check -p querymt-agent-core --all-targets --features test-support`: `MockSessionStore` compiles unchanged.
- [x] 1.4 Regenerate `types.ts` and diff it against the 1.1 baseline. Verify: the only difference is the position of the `UserPromptRecord` interface; its content is identical.

## 2. SQLite reads, indexes and read cache

- [x] 2.1 Add migration `0037_session_message_read_indexes` to `crates/agent-sqlite/src/session/sqlite_storage/migrations.rs` with the inlined DDL from design decision 4, and update `init_schema` in `crates/agent-sqlite/src/session/schema.rs`: add `idx_messages_session_order` and `idx_message_parts_prompt`, remove `idx_messages_session`. Verify with `cargo t-sqlite -E 'test(/legacy_baseline_upgrade_matches_fresh_install/)'` (`KNOWN_BENIGN_DRIFT` stays empty), and add an assertion there (or in the 2.4 test) that a migrated database holds `idx_messages_session_order` and `idx_message_parts_prompt` and not `idx_messages_session`. Scenarios: `session-prompt-records` / Upgraded database matches a fresh database.
- [x] 2.2 In `sqlite_storage/session_store.rs` `get_history`, order messages by `created_at, id` and parts by `message_id, sort_order` (design decision 3a). Add test `same_second_messages_keep_insertion_order` in `crates/agent-sqlite/src/session/sqlite_storage/tests.rs`: store three messages with the same `created_at` and assert that `get_history` returns them in insertion order. Scenarios: `session-prompt-records` / Same-second messages keep insertion order (history half).
- [x] 2.3 Override `get_user_prompts` for `SqliteStorage` with the query from design decision 3, kept in a `const`. Pass `message_ids` as one JSON-array parameter (`NULL` for all), resolve the session via `resolve_session_internal_id`, and decode only the returned prompt/steering parts. Add these tests in `sqlite_storage/tests.rs`, each comparing against `user_prompt_records(&get_history(..))` (now importable from `querymt_agent_core::session::prompt_records`) where relevant:
  - `prompt_and_steering_messages_produce_records`: a prompt with text and image blocks, an assistant message with tool use, and a steering message.
  - `record_position_counts_every_message`: positions 0 and 4.
  - `requested_ids_select_their_records`: two prompt ids, one assistant id and one unknown id give exactly two records in position order.
  - `request_without_ids_returns_every_prompt`: 5 prompts and 1 steering message give 6 records.
  - Extend `same_second_messages_keep_insertion_order`: prompt positions equal their indexes in `get_history`.

  Verify with `cargo t-sqlite -E 'test(/prompt_and_steering_messages_produce_records|record_position_counts_every_message|requested_ids_select_their_records|request_without_ids_returns_every_prompt|same_second_messages_keep_insertion_order/)'`. Scenarios: `session-prompt-records` / Prompt and steering messages produce records, Record position counts every message, Requested ids select their records, Request without ids returns every prompt, Same-second messages keep insertion order.
- [x] 2.4 Add test `session_message_reads_use_indexes` in `sqlite_storage/tests.rs`, following `last_unfinished_run_query_uses_run_lifecycle_index`: on `SqliteStorage::connect(":memory:")`, take `EXPLAIN QUERY PLAN` of the production prompt query `const` and of both `get_history` queries (make them `const`s too). Assert that the prompt query uses `idx_messages_session_order` and `idx_message_parts_prompt`, and that the history queries use `idx_messages_session_order` / `idx_message_parts_message` with no `USE TEMP B-TREE`. Verify with `cargo t-sqlite -E 'test(/session_message_reads_use_indexes/)'`. Scenarios: `session-prompt-records` / History and prompt reads use the indexes.
- [x] 2.5 Add test `large_tool_output_does_not_slow_prompt_records`: 1,000 messages holding 50 MB of tool-result parts and 20 prompts; time `get_user_prompts` for the 20 prompt ids through `SqliteStorage` and assert under 250 ms with 20 records. Add test `corrupt_tool_result_does_not_fail_prompt_records`: overwrite one tool-result row's `content_json` with invalid JSON via `conn_for_test()`, then assert that `get_user_prompts` still returns all prompts. Verify with `cargo t-sqlite -E 'test(/large_tool_output_does_not_slow_prompt_records|corrupt_tool_result_does_not_fail_prompt_records/)'`. Scenarios: `session-prompt-records` / Large tool output does not slow prompt records.
- [x] 2.6 Add `configure_read_cache` and its two constants to `sqlite_storage/mod.rs` (design decision 5) and call it in `connect_with_options` after `foreign_keys`, for both `migrate` values; it skips `:memory:` and empty paths. Add test `database_file_uses_read_cache`: open a store on a temp file with `connect`, then reopen it with `connect_with_options(path, false)`; on each `conn_for_test()` assert `PRAGMA cache_size` = -65536 and `PRAGMA mmap_size` = 268435456. Verify with `cargo t-sqlite -E 'test(/database_file_uses_read_cache/)'`. Scenarios: `session-prompt-records` / Database file uses the read cache.
- [x] 2.7 Wrap the SQLite `get_history` and `get_user_prompts` in `session_store.get_history` / `session_store.get_user_prompts` spans. Each span has `session.id`, plus `rows` and `bytes` recorded after the blocking read returns (design decision 7). Add `tracing-subscriber = "0.3"` to agent-sqlite's dev-dependencies, then run `cargo hakari generate` and `cargo hakari manage-deps`. Add tests `prompt_record_read_emits_a_span` (3 prompts → rows 3, bytes = summed prompt `content_json` length) and `full_history_read_emits_a_span` (10 parts → rows 10, bytes = summed parts length), using a test `tracing_subscriber::Layer` that captures span names and recorded fields. Verify with `cargo t-sqlite -E 'test(/prompt_record_read_emits_a_span|full_history_read_emits_a_span/)'` and `cargo hakari verify`. Scenarios: `session-prompt-records` / Prompt record read emits a span, Full history read emits a span.

## 3. Callers use the targeted read

- [x] 3.1 Make the `GetUserPrompts` handler in `crates/agent-runtime/src/agent/session_actor.rs` return `history_store().get_user_prompts(&self.session_id, msg.message_ids.as_deref())`. To write undecodable rows on the host, give `AgentConfigFixture` (`crates/agent-runtime/src/agent/remote/test_helpers.rs`) a `storage: Arc<SqliteStorage>` field and add a helper next to `remote_session_ref_with_host` that also returns it. Add these tests in `crates/agent/src/runtime_tests/remote_session_actor_ref.rs` (`cfg(all(test, feature = "remote"))`), driven over the mesh:
  - `peer_prompt_request_selects_requested_ids`: two prompt ids, an assistant id and an unknown id give two records in position order.
  - `corrupt_tool_result_does_not_fail_a_peer_prompt_request`: the host session holds an undecodable tool result (written via the host storage's `conn_for_test()`), and the reply still contains the requested records.

  Verify with `cargo t-agent -E 'test(/peer_prompt_request_selects_requested_ids|corrupt_tool_result_does_not_fail_a_peer_prompt_request/)'`. Scenarios: `session-prompt-records` / Requested ids select their records (peer path), Corrupt tool result does not fail a peer prompt request.
- [x] 3.2 In `crates/agent-runtime/src/session/load_snapshot.rs`:
  - Full load: call `get_user_prompts(session_id, None)`.
  - Windowed load: pass `None` when `read.history.complete`, otherwise `Some(&prompt_message_ids(&audit.events))`, and drop the timestamp filter.
  - Keep the `SessionNotFound` tolerance for journal-backed sessions.

  Add these tests in that module, against `SqliteStorage` through `load_session_snapshot_windowed`:
  - `full_load_includes_every_prompt`: 30 prompts.
  - `windowed_load_includes_only_window_prompts`: 30 runs, window of 20 turns, exactly the 20 window prompts.
  - `windowed_load_of_a_short_session_includes_every_prompt`: 5 runs, `history.complete`, all 5 prompts.
  - `window_first_prompt_shares_a_second_with_an_older_prompt`: the window's first run and an older prompt share a timestamp second; the window's prompt is included and the older one is not.
  - `corrupt_tool_result_does_not_fail_a_load`: an undecodable tool-result row; the load succeeds with prompt records.

  Existing tests `snapshot_history_failure_is_propagated` and `bookmarked_unattached_session_tolerates_missing_history` must keep passing unchanged.

  Verify with `cargo t-runtime -E 'test(/full_load_includes_every_prompt|windowed_load_includes_only_window_prompts|windowed_load_of_a_short_session_includes_every_prompt|window_first_prompt_shares_a_second_with_an_older_prompt|corrupt_tool_result_does_not_fail_a_load|snapshot_history_failure_is_propagated|bookmarked_unattached_session_tolerates_missing_history/)'`. Scenarios: `session-prompt-records` / Full load includes every prompt, Windowed load includes only window prompts, Windowed load of a short session includes every prompt, Window first prompt shares a second with an older prompt, Corrupt tool result does not fail a load.
- [x] 3.3 In `crates/agent-runtime/src/acp/stdio.rs`:
  - Change `stored_user_prompt_blocks` to take `Vec<UserPromptRecord>`.
  - `replay_loaded_session` uses `session_ref.get_user_prompts(None)`.
  - `actorless_replay_sources` uses `history_store().get_user_prompts(session_id, None)`.
  - Keep the `session.history.load` spans.
  - Update `assert_structured_user_message_replay` to build records.

  Add test `corrupt_tool_result_does_not_fail_session_update_replay`: a local session with a prompt and an undecodable tool result; `replay_loaded_session` succeeds and the user message chunks carry the structured blocks.

  Verify with `cargo t-runtime -E 'test(/corrupt_tool_result_does_not_fail_session_update_replay|structured_user_message_replay|replay_loaded_session/)'` plus the existing replay tests in the facade (`cargo t-agent worktree_ops_ext`). Scenarios: `session-prompt-records` / Corrupt tool result does not fail session update replay.
- [x] 3.4 Update `CHANGELOG.md` under `## Unreleased`:
  - `### Fixed`, one entry covering: faster prompt records for remote and local loads; corrupt non-prompt parts no longer failing loads, replay and peer prompt requests; windowed loads no longer including same-second older prompts.
  - `### Behavior changes`, one entry covering: migration `0037_session_message_read_indexes` (adds two indexes, drops `idx_messages_session`, runs on start) and the 64 MiB page cache / 256 MiB mmap on session database files.

  Verify that both entries are present.

## 4. Package checks

- [x] 4.1 Run `cargo t-core`, `cargo t-sqlite`, `cargo t-runtime` and `cargo t-agent`. Verify: all pass; record the pass/fail/skip counts here. The services package depends on sqlite as a dev-dependency, so also run `cargo t-services` (CI retests it on a sqlite change).
  - Results (nextest, CI feature sets): `t-core` 273 passed; `t-sqlite` 226 passed; `t-runtime` 2418 passed, 6 skipped; `t-agent` 527 passed, 1 skipped; `t-services` 1006 passed, 4 skipped. A first `t-runtime` run failed `connect_budget_defaults_are_bounded_and_reason_specific` only because `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS` was set in my shell; it passes with the variable unset.
- [x] 4.2 Run `cargo fmt -p querymt-agent-core -p querymt-agent-sqlite -p querymt-agent-runtime -p querymt-agent -- --check` and `cargo cl-core`, `cargo cl-sqlite`, `cargo cl-runtime`, `cargo cl-agent`. Verify: all clean.
- [x] 4.3 Run downstream compile checks: `cargo check -p querymt-mobile-ffi` and `PYO3_PYTHON=<python>=3.10> cargo check -p querymt-agent-py`. Verify: both compile.
- [x] 4.4 Measure on an online backup copy of a real `sessions.db` (never the original): open it with this build's `SqliteStorage::connect` so 0037 runs, record the migration time, and time `get_user_prompts` and `get_history` for the largest session. Verify: plans match design decisions 3 and 3a; record the numbers here.
  - Measured on an online backup (`sqlite3 -readonly ... .backup`) of the 3.6 GB local `sessions.db`, with 0037 removed first so this build ran it on open. The original was never opened for writing. Release build, largest session (2,350 messages), warm cache.
  - Migration 0037 on `SqliteStorage::connect`: 0.36 s.
  - `get_user_prompts`: 43 records in 1.0-1.4 ms. `get_history`: 2,350 messages in 13-14 ms. The 43 records equal `user_prompt_records(get_history)` (asserted).
  - Plans: history messages `SEARCH messages USING INDEX idx_messages_session_order`; history parts `SEARCH message_parts USING INDEX idx_message_parts_message`, no temp B-tree; prompt query `COVERING INDEX idx_messages_session_order` plus `idx_message_parts_prompt`, with only the final `USE TEMP B-TREE FOR ORDER BY` over the returned rows. This matches decisions 3 and 3a.
  - The temporary measuring example was deleted afterwards; the NAS effect remains in the workflow follow-up.
- [x] 4.5 Run `openspec validate fast-session-prompt-records --strict`. Verify: valid.

## Workflow follow-up

- Deploy to the NAS owner and reload session `01a1cf38-5e8e-7198-b1af-3817d0cf1ec6` from the LAN viewer. In otelite, check that `remote.session_ref.get_user_prompts` drops from about 12 s to milliseconds, and that the owner's `session_store.get_user_prompts` span shows small rows/bytes. If it doesn't, run `EXPLAIN QUERY PLAN` on the NAS database and check `PRAGMA cache_size` / `mmap_size` on the live connection.
- Consider the deferred non-goals (viewer-side prompt cache, not waiting on prompts during a load, a separate read connection) only if the re-measurement still shows prompt fetches on the critical path.
