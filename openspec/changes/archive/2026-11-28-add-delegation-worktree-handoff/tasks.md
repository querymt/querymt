# Tasks

## 1. Wire shape: `handoff_from` on the event

- [x] 1.1 Add `handoff_from: Option<String>` (`serde(default, skip_serializing_if = "Option::is_none")`) to `AgentEventKind::DelegationRequested` in `crates/agent-protocol-types/src/events.rs`; update every constructor and pattern (`tool_calls.rs`, `delegation/core.rs`, `acp/stdio.rs`, `control/delegation_notifications.rs` tests, `history_window.rs`). Extend the existing `DelegationRequested` round-trip test in `delegation_notifications.rs` so an event without the field deserializes and one with it round-trips. Verify `cargo check -p querymt-agent-protocol-types`, `cargo t-protocol`, `cargo t-services -E 'test(/delegation_notifications/)'`, `cargo fmt -p querymt-agent-protocol-types -- --check`, `cargo cl-protocol`.
  - Result: `cargo check -p querymt-agent-protocol-types`; `cargo t-protocol` 133 passed; `cargo t-services -E 'test(/delegation_notifications/)'` 13 passed (new `requested_event_handoff_from_is_additive_and_round_trips`); `cargo fmt` clean; `cargo cl-protocol` clean. Constructors updated in runtime, agent and session-core tests as well.

## 2. Services: split the handoff core (design D2)

- [x] 2.1 In `crates/agent-services/src/worktree/ops.rs`, split `WorktreeOps::handoff_announced` into `begin_handoff(host, target) -> PendingHandoff` and `PendingHandoff::commit(self, ops, successor)`. Rebuild `handoff_announced` on them with unchanged checks, error codes and announcement. Add `WorkspaceService::binding_for(record)` in `service.rs`. Covers git-worktree-workspaces / Clean handoff from an idle owner, Owner still active, Not the owner or not ready, Crash during handoff (unchanged behaviour). Verify `cargo t-services -E 'test(/handoff/)'` and `cargo t-runtime -E 'test(/handoff/)'` pass unchanged.
  - Result: `cargo t-services -E 'test(/handoff/)'` passed; the existing handoff scenarios live in `crates/agent` (`runtime_tests::handle::worktree_ops_ext`): `cargo t-agent -E 'test(/handoff/)'` 15 passed, unchanged. `cargo t-runtime -E 'test(/handoff/)'` matches only the new delegate-tool tests (the runtime package has no pre-existing handoff tests).
- [x] 2.2 Add a services test `pending_handoff_drop_releases_the_lease`: a `PendingHandoff` dropped without commit leaves the owner unchanged and lets `acquire_exclusive_activity` succeed again. Verify `cargo t-services -E 'test(/pending_handoff_drop_releases_the_lease/)'`, `cargo fmt -p querymt-agent-services -- --check`, `cargo cl-services`.
  - Result: `cargo t-services -E 'test(/handoff/)'` 4 passed (`pending_handoff_drop_releases_the_lease`, plus `pending_handoff_commit_transfers_ownership`); `cargo fmt` clean; `cargo cl-services` clean.

## 3. Runtime: find delegate sessions when stopping an owner (design D5)

- [x] 3.1 In `crates/agent-runtime/src/agent/handle/session_control.rs`, make `unload_session_if_idle` and `session_is_busy` fall back to `local_members()` registries (as `owned_session_ref` does) and run the in-runtime step on the member handle that holds the session. Add runtime test `handoff_unloads_an_idle_delegate_owner_in_a_config_runtime`: a config-file root with an isolated member completes a delegation, `/worktree handoff` on the delegate's worktree unloads the member's actor and transfers ownership. Verify `cargo t-runtime -E 'test(/handoff_unloads_an_idle_delegate_owner_in_a_config_runtime/)'` fails before the change and passes after it.
  - Result: `session_control.rs`: `unload_session_if_idle` and `session_is_busy` fall back to the built local member whose registry holds the session. Test `delegation::handoff_tests::handoff_unloads_an_idle_delegate_owner_in_a_config_runtime` (harness: root + isolated members, no profiles) FAILS with the change reverted (also `reviewer_takes_over_a_coders_worktree`, `delegation_handoff_to_a_delegate_session`) and passes with it. The test lives in `querymt-agent-runtime` (`cargo t-runtime`), on the scripted delegation harness extended with a real git repo and workspace service (`TestHarness::new_with_workspace*`).

## 4. Runtime: delegate tool, hook and request (design D1)

- [x] 4.1 Add `handoff_from` to the `delegate` tool schema and `DelegateTool::call` in `agent/builtin_tools/delegate.rs`: string only, rejected with `isolation = "shared"`. Extend the existing schema/argument tests there with `shared_isolation_with_handoff`. Covers isolated-delegation / Shared isolation with handoff. Verify `cargo t-runtime -E 'test(/builtin_tools::delegate/)'`.
  - Result: `cargo t-runtime -E 'test(/builtin_tools::delegate/)'` passed incl. `shared_isolation_with_handoff`, `handoff_from_must_be_a_non_empty_string`, `schema_exposes_handoff_from_as_a_string`.
- [x] 4.2 Add read-only `handoff_from` to `PreDelegationRequest` (`hooks/engine.rs`) and its JSON input. In the `delegate` branch of `agent/execution/tool_calls.rs`, parse it, pass it to the hook, force resolved isolation to isolated, write it into `tool_arguments`, disclose "runs in the existing worktree of delegation <id>" in the tool result, and emit it on `DelegationRequested`. Add test `handoff_is_disclosed_to_the_caller_and_hooks` (hook input, event field, tool result text). Covers isolated-delegation / Handoff is disclosed to the caller and hooks. Verify `cargo t-runtime -E 'test(/handoff_is_disclosed_to_the_caller_and_hooks/)'`.
  - Result: `PreDelegationRequest.handoff_from` + `PreDelegationCommandInput.handoff_from` (nullable string; schema fixture regenerated with the ignored `regenerate_committed_hook_schemas`), parsed in `tool_calls.rs`, forced isolated (a restricted mesh turn refuses a handoff instead of overriding its shared checkout), written to `tool_arguments`, disclosed in the tool result, emitted on `DelegationRequested`. `cargo t-runtime -E 'test(/handoff_is_disclosed_to_the_caller_and_hooks/)'` passes (hook input captured from a real command hook, event field, tool result text); `hooks::tests` 38 passed incl. the regenerated schema fixtures.

- [x] 4.3 Document `handoff_from` in the `pre_delegation` hook input reference and in `docs/docs/agent/delegation.md` (the `delegate` tool arguments). Verify the docs build with `mkdocs build -f docs/mkdocs.yml --strict` if available, otherwise by review.
  - Result: documented in `docs/docs/agent/delegation.md` (new `handoff_from` subsection, the `pre_delegation` paragraph) and `docs/docs/agent/hooks.md` (hook input). `mkdocs` is not installed here, so verified by review only (no `mkdocs build --strict` run); the new anchor `worktrees.md#delegating-to-a-successor-handoff_from` follows the heading slug.

## 5. Runtime: run a delegation by handoff (design D3, D4)

- [x] 5.1 Turn the preallocated workspace of `SessionMaterializer::prepare_new_session_with_workspace` into `Fresh(binding)` / `Handoff { binding, pending }`. For `Handoff`: write the barrier with `handoff_workspace_id`, replace `bind_owner` with `pending.commit`, never mark the workspace failed, and on a start failure after the transfer move ownership back (or leave the barrier and return `handoff_incomplete`). Add `LocalAgentHandle::create_handoff_delegation_session` and `MemberAgent::create_handoff_delegation_session` (local members only). Verify `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api`.
  - Result: `PreallocatedWorkspace::{Fresh, Handoff}` in `session_materializer.rs` (barrier names the workspace via `handoff_workspace_id`, `pending.commit` replaces `bind_owner`, start failure transfers ownership back and deletes the successor, or reports `handoff_incomplete` and keeps the barrier; the workspace is never marked failed). `LocalAgentHandle::create_handoff_delegation_session`, `MemberAgent::create_handoff_delegation_session` (remote refused). `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api` clean; `cargo cl-runtime` clean.

- [x] 5.2 In `delegation/core.rs`, thread `handoff_from` from `handle_envelope` into `execute_delegation_inner`. Add the handoff branch next to isolated allocation: look up the delegation, find its worktree, check the parent, run `begin_handoff` with the root as `OwnerHost`, create the session through the member, then announce `WorktreeOwnershipTransferred` through the root's `HandoffHost::announce_handoff`. Every refusal goes through `fail_delegation`.
  - Result: `handoff_from` threaded from the `DelegationRequested` event into `DelegationContext`; `begin_delegation_handoff` (unknown delegation, `no_isolated_workspace`, `not_parent_session`, then `WorktreeOps::begin_handoff` with the root as `OwnerHost`); the delegate session is created through the member; `announce_handoff` through the root records `WorktreeOwnershipTransferred { summary_seeded: false }`. Every refusal goes through `fail_delegation`; a `handoff_from` whose recorded isolation is not isolated also fails closed.

- [x] 5.3 Print the worktree's `delegation_id` (not the running delegation's id) in the `accept_delegation_result` hint and the `/worktree result` reference of the completion summary.
  - Result: The `accept_delegation_result` hint prints `binding.record.ownership.delegation_id` (the delegation that created the worktree). The `/worktree result` reference already prints the workspace id, which is unchanged by a handoff. Covered by `completion_names_the_worktrees_delegation`.

- [x] 5.4 Add runtime delegation tests on the existing isolated-delegation fixtures (scripted provider, real git repo):
  - `reviewer_takes_over_a_coders_worktree`: same checkout, coder edits present, no new worktree, coder session read-only with `owner_transferred`.
  - `delegation_handoff_to_a_delegate_session`: ownership-transferred event on the coder's stream with the delegate session and `summary_seeded = false`.
  - `delegate_handoff_successor_keeps_delegation_lineage`: parent is the orchestrator, origin `delegation`, not listed as a fork or worktree child.
  - `handoff_from_a_failed_or_cancelled_delegation`.
  - `second_round_after_a_review`.
  - `completion_names_the_worktrees_delegation`.

  Covers isolated-delegation / Reviewer takes over a coder's worktree, Handoff from a failed or cancelled delegation, Second round after a review, Completion names the worktree's delegation; git-worktree-workspaces / Delegation handoff to a delegate session, Delegate handoff successor keeps delegation lineage. Verify `cargo t-runtime -E 'test(/reviewer_takes_over|delegation_handoff_to_a_delegate|delegate_handoff_successor|handoff_from_a_failed|second_round_after|completion_names_the_worktree/)'`.
  - Result: All six tests in `delegation::handoff_tests` (`reviewer_takes_over_a_coders_worktree`, `delegation_handoff_to_a_delegate_session`, `delegate_handoff_successor_keeps_delegation_lineage`, `handoff_from_a_failed_or_cancelled_delegation` (both a failed and a cancelled coder), `second_round_after_a_review`, `completion_names_the_worktrees_delegation`) pass. Fixtures: scripted delegates (the coder's file edits are written into the worktree by the test), real git repo, real `WorkspaceService`. Note: the coder's read-only access is asserted through the registry (`get_workspace_by_session` empty, `transferred_to` = successor); the `owner_transferred` load disclosure itself is covered by the existing `worktree_ops_ext` tests.

- [x] 5.5 Add refusal tests, each asserting the error code, unchanged owner and no new worktree:
  - `handoff_from_another_sessions_delegation`
  - `handoff_from_a_shared_delegation`
  - `owner_still_running` (the running owner is not interrupted)
  - `unknown_delegation_handoff`

  Covers isolated-delegation / Handoff from another session's delegation, Handoff from a shared delegation, Owner still running, Unknown delegation. Verify `cargo t-runtime -E 'test(/handoff_from_another|handoff_from_a_shared|owner_still_running|unknown_delegation_handoff/)'`.
  - Result: `handoff_from_another_sessions_delegation`, `handoff_from_a_shared_delegation`, `owner_still_running` (the running coder is released afterwards and completes), `unknown_delegation_handoff` pass, each asserting the error code, owner unchanged and no new worktree.

- [x] 5.6 Add `delegate_successor_fails_to_start`: inject an actor start failure after the transfer (the registry wrapper used by the existing `attach_owner` failure test in `session_materializer.rs`, or a failing MCP root) and assert the delegation fails, the workspace stays ready and not failed, and it has exactly one owner. Covers git-worktree-workspaces / Delegate successor fails to start. Verify `cargo t-runtime -E 'test(/delegate_successor_fails_to_start/)'`.
  - Result: `delegate_successor_fails_to_start` passes: the failure is injected with a failing MCP attachment source on the reviewer member (the alternative named in the task, not the registry wrapper); the delegation fails, the worktree stays Ready, ownership is back with the coder's session, one worktree, the successor row is deleted.

- [x] 5.7 Add `accept_coder_work_plus_reviewer_fixes`: the coder changes A, the reviewer changes B by handoff, and `accept_delegation_result(D1, R2)` from the orchestrator creates one commit touching A and B. Covers isolated-delegation / Accept coder work plus reviewer fixes. Verify `cargo t-runtime -E 'test(/accept_coder_work_plus_reviewer_fixes/)'`.
  - Result: `accept_coder_work_plus_reviewer_fixes` passes: one new commit on the parent's branch touching `coder.txt` and `reviewer.txt`.

- [x] 5.8 Re-run the existing handoff, lineage and isolated-delegation suites to cover the unchanged git-worktree-workspaces scenarios (Native create records lineage, Explicit source on other surfaces, Isolated root without a source session, Isolated fork stays with its parent, Retired workspace does not move, Delegates remain hidden, Handoff with a summary, Native command reply, Handoff announced on every surface). Verify `cargo t-runtime -E 'test(/worktree|handoff|isolat|delegation/)'` and `cargo t-agent -E 'test(/worktree/)'`, then `cargo fmt -p querymt-agent-runtime -- --check` and `cargo cl-runtime`.
  - Result: `cargo t-runtime -E 'test(/worktree|handoff|isolat|delegation/)'` 242 passed; `cargo t-agent -E 'test(/worktree|handoff/)'` 76 passed (the existing handoff/lineage/worktree suites live in `querymt-agent`); `cargo fmt --all -- --check` clean; `cargo cl-runtime` clean.

- [x] 5.9 Document delegation by handoff in `docs/docs/agent/worktrees.md` (ownership handoff section and the isolated-delegation result flow, with a mermaid sequence) and `docs/docs/agent/delegation.md`. Note that delegates read `.agents` from their own checkout and that a gitignored `.agents/` is not in delegate baselines. Verify by review against the specs.
  - Result: `docs/docs/agent/worktrees.md`: new `Delegating to a successor (handoff_from)` section with a mermaid sequence, the `.agents`/gitignore note, and the result-acceptance paragraph under `Accepting a delegate result`; `delegation.md` as in 4.3. Reviewed against the specs; `mkdocs` unavailable.

## 6. `coder-team` profile (design D6)

- [x] 6.1 Write `crates/agent/examples/confs/coder_team.toml`:
  - `[profile] id = "coder-team"`; `[dotagents] enabled = true`; `[agent.workspace] enabled = true, baseline = "current_working_tree"`.
  - Orchestrator in plan mode with read-only tools plus `delegate` and `accept_delegation_result`, and `[agent.permissions] always_ask = ["accept_delegation_result"]`.
  - `coder` and `reviewer` isolated with edit tools and `shell`; `explorer` shared and read-only.
  - Orchestrator reminder and reviewer prompt as in design D6.

  Embed it in `crates/agent/src/profiles.rs` (`embedded_coder_team_profile_toml`) and add it to the `qmtcode` catalog only; the standard builder shared with the mobile catalog stays unchanged.
  - Result: `crates/agent/examples/confs/coder_team.toml` written; `profiles::embedded_coder_team_profile_toml` inlines its prompts. It is added to the qmtcode catalog only (per the maintainer, the mobile catalog must not get it), so `standard_embedded_profile_catalog_builder` is unchanged.

- [x] 6.2 Add tests in `crates/agent/src/profiles.rs`:
  - `coder_team_profile_loads`: not in the standard catalog, listed as embedded when added with `embedded_coder_team_profile_toml`, single config kind; the roster has `coder`, `reviewer` and `explorer` with isolations isolated, isolated, shared; the workspace service is on; `.agents` loading is on; `accept_delegation_result` is in `always_ask`.

  Verify `cargo t-agent -E 'test(/coder_team_profile_loads|embedded_profile/)'` and the qmtcode example tests (`qmtcode_catalog_uses_inline_embedded_default` lists 3 profiles).
  - Result: `cargo t-agent -E 'test(/coder_team_profile_loads|embedded_profile|standard_embedded/)'` 6 passed; `cargo nextest run -p querymt-agent --features dashboard-server,oauth,zbus-secret-service,systemd-credentials,remote --example qmtcode` 21 passed (catalog now lists 3 profiles).

- [x] 6.3 Add `coder-team` to `docs/docs/agent/examples.md` and `profiles.md` (what each member does and the review loop). Add a `CHANGELOG.md` Unreleased entry covering the `delegate` argument, the event and hook field, the delegate owner lookup fix, and the new profile. Verify by review.
  - Result: `docs/docs/agent/examples.md` (new section with a mermaid review loop), `profiles.md`, `configuration.md` list the profile; `CHANGELOG.md` Unreleased has Added entries (`handoff_from` incl. the event/hook wire fields, `coder-team`) and a Fixed entry for the delegate owner lookup. Reviewed; `mkdocs` unavailable.

## 7. Integration and downstream checks

- [x] 7.1 Build and test the downstream packages of the protocol type change: `cargo t-core`, `cargo t-services`, `cargo t-agent --no-run` then `cargo t-agent -E 'test(/delegat|worktree|profile/)'`, `cargo cl-agent`, and `cargo t-gpui` plus `cargo cl-gpui` (session-core consumer).
  - Result: `cargo t-core` 214 passed; `cargo t-services` 1084 passed (4 skipped); `cargo t-agent --no-run` then `cargo t-agent -E 'test(/delegat|worktree|profile/)'` 208 passed; `cargo cl-agent` clean; `cargo t-gpui` 856 passed (7 skipped); `cargo cl-gpui` clean; `cargo cl-runtime` clean; qmtcode example tests 21 passed. Runtime default-feature `cargo check` shows only pre-existing warnings.

- [x] 7.2 Run `openspec validate add-delegation-worktree-handoff --strict` and fix every finding.
  - Result: `openspec validate add-delegation-worktree-handoff --strict` -> valid.
- [x] 7.3 Manual end-to-end with an isolated test instance (never the user's running services or database). Run `qmtcode` with a fresh database and the `coder-team` profile on a scratch git repo, and check:
  - the coder edits in a worktree;
  - the reviewer runs by handoff in the same checkout and fixes a seeded bug;
  - the merge prompt appears and one commit lands.

  Record the observed delegation ids and commit in this task's notes.
  - Result: verified manually by the user: the coder edits in a worktree, the reviewer runs by handoff in the same checkout and fixes a seeded bug, the merge prompt appears and one commit lands. Delegation ids and the commit hash were not captured in these notes.

## 8. Review fixes (CodeRabbit review, validated)

- [x] 8.1 A finishing delegation holds its delegate's worktree. `LocalAgentHandle::delegation_holds` (`agent/handle/delegation_hold.rs`): from session creation until just before the terminal status transition (success, failure, cancel) the orchestrator holds the delegate session, which `session_is_busy` / `unload_session_if_idle` then report busy, plus a shared checkout activity lease. Verification, the history read and `publish_result` can no longer race a handoff, archive or snapshot merge (all refused `owner_active`, nothing unloaded). Test `handoff_waits_for_a_finishing_delegation` (a blocking `post_delegation` hook holds D1 in its finishing phase; fails with the hold removed).
- [x] 8.2 Acceptance judges the run that published the result. `ResultProvenance.delegation_id` (nullable `workspace_results.delegation_id`, migration `0042_workspace_result_publisher`), written by `WorkspaceService::publish_delegation_result` on delegation completion; `WorkspaceService::result_publisher` finds it and `accept_delegation_result` passes that delegation's status. Tests `accept_after_a_failed_coder` (also: a result published without a delegation is still refused `delegation_not_complete`) and `revert_transfer_restores_a_writable_previous_owner`.
- [x] 8.3 A delegate successor whose finalization fails after the transfer (actor already started) returns the workspace like a failed actor start (`create_delegation_session_in_workspace` calls `roll_back_handoff_successor`). The rollback now uses the new `WorkspaceRepository::revert_transfer` instead of a reverse `transfer_owner`: the reverse transfer left the previous owner marked as handed off (read-only, pointing at a deleted session) and a second handoff from it failed on the `workspace_handoffs` primary key. Tests `delegate_successor_fails_after_its_actor_started` (injected failing `session_start` hook, `DelegateBehavior::SessionStartHookFails`) and the extended `delegate_successor_fails_to_start`.
- [x] 8.4 `accept_coder_work_plus_reviewer_fixes` no longer publishes by hand or hard-codes the status: the reviewer (`PauseBeforeLlmCall`) writes file B during its own run, and the test accepts the commit named in the completion text through the real `accept_delegation_result` tool.
- [x] 8.5 Docs: `worktrees.md` (finishing hold, rollback, acceptance by publisher), `delegation.md`, `examples.md` (`baseline_local_edits` with `baseline = "current_working_tree"`), `CHANGELOG.md`.
- Not changed: the `owner_transferred` load disclosure is still asserted through the registry (`transferred_to`); `read_only_reason` reads the same marker and the existing `worktree_ops_ext` tests cover the disclosure. Failed or cancelled handoff delegates do not publish a result (the spec sentence now says "when it completes").

## Workflow follow-up

- Report the querymt-desktop follow-up: optionally render "continues delegation <id>" for `delegation_requested` events that carry `handoff_from`.
- Archive the change after review.
