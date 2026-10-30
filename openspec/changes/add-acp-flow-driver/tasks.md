# Tasks

## 1. Crate and definitions

- [ ] 1.1 Create `crates/flow-driver` with a `flow` binary and no dependency on internal QueryMT crates; verify `cargo tree -p flow-driver` shows no `querymt*` crates and `flow --help` lists `run|plan|status|approve|answer|resume|abort|clean`
- [ ] 1.2 Implement `openspec/flow.yaml` parsing with schema version, unknown-key rejection, `copy_untracked` and the built-in default workflow; verify unit tests for valid, unknown-key and missing-gate cases
- [ ] 1.3 Implement machine settings (agents with `ws://` or stdio command, role → agent/profile/budget/read_only, limits) with validation; verify tests for missing role and invalid budget
- [ ] 1.4 Implement pi-format `dependencies.yaml` parsing, archived-blocker satisfaction and cycle detection; verify tests including a fixture copied from pi-agent-dashboard
- [ ] 1.5 Document the file formats in `crates/flow-driver/README.md` and verify the documented examples parse in a doc test

## 2. Deterministic engine and journal

- [ ] 2.1 Define versioned `Action` and `Event` enums with correlation ids; verify a JSON snapshot test pins the format
- [ ] 2.2 Implement the per-change state machine and chunk loop (progress rule, gate carry-forward, all caps); verify table-driven transition tests for each cap and for preflight failure and triage skip
- [ ] 2.3 Implement cross-change scheduling (blockers, change/session/resource limits, longest-chain priority, one writer per workspace, blocked isolation); verify property tests that limits are never exceeded
- [ ] 2.4 Implement structured outputs (findings, blocked report) and handoff extraction; verify parser tests for valid, malformed and missing cases
- [ ] 2.5 Implement the journal (`<git-common-dir>/flow/<run>/journal.jsonl`, fsync per record, file lock, definitions hash) and replay; verify a recorded fixture replays to identical actions and a second driver is refused

## 3. ACP client and agent backends

- [ ] 3.1 Implement the WebSocket and stdio ACP client transports with client-role request handling (`session/request_permission`, elicitation, `fs/*` scoped to the workspace); verify tests against a scripted fake ACP agent for both transports
- [ ] 3.2 Implement capability detection and the `QueryMtBackend` / `PlainAcpBackend` selection recorded per run; verify tests for a capabilities response and for `method_not_found`
- [ ] 3.3 Implement sessions per step (cwd, profile/model when supported, names), prompt templates with closed variables, and full final-message capture; verify rendering snapshots and a fake-agent chunk test
- [ ] 3.4 Implement budget tracking from `usage_update`, wrap-up via `querymt/session/steer` with the run id from `runtimeState`, the cancel-plus-follow-up fallback, cancel at `stop` and timeout when usage is absent; verify fake-agent tests for each path
- [ ] 3.5 Implement the permission policy (in-workspace edits, read-only denial, execute allowlist), the auto-approve warning in `flow plan`, and post-step snapshots producing `read_only_violation`/`foreign_write`; verify tests for an outside edit, a reviewer edit and a shell write
- [ ] 3.6 Implement the inbox for questions and elicitations with `flow answer` and policy answers; verify a fake-agent test where a question is answered through the inbox
- [ ] 3.7 Implement resume and reattach via `session/load`, falling back to evaluating the step as ended; verify tests for both cases

## 4. Workspaces, checkpoints and integration

- [ ] 4.1 Implement `QueryMtWorkspaces` (create with `current_working_tree`, handoff without summary, publish, result, archive); verify against a test qmtcode instance with its own store and worktree root that untracked `.agents/skills` appear in the workspace
- [ ] 4.2 Implement `GitWorkspaces` with gix (worktree on `flow/<change>`, `copy_untracked`, checkpoint commits); verify tests in a temp repository
- [ ] 4.3 Implement clean-base preflight and in-memory merge forecasts that write nothing; verify tests for dirty tracked files and for clean and conflicting forecasts leaving the object database unchanged
- [ ] 4.4 Implement archive (`openspec archive --yes`, strict spec validation, one spec-sync retry); verify a test where archive fails twice and blocks
- [ ] 4.5 Implement the merge queue (delta onto base tip, queue worktree gate, single commit with a `Flow-Change` trailer, CAS ref update, touched-path checkout with `touches_local_changes` refusal, requeue when base moves); verify tests for success, moved base, local-work collision and that inherited untracked baseline files are not committed
- [ ] 4.6 Use `querymt/worktree/integrate` when it is advertised, falling back to the driver's own integration; verify with a fake agent that advertises it

## 5. CLI and docs

- [ ] 5.1 Implement `flow plan` (dry run with agent capabilities), `status`, `approve`, `answer`, `abort` (cancel sessions, kill gates, keep workspaces) and `clean`; verify CLI tests including that `plan` creates nothing
- [ ] 5.2 Write a usage guide (QueryMT setup with `--acp-ws` and `[agent.workspace]`, a plain ACP agent setup, limits, inbox, recovery) and verify the documented commands run against a fixture repository

## 6. End-to-end verification

- [ ] 6.1 Run an end-to-end test with a fake ACP agent on a fixture repository: two independent changes and one dependent change, tiny budgets forcing multiple chunks; verify they integrate in dependency order and that replay of the journal matches
- [ ] 6.2 Pilot against a separate qmtcode instance (never `~/.qmt/sessions.db`) on the remaining tasks of `add-portable-repository-knowledge`; verify the run completes or blocks with a recorded reason, sessions are visible in the desktop app, and record peak context and duration per chunk

## Workflow follow-up

- Decide between this change and `add-native-change-workflows` before implementation starts.
- Consider a separate QueryMT change adding `querymt/worktree/integrate` after the pilot.
- Archive the change after review requirements are satisfied.
