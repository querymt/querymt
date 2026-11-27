# Tasks

## 1. Wire shape: `handoff_from` on the event

- [ ] 1.1 Add `handoff_from: Option<String>` (`serde(default, skip_serializing_if = "Option::is_none")`) to `AgentEventKind::DelegationRequested` in `crates/agent-protocol-types/src/events.rs`; update every constructor and pattern (`tool_calls.rs`, `delegation/core.rs`, `acp/stdio.rs`, `control/delegation_notifications.rs` tests, `history_window.rs`). Extend the existing `DelegationRequested` round-trip test in `delegation_notifications.rs` so an event without the field deserializes and one with it round-trips. Verify `cargo check -p querymt-agent-protocol-types`, `cargo t-protocol`, `cargo t-services -E 'test(/delegation_notifications/)'`, `cargo fmt -p querymt-agent-protocol-types -- --check`, `cargo cl-protocol`.

## 2. Services: split the handoff core (design D2)

- [ ] 2.1 In `crates/agent-services/src/worktree/ops.rs`, split `WorktreeOps::handoff_announced` into `begin_handoff(host, target) -> PendingHandoff` and `PendingHandoff::commit(self, ops, successor)`. Rebuild `handoff_announced` on them with unchanged checks, error codes and announcement. Add `WorkspaceService::binding_for(record)` in `service.rs`. Covers git-worktree-workspaces / Clean handoff from an idle owner, Owner still active, Not the owner or not ready, Crash during handoff (unchanged behaviour). Verify `cargo t-services -E 'test(/handoff/)'` and `cargo t-runtime -E 'test(/handoff/)'` pass unchanged.
- [ ] 2.2 Add a services test `pending_handoff_drop_releases_the_lease`: a `PendingHandoff` dropped without commit leaves the owner unchanged and lets `acquire_exclusive_activity` succeed again. Verify `cargo t-services -E 'test(/pending_handoff_drop_releases_the_lease/)'`, `cargo fmt -p querymt-agent-services -- --check`, `cargo cl-services`.

## 3. Runtime: find delegate sessions when stopping an owner (design D5)

- [ ] 3.1 In `crates/agent-runtime/src/agent/handle/session_control.rs`, make `unload_session_if_idle` and `session_is_busy` fall back to `local_members()` registries (as `owned_session_ref` does) and run the in-runtime step on the member handle that holds the session. Add runtime test `handoff_unloads_an_idle_delegate_owner_in_a_config_runtime`: a config-file root with an isolated member completes a delegation, `/worktree handoff` on the delegate's worktree unloads the member's actor and transfers ownership. Verify `cargo t-runtime -E 'test(/handoff_unloads_an_idle_delegate_owner_in_a_config_runtime/)'` fails before the change and passes after it.

## 4. Runtime: delegate tool, hook and request (design D1)

- [ ] 4.1 Add `handoff_from` to the `delegate` tool schema and `DelegateTool::call` in `agent/builtin_tools/delegate.rs`: string only, rejected with `isolation = "shared"`. Extend the existing schema/argument tests there with `shared_isolation_with_handoff`. Covers isolated-delegation / Shared isolation with handoff. Verify `cargo t-runtime -E 'test(/builtin_tools::delegate/)'`.
- [ ] 4.2 Add read-only `handoff_from` to `PreDelegationRequest` (`hooks/engine.rs`) and its JSON input. In the `delegate` branch of `agent/execution/tool_calls.rs`, parse it, pass it to the hook, force resolved isolation to isolated, write it into `tool_arguments`, disclose "runs in the existing worktree of delegation <id>" in the tool result, and emit it on `DelegationRequested`. Add test `handoff_is_disclosed_to_the_caller_and_hooks` (hook input, event field, tool result text). Covers isolated-delegation / Handoff is disclosed to the caller and hooks. Verify `cargo t-runtime -E 'test(/handoff_is_disclosed_to_the_caller_and_hooks/)'`.
- [ ] 4.3 Document `handoff_from` in the `pre_delegation` hook input reference and in `docs/docs/agent/delegation.md` (the `delegate` tool arguments). Verify the docs build with `mkdocs build -f docs/mkdocs.yml --strict` if available, otherwise by review.

## 5. Runtime: run a delegation by handoff (design D3, D4)

- [ ] 5.1 Turn the preallocated workspace of `SessionMaterializer::prepare_new_session_with_workspace` into `Fresh(binding)` / `Handoff { binding, pending }`. For `Handoff`: write the barrier with `handoff_workspace_id`, replace `bind_owner` with `pending.commit`, never mark the workspace failed, and on a start failure after the transfer move ownership back (or leave the barrier and return `handoff_incomplete`). Add `LocalAgentHandle::create_handoff_delegation_session` and `MemberAgent::create_handoff_delegation_session` (local members only). Verify `cargo check -p querymt-agent-runtime --features remote,oauth,plugin-loaders,api`.
- [ ] 5.2 In `delegation/core.rs`, thread `handoff_from` from `handle_envelope` into `execute_delegation_inner`. Add the handoff branch next to isolated allocation: look up the delegation, find its worktree, check the parent, run `begin_handoff` with the root as `OwnerHost`, create the session through the member, then announce `WorktreeOwnershipTransferred` through the root's `HandoffHost::announce_handoff`. Every refusal goes through `fail_delegation`.
- [ ] 5.3 Print the worktree's `delegation_id` (not the running delegation's id) in the `accept_delegation_result` hint and the `/worktree result` reference of the completion summary.
- [ ] 5.4 Add runtime delegation tests on the existing isolated-delegation fixtures (scripted provider, real git repo):
  - `reviewer_takes_over_a_coders_worktree`: same checkout, coder edits present, no new worktree, coder session read-only with `owner_transferred`.
  - `delegation_handoff_to_a_delegate_session`: ownership-transferred event on the coder's stream with the delegate session and `summary_seeded = false`.
  - `delegate_handoff_successor_keeps_delegation_lineage`: parent is the orchestrator, origin `delegation`, not listed as a fork or worktree child.
  - `handoff_from_a_failed_or_cancelled_delegation`.
  - `second_round_after_a_review`.
  - `completion_names_the_worktrees_delegation`.

  Covers isolated-delegation / Reviewer takes over a coder's worktree, Handoff from a failed or cancelled delegation, Second round after a review, Completion names the worktree's delegation; git-worktree-workspaces / Delegation handoff to a delegate session, Delegate handoff successor keeps delegation lineage. Verify `cargo t-runtime -E 'test(/reviewer_takes_over|delegation_handoff_to_a_delegate|delegate_handoff_successor|handoff_from_a_failed|second_round_after|completion_names_the_worktree/)'`.
- [ ] 5.5 Add refusal tests, each asserting the error code, unchanged owner and no new worktree:
  - `handoff_from_another_sessions_delegation`
  - `handoff_from_a_shared_delegation`
  - `owner_still_running` (the running owner is not interrupted)
  - `unknown_delegation_handoff`

  Covers isolated-delegation / Handoff from another session's delegation, Handoff from a shared delegation, Owner still running, Unknown delegation. Verify `cargo t-runtime -E 'test(/handoff_from_another|handoff_from_a_shared|owner_still_running|unknown_delegation_handoff/)'`.
- [ ] 5.6 Add `delegate_successor_fails_to_start`: inject an actor start failure after the transfer (the registry wrapper used by the existing `attach_owner` failure test in `session_materializer.rs`, or a failing MCP root) and assert the delegation fails, the workspace stays ready and not failed, and it has exactly one owner. Covers git-worktree-workspaces / Delegate successor fails to start. Verify `cargo t-runtime -E 'test(/delegate_successor_fails_to_start/)'`.
- [ ] 5.7 Add `accept_coder_work_plus_reviewer_fixes`: the coder changes A, the reviewer changes B by handoff, and `accept_delegation_result(D1, R2)` from the orchestrator creates one commit touching A and B. Covers isolated-delegation / Accept coder work plus reviewer fixes. Verify `cargo t-runtime -E 'test(/accept_coder_work_plus_reviewer_fixes/)'`.
- [ ] 5.8 Re-run the existing handoff, lineage and isolated-delegation suites to cover the unchanged git-worktree-workspaces scenarios (Native create records lineage, Explicit source on other surfaces, Isolated root without a source session, Isolated fork stays with its parent, Retired workspace does not move, Delegates remain hidden, Handoff with a summary, Native command reply, Handoff announced on every surface). Verify `cargo t-runtime -E 'test(/worktree|handoff|isolat|delegation/)'` and `cargo t-agent -E 'test(/worktree/)'`, then `cargo fmt -p querymt-agent-runtime -- --check` and `cargo cl-runtime`.
- [ ] 5.9 Document delegation by handoff in `docs/docs/agent/worktrees.md` (ownership handoff section and the isolated-delegation result flow, with a mermaid sequence) and `docs/docs/agent/delegation.md`. Note that delegates read `.agents` from their own checkout and that a gitignored `.agents/` is not in delegate baselines. Verify by review against the specs.

## 6. `coder-team` profile (design D6)

- [ ] 6.1 Write `crates/agent/examples/confs/coder_team.toml`:
  - `[profile] id = "coder-team"`; `[dotagents] enabled = true`; `[agent.workspace] enabled = true, baseline = "current_working_tree"`.
  - Orchestrator in plan mode with read-only tools plus `delegate` and `accept_delegation_result`, and `[agent.permissions] always_ask = ["accept_delegation_result"]`.
  - `coder` and `reviewer` isolated with edit tools and `shell`; `explorer` shared and read-only.
  - Orchestrator reminder and reviewer prompt as in design D6.

  Embed it in `crates/agent/src/profiles.rs` next to `coder-delegate`.
- [ ] 6.2 Add tests in `crates/agent/src/profiles.rs`:
  - `coder_team_profile_loads`: listed as embedded, single config kind; the roster has `coder`, `reviewer` and `explorer` with isolations isolated, isolated, shared; the workspace service is on; `.agents` loading is on; `accept_delegation_result` is in `always_ask`.

  Verify `cargo t-agent -E 'test(/coder_team_profile_loads|embedded_profile/)'`.
- [ ] 6.3 Add `coder-team` to `docs/docs/agent/examples.md` and `profiles.md` (what each member does and the review loop). Add a `CHANGELOG.md` Unreleased entry covering the `delegate` argument, the event and hook field, the delegate owner lookup fix, and the new profile. Verify by review.

## 7. Integration and downstream checks

- [ ] 7.1 Build and test the downstream packages of the protocol type change: `cargo t-core`, `cargo t-services`, `cargo t-agent --no-run` then `cargo t-agent -E 'test(/delegat|worktree|profile/)'`, `cargo cl-agent`, and `cargo t-gpui` plus `cargo cl-gpui` (session-core consumer).
- [ ] 7.2 Run `openspec validate add-delegation-worktree-handoff --strict` and fix every finding.
- [ ] 7.3 Manual end-to-end with an isolated test instance (never the user's running services or database). Run `qmtcode` with a fresh database and the `coder-team` profile on a scratch git repo, and check:
  - the coder edits in a worktree;
  - the reviewer runs by handoff in the same checkout and fixes a seeded bug;
  - the merge prompt appears and one commit lands.

  Record the observed delegation ids and commit in this task's notes.

## Workflow follow-up

- Report the querymt-desktop follow-up: optionally render "continues delegation <id>" for `delegation_requested` events that carry `handoff_from`.
- Archive the change after review.
