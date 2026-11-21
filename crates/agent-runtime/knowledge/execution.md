---
id: pilot-execution
title: Agent execution loop
description: The execution state machine, tool-call lifecycle, parallel waves, LLM retry and compaction trigger behind one prompt turn.
tags: execution, state-machine, tool-calls, retry, compaction
knowledge_paths: ["crates/agent-runtime/src/agent/execution/**"]
knowledge_sources: ["crates/agent-runtime/src/agent/execution/wave_plan.rs", "crates/agent-runtime/src/agent/execution/llm_retry.rs"]
---

# Agent execution loop

A prompt turn is driven by `execute_cycle_state_machine` in `crates/agent-runtime/src/agent/execution/mod.rs`,
started by the session actor through `ctx.spawn()` so the actor stays responsive to
cancel and mode changes while a turn runs.

## States and transitions

The loop moves an `ExecutionState` (`crates/agent-runtime/src/middleware/state.rs`; a position
that carries only what its step consumes, never run state) through
`BeforeLlmCall`, `CallLlm`, `AfterLlm` and `ProcessingToolCalls`. After tool results are
stored it returns to `BeforeLlmCall`, or enters `WaitingForEvent` when a delegation must
finish first. A turn ends in `Complete`, `Stopped` (a middleware stop) or `Cancelled`.
The transition functions live in `crates/agent-runtime/src/agent/execution/transitions.rs`.

Source: `crates/agent-runtime/src/agent/execution/mod.rs`, `crates/agent-runtime/src/agent/execution/transitions.rs`.

## Run state: one owner per lifetime

The state of a run is split by mutability in `crates/agent-runtime/src/agent/execution/turn.rs`:

- `TurnEnv` is everything fixed when the run starts: session id, run id, the turn-pinned
  `SessionHandle`, tool configuration, cancellation token, steering inbox, cycle
  accounting and the run's `ToolSet`. It is built with a struct literal in
  `execute_prompt_inner` (`session_actor.rs`), so a missing field is a compile error,
  and step functions take it as `&TurnEnv`, including the concurrent tool wave.
- `Turn` is everything that changes while the run executes: the `RuntimeContext`
  (active task, intent), the run objective, the `History`, the statistics, the queued
  notices, the request overrides, the model profile, delegations suspended by
  steering, the repository-knowledge delivery budget, budget warnings, the pre-turn
  snapshot (`PreTurnSnapshot`: `None`, `Pending(task)`, `Ready(snapshot)`) and
  `TurnBudgets`, the counters that bound the run (Stop-hook continuations per reason,
  completion-guard and stop-recovery counts, model requests, the last compaction and
  progress-review positions). It has one owner, the loop, and is borrowed `&mut` by the
  steps that change it. It holds no lock, atomic or cell, so a new run starts from
  `Turn::new` and nothing run-scoped is reachable from another session or a later run.
- `SessionRuntime` keeps only session-scoped caches and grants (session grants, MCP tool
  state, accounting, cache prefix) behind short synchronous locks.

A tool wave shares `&TurnEnv` and `&Turn`: `execute_prepared_tool_call` only reads the
run's `RuntimeContext` store and active task, and writes (progress entries, artifacts,
delegations) go to the session store, not to `Turn`. Anything a wave produces for the run
(`snapshot_part`, `hook_contexts`) comes back in its `ToolResult` and is folded in after
the wave. `Turn` must stay `Sync` for that; `DelegationRecovery` wraps its future in a
`SyncWrapper` for this reason.

Source: `crates/agent-runtime/src/agent/execution/turn.rs`, `crates/agent-runtime/src/agent/session_actor.rs`.

## History: one writer, one projection

The run holds the messages the session store holds (`History` over `Vec<AgentMessage>`,
`turn.rs`). `History::load` reads them once, where the run is set up
(`execute_prompt_inner`); the loop, the native `/compact` and compaction read the run's
history and never the store. `History::commit` is the only way to grow it, the prompt of
the run included: it writes each message to the store and appends it only after the write
succeeded, so a request never carries a message the store does not hold and a failed write
fails the run. Undo/redo snapshot records are the one other write (`store_snapshot_record`):
they are not conversation and a load drops them. Everything the run reads from the history
(the tool baseline in effect, the latest mode reminder, the number of user turns) is looked
up in the messages when asked; nothing is cached in a field beside them. After a compaction
boundary was committed, `History::keep_effective` drops what it closed, as a load would.

`querymt_agent_core::projection::project_request` is the one place a stored history becomes
the messages of a request: a notice is user text, a tool baseline is the request's tool list
and never a message, a tool change after the baseline in effect is a message only for a
provider that takes them, native provider output is replayed only to the target it came
from, and prompt content is cut at the size limit. `projection::tool_baseline` is the one
lookup of the baseline in effect (the projection, `tool_history::offered` and the origin
check all use it). A model request, a compaction request and every token estimate
(`pruning::estimate_history_tokens`, with the provider's `tool_updates`) read the history
through the projection, so what a reload would send is what the run sends. Cache hints are
put on the messages as they are sent (`cache_hints.rs`), after any context hook.

The identity native replay is checked against is `SessionHandle::output_target`: the session
store records no endpoint, so it is completed from the latest stored output of the same
provider and model. It is a gate, not proof of the endpoint configured now: the provider is
the authority and re-checks every output's provenance against its own configuration (OpenAI,
xAI and Codex), degrading what does not match to the portable form. Taking the identity from
the provider itself needs the run's resolved model (`unify-model-call-path`).

Skipped results of a batch that steering interrupted are committed before the steering that
skipped them (`commit_steering` with the results as `preceding`), so the stored order is the
sent order. The run does not reload its history when it completes: it already holds what a
load would give it. `TurnStack::stopped` drops what the middleware queued while consulted
when none of them recovers, so the loop and the test rigs share that rule.

Source: `crates/agent-runtime/src/agent/execution/turn.rs`, `crates/agent-core/src/projection.rs`,
`crates/agent-runtime/src/agent/execution/cache_hints.rs`.

## Tool-call lifecycle

Each call is prepared, then executed (`crates/agent-runtime/src/agent/execution/tool_calls.rs`).

1. Preparation rejects invalid outer JSON before any hook or permission check; it never
   degrades to empty arguments.
2. Arguments are normalized to *effective* arguments, pre-tool hooks run, a hook may
   replace the input or block the call, then the arguments are validated against the
   tool schema.
3. Execution emits the start event, optionally snapshots the workspace, checks the
   permission against the session tool configuration, runs the tool, runs post-tool
   hooks on the complete output, and only then truncates the model-visible output.
4. Hook context contributions travel with the `ToolResult` and are stored after all
   result blocks, so call/result adjacency in the provider protocol is preserved.

A result that a hook blocked, suppressed or redacted must never regain information
through later enrichment.

## Parallel waves

All calls of one model response are planned by `plan_waves`
(`crates/agent-runtime/src/agent/execution/wave_plan.rs`), a pure function. Calls in one wave never conflict
on file access and may run concurrently; waves run one after another. A call that is not
`ParallelSafe` is a barrier and runs alone; no call moves across a barrier. Of two
conflicting calls the earlier one in model order lands in the earlier wave.

## Retry

Two retry paths, non-streaming `call_with_retry` and stream recreation, share one
attempt budget, one backoff calculation and one set of events. Both live in
`crates/agent-runtime/src/agent/execution/llm_retry.rs`. Which errors are transient is decided by
`LLMError::is_retryable()` in the `querymt` crate; read it there rather than from this
note.

- **Non-streaming calls** go through `call_with_retry`. The configured `max_retries`
  (read through `max_attempts()`, at least 1) counts the first request, so the default of
  3 means three requests in total.
- **Streaming calls** rebuild the stream on failure (`handle_stream_failure`, a
  `StreamRetryBudget`). A recreation spends both `max_stream_retries` (default 2) and one
  of the shared `max_attempts`. If output was already emitted the retry still happens and
  may repeat visible text (`TODO(stream-retry-safety)`).
- **Delay** (`retry_delay_secs`): the provider's retry hint when present, capped at
  `max_wait_secs` and not jittered; otherwise a base wait times `backoff_multiplier` to the
  power of (retry number minus one), with proportional jitter (`jitter_ratio`), capped at
  `max_wait_secs`. The base is `transport_wait_secs` (default 10) for failures the provider
  never answered (`LLMError::Transport`, `HttpError`, `IoError`: connection failure,
  timeout, broken or undecodable stream) and `default_wait_secs` (default 60) for
  everything else, including `PluginError`, whose cause is unknown. The first retry waits
  the base.
- **Events:** each wait emits `RateLimited` or `LlmRetryWait` (streams also
  `StreamRecovering`) and each resume `RateLimitResume` or `LlmRetryResume`. The wait is
  a `select!` against the cancel token, so cancelling during a delay ends the turn
  without another attempt.

## Compaction

After a turn, `crates/agent-runtime/src/agent/execution/maintenance.rs` runs pruning, and AI compaction when
the context threshold is reached. Automatic compaction is allowed once between two main
model requests; a second attempt without progress fails with an explicit message instead
of looping. Post-compaction hooks may add context for the new window.

Related: [sessions.md](../../agent-services/knowledge/sessions.md) for the stored history and the compaction layers,
[tools.md](../../agent-services/knowledge/tools.md) for scheduling inputs, [repository-knowledge.md](../../agent-knowledge/knowledge/repository-knowledge.md)
for card delivery that hooks into this loop.
