---
id: pilot-execution
title: Agent execution loop
description: The execution state machine, tool-call lifecycle, parallel waves, LLM retry and compaction trigger behind one prompt turn.
tags: execution, state-machine, tool-calls, retry, compaction
knowledge_paths: ["crates/agent/src/agent/execution/**"]
knowledge_sources: ["crates/agent/src/agent/execution/wave_plan.rs"]
---

# Agent execution loop

A prompt turn is driven by `execute_cycle_state_machine` in `src/agent/execution/mod.rs`,
started by the session actor through `ctx.spawn()` so the actor stays responsive to
cancel and mode changes while a turn runs.

## States and transitions

The loop moves an `ExecutionState` (`src/middleware/state.rs`) through
`BeforeLlmCall`, `CallLlm`, `AfterLlm` and `ProcessingToolCalls`. After tool results are
stored it returns to `BeforeLlmCall`, or enters `WaitingForEvent` when a delegation must
finish first. A turn ends in `Complete`, `Stopped` (a middleware stop) or `Cancelled`.
The transition functions live in `src/agent/execution/transitions.rs`.

Source: `src/agent/execution/mod.rs`, `src/agent/execution/transitions.rs`.

## Tool-call lifecycle

Each call is prepared, then executed (`src/agent/execution/tool_calls.rs`).

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
(`src/agent/execution/wave_plan.rs`), a pure function. Calls in one wave never conflict
on file access and may run concurrently; waves run one after another. A call that is not
`ParallelSafe` is a barrier and runs alone; no call moves across a barrier. Of two
conflicting calls the earlier one in model order lands in the earlier wave.

## Retry

Transient provider errors are retried by `call_with_retry`
(`src/agent/execution/llm_retry.rs`). The delay uses the provider's retry hint when one
exists, capped by the configured maximum wait, otherwise exponential backoff. Attempts
are bounded by the rate-limit policy and the wait respects cancellation.

## Compaction

After a turn, `src/agent/execution/maintenance.rs` runs pruning, and AI compaction when
the context threshold is reached. Automatic compaction is allowed once between two main
model requests; a second attempt without progress fails with an explicit message instead
of looping. Post-compaction hooks may add context for the new window.

Related: [sessions.md](sessions.md) for the stored history and the compaction layers,
[tools.md](tools.md) for scheduling inputs, [repository-knowledge.md](repository-knowledge.md)
for card delivery that hooks into this loop.
