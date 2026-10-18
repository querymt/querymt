# Tasks

## Review remediation status

Sections 1-9 record earlier implementation progress. Their checked tasks and the validation notes below describe the pre-remediation branch; they do not establish compliance with the revised specs. In particular, task 2.1's CLI implementation is superseded by section 10. Sections 10-18 are the approved, not-yet-implemented remediation plan covering review findings F1-F9. Tasks 7.4 and 8.3 remain open and require renewed validation after remediation. No implementation fixes or new Cargo/test outcomes are claimed by this planning update.

Finding map: F1 authoritative fork cwd -> 11 and 16; F2 fail-closed fork creation -> 11; F3 removal ref race -> 12; F4 newly staged ignored-file capture -> 13; F5 unsupported-layout detection -> 13; F6 slash execution -> 14; F7 replayable provisioning -> 15; F8 alternate Git-directory layouts -> 16; F9 baseline-relative results -> 17. Section 10 covers the native gix migration and existing snapshot compatibility; section 18 verifies the combined behavior.

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

## Historical pre-remediation validation notes

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

## 10. Native gix dependency and typed repository operations

- [x] 10.1 Upgrade gix 0.80 to 0.89, explicitly enabling sha1 with required basic/attributes/worktree-mutation features and updating Cargo.lock; verify the supported toolchain meets Rust 1.88 and cargo check -p querymt-agent succeeds, then run formatting only after the check passes.
- [x] 10.2 Migrate discovery/error classification and affected gix call sites, including the existing snapshot backend, to 0.89 APIs and gix::Result; verify missing/untrusted/unusable repository errors remain distinguishable and existing snapshot/discovery tests pass without changing undo coverage.
- [x] 10.3 Replace GitRunner's argv/stdout boundary with typed injectable repository operations and fault-injection support; verify allocation/capture/lifecycle tests can inject operation failures without parsing Git commands or fabricating porcelain output, and blocking native calls do not run on async executor threads.
- [x] 10.4 Implement native linked-worktree creation, inventory, inspection, and removal using high-level gix APIs while retaining QueryMT's stricter ownership/activity/ignored-file/result checks; verify distinct concurrent checkouts, no-force removal, and unchanged source files/index/HEAD in real-repository tests.
- [x] 10.5 Replace remaining production Git subprocess calls for private index/tree/commit/ref operations, status, ancestry, and baseline-relative diffs with native primitives; verify a source audit and built-in operation tests show no Git executable dependency, and inherited Git directory/worktree/index overrides cannot redirect operations.
- [x] 10.6 Preserve the configured native snapshot backend rather than adding another undo implementation; verify linked-worktree and subdirectory snapshot track/diff/restore/gc, undo/redo, legacy histories, metadata exclusion, and snapshots-disabled isolation under the upgraded dependency. Existing Git CLI test fixtures may remain where tests are not claiming Git-free execution.
- [x] 10.7 Document the native backend, supported toolchain/features, repository-opening policy, and absence of a built-in Git executable requirement in existing agent/worktree docs; verify documentation distinguishes built-in operations from arbitrary shell commands and configured external filters.

## 11. Authoritative fork binding and fail-closed startup (F1, F2)

- [x] 11.1 Introduce a single effective workspace/cwd resolution path for persisted and live sessions and audit delegation, native worktree creation, verification/hooks, and undo child filtering; verify an isolated fork with contents different from its source supplies its own files/cwd to each consumer, including after restart and nested delegation.
- [x] 11.2 Persist fork execution cwd consistently with the workspace owner association; verify real materializer/store tests assert the resulting session row, binding, list metadata, load, and resume agree without manually updating sessions.cwd in test setup.
- [x] 11.3 Validate fork policy/options before conversation copying and persist pending isolation intent with any newly copied isolated session; verify invalid requests create no runnable fork and legacy unbound local sessions remain local.
- [ ] 11.4 Publish owner binding, execution cwd, and runnable state atomically in SQLite or behind a durable startup barrier; inject failures/process interruption after conversation copy, allocation, and before/after owner binding and verify load/resume/dispatch never run the pending fork in its source checkout.
- [ ] 11.5 Add explicit compensation/recovery for failed fork creation, preserving history or recoverable files as appropriate and cleaning only newly allocated unused resources; verify listing exposes pending/failed state, retries do not create duplicate owners, and restart cannot silently downgrade isolation.
- [ ] 11.6 Add parent/fork undo regression coverage through the real fork creation path; verify parent undo excludes isolated fork snapshots despite source ancestry, fork-local undo/redo works after restart, and existing compatible shared-child aggregation still passes.
- [ ] 11.7 Update existing fork/session documentation with effective-cwd authority and pending/failed creation behavior; verify documented error/recovery examples match the tested API and no live workspace switching is implied.

## 12. Race-safe branch preservation during removal (F3)

- [ ] 12.1 Carry the observed branch target from preservation checks into conditional deletion using delete_local_branches_if_unchanged or an equivalent expected-target transaction; verify deterministic injection of an external ref update between check and deletion retains the moved branch and new history.
- [ ] 12.2 Apply conditional deletion and fresh safety checks during interrupted Removing recovery; verify branch movement, checkout disappearance, already-deleted refs, and config-cleanup partial failures are handled idempotently without unconditional branch deletion.
- [ ] 12.3 Verify native worktree removal still refuses active/non-archived owners, dirty/untracked/ignored content, foreign worktrees, and unpreserved commits, independent of status.showUntrackedFiles configuration; add real-repository regressions that assert no recoverable files or refs are lost.
- [ ] 12.4 Document external-writer removal races and retained-branch outcomes in existing lifecycle docs; verify API/native-command responses disclose retained refs and do not claim complete deletion after a partial or refused operation.

## 13. Complete working-state capture and unsupported-feature checks (F4, F5)

- [ ] 13.1 Reconcile tracked membership from HEAD plus the real index into private capture state without importing the parent's staging partition; verify a newly force-added ignored file absent from HEAD is captured with subsequent working edits, while genuinely untracked ignored files remain excluded and the real index stays byte-for-byte unchanged.
- [ ] 13.2 Preserve supported Git attributes/filter semantics, deletions, modes, executable bits, symlinks, binary data, hidden files, and partial staging with native capture; verify object contents/modes and source HEAD/index/files/stash are unchanged, including staged additions/deletions and unchanged-tree HEAD reuse.
- [ ] 13.3 Normalize capture/validation to the resolved checkout root while retaining relative cwd only for execution; verify subdirectory sessions capture the intended repository-wide baseline and cannot bypass root-level sparse-checkout or submodule rejection, using HEAD/index gitlinks rather than only checking .gitmodules.
- [ ] 13.4 Resolve effective attributes for candidate paths, including nested .gitattributes, Git-directory attributes, and macros; verify unsupported LFS capture fails with feature/path diagnostics in each case before child execution and does not accept an incomplete baseline based on a root-file text scan.
- [ ] 13.5 Extend before/after HEAD/index/content validation to the corrected candidate set and native capture; verify externally changing tracked additions and attribute/layout state trigger bounded retry or explicit failure, sibling batch baselines remain identical, and no gate is held while awaiting children.
- [ ] 13.6 Update existing capture documentation for newly staged tracked files, effective attributes, subdirectory/root semantics, unsupported features, external filters, and ignored-file exclusions; verify each claimed supported/unsupported case has a corresponding regression test and diagnostic.

## 14. Workspace-scoped slash-command execution (F6)

- [ ] 14.1 Use the effective session's slash registry for both advertised catalog and actual prompt expansion, including workspace includes and script source paths; verify two checkouts containing differently defined commands expand/execute their own versions and an isolated-only command is invocable.
- [ ] 14.2 Preserve global command policy, scripts-disabled behavior, native /worktree interception, and legacy local sessions; verify actual handle/actor prompt execution before and after load/resume rather than only testing registry discovery.
- [ ] 14.3 Document workspace-scoped command discovery and invocation in existing command/worktree docs; verify examples use the same sources advertised by the catalog and do not imply automatic rewriting of absolute paths.

## 15. Replayable provisioning and retained baseline refs (F7)

- [ ] 15.1 Extend persisted provisioning intent/migrations with source identity/path, requested ref/policy, relative cwd, intended resources, operation ID, and phase information sufficient for baseline-absent recovery; verify existing databases migrate and the true first persisted intent can be round-tripped without a fabricated baseline.
- [ ] 15.2 Implement recovery of the actual baseline-absent intent, safely retrying validated capture from the recorded source or retaining an actionable non-runnable state; verify unavailable/changed sources and recovery-time capture disclosure without duplicate resources or silent startup.
- [ ] 15.3 Once baseline identity is durable, keep it immutable and idempotently ensure its retained ref exists at the expected target before Ready; verify a crash between baseline binding and ref publication restores that exact baseline and conflicting refs are refused rather than overwritten.
- [ ] 15.4 Add fault injection at intent-only, capture, baseline binding, baseline-ref publication, branch creation, checkout creation, and final binding/Ready boundaries; verify recovery preserves unknown directories, exact bound baseline identity, retained reachability, and single ownership across restart/processes.
- [ ] 15.5 Document provisioning phases, source requirements, recovery-time versus previously bound baselines, conflicting-ref errors, and pending-session behavior; verify recovery examples against fixtures interrupted at the real persistence boundaries.

## 16. Alternate Git-directory layouts and checkout identity (F8, F1)

- [ ] 16.1 Replace common_dir.parent() and equivalent checkout reconstruction with explicit canonical repository/common-dir and per-checkout handles throughout lifecycle/capture/review/recovery; verify repository administration never uses a guessed working directory.
- [ ] 16.2 Add full-lifecycle fixtures for separate Git directories, existing linked sources, subdirectory sessions, and linked checkouts backed by bare common repositories; verify create/status/capture/result/recover/archive/remove succeed while a bare source without a checkout fails explicitly.
- [ ] 16.3 Verify canonical path aliases cannot bypass single-owner checks or split checkout/admin locking for these layouts; document supported layouts and diagnostics in existing worktree docs and confirm project grouping does not merge execution/index identities.

## 17. Immutable baseline-relative result publication and review (F9)

- [ ] 17.1 Expose one typed result publication/review service for delegates and top-level workspaces, with additive API/native-command support for explicit top-level publication; verify uncommitted results become retained immutable objects without modifying the real index/files and publication failures preserve the checkout with explicit errors.
- [ ] 17.2 Return baseline/result OIDs and refs, changed paths and change kinds from the immutable baseline-to-result diff, verification state, and captured HEAD separately from live HEAD/dirty status; verify a parent-edit-to-A/child-edit-to-B case reports only B as child work, including top-level and delegate review paths.
- [ ] 17.3 Preserve previous result refs and review content across later file/HEAD changes and subsequent publication; verify create-only unique refs, inspection without silent republication, accurate skipped/pending/failed verification, capture-error reporting, and review availability after restart and snapshot GC.
- [ ] 17.4 Propagate baseline-relative review information or durable review references through completion, failure/cancellation, and summarizer output, including dirty-baseline merge warnings; verify real dispatch/output tests do not merely assert opaque commit IDs and never claim automatic integration.
- [ ] 17.5 Update existing API/command schemas, generated artifacts where required by repository conventions, and result-review docs; verify examples publish/review a top-level result and a delegate delta, distinguish immutable from live state, and warn that raw branch merges can import inherited baseline edits.

## 18. Revised cross-feature acceptance and release checks

- [ ] 18.1 Run combined acceptance after remediations: divergent isolated fork, nested/parallel delegates, newly staged ignored file, workspace-specific slash invocation, child verification, parent/fork undo, restart/recovery, immutable result review, and archive/removal; verify all revised spec scenarios together and record outcomes without reusing historical pass claims.
- [ ] 18.2 Run a production-operation test with Git unavailable after fixture setup, covering creation, capture, inspection/publication, provisioning/removal recovery, and conservative removal; combine with deterministic external-ref races and multi-process ownership/activity tests to verify native execution, no silent sharing, and no lost recoverable work.
- [ ] 18.3 Rerun cargo check -p querymt-agent and affected feature combinations, then formatting, default and api/oauth/remote tests, and repository-prescribed strict lint checks; record exact commands/results and unrelated blockers, and complete original tasks 7.4/8.3 only after the revised manual workflow and validation pass.
- [ ] 18.4 Validate the final planning/implementation alignment with openspec validate add-git-worktree-isolation --strict and git diff --check; verify all F1-F9 mappings and native-backend tasks have evidence before proposing archival.
