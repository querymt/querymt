# Tasks

## 1. Model classification

- [x] 1.1 Add `normalize_model_id`, which strips Bedrock and router prefixes up to `anthropic.` or `anthropic/` and drops `-vN:M`, `@…` and date suffixes. Then add a Claude model-ID parser for the `claude-{family}-{major}[-{minor}]` form, the legacy `claude-{major}[-{minor}]-{family}` form, and `claude-mythos-preview`.
  - Verify with unit tests for `us.anthropic.claude-opus-4-7-v1:0`, `claude-sonnet-4-6@20260101`, `anthropic/claude-sonnet-5-5`, `claude-3-7-sonnet-20250219`, `claude-mythos-preview` and an unparseable ID.
- [x] 1.2 Add `ThinkingClass` (default-on adaptive, opt-in adaptive, budget) and the version predicates `rejects_sampling_params` and `rejects_forced_tool_choice` from the spec, and replace `is_adaptive_reasoning_model`.
  - Verify with a table test covering `claude-opus-4-5`, `claude-sonnet-4-6`, `claude-opus-4-8`, `claude-opus-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`, `claude-fable-5-1` and `claude-mythos-preview`.
- [x] 1.3 Add optional `adaptive_thinking: Option<bool>` to the Anthropic provider config, in both the schema and the factory: `true` forces opt-in adaptive, `false` forces budget.
  - Verify with a config-schema test and override unit tests in both directions.
  - Verify that the sampling and tool-choice predicates ignore the override.

## 2. Request shaping

- [x] 2.1 Build `thinking` from the class table: no thinking for auto on default-on adaptive and budget models; adaptive for auto on opt-in adaptive models; adaptive for an explicit effort on either adaptive class; enabled with a budget for an explicit effort on budget models.
  - Verify with request-body tests for auto/explicit × all three classes.
- [x] 2.2 Add `output_config.effort` (low, medium, high, max) for explicit efforts on adaptive classes only.
  - Verify with a request-body test, and check that auto requests and budget-model requests omit it.
- [x] 2.3 Fit budgets to `clamp(budget, 1024, max_tokens/2)`, and omit thinking when `max_tokens < 2048`.
  - Verify with tests for `Max` at 32k (16,000), `reasoning_budget_tokens = 20,000` at 32k (16,000), and `max_tokens = 1500` (no thinking).

## 3. Sampling and tool-choice compatibility

- [x] 3.1 Replace the "temperature = 1 when effort is set" rule with the sampling filter:
  - drop all of temperature, top_p and top_k on models that reject sampling parameters;
  - when thinking is sent on other models, drop temperature and top_k, and keep top_p only within 0.95–1;
  - otherwise keep the configured values;
  - log each dropped value at debug level.
  - Verify with tests for `claude-sonnet-5-5` with temperature 0.2 and top_p 0.9 (both omitted), `claude-sonnet-4-6` in auto with temperature 0.2 (omitted), `claude-sonnet-4-5` in auto with temperature 0.2 (kept), and a budget model with an explicit effort and top_p 0.9 (omitted) or 0.97 (kept).
- [x] 3.2 Handle forced `tool_choice` (`any` or a named tool): drop thinking on budget models, and return `LLMError::InvalidRequest` before sending on models that reject forced tool choice.
  - Verify with tests for `claude-sonnet-4-5` with high effort and `any` (no thinking), `claude-sonnet-5-5` with a named tool (error), and `claude-opus-4-8` with `any` (sent unchanged).

## 4. Thinking replay

- [x] 4.1 In the core legacy stream accumulator (`querymt/src/chat/streaming.rs`):
  - start a new reasoning item when the last reasoning item is already signed or is not the last output item;
  - make a signature with no open unsigned item create an empty signed reasoning item.
  - Verify with accumulator tests for two consecutive signed blocks (two items), an empty signed block (one item with empty text and the signature), and an unchanged single-block case.
- [x] 4.2 Add `StreamChunk::RedactedThinking(String)`. Accumulate it as a reasoning item with `encrypted_content` and the `anthropic_block = "redacted_thinking"` extension.
  - Update the exhaustive consumers: `agent/execution/transitions.rs` (no UI output), `agent/execution/llm_retry.rs` (counts as content) and `py/querymt-py`.
  - Verify that the Debug output redacts the data, that the chunk survives the Extism serialization round trip, and that `cargo check` passes for the workspace.
- [x] 4.3 In the Anthropic provider, emit `RedactedThinking` for `redacted_thinking` blocks at `content_block_start`, and make sure empty thinking blocks still emit their signature.
  - Verify with streaming tests for an omitted-display block (empty `thinking_delta` followed by `signature_delta`) and a redacted block.
- [x] 4.4 Build non-streaming `ChatOutput` items in content order, with per-block reasoning items carrying signatures or redacted data, instead of `from_projections`.
  - Verify with a non-streaming test for two thinking blocks, a redacted block, text and a `tool_use`.
- [x] 4.5 Replay: send every signed reasoning item as a `thinking` block even when its text is empty, and send marked redacted items as `redacted_thinking{data}`.
  - Verify with request-body tests for the omitted tool-use round trip, two blocks in order, and the redacted round trip.
  - Verify that an unsigned item is still skipped, and that portable projection still strips signatures and encrypted data.

## 5. Response mapping

- [x] 5.1 Add a provider-local usage type with `output_tokens_details.thinking_tokens` and convert it to `querymt::Usage` (`reasoning_tokens`) for `message_start`, `message_delta` and non-streaming responses.
  - Update `test_usage_deserialization_with_cache`, and add streaming and non-streaming thinking-token tests.
- [x] 5.2 Route the non-streaming finish reason through `map_stop_reason`, and complete the mapping: `refusal` → content filter, `model_context_window_exceeded` → length, `pause_turn` → other, with the raw value logged at debug level. Log `stop_details.category` on refusal.
  - Verify with table tests for every stop reason on both paths.

## 6. Heuristics and docs

- [x] 6.1 Update the Claude comments in `crates/agent/src/model_heuristics.rs`, and add a test that Anthropic defaults never insert `reasoning_effort` (auto is resolved by the provider).
  - Verify with `cargo test -p querymt-agent model_heuristics`.
- [x] 6.2 Document in the Anthropic provider docs:
  - the model classes and auto behaviour, including the BREAKING change for Opus/Sonnet 4.6–4.8;
  - the effort mapping and budget fitting;
  - which sampling parameters are dropped;
  - the forced tool-choice rules;
  - the `adaptive_thinking` override, including that `false` is rejected by the API on Opus 4.7+ and 5.x;
  - that "auto" on Claude 5.x does not bound thinking.
  - Verify that the docs build or render.

## 7. Integration

- [x] 7.1 Run `cargo test -p qmt-anthropic` (both the native build and the `--features extism` build), `cargo test -p querymt` for the chat stream tests, `cargo clippy`, and `cargo fmt --check` for the touched crates.
  - Verify all green.
- [x] 7.2 Run manual live checks:
  - `claude-sonnet-5-5` in "auto" with a reasoning-heavy tool-use prompt: the logged request has no `thinking` and no sampling parameters, the second request replays the empty signed thinking block without a 400, and `llm_request_end` reports non-zero `reasoning_tokens`.
  - `claude-opus-4-8` in "auto": the request carries adaptive thinking.

  Record the results in the change notes.

## Live check results (7.2)

Run against the real API with a temporary harness (since removed), a tool-use prompt, `max_tokens` 8000, and configured `temperature` 0.2, `top_p` 0.9 and `top_k` 40. Both non-streaming and streaming were exercised, including the second request that replays the first turn with its tool result.

- `claude-sonnet-5-5`, "auto":
  - Request 1 had no `thinking`, no `output_config`, and no `temperature`, `top_p` or `top_k`.
  - The response carried an empty-text signed thinking block, text and a `tool_use`.
  - Request 2 replayed `thinking` (empty text, with signature), then `tool_use`, then text. The API returned HTTP 200 on both paths.
  - Usage reported `reasoning_tokens` = 27 of 202 output tokens (non-streaming) and 27 of 204 (streaming).
- `claude-opus-4-8`, "auto":
  - Request 1 carried `thinking: {type: "adaptive"}` with no budget and no `output_config`, and no sampling parameters.
  - The empty signed block was replayed in request 2 with HTTP 200 on both paths.
  - Usage reported `reasoning_tokens` = 25 (non-streaming) and 26 (streaming).

Not exercised live: `redacted_thinking` blocks (covered by unit tests only, as the API did not return one), and an explicit effort on a live model.
