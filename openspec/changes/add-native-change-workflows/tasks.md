# Tasks

## 1. Definitions and core crate scaffolding

- [ ] 1.1 Create `crates/flow-core` (serde-only, no tokio/fs/clock) and add it to the workspace; verify `cargo check -p flow-core` passes and `cargo tree -p flow-core` shows no tokio or gix
- [ ] 1.2 Implement `openspec/flow.yaml` parsing with schema version, unknown-key rejection and the built-in default workflow; verify unit tests for valid, unknown-key, unsupported-version and missing-gate cases
- [ ] 1.3 Implement machine role settings (role → profile/model, budget `wrap_up`/`stop`, read_only, limits) with validation; verify tests for missing role and `wrap_up >= stop`
- [ ] 1.4 Implement pi-format `dependencies.yaml` parsing (`blocked_by`, `not_blocked_by`, `related`), archived-blocker satisfaction and cycle detection; verify tests including a fixture copied from pi-agent-dashboard
- [ ] 1.5 Document the file formats in `docs/docs/` (workflow file, machine settings, dependencies) and verify the documented examples parse in a doc test

## 2. Deterministic engine

- [ ] 2.1 Define versioned `Effect` and `Event` enums with `effect_id` correlation and serde round-trip tests; verify a JSON snapshot test pins the wire format
- [ ] 2.2 Implement the per-change state machine (preflight → workspace → implement → verify → review → triage → fix → archive → integrate) as `step(state, event)`; verify table-driven transition tests including preflight failure and triage skip
- [ ] 2.3 Implement the chunk loop with progress rule, gate-failure carry-forward and caps (chunks, no-progress, review/fix rounds, malformed retries); verify tests for each cap blocking the change
- [ ] 2.4 Implement cross-change scheduling (blockers integrated, change/session/resource limits, longest-chain priority, one writer per workspace, switches only at step boundaries, blocked isolation); verify property tests that limits are never exceeded
- [ ] 2.5 Implement structured output schemas (findings, blocked report) and handoff extraction; verify parser tests for valid, malformed and missing sections
- [ ] 2.6 Implement replay and definitions-hash checking; verify a recorded multi-change fixture replays to identical effects and that a changed hash refuses resume

## 3. QueryMT worktree extensions

- [ ] 3.1 Add optional `workflow_run_id` to `WorkspaceOwnership` with a storage migration; verify existing workspace records load unchanged and a repo test round-trips the new field
- [ ] 3.2 Add `accept_workspace_result` sharing prepare/protect/hooks/finalize/recover with delegation acceptance, authorized by workflow run and expected base HEAD; verify integration tests for success, `touches_local_changes`, moved HEAD and crash recovery, and that existing delegation acceptance tests still pass
- [ ] 3.3 Add `forecast_merge` using gix `merge_commits` with object memory; verify tests that clean and conflicting forecasts are reported and that the object database and checkouts are unchanged afterwards
- [ ] 3.4 Wire `AgentMode::is_read_only()` into mutating-tool permission checks; verify a test that review mode denies `edit` and `write_file`

## 4. Workflow actor and run execution

- [ ] 4.1 Add the `workflow` feature and `crates/agent/src/workflow/` with a `WorkflowActor` that persists runs as `workflow` sessions under a run lease; verify a test that a second process cannot drive the same run
- [ ] 4.2 Implement effect adapters: workspace create/handoff/publish/archive, session start with role profile and named sessions, `openspec --json` progress and archive, command runner with resource semaphores, timeouts and bounded output tails; verify adapter tests against a temp repository
- [ ] 4.3 Implement prompt templates with a closed variable set and repo overrides; verify rendering snapshots and that an unknown variable fails at load
- [ ] 4.4 Implement context budget enforcement from `LlmRequestEnd` (single wrap-up steer with `expected_run_id`, cancel at stop, refused-steer recording); verify tests with a scripted provider reaching both thresholds
- [ ] 4.5 Implement the workflow permission policy (in-workspace edits only; read-only roles denied) and post-step snapshots producing `read_only_violation`; verify tests for an out-of-workspace edit and a shell write by a reviewer
- [ ] 4.6 Implement the inbox (triage, approvals, cap overrides, intercepted elicitations) persisted as journal events; verify a test where an unattended session's question is answered through the inbox
- [ ] 4.7 Implement idempotent recovery (requested-without-result effects, interrupted sessions evaluated as ended, integration detection); verify restart tests during a chunk and right after integration

## 5. Merge queue

- [ ] 5.1 Implement the queue workspace flow (reset to base, apply final checkpoint delta, full gate, integrate with expected HEAD, requeue on move); verify an integration test with two changes integrating in order
- [ ] 5.2 Implement post-integration forecasts, conflict-resolution chunk scheduling and base sync at chunk boundaries via a fresh workspace; verify tests for a clean sync and a conflicted forecast
- [ ] 5.3 Implement clean-base preflight and workspace archival after integration; verify a dirty tracked file refuses the run and integrated workspaces remain listed as archived

## 6. Control surfaces

- [ ] 6.1 Add `querymt/workflow/{start,status,plan,approve,answer,resume,abort}` ext methods and list them in `querymt/capabilities`; verify ext-method tests including abort keeping workspaces
- [ ] 6.2 Add `workflow_*` event kinds, regenerate typeshare types for the desktop, and verify the generated types compile
- [ ] 6.3 Add `qmt flow` subcommands mirroring the ext methods, including `plan --dry-run`; verify a CLI test that dry run leaves repo, store and worktree storage unchanged
- [ ] 6.4 Document running workflows (setup, limits, inbox, recovery) in `docs/docs/` and verify the documented commands run against a fixture repository

## 7. End-to-end verification

- [ ] 7.1 Run an end-to-end test on a fixture repository with two independent changes and one dependent change using scripted providers (separate store and worktree root, never `~/.qmt/sessions.db`); verify all three integrate in dependency order with tiny budgets forcing multiple chunks
- [ ] 7.2 Pilot on the remaining tasks of `add-portable-repository-knowledge` in a separate qmtcode instance; verify the run completes or blocks with a recorded reason, and record peak context and duration per chunk

## Workflow follow-up

- Archive the change after review requirements are satisfied.
- Decide between this change and `add-acp-flow-driver` before implementation starts.
