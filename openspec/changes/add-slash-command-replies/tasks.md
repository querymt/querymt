# Tasks

Prerequisites: `add-slash-command-records` is done; `slash-command-actions` tasks 1.1 and 1.2 and `slash-command-transcript` are done (group 3 needs them).

Runtime commands use the CI feature set `remote,oauth,plugin-loaders,api` (`cargo t-runtime`, `cargo cl-runtime`). Facade commands (`cargo t-agent`, `cargo cl-agent`) never build the wasm UI and need no `QMT_UI_DIST` (see `.cargo/config.toml`). App paths are relative to `crates/querymt-gpui`.

Tasks 1.1, 1.2, 2.1 and 2.2 were `add-slash-command-records` tasks 3.4, 3.5, 4.3 and 4.4.

## 1. Agent (`querymt-agent-runtime`, `querymt-agent`)

- [ ] 1.1 Emit `SlashCommandReply` for `CompactionOutcome::NothingToCompact` and `CompactionOutcome::Blocked` in place of `HookNotice` (design D1). Covers `slash-commands` / Compact on an empty conversation replies, Compact blocked by a hook replies, Failed command prompt carries no record. Tests named after the scenarios. Add the **BREAKING** entry to `CHANGELOG.md` Unreleased. Verify: `cargo t-runtime -E 'test(/compact_on_an_empty_conversation_replies|compact_blocked_by_a_hook_replies|failed_command_prompt_carries_no_record/)'`.
- [ ] 1.2 Move `client_prompt_id_from_meta` where the worktree handler can call it. In `handle/ext_worktree_ops.rs`, build the record in `handle_native_worktree_prompt`, set it and `code` on `WorktreeCommandOutput` and `WorktreeCommandCompleted`, and pass it through `run_native_command` to `announce_handoff` and `announce_archived` (design D2). Covers `slash-commands` / Worktree list reply carries the record, Worktree reply carries the client prompt id, Refused worktree command carries its error code, Handoff announcement carries the record, Archive issued from another session, Replayed worktree publish reply keeps its record, Transient reply is not replayed. Tests named after the scenarios in `crates/agent/src/runtime_tests/handle/worktree_ops_ext.rs`. Verify: `cargo t-agent -E 'test(/worktree_list_reply_carries_the_record|worktree_reply_carries_the_client_prompt_id|refused_worktree_command_carries_its_error_code|handoff_announcement_carries_the_record|archive_issued_from_another_session|replayed_worktree_publish_reply_keeps_its_record|transient_reply_is_not_replayed/)'`.
- [ ] 1.3 Package checks: `cargo t-runtime`, `cargo fmt -p querymt-agent-runtime -- --check`, `cargo cl-runtime`, `cargo t-agent -E 'test(/slash|worktree|compact/)'`, `cargo cl-agent`. Record pass, fail and skip counts.

## 2. Session view (`querymt-agent-session-core`)

- [ ] 2.1 Add `EntryKind::CommandReply`, `command_reply_chunk` and its id index, and let it open a turn in `store.rs` `Store::push` (design D3). Covers `session-view-state` / Command reply is not an assistant message, Command reply does not join the preceding assistant message, Command reply exposes its error code, Agent message without a record stays an assistant message, Command reply outside a run starts a turn, Command reply during a run joins the running turn. Tests named after the scenarios in `tests/live_updates.rs` and `tests/runs.rs`. Verify: `cargo nextest run -p querymt-agent-session-core --features test-support -E 'test(/command_reply_is_not_an_assistant_message|command_reply_does_not_join_the_preceding_assistant_message|command_reply_exposes_its_error_code|agent_message_without_a_record_stays_an_assistant_message|command_reply_outside_a_run_starts_a_turn|command_reply_during_a_run_joins_the_running_turn/)'`.
- [ ] 2.2 Replace the pending prompt by the entry that names its client prompt id, and keep `prompt_completed` ending the submission (design D3, Pending prompt). Covers `session-view-state` / Command reply replaces the pending prompt, Manual compaction replaces the pending prompt, Run state returns to idle after a command answered without a run. Tests named after the scenarios in `tests/runs.rs`; update `prompt_answered_without_a_run_settles_after_earlier_runs` only if its input no longer describes an agent without records. Verify: `cargo nextest run -p querymt-agent-session-core --features test-support -E 'test(/command_reply_replaces_the_pending_prompt|manual_compaction_replaces_the_pending_prompt|run_state_returns_to_idle_after_a_command_answered_without_a_run/)'`.
- [ ] 2.3 Regenerate the golden traces if needed (`cargo run -p querymt-agent-session-core --example regen-traces --features test-support`), then package checks: `cargo nextest run -p querymt-agent-session-core --features test-support`, `cargo fmt -p querymt-agent-session-core -- --check`, `cargo clippy -p querymt-agent-session-core --all-targets --features test-support -- -D warnings`, `cargo check -p querymt-gpui --all-targets --locked`.

## 3. App (`crates/querymt-gpui`)

- [ ] 3.1 Add `ItemKind::Command` with `CommandState` (design D4) and project `EntryKind::CommandReply` into it, with JSON output indented. Verify: projection tests `worktree_list_and_its_output`, `refused_command`, `reply_stored_by_the_agent_survives_a_reload`, `reply_the_agent_does_not_store_is_gone_after_a_reload`, `compact_on_an_empty_conversation`.
- [ ] 3.2 Project a pending or failed local prompt that names an advertised command with `is_action()` as `ItemKind::Command` in `Running` or `Failed` (design D5); a command without a descriptor stays a prompt bubble. Verify: projection tests `pending_action_command`, `pending_prompt_command`, `failed_action_command`, `pending_command_without_a_descriptor`, `compact_shows_no_prompt_bubble`.
- [ ] 3.3 Draw `ItemKind::Command`: the row, its three states and the output block with the density's line cap and Copy on hover (design D6). Verify: session view tests that the row has no prompt-bubble fill (by `debug_selector`), that a `Failed` row shows the alert mark and its message, that a `Running` row shows the spinner, and that long output is clipped with "more".
- [ ] 3.4 Consumers (design D7): `src/quick/activity.rs` `observe` ends the scan at a command row; `activity`, the row hash and the row-kind label in `src/ui/session_view.rs`; `Row::Command` in `src/session/golden.rs`, then regenerate the golden files with `UPDATE_GOLDEN=1`. Verify: test `error_before_a_command_row_is_not_current`; a `Running` command row gives "Running /worktree…"; the golden diff only changes cases with command replies.
- [ ] 3.5 Package checks: `cargo nextest run --package querymt-gpui --bins --locked`, `cargo clippy --package querymt-gpui --all-targets --locked -- -D warnings`, `cargo fmt -p querymt-gpui -- --check`, `RUSTC_BOOTSTRAP=1 cargo build --locked --bin querymt-gpui --target wasm32-unknown-unknown`.
- [ ] 3.6 By hand on macOS, light and dark themes, both densities:
  - `/worktree list` shows as a command row with indented output and is gone after a reload;
  - `/worktree publish` is still a command row after a reload;
  - `/compact keep X` shows one compaction row and no prompt bubble;
  - `/compact` in an empty session shows a command row with the reply;
  - an agent without records shows prompts and replies as before.
  Verify: each item noted here as pass or fail.

## 4. Integration

- [ ] 4.1 Run `openspec validate add-slash-command-replies --type change --strict --no-interactive`. Verify: it passes.

## Workflow follow-up

- Archive after `add-agent-session-core`, `add-slash-command-records`, `slash-command-actions` and `slash-command-transcript`.
- If `add-agents-slash-command` lands before this change, switch its output to `SlashCommandReply` here; if after, that change does.
- Report the querymt-desktop follow-up from proposal.md, Impact.
