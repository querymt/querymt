# Design

## Context

See proposal.md (Why). Relevant current state:

- **Managed worktrees (gix only).** `crates/agent/src/worktree/` (~25.6k lines) covers:
  - allocation with `BaselineMode::CurrentWorkingTree`, which includes untracked, non-ignored files (`repo_ops.rs:1450`)
  - `publish_result`, which writes create-only `refs/qmt/results/<ws>/<id>` refs and never touches the index (`service.rs:754`)
  - handoff with `summary: false`
  - three-way `merge_trees` with conflict and local-work refusals (`prepare.rs`)
  - commit hooks, finalize and crash recovery

  Integration is only reachable through `accept_delegation_result`, which requires a completed delegation and its parent session (`integrate.rs:329`).
- **Session control.** `AgentSession::steer(expected_run_id, …)`, `runtime_state` and `cancel`. Steers are persisted and folded into the run objective (`execution/mod.rs:149`, `objective.rs:167`).
- **Context tokens.** `LlmRequestEnd` events carry `context_tokens` and are delivered in-process through `EventFanout::subscribe`.
- **Permissions.** Permission requests can be decided by hooks (`tool_calls.rs:1068`). `AgentMode::is_read_only()` has no callers, so plan and review modes do not block edits.
- **Persistence.** The event journal, run leases (`session/run_lease.rs`) and orphaned-run reconciliation (`session/run_reconcile.rs`) exist. The scheduler is a kameo actor that sends prompts to session actors.
- **Extism host.** It currently offers only HTTP, logging and stream chunks (`crates/querymt/src/plugin/extism_impl/host/functions.rs`).
- **Existing manual workflow.** Chunks used 132k–426k tokens each, and 58k–115k of that was startup reading. Questions from the archive skill (`question` tool) blocked unattended use.

## Goals / Non-Goals

**Goals:**
- All workflow decisions are made in a pure core whose effects and events are serializable, so the core can later run as a WASM engine.
- Reuse the worktree, session, journal and permission machinery instead of duplicating it.
- Run several changes in parallel, with deterministic, serialized integration.

**Non-Goals:**
- Driving non-QueryMT agents. The ACP alternative covers that.
- WASM hosting in this change.
- Interactive explore and propose steps. Planning stays a conversation with the user.
- Parallel task sections inside one change.
- PR, CI or review-bot integration.

## Decisions

### D1. Pure core crate `flow-core` with `step(state, event) -> (state, effects)`
The core does not depend on tokio, the filesystem, a clock, randomness or `crates/agent`. It is serde only.
- **Effects:** `CreateWorkspace`, `StartSession`, `Steer`, `Cancel`, `RunCommand{resources}`, `Publish`, `Forecast`, `Integrate`, `Archive`, `AskHuman`, `SetTimer`, `QueryProgress`.
- **Events:** the matching results, plus `Usage{context_tokens}`, `RunEnded`, `Answered`, `TimerFired` and `Started{ids, now}`.

Every effect carries an `effect_id`, and every event carries the `effect_id` it answers. Both are versioned enums (`#[serde(tag="type")]`, `v: u32`) that use no Rust-only types.

*Alternative:* an async engine calling services directly. It is simpler at first, but replay becomes impossible and the WASM path would need a rewrite.

### D2. QueryMT hosts the core in a `WorkflowActor` (kameo)
One actor per run. It loads the definitions, feeds events to the core and executes effects through adapters:
- `WorktreeOps`
- `AgentSession`
- a command runner with resource semaphores
- the `openspec` CLI with `--json`
- the inbox

The actor is the only writer of the run journal. It lives in `crates/agent/src/workflow/` behind a `workflow` cargo feature.

*Alternative:* extending the delegation orchestrator. Rejected because delegation is planner-LLM-driven and keyed to parent sessions.

### D3. Journal = a session of kind `workflow`
Each run is stored as a session with kind `workflow`, following the `dotagents_automation` precedent. Its event stream holds `workflow_*` events: event, effect, prompt, decision. This reuses the ordering, persistence and desktop streaming the event journal already provides. A run lease prevents two processes from driving the same run.

*Alternative:* new tables. Rejected because they would duplicate ordering and subscription.

### D4. Idempotent effects and recovery
Before an effect runs, the actor records `effect_requested`. When it completes, the actor records the resulting event. On resume, it re-executes each requested effect that has no result, using a natural idempotency key:
- workspace by run and change
- publish result ref recorded
- integration detected through the `refs/qmt/integrations` ref and base HEAD ancestry
- sessions found by name `flow/<run>/<change>/<step>/<n>`

An interrupted session is never resumed. It is evaluated as ended (`QueryProgress`).

### D5. Progress comes from `openspec instructions apply --change <c> --json`
`tasks[].done` counts are compared before and after each chunk. The `state == all_done` check gates exit from the loop. The agent's self-reports are ignored.

### D6. Budget is enforced by the actor, not by middleware or hooks
The actor watches `LlmRequestEnd.context_tokens` for the step's session. At `wrap_up` it sends one steer with the current `run_id` from `runtime_state`. At `stop` it cancels.

*Alternatives:*
- A context hook. Its text is not persisted and it cannot stop a run.
- Agent-side budget middleware. Possible later, but it is not needed because the actor is in-process.

### D7. Read-only roles through permission decisions plus post-checks
Workflow sessions get a permission policy installed by the actor. Mutating tools are denied for read-only roles. Edits are allowed for writer roles only when every path in `extract_locations` is inside the step's workspace.

Opaque writes through shell or MCP cannot be fenced, so after every read-only step the actor compares workspace status, the base checkout and idle workspaces with their pre-step snapshots and blocks the change with `read_only_violation` on any difference. Wiring `AgentMode::is_read_only()` for the mode itself is a small extra fix done here.

### D8. Generalize integration to workflow-owned workspaces
`WorkspaceOwnership` gains an optional `workflow_run_id`, and a new `accept_workspace_result(AcceptWorkspaceRequest)` is added. It shares preparation, protection, hooks, finalize and recovery with `accept_delegation_result`, but authorizes by workflow run instead of by delegation parent. Existing delegation behavior and refusal codes do not change.

### D9. Merge forecast in memory
`RepositoryOps::forecast_merge(base, ours, theirs)` uses gix `merge_commits` on a repository opened `with_object_memory()`, so nothing is written. It returns conflict paths or `clean`.

### D10. Merge queue through a queue workspace
For each change in the queue:
1. A dedicated managed workspace is reset to the current base.
2. The final checkpoint delta is applied to it with the same merge and prepare logic.
3. The full gate runs there.
4. `accept_workspace_result` targets the base checkout and requires the HEAD recorded in step 1. If HEAD moved, it refuses and the change is requeued.

Only one queue item runs at a time.

### D11. Syncing a running change with a new base
This is done only at chunk boundaries, and only on a clean forecast. A new workspace is allocated from the new base. The change's checkpoint delta is applied there, and ownership continues in the new workspace. This avoids rebasing the existing checkout. Conflicts schedule one resolution chunk in the old workspace, with the conflict paths in the prompt.

### D12. Prompts are versioned templates
The templates are `apply_chunk`, `fix_gate`, `review`, `fix_findings`, `resolve_conflict` and `spec_sync`. They are embedded in the default workflow and can be overridden by repository-relative paths in `flow.yaml`. Variables are a closed set. Unknown variables fail at load.

The apply prompt forbids the `question` tool. Questions still route to the inbox (D13) as a safety net.

### D13. Inbox
Inbox items are persisted as journal events and exposed through `querymt/workflow/status` and `answer`. Elicitations from workflow sessions are intercepted by the actor, using the existing elicitation path keyed by session, instead of being forwarded to an absent client.

## Risks / Trade-offs

- [Workflow logic ships with qmtcode releases] → The core is a separate crate with replay tests. Definitions and prompts live in the repository.
- [`crates/agent` grows while a shell refactor is planned] → All logic is in `flow-core`. The agent module only adapts effects. The effect and event enums are the intended future engine interface.
- [Opaque shell writes escape permission checks] → Post-step snapshots, plus one worktree per change.
- [Wrap-up arrives late, during long tool calls] → The hard `stop` threshold applies. The interrupted chunk is evaluated from state.
- [The checkbox is a model claim] → Gates run every chunk. Review and the full gate run on the merged result.
- [Disk and compile load with parallel worktrees] → The `cargo` resource capacity and `limits.changes` defaults (2 and 3) apply. The workflow `env` supports a shared compile cache.
- [Event schema drift across upgrades] → Versioned events. Resume refuses unknown versions with a clear error.

## Migration Plan

The feature is opt-in: it is behind the `workflow` cargo feature, and runs exist only once started. Rollback means disabling the feature. Journals remain readable as ordinary sessions, and workspaces stay managed by the existing worktree tooling.

## Open Questions

- Should the default chunk gate scope (crates touched since base) live in a provided script or in workflow-file globs? This can be decided during the pilot without changing the specs.
