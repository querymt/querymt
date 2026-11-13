# Tasks

Verification commands use the querymt-agent CI feature set. Prepare the dashboard fixture once per shell:

```sh
export QMT_UI_DIST="$(mktemp -d)/dashboard-ui"; mkdir -p "$QMT_UI_DIST"
printf '<!doctype html><title>QueryMT</title>\n' > "$QMT_UI_DIST/index.html"
printf '{"revision":"ci","target":"embedded","acpWebSocketPath":"/acp/ws"}\n' > "$QMT_UI_DIST/querymt-ui.json"
export F='dashboard,oauth,zbus-secret-service,systemd-credentials,remote'
```

The zbus/systemd features only take effect on Linux, so on macOS those paths stay unverified.

## 1. Store read for prompt records

- [ ] 1.1 Add `SessionStore::get_user_prompts(session_id, message_ids: Option<&[String]>)` to `crates/agent/src/session/store.rs`, with the default implementation from design decision 1 (`get_history` + `user_prompt_records` + `filter_prompt_records`). Verify with `cargo check -p querymt-agent --features "$F" --all-targets`: `MockSessionStore` and every other implementor compile unchanged.
- [ ] 1.2 In `crates/agent/src/session/sqlite_storage/session_store.rs` `get_history`, order messages by `created_at, id` and parts by `message_id, sort_order` (design decision 3). Add test `same_second_messages_keep_insertion_order` in `crates/agent/src/session/sqlite_storage/tests.rs`: store three messages with the same `created_at` and assert that `get_history` returns them in insertion order. Scenarios: `session-prompt-records` / Same-second messages keep insertion order (history half).
- [ ] 1.3 Override `get_user_prompts` for `SqliteStorage` with the query from design decision 2. Pass `message_ids` as one JSON-array parameter (`NULL` for all), resolve the session via `resolve_session_internal_id`, and decode only the returned prompt/steering parts. Add these tests in `crates/agent/src/session/sqlite_storage/tests.rs`, each comparing against `user_prompt_records(&get_history(..))` where relevant:
  - `prompt_and_steering_messages_produce_records`: a prompt with text and image blocks, an assistant message with tool use, and a steering message.
  - `record_position_counts_every_message`: positions 0 and 4.
  - `requested_ids_select_their_records`: two prompt ids, one assistant id and one unknown id give exactly two records in position order.
  - `request_without_ids_returns_every_prompt`: 5 prompts and 1 steering message give 6 records.
  - Extend `same_second_messages_keep_insertion_order`: prompt positions equal their indexes in `get_history`.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/prompt_and_steering_messages_produce_records|record_position_counts_every_message|requested_ids_select_their_records|request_without_ids_returns_every_prompt|same_second_messages_keep_insertion_order/)'`. Scenarios: `session-prompt-records` / Prompt and steering messages produce records, Record position counts every message, Requested ids select their records, Request without ids returns every prompt, Same-second messages keep insertion order.
- [ ] 1.4 Add test `large_tool_output_does_not_slow_prompt_records`: 1,000 messages holding 50 MB of tool-result parts and 20 prompts; time `get_user_prompts` for the 20 prompt ids through `SqliteStorage` and assert under 250 ms with 20 records. Add test `corrupt_tool_result_does_not_fail_prompt_records`: overwrite one tool-result row's `content_json` with invalid JSON via `conn_for_test()`, then assert that `get_user_prompts` still returns all prompts. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/large_tool_output_does_not_slow_prompt_records|corrupt_tool_result_does_not_fail_prompt_records/)'`. Scenarios: `session-prompt-records` / Large tool output does not slow prompt records.
- [ ] 1.5 Wrap `get_history` and the SQLite `get_user_prompts` in `session_store.get_history` / `session_store.get_user_prompts` spans. Each span has `session.id`, plus `rows` and `bytes` recorded after the blocking read returns (design decision 5). Add tests `prompt_record_read_emits_a_span` (3 prompts → rows 3, bytes = summed prompt `content_json` length) and `full_history_read_emits_a_span` (10 parts → rows 10, bytes = summed parts length), using a test `tracing_subscriber::Layer` that captures span names and recorded fields. Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/prompt_record_read_emits_a_span|full_history_read_emits_a_span/)'`. Scenarios: `session-prompt-records` / Prompt record read emits a span, Full history read emits a span.

## 2. Callers use the targeted read

- [ ] 2.1 Make the `GetUserPrompts` handler in `crates/agent/src/agent/session_actor.rs` return `history_store().get_user_prompts(&self.session_id, msg.message_ids.as_deref())`. Add these tests in `crates/agent/src/agent/remote/session_actor_ref_remote_tests.rs` (`cfg(all(test, feature = "remote"))`), driven over the mesh through `remote_session_ref_with_host`:
  - `peer_prompt_request_selects_requested_ids`: two prompt ids, an assistant id and an unknown id give two records in position order.
  - `corrupt_tool_result_does_not_fail_a_peer_prompt_request`: the host session holds an undecodable tool result (written via the host storage's `conn_for_test()`), and the reply still contains the requested records.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/peer_prompt_request_selects_requested_ids|corrupt_tool_result_does_not_fail_a_peer_prompt_request/)'`. Scenarios: `session-prompt-records` / Requested ids select their records (peer path), Corrupt tool result does not fail a peer prompt request.
- [ ] 2.2 In `crates/agent/src/session/load_snapshot.rs`:
  - Full load: call `get_user_prompts(session_id, None)`.
  - Windowed load: pass `None` when `read.history.complete`, otherwise `Some(prompt_message_ids(&audit.events))`, and drop the timestamp filter.
  - Keep the `SessionNotFound` tolerance for journal-backed sessions.

  Add these tests in that module, against `SqliteStorage` through `load_session_snapshot_windowed`:
  - `full_load_includes_every_prompt`: 30 prompts.
  - `windowed_load_includes_only_window_prompts`: 30 runs, window of 20 turns, exactly the 20 window prompts.
  - `windowed_load_of_a_short_session_includes_every_prompt`: 5 runs, `history.complete`, all 5 prompts.
  - `window_first_prompt_shares_a_second_with_an_older_prompt`: the window's first run and an older prompt share a timestamp second; the window's prompt is included and the older one is not.
  - `corrupt_tool_result_does_not_fail_a_load`: an undecodable tool-result row; the load succeeds with prompt records.

  Existing tests `snapshot_history_failure_is_propagated` and `bookmarked_unattached_session_tolerates_missing_history` must keep passing unchanged.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/full_load_includes_every_prompt|windowed_load_includes_only_window_prompts|windowed_load_of_a_short_session_includes_every_prompt|window_first_prompt_shares_a_second_with_an_older_prompt|corrupt_tool_result_does_not_fail_a_load|snapshot_history_failure_is_propagated|bookmarked_unattached_session_tolerates_missing_history/)'`. Scenarios: `session-prompt-records` / Full load includes every prompt, Windowed load includes only window prompts, Windowed load of a short session includes every prompt, Window first prompt shares a second with an older prompt, Corrupt tool result does not fail a load.
- [ ] 2.3 In `crates/agent/src/acp/stdio.rs`:
  - Change `stored_user_prompt_blocks` to take `Vec<UserPromptRecord>`.
  - `replay_loaded_session` uses `session_ref.get_user_prompts(None)`.
  - `actorless_replay_sources` uses `history_store().get_user_prompts(session_id, None)`.
  - Keep the `session.history.load` spans.
  - Update `assert_structured_user_message_replay` to build records.

  Add test `corrupt_tool_result_does_not_fail_session_update_replay`: a local session with a prompt and an undecodable tool result; `replay_loaded_session` succeeds and the user message chunks carry the structured blocks.

  Verify with `cargo nextest run -p querymt-agent --features "$F" -E 'test(/corrupt_tool_result_does_not_fail_session_update_replay|structured_user_message_replay|replay_loaded_session/)'` plus the existing replay tests in `agent/handle/tests/worktree_ops_ext.rs` (`cargo nextest run -p querymt-agent --features "$F" worktree_ops_ext`). Scenarios: `session-prompt-records` / Corrupt tool result does not fail session update replay.
- [ ] 2.4 Add an Unreleased `### Fixed` entry to `CHANGELOG.md` covering three things:
  - faster prompt records for remote and local loads
  - corrupt non-prompt parts no longer failing loads, replay and peer prompt requests
  - windowed loads no longer including same-second older prompts

  Verify that the entry is present under `## Unreleased` → `### Fixed`.

## 3. Package checks

- [ ] 3.1 Run `cargo nextest run -p querymt-agent --features "$F"`. Verify: all pass; record the pass/fail/skip counts here.
- [ ] 3.2 Run `cargo fmt -p querymt-agent -- --check` and `cargo clippy -p querymt-agent --all-targets --features "$F" -- -D warnings`. Verify: both are clean.
- [ ] 3.3 Run downstream compile checks (from `cargo tree -i querymt-agent --workspace --depth 1`): `cargo check -p querymt-mobile-ffi` and `PYO3_PYTHON=<python>=3.10> cargo check -p querymt-agent-py`. Verify: both compile.
- [ ] 3.4 Run `openspec validate fast-session-prompt-records --strict`. Verify: valid.

## Workflow follow-up

- Deploy to the NAS owner and reload session `01a1cf38-5e8e-7198-b1af-3817d0cf1ec6` from the LAN viewer. In otelite, check that `remote.session_ref.get_user_prompts` drops from about 12 s to milliseconds, and that the owner's `session_store.get_user_prompts` span shows small rows/bytes. If it doesn't, run `EXPLAIN QUERY PLAN` on the NAS database.
- Consider the deferred non-goals (viewer-side prompt cache, not waiting on prompts during a load) only if the re-measurement still shows prompt fetches on the critical path.
