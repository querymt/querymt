# Proposal

## Why

Session `01a16d18-6bf3-7219-849e-967cc8b81124` failed with an Anthropic `invalid_request_error` (`messages.36: tool_use ids were found without tool_result blocks immediately after`). A reply cut off by the output-token limit contained one `write_file` call. The Anthropic parser flushed the interrupted block as a completed call marked invalid, so the stream finished `Completed` with finish reason `Length`. `transition_after_llm` only synthesizes tool results for `Incomplete` replies; for a `Completed`+`Length` reply it stopped without storing the gated `INVALID_JSON` result. Output-limit recovery then continued with an unanswered `tool_use` in history.

The cut-off also happened at 32k although the turn had already escalated to 128k: request overrides last one request, so the escalation was lost on the next tool-loop step, and escalation can't be spent twice per turn.

Two archived changes conflicted: output-limit-recovery D4 assumed a length stop inside a call yields `Incomplete`; harden-tool-argument-handling D9 later made Anthropic flush open blocks as completed (invalid) calls. Neither spec stated which wins, and no end-to-end test covered the combination.

## What Changes

- A reply is a length cut-off when its finish reason is `length`, regardless of the output status the provider reports.
- No call from a cut-off reply runs. Each call gets exactly one error result: invalid calls get the `INVALID_JSON` result plus split-the-write guidance; valid calls get a skipped result with the guidance. The run then stops with model token limit and goes to recovery.
- After an escalation, every later request in the turn uses the escalated cap (re-clamped to the context room per request). Escalation remains once per turn.
- Continuation applies to any cut-off that can't be escalated, not only cut-offs "at the ceiling" (spec amended to match existing behaviour).
- Before each model request, every assistant tool call must be answered by exactly one result. A violation is a bug: debug builds assert; release builds repair it with a cancelled result and log an error.

## Capabilities

### New Capabilities
- None.

### Modified Capabilities
- `output-limit-recovery`: cut-off classification by finish reason, per-call results on cut-off, escalated cap kept for the turn, continuation condition.
- `item-aware-chat-output`: request history always pairs tool calls with results.

## Impact

- `crates/agent/src/agent/execution/transitions.rs` (cut-off branch of `transition_after_llm`, pre-request guard in `transition_before_llm_call`).
- `crates/agent/src/middleware/output_limit.rs` (`on_step_start` keeps the escalated cap).
- `crates/agent/src/middleware/state.rs` (`LlmResponse::is_length_cutoff`).
- `crates/agent/src/model.rs` (pairing check reused from history repair).
- No storage, event or wire changes.
