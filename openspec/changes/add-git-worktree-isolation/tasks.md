# Tasks

## 1. Workspace model and persistence

- [x] 1.1 Add worktree domain types for identity, baseline mode, lifecycle, policy, ownership, and result provenance; verify serialization/default-policy tests preserve local behavior when options are absent.
- [x] 1.2 Add SQLite migrations and a workspace repository exposed through StorageBackend, including nullable session association and delegation baseline/result links; verify migration tests against an existing database and registry CRUD/unique-owner tests.
- [x] 1.3 Add canonical common-directory repository identity, checkout identity, and relative-cwd resolution; verify real-repository tests from a root, subdirectory, and existing linked worktree, including absent baseline subdirectories and non-Git rejection.
- [x] 1.4 Document model invariants and durable storage configuration in existing agent documentation/API docs; verify examples distinguish project identity, execution cwd, and snapshot storage.

## 2. Git worktree lifecycle

- [x] 2.1 Add the injectable Git operations adapter using explicit argv, ref validation, controlled Git environment, and existing gix helpers where appropriate; verify missing-Git, invalid-ref, shell-metacharacter, and inherited-Git-environment tests cannot redirect execution.
- [x] 2.2 Implement allocation of unique branches and linked worktrees under durable storage with provisioning/ready transitions; verify two concurrent creations produce distinct writable checkouts without changing source HEAD/index/files.
- [x] 2.3 Add cross-process repository-administration locking and idempotent provisioning recovery; verify subprocess concurrency and fault-injection tests at intent, branch, checkout, and binding boundaries, preserving pre-existing unknown directories.
- [x] 2.4 Implement live status, missing-workspace reconciliation, and explicit conservative removal; verify tests refuse active, dirty, untracked, ignored, externally owned, and unpreserved-result cases and report retained refs on successful clean removal.
- [x] 2.5 Document lifecycle/error/recovery operations and no-force-delete semantics; verify documented recovery steps against an interrupted temp-repository fixture.

## 3. Parent working-state baselines

- [x] 3.1 Add checkout-scoped cooperative mutation gates around built-in mutating operations, undo/redo, and baseline capture, with explicit handling of opaque shell/MCP operations; verify no gate is held while waiting for a delegate and concurrent capture/mutation tests cannot deadlock.
- [x] 3.2 Implement private-index baseline capture seeded from HEAD, reconciling actual tracked working contents and non-ignored untracked files without modifying the source index; verify partial staging, deletions, tracked ignored/dotfiles, binary files, symlinks, executable modes, and unchanged source index/HEAD/stash tests.
- [x] 3.3 Add immutable baseline refs with internal commit identity and reuse HEAD for identical trees; verify capture works without user commit identity, does not invoke commit hooks, and preserves baseline reachability across restart.
- [x] 3.4 Validate HEAD/index/content manifests around capture with bounded retry and explicit unsupported-layout checks; verify externally changing sources, unmerged indexes, unborn HEAD, sparse checkout, submodules, and LFS cases fail or retry before child execution rather than silently omit content.
- [x] 3.5 Add shared baseline identity for one parallel delegation batch and independent capture for later requests; verify siblings receive identical starting contents while later delegates see subsequent parent edits.
- [x] 3.6 Document supported Git layouts, ignored-file exclusions, staging semantics, private-object privacy, and external-writer consistency limits; verify each limitation maps to an explicit diagnostic or tested supported behavior.

## 4. Session binding and workspace consumers

- [x] 4.1 Add typed local/worktree creation and commit/current-working-tree baseline options in configuration and agent entry points; verify compatibility defaults and invalid option combinations, including explicit ref with current-working-tree capture.
- [x] 4.2 Resolve and persist workspace association before actor/MCP/index startup in SessionMaterializer; verify new/load/resume consistently use the bound cwd and fail closed for missing paths, conflicting cwd requests, and duplicate writable owners.
- [x] 4.3 Allocate a new working-state workspace for isolated conversation forks, separately recording the historical message fork point; verify forks do not share paths and disclose that files reflect current source contents rather than historical filesystem state.
- [x] 4.4 Route tool context, indexes, project configuration/skills, prompts, and execution hook context through the effective workspace; verify identical relative paths resolve differently in two sessions and indexes are not shared by common repository identity.
- [x] 4.5 Validate MCP/editor bridge roots and local-versus-remote filesystem placement; verify mismatched fixed-root integrations are diagnosed, remote filesystem isolation fails before dispatch, and remote inference with local tools remains supported.
- [x] 4.6 Document session creation/resume/fork semantics and configuration with examples in existing agent docs; verify examples identify actual cwd and do not promise sandbox confinement or live switching.

## 5. Existing undo/redo compatibility

- [x] 5.1 Keep the configured SnapshotBackend/GitSnapshotBackend as the sole undo backend and pass bound session cwd through existing capture/restore call sites; verify temp linked-worktree undo/redo tests preserve sibling files, project refs, and Git administrative markers without adding a second snapshot implementation.
- [x] 5.2 Persist workspace provenance for new snapshot/revert state and validate all selected entries before restore, including fallback paths; verify cross-workspace rejection causes no partial restoration and same-workspace restart/resume retains undo history.
- [x] 5.3 Update undo_impl child-history aggregation to include only compatible shared-workspace children, excluding isolated delegates; verify parent undo succeeds without touching isolated children and existing shared-checkout delegation undo tests remain valid.
- [x] 5.4 Treat inherited foreign-workspace fork snapshots as context-only while allowing new fork turns to be undone; verify fork, subdirectory cwd, legacy local history, and snapshots-disabled cases.
- [x] 5.5 Document existing snapshot coverage and independence from baseline/result refs; verify snapshot-GC tests leave workspace baseline/result references reachable and worktree operation remains available with snapshots disabled.

## 6. Delegation execution and retained results

- [x] 6.1 Extend delegate schema, tool-call parsing, persisted request policy, and orchestrator dispatch with inherited/explicit isolation; verify isolated parents isolate siblings by default, local compatibility remains intact, explicit overrides are disclosed, and allocation errors never launch shared children.
- [x] 6.2 Allocate child workspaces before create_delegation_session and persist their parent/batch/baseline association; verify concurrent same-path edits in siblings leave parent and each other untouched, including nested delegation.
- [x] 6.3 Route verification and post-allocation hooks to child cwd while retaining explicit parent context for pre-allocation hooks; verify a check deliberately yielding opposite parent/child outcomes reports the child result and hook payloads identify the correct roots.
- [x] 6.4 Reuse repository capture for immutable uncommitted results and persist baseline-relative review metadata; verify inherited parent edits are not misattributed to the child and result publication leaves child staging state and parent checkout unchanged.
- [x] 6.5 Include retained workspace/result references in completion, failure, cancellation, and summarizer output; verify lifecycle tests retain edits on failure/cancellation and report capture errors without claiming successful integration.
- [x] 6.6 Document result review and manual baseline-relative integration guidance, explicitly warning against raw merges of dirty-parent baselines; verify a two-file parent/child example identifies only the delegate's delta.

## 7. Agent operations and command surface

- [x] 7.1 Expose typed create/list/status/result/remove operations and additive ACP extensions without modifying upstream protocol types; verify API tests report new session/workspace IDs, paths, baselines, and errors consistently.
- [x] 7.2 Add native `/worktree create`, `list`, `status`, `result`, and `remove` dispatch backed by the service, including an explicit include-local-changes option; verify parsing and authorization tests and ensure create does not mutate the current actor's workspace or invoke an LLM for Git administration.
- [x] 7.3 Add workspace metadata to session views/events and group associated sessions by repository identity while retaining legacy grouping for local sessions; verify projection/listing tests show one project with distinct execution paths without changing unrelated ACP update behavior.
- [ ] 7.4 Document and manually exercise the complete opt-in session/delegate workflow and command/API examples; verify result inspection and safe removal match the stated no-auto-merge/no-force-delete behavior.

## 8. Cross-feature acceptance

- [x] 8.1 Run a real-Git integration scenario with two top-level isolated sessions and multiple delegates from a dirty parent, then verify baseline contents, independent edits, child verification, parent/child undo, restart/resume, and retained baseline-relative results together.
- [x] 8.2 Run multi-process allocation/capture/removal and crash-recovery acceptance tests, verifying no duplicate owners, deadlocks, silent sharing fallback, or deletion of recoverable work.
- [ ] 8.3 Run the affected agent test suites and formatting/lint checks using the repository's existing commands; record outcomes and verify legacy local sessions, shared delegation, snapshots, forks, and additive protocol behavior remain compatible.

## Resume validation notes

- Native command parsing/dispatch, unchanged source actor, unknown-session rejection, result/status/list operations, and conservative removal: passing real-Git handle tests.
- Session projection, paginated repository grouping, distinct execution paths, and durable binding events: passing handle test.
- Parent/pre-allocation and child/post-allocation hook payloads and process cwd: passing hook test; committed input schemas regenerated.
- Combined real-Git acceptance: two isolated top-level workspaces, dirty-parent parallel delegates, child-specific verification, independent snapshot restore/redo, SQLite reopen/resume, retained baseline-relative result, and source metadata preservation pass. This caught and fixed relative FileAssertion paths ignoring verification cwd.
- Three subprocesses using one SQLite database/repository: allocation, private capture, owner uniqueness, refusal to remove recoverable dirty files, clean explicit removal, and retained refs pass. Existing provisioning recovery/fault-injection and process lock tests also pass in the agent suite.
- `cargo check -p querymt-agent`: passes. `cargo check -p querymt-agent --features api,oauth,remote`: passes with warnings.
- Latest review-fix validation: `cargo check -p querymt-agent --tests --features api,oauth,remote` passes with existing websocket warnings. `cargo test -p querymt-agent --lib`: 2462 passed, 0 failed, 4 ignored; `cargo test -p querymt-agent --lib --features api,oauth,remote`: 2697 passed, 0 failed, 4 ignored.
- Latest `cargo fmt -p querymt-agent -- --check`, `git diff --check`, and `openspec validate add-git-worktree-isolation`: pass.
- Regression coverage includes tool-context gate propagation, preallocated delegate runtime binding, managed checkout alias/subdirectory owner checks, fork inheritance, content-only race validation, shared batch baselines after parent edits, immutable subsequent result publication, typed foreign-snapshot rejection, archive/removal recovery, legacy archival migration preservation, MCP root rejection, remote target rejection before allocation, and cross-process activity leases.
- Latest strict `cargo clippy -p querymt-agent --lib --tests --features api,oauth,remote -- -D warnings` is blocked only by pre-existing `acp/websocket.rs` issues: unused StatusCode import (line 42) and unused has_allowed_websocket_origin helper (line 669). Worktree-related warnings/lints are cleaned up. The earlier dashboard source-download failure remains historical; dashboard/all-target lint validation was not repeated in this review-fix session. Task 8.3 remains open; no strict lint pass is claimed.
- Task 7.4 remains open for an interactive opt-in session/delegate walkthrough. History-preserving cleanup is now implemented: close all checkout actors, explicitly archive execution, then conservatively remove. Archive retains the durable binding/history and permanently refuses resume/load; shared actor activity leases block archive/removal across processes. Native/API documentation and real-Git archive/remove extension tests cover this workflow.

## 9. Review corrections

- [x] 9.1 Propagate runtime checkout gates into tool contexts and install preallocated delegate ownership/gates before actor, MCP, index and binding-event startup; reject managed-path local attachment and unsupported remote targets.
- [x] 9.2 Inherit isolation for forks of managed sessions, reject local downgrade, and retain local-source compatibility.
- [x] 9.3 Validate private content trees and real index entries before/after capture, including the captured tree itself; reuse a persisted immutable baseline for each dispatched tool-result batch and recapture for later batches.
- [x] 9.4 Publish unique create-only immutable result refs and disclose skipped verification accurately.
- [x] 9.5 Atomically claim Removing before Git deletion, reject non-ready owner attachment, expose explicit provisioning/removal recovery, and preserve history through inactive-only archive/remove with a tested schema migration.
- [x] 9.6 Persist snapshot capture-time workspace provenance and inherited context markers; preflight all selected snapshots before restoring and retain typed local undo/redo errors.
- [x] 9.7 Reject unverified fixed-root MCP integrations before startup through a workspace-root validation hook; separate editor isolation from mutation-gate presence.
- [x] 9.8 Run default and api/oauth/remote library suites, formatting and lint checks; record remaining validation limitations.
