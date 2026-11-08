# Proposal: local-model-output-limits

## Why
Long unattended runs on local llama.cpp models end on `ModelTokenLimit` when the model loops inside its reasoning. Output-limit recovery cannot help: the llama.cpp provider ignored per-request `max_tokens` and `reasoning_effort`, nothing bounded reasoning, and once recovery gave up the stop was published without Stop hooks being consulted.

## What Changes
- The llama.cpp provider honours per-request `max_tokens` and `reasoning_effort` overrides and reports that it does, so escalation and lower-effort continuation reach local models.
- New llama.cpp setting `thinking_budget`: once a reply has spent that many tokens reasoning, the provider closes the reasoning block so the model has to answer.
- When a token-limit stop survives middleware recovery, Stop hooks run with `stop_reason = "ModelTokenLimit"` and may continue the turn, within `execution.max_stop_hook_continuations`.

## Capabilities
### New Capabilities
- None.

### Modified Capabilities
- `output-limit-recovery`: unrecovered token-limit stops reach Stop hooks; local providers can bound reasoning per reply.

## Non-goals
- Changing what the recovery middleware does for a thinking-only cut-off on providers without per-request effort (deferred).
- A thinking budget for speculative (MTP) decoding.

## Impact
- `crates/providers/llama-cpp` (config, provider, generation loops), `crates/agent/src/agent/execution/mod.rs`, docs for hooks and the llama.cpp provider.
