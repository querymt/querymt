# Tasks

## 1. Repository operations (querymt-agent-services)

- [x] 1.1 Add `RepositoryOps::commit_tree(common_dir, commit) -> Option<String>` (gix peel to tree) to the trait, `GixRepositoryOps`, the `repo_ops.rs` test double and the `lifecycle.rs` / `capture.rs` test wrappers; verify with a `repo_ops.rs` unit test `commit_tree_resolves_the_commit_tree` (`cargo t-services -E 'test(/commit_tree_resolves_the_commit_tree/)'`)
  - Verified: `cargo nextest run -p querymt-agent-services -E 'test(/commit_tree_resolves.../)'` 1 passed. Note: the trait impl and test double are `GixRepoOps` / `FaultInjectingRepoOps`.
- [x] 1.2 Add the `RemoveCheckout { RefuseDirty, DiscardPreserved }` argument to `RepositoryOps::remove_worktree` (gix `Force::Never` / `Force::DiscardChanges`), pass `RefuseDirty` from every existing caller, and make the test double record the mode; verify with `repo_ops.rs` test `remove_worktree_refuses_dirty_unless_discarding` (dirty checkout refused with `RefuseDirty`, removed with `DiscardPreserved`, locked worktree kept in both)
  - Verified: `remove_worktree_refuses_dirty_unless_discarding` passed; `FaultInjectingRepoOps::removal_modes()` records the mode.
- [x] 1.3 Pass the private commit message into `RepositoryOps::capture_working_state` and `capture::capture_working_state_validated`; baseline capture keeps "QueryMT private baseline", `WorkspaceService::publish_result` passes "QueryMT result <workspace-id>"; verify with `service.rs` test `published_result_commit_has_its_own_message` and that the existing capture tests still pass (`cargo t-services -E 'test(/capture|publish/)'`)
  - Verified: `published_result_commit_has_its_own_message` passed; the full `cargo t-services` run covers the capture tests (1052 passed, 4 skipped). `BASELINE_COMMIT_MESSAGE` is exported from `repo_ops`.

## 2. Removal accepts published working state (querymt-agent-services)

Covers git-worktree-workspaces / Remove after publishing uncommitted work, Edited after publishing, Ignored files with a matching publication, Recovery of an interrupted removal of published work, Dirty or active workspace removal.

- [x] 2.1 Add `stale_result: Option<String>` to `WorktreeError::UnpreservedContent` with the message from design D3 (commit, publish or delete; stale-result sentence when set); update every constructor; verify with `error.rs` test `unpreserved_content_names_publish_and_stale_result` (code stays `unpreserved_content`)
  - Verified: `unpreserved_content_names_publish_and_stale_result` passed.
- [x] 2.2 Implement design D1 in `WorkspaceManager::remove_locked` step 2 and select `RemoveCheckout::DiscardPreserved` in step 4 only when `preserved_by` is set; add `preserved_by` to `RemovalReport` (and its `Display`); write red tests first in `lifecycle.rs`: `remove_after_publishing_uncommitted_work` (succeeds, checkout gone, `preserved_by` = result ref, ref retained, result review still lists the edits), `edited_after_publishing` (refused, `stale_result` names the newest result, files intact), `ignored_files_with_a_matching_publication` (refused, ignored path listed), and assert via the test double that no `DiscardPreserved` removal happens in the refused cases
  - Verified: `remove_after_publishing_uncommitted_work`, `edited_after_publishing`, `ignored_files_with_a_matching_publication`, `unpublished_dirty_checkout_names_no_stale_result` passed. Existing `service.rs` acceptance tests (`production_lifecycle_works_without_a_git_executable`, `multiprocess_workspace_acceptance`) asserted that a published dirty checkout is refused; they now make an unpublished edit after publishing before expecting the refusal (spec: only unmatched working state is refused).
- [x] 2.3 Cover interrupted removal of published work: extend the existing interrupted-removal test pattern (`lifecycle.rs` around the `Removing` recovery test) as `recovery_of_an_interrupted_removal_of_published_work` (recovery re-checks and completes when the tree still matches, refuses after an edit); verify with `cargo t-services -E 'test(/published|interrupted_removal/)'`
  - Verified: `recovery_of_an_interrupted_removal_of_published_work` passed (RemoveWorktree fault leaves `Removing`; edit refused; restored content completes).
- [x] 2.4 Add `preserved_by` (skipped when `None`) to `WorktreeRemoveResponse` in `ops.rs` `removal_response`; verify the existing ops tests and `py/querymt-agent-py` build still compile (`cargo check -p querymt-agent-py` with `PYO3_PYTHON` set to Python ≥ 3.10)
  - Verified: `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py` finished OK.
- [x] 2.5 Update `docs/docs/agent/worktrees.md`: removal after publish, the `preserved_by` field in the removal report section, and the `unpreserved_content` row in the error table; verify the documented commands match `NATIVE_USAGE`
  - Verified: no new commands documented; `NATIVE_USAGE` unchanged (`remove [id] [--archive]`, `publish [id]`).
- [x] 2.6 Package checks: `cargo check -p querymt-agent-services`, `cargo t-services`, `cargo fmt -p querymt-agent-services -- --check`, `cargo cl-services`
  - Verified: `cargo check -p querymt-agent-services --tests` OK; `cargo t-services` 1052 passed, 4 skipped; fmt check OK; `cargo cl-services` clean.

## 3. Protocol events and replay (querymt-agent-protocol-types)

- [x] 3.1 Add `AgentEventKind::WorktreeCommandCompleted { id, command, code, archived, text }` (durable, typeshare, `as_str` = `worktree_command_completed`) and `WorktreeArchived.removal: Option<ArchiveRemoval>` (`Removed` | `Refused { code, message }`, omitted when `None`); verify with an `events.rs` serde round-trip test `worktree_command_completed_round_trips` and the durability classification test
  - Verified: `worktree_command_completed_round_trips` (durable, tag, wire shape), `worktree_archived_removal_is_optional_and_tagged`, `event_kind_tag_matches_serde_tag` (106 samples) passed.
- [x] 3.2 Project both in `replay.rs` `replayable_updates`: `WorktreeCommandCompleted` → `AgentMessageChunk` with `messageId = worktree-cmd-<id>` and `_meta.querymt.worktreeCommand = { command, code?, archived? }`; `worktree_archived_chunk` renders the three texts of design D6 (no "run `/worktree remove`" when removal was attempted); verify with `replay.rs` tests `mutating_command_reply_is_replayed` (identity and meta) and `archived_update_replayed_with_the_removal_outcome`
  - Verified: both passed in `cargo t-protocol` (127 passed). `worktree_archived_chunk` now takes `Option<&ArchiveRemoval>`; new `worktree_command_chunk`.
- [x] 3.3 Extend `crates/agent-session-core/tests/snapshot.rs` `workspace_announcements_are_conversation_entries` with a `worktree_command_completed` event; verify with `cargo nextest run -p querymt-agent-session-core -E 'test(/workspace_announcements/)'`
  - Verified: needs `--features test-support`; `cargo nextest run -p querymt-agent-session-core --features test-support` 133 passed (filtered run: 1 passed).
- [x] 3.4 Package checks: `cargo check -p querymt-agent-protocol-types`, `cargo t-protocol`, `cargo fmt -p querymt-agent-protocol-types -- --check`, `cargo cl-protocol`
  - Verified: check OK, `cargo t-protocol` 127 passed, fmt check OK, `cargo cl-protocol` clean.

## 4. Deferred archive announcement (querymt-agent-services, querymt-agent-runtime)

Covers git-worktree-workspaces / One-step cleanup, Archive succeeds, removal refused, Archived update replayed with the removal outcome.

- [x] 4.1 Change `OwnerHost::announce_archived` to take the removal outcome; reorder `WorktreeOps::remove_announced` to quiesce → `archive_held` → `remove_held` → announce with outcome, returning the announcement on both success and `RemovalRefused`; `archive_announced` passes `None`; update the runtime `OwnerHost for LocalAgentHandle` impl; verify `cargo check -p querymt-agent-services -p querymt-agent-runtime --features querymt-agent-runtime/remote,querymt-agent-runtime/oauth,querymt-agent-runtime/plugin-loaders,querymt-agent-runtime/api`
  - Verified: that `cargo check` finished OK. `remove_announced` returns a `RemoveOutcome { result, announcement }`; the removal outcome type is `querymt_agent_core::events::ArchiveRemoval`.
- [x] 4.2 In `run_native_command`, `NativeCommand::Remove` uses the announcement as the reply when it is on the issuing session's stream (as `Archive` does), on both success and refusal; verify with facade tests in `crates/agent/src/runtime_tests/handle/worktree_ops_ext.rs`: extend `remove --archive` coverage as `archive_succeeds_removal_refused` (native command from the dirty owner: one `WorktreeArchived` with `removal = Refused { unpreserved_content, .. }`, recorded after the removal attempt, delivered before the prompt response, workspace archived, files intact) and `one_step_cleanup` (clean owner: `removal = Removed`)
  - Verified: `QMT_UI_DIST=/Users/wiking/qmt/querymt-desktop/build-embedded cargo t-agent -E 'test(/archive_succeeds_removal_refused|one_step_cleanup/)'` passed (also `remove_after_publishing_through_the_native_command`).

## 5. Durable native command replies (querymt-agent-runtime)

Covers git-worktree-workspaces / Mutating command reply is replayed, Refused removal reply survives reload, Announcement replaces the reply, Output is not replayed, Output reaches attached clients before the response, No bridge route registered.

- [x] 5.1 Add `NativeCommand::durable_reply()` and, in `handle_native_worktree_prompt`, record `WorktreeCommandCompleted` via `emit_event_persisted` for mutating commands without an announcement tip (fallback to the transient output on persist failure, logged); keep `WorktreeCommandOutput` for `create`, `list`, `status`, `result` and usage errors
  - Verified: exercised by the 5.2 facade tests.
- [x] 5.2 Facade tests in `worktree_ops_ext.rs`: `mutating_command_reply_is_replayed` (publish, then reload: reply replayed with the same `messageId` and meta), `refused_removal_reply_survives_reload` (`/worktree remove` on a dirty archived workspace from another session, then reload: refusal text with paths replayed), `announcement_replaces_the_reply` (owner `/worktree archive`: exactly one durable update), and adjust `native_command_output_is_a_session_update_covered_by_the_event_tip` (which asserts the output is not journaled) to use an inspection command, adding `output_is_not_replayed` for `list`/`status`/usage errors; verify existing delivery-order and no-bridge tests still pass (`QMT_UI_DIST=<fixture> cargo t-agent -E 'test(/worktree_ops_ext/)'`)
  - Verified: `mutating_command_reply_is_replayed`, `refused_removal_reply_survives_reload`, `announcement_replaces_the_reply`, `output_is_not_replayed` passed; `native_command_output_is_a_session_update_covered_by_the_event_tip` already used `/worktree list` (an inspection command), so it is unchanged. `cargo t-agent -E 'test(/worktree/)'`: 71 passed.
- [x] 5.3 Update `docs/docs/agent/worktrees.md` (native output section, currently "never journaled or replayed") and add a **BREAKING** entry under "Unreleased" in `CHANGELOG.md` for the durable replies and the archived-update change; verify the docs build if mkdocs is available, otherwise review the rendered Markdown
  - Verified: mkdocs not run; Markdown reviewed (mermaid block for the removal decision, native output section, archive announcement section, cancel note). CHANGELOG entry added under Unreleased / Breaking.

## 6. Cancel on read-only sessions (querymt-agent-runtime)

Covers git-worktree-workspaces / Cancel an archived owner, Cancel a previous owner.

- [x] 6.1 In `send_agent_impl.rs` `cancel`, return `Ok(())` when `session_materializer.read_only_reason` is `Some`, before `execute_session_operation`; verify with facade tests `cancel_an_archived_owner` and `cancel_a_previous_owner` built with the `remote` feature (`cargo t-agent -E 'test(/cancel_an_archived_owner|cancel_a_previous_owner/)'`), and confirm a handoff summary cancel still works (existing `cancelling_the_owner_stops_a_handoff_summary_in_flight`)
  - Verified: both new tests fail without the guard (`session/cancel` returned an error) and pass with it; the handoff-summary cancel test still passes in `cargo t-agent -E 'test(/worktree/)'`.
- [x] 6.2 Runtime package checks: `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api`, `cargo t-runtime -E 'test(/worktree|cancel/)'`, `cargo fmt -p querymt-agent-runtime -- --check`, `cargo cl-runtime`
  - Verified: check OK; `cargo t-runtime -E 'test(/worktree|cancel/)'` 135 passed; fmt check OK; `cargo cl-runtime` clean.

## 7. Integration

- [x] 7.1 Downstream: `cargo nextest run -p querymt-agent-session-core`, `cargo check -p querymt-agent-core`, `QMT_UI_DIST=<fixture> cargo t-agent -E 'test(/worktree/)'`, `cargo t-agent --no-run`, `cargo cl-agent`, and `cargo check -p querymt-agent-py` (PYO3_PYTHON ≥ 3.10)
  - Verified: session-core 133 passed (`--features test-support`); `cargo check -p querymt-agent-core` OK; `cargo t-agent -E 'test(/worktree/)'` 71 passed; `cargo t-agent --no-run` OK; `cargo cl-agent` clean; `cargo check -p querymt-agent-py` OK (Python 3.14). The zbus/systemd features only take effect on Linux and are unverified on macOS.
- [x] 7.2 Manual check against a test instance (never the user's running qmtcode or `~/.qmt/sessions.db`): dirty worktree → `/worktree publish` → `/worktree remove --archive` removes it and reports `preserved_by`; with an edit after publish, the refusal is visible, survives reload, and `session/cancel` returns no error
  - Verified on an isolated candidate (`CARGO_TARGET_DIR=/tmp/qmt-lab-target`, fresh HOME/config/`sessions.db`/port, fixture repo with `[agent.workspace]` enabled; driven with `qmacp`; the user's running qmtcode and `~/.qmt/sessions.db` were not touched, only the lab process was stopped):
    - dirty owner → `/worktree publish` → `/worktree remove --archive`: checkout deleted, reply "archived and removed"; result commit message `QueryMT result <id>`; the same removal issued from another session returned `preserved_by` = result ref, also listed in `retained_refs`.
    - edit after publish → `/worktree remove --archive`: refusal visible ("archived, but removal was refused (unpreserved_content) ... differs from the last published result <ref>; publish again"), files intact, the refusal is replayed on reload, and `qmacp cancel` on the read-only session returned `ok: true`.
    - Observation (not caused by this change; `session_control.rs` is untouched): through the profile-bound runtime, a new prompt to an actor-less archived session is rejected with `unknown session for bound runtime`, so `/worktree status` on an archived owner could not be exercised over qmacp (it is covered by the facade tests).
- [x] 7.3 `openspec validate allow-removal-of-published-worktrees --strict`
  - Verified: `Change 'allow-removal-of-published-worktrees' is valid`.

## Workflow follow-up

- Report the querymt-desktop / querymt-gpui follow-up: render `_meta.querymt.worktreeCommand` replies (error styling by `code`) and switch to read-only live on `worktreeArchived` / `worktreeHandoff` metadata.
- Archive the change after review.
