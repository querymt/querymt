# Tasks

## 1. Cut-off results (transitions)

- [x] 1.1 Add `LlmResponse::is_length_cutoff()`. Verify: unit test alongside `is_thinking_only_length_cutoff`.
- [x] 1.2 In `transition_after_llm`, synthesize results when `output_status == Incomplete || is_length_cutoff()`: invalid calls get `INVALID_JSON` (+ split guidance on length), valid calls get the skipped text; `debug_assert` the trailing `Length` arm has no calls. Verify: new tests for `Completed`+`Length`+marked call, valid call, mixed calls; extended `length_cutoff_inside_tool_call_skips_the_call_and_stops_without_retry` asserts `INVALID_JSON` for the interrupted call.

## 2. Escalated cap for the turn (output_limit)

- [x] 2.1 Add `on_step_start` that applies `escalated_cap` when no cap override is set (escalation retry and continuation keep setting it directly; see design D3). Verify: tests for sticky cap on a fresh step, explicit override preserved, cleared at turn start, `continuation_repeats_the_escalated_cap` still passes.
- [x] 2.2 Update module docs. Verify: doc text matches spec.

## 3. Pre-request pairing guard

- [x] 3.1 Add a `ChatMessage`-level repair for unanswered tool calls in request messages, called from `transition_before_llm_call` with `error!` + `debug_assert!`. Verify: unit tests for unanswered call repaired and answered history unchanged.

## 4. Regression and validation

- [x] 4.1 End-to-end test: escalate → tool round at 128k → cut-off with flushed invalid call → continuation request has no unanswered call and uses 128k. Verify: test passes.
- [x] 4.2 Run `cargo nextest run -p querymt-agent` and `cargo clippy -p querymt-agent --all-targets`. Verify: green.

## Workflow follow-up

- Archive the change and sync deltas into `openspec/specs/`.
