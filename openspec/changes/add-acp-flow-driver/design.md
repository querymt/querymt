# Design

## Context

See proposal.md (Why). Relevant facts about the agents this driver talks to:

- **ACP endpoints.** QueryMT exposes ACP over stdio and over WebSocket (`/acp/ws`, used by the desktop app and qmacp). Sessions created by any client appear live in the desktop app.
- **QueryMT extensions this driver can use** (discoverable through `querymt/capabilities`):
  - `querymt/worktree/{create,handoff,list,status,result,publish,archive,remove,recover}` (gix-based; the `current_working_tree` baseline includes untracked, non-ignored files)
  - `querymt/session/{steer,queue,runtimeState}`
- **Usage updates.** Standard ACP `usage_update` notifications carry `used` and `size` after every model response (`crates/agent/src/acp/shared.rs:1989`).
- **Permissions.** QueryMT sends `session/request_permission` to the client for tools that require permission. Plan and review modes are not enforced server-side (`AgentMode::is_read_only()` has no callers). Shell and MCP writes do not ask.
- **Integration over ACP.** QueryMT exposes no ACP method for integrating a top-level workspace result. `accept_delegation_result` is internal and specific to delegations.
- **qmacp.** It has a native WebSocket ACP client, but it drops the `usage_update` fields (`qmacp/src/events.rs:96`) and always cancels questions.

## Goals / Non-Goals

**Goals:**
- A standalone driver that works with QueryMT through published ACP and extension methods only, and degrades to plain ACP agents.
- Deterministic, replayable decisions, plus parallel changes with a serialized local merge queue.
- No QueryMT changes required for the first version.

**Non-Goals:**
- Embedding in qmtcode. That is the alternative `add-native-change-workflows`.
- Interactive explore and propose steps.
- Parallel task sections inside one change.
- PR, CI or review-bot integration.
- Remote (mesh) sessions: the driver needs filesystem access to the workspaces.

## Decisions

### D1. Engine and I/O are split inside the driver
`engine` is a pure module, `step(state, event) -> (state, actions)`, with versioned serde enums. The `runtime` is async tokio code that executes actions through backends and journals events.

The same split as the native proposal keeps replay possible. It also lets the engine move into a shared crate later if both approaches ever coexist.

### D2. Backend trait with two implementations
`AgentBackend` operations: `connect`, `capabilities`, `new_session(cwd, role)`, `prompt`, `cancel`, `wrap_up`, `load_session`, plus a stream of usage, permission, question and end events.
- `QueryMtBackend`: uses the extensions when they are detected.
- `PlainAcpBackend`: standard ACP only.

`WorkspaceBackend` operations: `create`, `handoff`, `checkpoint`, `archive`, `status`.
- `QueryMtWorkspaces`: `querymt/worktree/*`.
- `GitWorkspaces`: driver-managed gix worktrees.

Backends are chosen per agent from the capabilities recorded at run start, so a resumed run never silently changes backend.

*Alternative:* QueryMT-only. Simpler, but it gives up the portability that is the main reason to build the driver externally.

### D3. Transport
WebSocket through `tokio-tungstenite` with the ACP JSON-RPC framing, plus stdio child processes through `agent-client-protocol`'s client-side connection. One connection per agent is shared by all of that agent's sessions.

The driver implements the ACP client role: it answers `session/request_permission`, elicitation and question requests, and `fs/*` requests (scoped to the session's workspace).

*Alternative:* reuse the qmacp library. It would first need to forward `usage_update` and accept custom request handlers. That reuse is optional and can happen later.

### D4. Budget from `usage_update`
The driver tracks `used` per session. Wrap-up through steer requires the current run id from `runtimeState`, so the driver queries it just before steering. A steer that is rejected or not steerable falls back to cancel plus a follow-up prompt. Agents that never send `usage_update` fall back to a wall-clock timeout.

### D5. Permission policy in the client
Locations come from the permission request's `toolCall.locations` and the `rawInput` paths. An edit is granted only when every location canonicalizes inside the workspace. Read-only roles: kinds `edit`, `delete` and `move` are denied, and `execute` is granted only for an allowlist of read-only commands configured per role (default: none).

Post-step snapshots (gix status plus HEAD of each workspace and the base checkout) catch writes that never asked for permission.

QueryMT sessions should be configured so that mutating tools require permission. The driver warns at `flow plan` when the role's profile auto-approves edits.

### D6. Workspace lifecycle with QueryMT
- **Create:** `querymt/worktree/create {cwd: <base checkout>, baseline: current_working_tree, name}` creates the first chunk session.
- **Next chunk:** `querymt/worktree/handoff {summary:false}` gives ownership to a new session.
- **Checkpoint:** `publish` creates an immutable result ref.
- **Review:** reviewers run as separate read-only sessions in the same workspace while no writer is active (one writer per workspace). QueryMT allows multiple connections, but only one writable owner.

### D7. Integration done by the driver with gix
QueryMT exposes no workspace-integration method, so the driver integrates itself:
1. Compute the delta `baseline → checkpoint` (both commits come from `worktree/result` or the driver's own refs).
2. Three-way merge it onto the base tip in memory.
3. Write the merged tree into the queue worktree and run the full gate.
4. Create one commit with parent = the recorded base tip, then advance the branch with a compare-and-swap ref update.
5. If the base branch is checked out in the base checkout, check out only the touched paths, after verifying they carry no local state.

This repeats the logic of QueryMT's `prepare`/`protect`/`finalize` in a smaller form, without commit hooks in the first version.

*Alternative:* a new QueryMT extension `querymt/worktree/integrate`. Better long term (hooks, recovery), so it is listed as an optional follow-up. The driver uses it when it is advertised.

### D8. Journal on disk
`<git-common-dir>/flow/<run-id>/journal.jsonl` is append-only and fsynced per record. `lock` uses an OS file lock. `state.json` is a derived cache. On resume, the driver replays the journal and re-runs actions that were requested but have no recorded result, using idempotency keys: session names, the workspace per change, checkpoint commits, and integration detected when the base contains the integration commit's trailer `Flow-Change: <change>`.

### D9. Prompts
Prompts are versioned templates (`apply_chunk`, `fix_gate`, `review`, `fix_findings`, `resolve_conflict`, `spec_sync`) with a closed set of variables, embedded and overridable from the repository. The apply prompt forbids questions, and any question still goes to the inbox.

### D10. Packaging
`crates/flow-driver` is a workspace member that builds a `flow` binary. It depends on no internal QueryMT crate, so it can be moved to its own repository without code changes.

## Risks / Trade-offs

- [Integration logic duplicates QueryMT's] → Kept minimal (delta, merge, CAS commit, touched-path checkout with local-work refusal). The driver switches to `querymt/worktree/integrate` once it exists.
- [Writes that never ask for permission] → Post-step snapshots, one workspace per change, and a warning when the profile auto-approves edits.
- [Driver and agent can drift in protocol versions] → Capabilities are recorded at run start. Resume refuses when a required extension has disappeared.
- [Another long-running process to keep alive] → Reattach through `session/load`. A dead driver leaves agent sessions running, and they are evaluated on resume.
- [Steering is a QueryMT-only extension] → The cancel-plus-follow-up fallback is defined and tested.
- [The desktop app could answer permissions first] → Requests go to the connection that owns the session prompt. The driver creates its own sessions, so it is that connection. This is verified in the pilot.

## Migration Plan

This is a new binary, and nothing changes in QueryMT. Rollback means not running it. Driver-managed worktrees and branches can be removed with `flow clean`. QueryMT workspaces stay managed by QueryMT.

## Open Questions

- Should `querymt/worktree/integrate` be proposed as a separate QueryMT change after the pilot? The driver works without it.
