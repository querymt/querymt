# Design

## Context

See proposal.md - Why. Relevant current state:

- `ChatStreamAccumulator::finish` (`querymt/src/chat/streaming.rs`) returns `Incomplete` for `Length` only while legacy calls are still open. The Anthropic parser (harden-tool-argument-handling D9) flushes open blocks as marked `ToolUseComplete` before `Done`, so the accumulator sees no open calls and returns `Completed` with finish reason `Length`. Anthropic non-streaming parsing also reports `Completed`.
- `transition_after_llm` synthesizes results only in its `output_status == Incomplete` branch. For `Completed`+`Length` it falls through to the `Length` match arm, which returns `Stopped{ModelTokenLimit}` and drops `gated_results` (the `INVALID_JSON` result of the marked call).
- The `Incomplete`+`Length` branch clears `gated_results` and gives every call only the split text, so invalid calls never get `INVALID_JSON` (tool-argument-handling "Invalid outer arguments never execute").
- `ConversationContext.request_overrides` lasts one request (output-limit-recovery D3). `OutputLimitRecoveryMiddleware` re-applies `escalated_cap` only on continuation (`continuation_options`), not on ordinary tool-loop steps.
- `repair_unmatched_tool_calls` (`model.rs`) repairs unmatched calls only when history is loaded from storage.

## Goals / Non-Goals

**Goals:** the delta requirements in `specs/`.

**Non-Goals:**
- Running valid calls that completed before a cut-off (decided: they are skipped, unchanged from today).
- Keeping the escalated cap across turns or sessions.
- Changing the one-request semantics of `request_overrides` or the `chat-generation-options` contract.
- Recovering content the provider never streamed (a cut-off call's missing arguments are lost by design).

## Decisions

### 1. One cut-off predicate
`LlmResponse::is_length_cutoff()` = `finish_reason == Some(Length)`. `transition_after_llm` enters the result-synthesis branch when `output_status == Incomplete || is_length_cutoff()`. This makes classification independent of whether a provider flushes interrupted blocks.
*Alternative rejected:* make the accumulator return `Incomplete` for any `Length` with tool calls. It would also have to handle non-streaming parsing in every provider, and the agent would still depend on provider status for safety.

### 2. Per-call results on a cut-off
Within that branch, each call (already de-duplicated) gets exactly one result:
- invalid (provider-marked, malformed or non-object): `invalid_input_result(call)` plus a second text part with the split guidance on a length cut-off;
- valid: the skipped result (`skipped_call_text`).
Non-length `Incomplete` keeps today's behaviour (gated results for invalid calls, generic skip text for the rest). Length cut-offs with results go through `store_results_then_stop_on_length`; with no calls, `Stopped{ModelTokenLimit}` as before. The trailing `Length` match arm is now reachable only without tool calls (`debug_assert`).

### 3. Escalated cap kept via `on_step_start`
`OutputLimitRecoveryMiddleware::on_step_start` sets `request_overrides.max_tokens = turn.escalated_cap` when the turn has escalated and no other override set a cap. Every `BeforeLlmCall` passes through `run_step_start` (tool-loop steps, escalation retries, continuations), and `transition_before_llm_call` re-clamps the cap to the context room. So:
- `escalate_to` and `continuation_options` still set the cap themselves. That keeps the escalation retry correct without the step hook (tests call `on_after_llm` alone) and lets the continue event report `to_max_tokens`. `on_step_start` leaves an existing cap override alone, so the two never conflict.
- `escalated_cap` stores the unclamped target `min(ceiling, room at escalation)`; per-request clamping covers later shrinkage. Room freed by compaction is not used to go above that target (kept simple; the ceiling still bounds it).
- The turn state is cleared at turn start (`on_turn_start`/`reset`) as today, so the next turn starts at the initial cap.
*Alternative rejected:* making `request_overrides` sticky in core. It changes the generic override contract for every middleware to serve one.

### 4. Pre-request pairing guard
After request messages are built in `transition_before_llm_call`, a `ChatMessage`-level check finds assistant function calls not answered in the following user message. Unanswered ones get a cancelled error result inserted (into the next user message, ahead of other parts, or a new user message). The repair logs `error!` with the call ids and is guarded by `debug_assert!` so tests fail. The stored transcript is untouched.
*Alternative rejected:* failing the run. It turns a recoverable integrity bug into a lost user run, which is what this change fixes.

## Risks / Trade-offs

- [Escalated cap raises cost per step for the rest of the turn; budget-thinking models get a larger thinking budget] → Bounded to one turn; usage is reported per request.
- [Higher `max_tokens` may reserve more output-token rate limit] → Bounded to the turn after a real cut-off.
- [OpenAI structured `Incomplete` with malformed calls now shows `INVALID_JSON`] → Intended by tool-argument-handling.
- [debug_assert in the guard could fire in debug-built user binaries] → Only on a real integrity bug, which otherwise leads to a provider 400.

## Migration Plan

No storage or wire change. Rollback: revert; existing sessions are repaired at load by `repair_unmatched_tool_calls`.
