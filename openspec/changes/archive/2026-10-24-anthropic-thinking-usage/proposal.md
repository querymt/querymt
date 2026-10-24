# Proposal

## Why

Session `01a16d18-6bf3-7219-849e-967cc8b81124` ended after `claude-sonnet-5-5` spent its whole 32,000-token output allowance on hidden thinking and returned no text or tool call. Claude 5.x models think adaptively by default at their default effort (`high` on most models), so nothing in the request bounded that thinking. The Anthropic provider also made it invisible, and is wrong for newer models in several ways:

- The provider recognises adaptive thinking with a fixed list of names (`opus-4-6`, `sonnet-4-6`), so every later model (4.7, 4.8, 5.x) is treated as a budget-mode model. An explicit effort then sends `thinking: {type: "enabled", budget_tokens}`, which Opus 4.7+ and every 5.x model reject with a 400.
- An explicit effort on an adaptive model is dropped instead of being sent as `output_config.effort`, so the one documented control for thinking depth is never used.
- The thinking budgets are not fitted to `max_tokens`. `Max` asks for 31,999 thinking tokens under a 32,000 cap.
- Sampling parameters are sent unconditionally. Opus 4.7+ and every 5.x model reject any non-default `temperature`, `top_p` or `top_k`, with or without thinking.
- Signed thinking blocks with empty text are dropped on replay. Opus 4.7+ and 5.x default to `display: "omitted"`, so every thinking block is empty, and tool-use turns lose the blocks the API requires to be passed back unchanged. `redacted_thinking` blocks are not handled at all, and the non-streaming path drops signatures.
- `reasoning_tokens` is always 0 for Anthropic, so nothing downstream can see that thinking used up the output.

This change makes thinking requests valid and their cost visible. It does not bound thinking spend. The documented controls for that are `max_tokens` and `effort`, and choosing them per request is the job of `output-limit-recovery`.

## What Changes

- **Model classes from the parsed model ID.** Claude model IDs, including Bedrock, Vertex and router forms, are normalised and parsed into family and version. Each model falls into one class:
  - *default-on adaptive*: every Claude 5 or later model, Fable and Mythos (including Mythos Preview);
  - *opt-in adaptive*: Opus and Sonnet 4.6 to 4.8;
  - *budget*: everything older, plus IDs that can't be parsed.

  A provider config override `adaptive_thinking` can force opt-in adaptive (`true`) or budget (`false`).
- **"auto" effort (no effort set):**
  - default-on adaptive: sends no `thinking` and no effort, so the API defaults apply.
  - opt-in adaptive: sends `thinking: {type: "adaptive"}`. **BREAKING (behaviour):** "auto" now turns thinking on for Opus 4.6–4.8 and Sonnet 4.6, which raises cost and latency there.
  - budget: sends nothing, as today.
- **Explicit effort** (low, medium, high, max):
  - adaptive classes: `thinking: {type: "adaptive"}` plus `output_config.effort`.
  - budget models: enabled thinking with a budget capped at half of `max_tokens` (minimum 1,024). Thinking is omitted when the cap can't fit the minimum.
  - No `xhigh` level is added.
- **Sampling compatibility:**
  - On Opus 4.7+ and every 5.x, Fable and Mythos model, `temperature`, `top_p` and `top_k` are never sent.
  - On other models, whenever thinking is sent, `temperature` and `top_k` are omitted and `top_p` is sent only within 0.95–1.
- **Forced tool use:**
  - On budget models, a forced `tool_choice` (`any` or a named tool) drops thinking from that request.
  - On Opus 5.5+, Sonnet 5.5+, Fable 5.1+ and Mythos 5.1+, a forced `tool_choice` is rejected locally with an invalid-request error, because the API rejects it on every request.
- **Thinking replay:**
  - Each Anthropic thinking block becomes its own reasoning item with its own signature, in order, in both streaming and non-streaming responses.
  - Signed blocks are replayed unchanged even when their text is empty.
  - `redacted_thinking` blocks are kept and replayed with their opaque `data`.
- **Usage:** thinking tokens (`output_tokens_details.thinking_tokens`) are mapped to `reasoning_tokens`, in streaming and non-streaming responses.
- **Stop reasons** are mapped completely, through one shared mapping:
  - `refusal` → content filter, with `stop_details.category` logged;
  - `model_context_window_exceeded` → length;
  - `pause_turn` → non-terminal "other", with its raw value kept for diagnostics.

## Capabilities

### New Capabilities
- `anthropic-messages-protocol`: how QueryMT turns generic reasoning and generation settings into Anthropic Messages API requests (model classes, thinking mode, effort, budget fitting, sampling and tool-choice compatibility, thinking replay), and how it reports Anthropic usage and stop reasons back in generic terms.

### Modified Capabilities
- None.

## Impact

- **Code (provider):** `crates/providers/anthropic/src/lib.rs`:
  - model-ID parsing and classes, replacing `is_adaptive_reasoning_model`;
  - `effort_budget_tokens` fitting;
  - request building in `chat_request`, with a new `output_config`;
  - sampling and `tool_choice` filtering;
  - replay of thinking and redacted blocks;
  - streaming and non-streaming block handling, including `redacted_thinking`;
  - `map_stop_reason` shared by both paths;
  - a provider-local usage type;
  - the new `adaptive_thinking` config field.
- **Code (core):** `crates/querymt/src/chat`:
  - `StreamChunk` gains a `RedactedThinking` variant;
  - the legacy stream accumulator starts a new reasoning item after a signed one, and stores redacted data as `encrypted_content`.

  Chunk consumers that match exhaustively (`agent/execution/transitions.rs`, `agent/execution/llm_retry.rs`, `py/querymt-py`) are updated.
- **Code (agent):** `crates/agent/src/model_heuristics.rs` changes only comments and tests; "auto" stays unset there.
- **Wire:**
  - Anthropic request bodies change for opt-in adaptive models in "auto", for explicit efforts, and for configured sampling values on newer models.
  - Replayed history now includes empty-text signed blocks and redacted blocks.
  - Nothing changes in ACP or storage schemas.
- **Builds:** the provider ships as an Extism plugin and as an rlib, so both builds need the change. A plugin built with the new chunk variant needs a host that knows it.
- **Related:** `output-limit-recovery` builds on this. It sets `max_tokens` and lowers effort per request, and relies on accurate `reasoning_tokens`. This change ships on its own, but bounding thinking spend needs both.
