---
id: agent-delegation
title: Delegation lifecycle
description: How a planner's delegation request is claimed, run in a child session (optionally in an isolated worktree), summarized, verified, completed or cancelled, and how the parent waits for it.
tags: delegation, quorum, orchestrator, worktree, isolation, cancellation
knowledge_paths: ["crates/agent-runtime/src/delegation/*.rs", "crates/agent-core/src/delegation/*.rs"]
knowledge_sources: ["crates/agent-runtime/src/delegation/core.rs", "crates/agent-core/src/delegation/model_overrides.rs"]
---

# Delegation lifecycle

A planner agent hands work to a delegate through the `delegate` tool. The execution loop
(`crates/agent-runtime/src/agent/execution/tool_calls.rs`) resolves the delegation's
isolation (`inherit` becomes `isolated` or `shared`, using the tool call, then the delegate's
configured value, then the parent's execution mode), records the delegation and emits
`DelegationRequested`. Everything after that is done by the `DelegationOrchestrator` in
`crates/agent-runtime/src/delegation/core.rs`, which therefore only sees `Isolated` or
`Shared`. Configuration
fields are described in the agent-config note; user documentation is
`docs/docs/agent/delegation.md`.

## Pieces

- `DelegationOrchestrator`: listens for delegation events on the `EventFanout`
  (`start_listening`), owns the map of active delegations and a semaphore sized by
  `max_parallel_delegations`.
- `DelegationOrchestratorConfig`: `cwd`, `inject_results`, `run_verification`, `wait_policy`,
  `wait_timeout_secs`, `cancel_grace_secs`, `max_parallel_delegations`. Builder methods
  (`with_verification`, `with_wait_policy`, ...) set them.
- `AgentRegistry` / `DefaultAgentRegistry` (`agent_registry.rs`, bottom of `core.rs`): maps a
  delegate id to its `AgentInfo` (description, capabilities, configured isolation) and its
  `AgentHandle`.
- `DelegationSummarizer` (`summarizer.rs`): optional LLM call that writes an implementation
  brief from the parent history.
- `DelegateModelOverrideStore` (`agent-core/src/delegation/model_overrides.rs`): per
  (parent session, agent id) model and reasoning-effort overrides.

## Admission

On `DelegationRequested` (`handle_envelope`):

1. When the orchestrator has a `profile_id`, the parent session's profile binding is read
   (up to 3 attempts). A session owned by another profile is ignored. An unbound session is
   ignored with an error log. If the owner cannot be read, the delegation is failed.
2. `claim_delegation` moves the record to claimed. Only transient storage errors are retried
   (3 attempts total). `AlreadyClaimed` and terminal states are ignored, so duplicate events
   run a delegation once; `NotFound` emits `DelegationFailed`.
3. The target must be registered with a handle, otherwise the delegation fails.
4. A task is spawned but waits on a start signal until it is registered in
   `active_delegations`; if shutdown began meanwhile it is aborted and failed. Then it takes a
   semaphore permit, so delegations beyond `max_parallel_delegations` queue.

## Execution (`execute_delegation`)

In order:

1. **Isolation gate.** `Isolated` fails closed when there is no workspace service or the target
   does not support local workspaces. Requested isolation never falls back to shared
   execution, and no child session is created.
2. **Parent reasoning effort** is read from the parent's session LLM config at this moment;
   a missing or unreadable config fails the delegation.
3. **Parent cwd** is resolved through `WorkspaceService::resolve_session_cwd` (the durable
   workspace binding wins over the stored row). Failure to resolve fails the delegation; the
   child is never launched into a source checkout as a fallback.
4. **Isolated workspace** is allocated before the child session exists
   (`allocate_for_delegation`, under the parent's mutation gate for baseline capture only).
   The child's cwd is then its own workspace, never the parent's checkout.
5. **`delegation_start` hook** runs against the real execution root; its additional context is
   appended to the planning context.
6. **Child session** is created through `AgentHandle` (`create_isolated_delegation_session` or
   `create_delegation_session`). If that fails, an allocated workspace is marked failed and
   retained, not left Ready. The child inherits the parent's UI bridge route when local.
7. With a `profile_id`, the child's runtime binding is persisted; a failure tears down the
   child session and fails the delegation.
8. **Routing and model.** A routing-table policy with a resolved peer sets the child's provider
   node. Then overrides are resolved (`DelegateModelOverrideStore::resolve`: durable
   assignments in the session store win even if empty, the in-memory cache is only a
   fallback). A model override is applied with `set_session_model`; reasoning effort is applied
   afterwards because a model change rebuilds the LLM config. With no reasoning override the
   child inherits the parent's effort. Any failure here tears down the child and fails.
9. `SessionForked` (origin delegation) is emitted, including the confirmed model.
10. **Planning summary.** If a summarizer is configured, parent history is summarized, saved
    with `set_delegation_planning_summary`, and injected as `<planning-context>` through
    `set_planning_context`. The summary is charged to the cycle that requested the
    delegation. A refused (price cap) or failed summary is skipped; the delegate still runs.
11. **Prompt.** The child is prompted with `build_delegation_prompt`; the call is raced against
    the cancellation token.

## Outcomes

- **Cancelled** during the prompt: the child session is cancelled, status moves
  `Running` to `Cancelled`, `DelegationCancelled` is emitted, and the retained workspace
  location (if any) is announced. Nothing is integrated or deleted.
- **Stop reason other than `EndTurn`** (context limit, max turns, refusal): failure
  ("stopped prematurely"), because the output is likely incomplete.
- **Prompt error:** failure.
- **`EndTurn`:** optional verification (`run_verification`, using a `VerificationSpec` built
  from the delegation, run in the delegate's own cwd; a delegation without a spec counts as
  passed). A failed verification fails the delegation. Otherwise the child history is turned
  into a summary (`extract_session_summary_from_history`), the `post_delegation` hook may append
  context, and for isolated runs `publish_result` captures the delegate's uncommitted work
  as a baseline-relative review artifact. A capture error is disclosed in the summary but
  never fails the delegation or removes the workspace. The summary says nothing was merged;
  integration is a separate `accept_delegation_result` step. Status moves `Running` to
  `Complete`, `DelegationCompleted` is emitted, and with `inject_results` the summary is
  injected into the parent.

`fail_delegation` is the single failure path: transition `Running` (else `Requested`) to
`Failed`, resolve the child's terminal attempt, run the `delegation_failure` hook, emit
`DelegationFailed`, and inject the failure into the parent when `inject_results` is on.
Status transitions are compare-and-set; losing the race (already cancelled, for example)
silently ends the task.

## Cancellation and shutdown

- `DelegationCancelRequested` cancels one delegation's token, then waits up to
  `cancel_grace_secs` for the task to finish before aborting it.
- A parent `Cancelled` event does the same for every active delegation of that session.
- `cancel_active_delegations` (shutdown) sets `shutting_down`, cancels all, waits 1 second per
  task, aborts, and records `Cancelled`; the orchestrator ignores events afterwards.
- `DelegationCancellation` resolves the child's terminal attempt exactly once, whichever path
  gets there first.

## How the parent waits

The parent's execution loop (`crates/agent-runtime/src/agent/execution/wait.rs`) enters a
delegation wait after the tool call. With `delegation_wait_policy = any` it resumes on the
first delegation outcome (completion, failure or cancellation); with `all` it waits for every
outstanding delegation. In the `all` path `delegation_wait_timeout_secs` is a no-progress
timeout that is re-armed each time one finishes, so queued delegations do not spend their
budget waiting for a permit; `0` disables it. New user steering interrupts either wait and
leaves the running delegations to be reconciled later.

## Invariants

- A delegation is claimed once; duplicates are ignored.
- Isolated execution never silently degrades to the parent's checkout.
- An isolated workspace is allocated before its session and is retained, never auto-removed,
  on failure or cancellation.
- Delegate result capture and summaries never claim anything was integrated.
- A failed summary never fails a delegation.
- Model overrides are applied before reasoning effort.

Source: `crates/agent-runtime/src/delegation/core.rs`, `summarizer.rs`,
`crates/agent-core/src/delegation/model_overrides.rs`,
`crates/agent-runtime/src/agent/execution/wait.rs`.
