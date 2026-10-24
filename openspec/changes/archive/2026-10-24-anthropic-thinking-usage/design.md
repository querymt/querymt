# Design

## Context

- Session reasoning effort "auto" is stored as `None` (`send_agent_config.rs:281`, `acp/shared.rs:1921`). The LLM config then has no `reasoning_effort`, so the provider's `self.reasoning_effort` is `None`.
- `Anthropic::chat_request` (`crates/providers/anthropic/src/lib.rs:1206-1222`) builds `thinking` only from `self.reasoning_effort`.
  - With `None` it sends nothing. On Claude 5.x that means adaptive thinking at the model's default effort. On Opus/Sonnet 4.6–4.8 it means thinking off.
  - The documented thinking table (no field / adaptive / enabled / disabled per model) is the source for the classes below.
- `is_adaptive_reasoning_model` (`lib.rs:881`) matches only `opus-4-6`/`sonnet-4-6`. On 4.7+ and 5.x an explicit effort therefore sends `enabled` + `budget_tokens`, which those models reject with a 400.
- `effort_budget_tokens` (`lib.rs:888`) maps `Max` to 31,999 and is never fitted to `self.max_tokens` (default 32,000 from `model_heuristics.rs:232`).
- Sampling: `temperature` is forced to 1 only when an effort is set (`lib.rs:1253`). `top_p` and `top_k` are always passed through (`lib.rs:1261-1262`). Opus 4.7+ and every 5.x/Fable/Mythos model reject any non-default value, and reject `temperature` together with `top_p`.
- `tool_choice` is passed through unchanged (`lib.rs:1185-1204`).
- Thinking replay (`lib.rs:1078-1097`): signed reasoning is sent only `if !text.is_empty()`. With `display: "omitted"`, the default on Opus 4.7+ and 5.x, every block is empty, so tool-use turns lose their thinking.
- Streaming (`lib.rs:1417-1472`):
  - Thinking text goes out as `StreamChunk::Thinking` and the signature as `StreamChunk::ThinkingSignature` at block stop.
  - The core legacy accumulator (`querymt/src/chat/streaming.rs:661-686`) appends text to the *last* reasoning item and sets the signature on the *last* reasoning item. Two consecutive blocks therefore merge into one item and the first signature is overwritten.
  - `redacted_thinking` blocks are ignored.
- Non-streaming (`lib.rs:617-676`) keeps only the first thinking block's text through `ChatOutput::from_projections`. No signature survives, and empty thinking is dropped.
- `querymt::Usage` (`crates/querymt/src/lib.rs:222`) is shared by every provider and is deserialized directly from the Anthropic `usage` object. Its `reasoning_tokens` has no alias, and Anthropic nests the count under `output_tokens_details.thinking_tokens`, so it is always 0.
- Stop reasons are mapped twice, in streaming (`map_stop_reason`, `lib.rs:786`) and non-streaming (`lib.rs:666`), and both leave gaps.
- `ChatReasoningItem` (`querymt/src/chat/output.rs:798`) already has `encrypted_content` and `signature`. Portable projection strips both (`output.rs:390-399`), so provider-opaque data never leaks to other providers.
- The provider is built both as an Extism plugin and as an rlib. Stream chunks cross the plugin boundary serialized.

## Goals / Non-Goals

**Goals:**
- Every request we build is valid for its model: thinking mode, effort, sampling, `tool_choice`.
- "auto" keeps API defaults where thinking is already on by default, and turns adaptive thinking on for the opt-in adaptive models.
- Thinking content round-trips one-to-one, including empty and redacted blocks.
- Thinking usage and stop reasons are reported correctly to the agent.

**Non-Goals:**
- Bounding thinking spend: choosing `max_tokens`, per-request effort, and recovering from length stops all belong to `output-limit-recovery`.
- `xhigh` effort, `thinking: {type: "disabled"}` / `between_tools`, and an explicit "off" reasoning level.
- Thinking `display` selection (`summarized` / `omitted` / `updates`) and rendering progress updates.
- The prefix-check `block_binding` controls, and per-message effort (beta).
- Summing `usage.iterations[]`, and continuing `pause_turn`.
- Other providers' reasoning mapping.

## Decisions

### 1. "auto" is decided in the provider, not in the agent heuristics
"auto" stays `None` all the way to the provider. The provider then applies the class rule.
- Alternative: have `model_heuristics` insert `reasoning_effort` for Claude. Rejected: it would make "auto" look like an explicit effort in stored configs, the session UI and other callers of the library, and the model-version knowledge belongs with the wire format.
- The heuristics file only gets updated comments and tests that pin "auto stays unset for Anthropic".

### 2. Three model classes from a normalised, parsed ID
- `normalize_model_id`:
  - lower-case the ID;
  - strip any prefix through `anthropic.` or `anthropic/`;
  - strip `-v\d+(:\d+)?`, `@…` and a trailing 8-digit date.
- Then parse `claude-{family}-{major}[-{minor}]`, or the legacy `claude-{major}[-{minor}]-{family}`, into `ClaudeModel { family, major, minor }`. `claude-mythos-preview` parses as family `mythos` with no version.
- `ThinkingClass`:
  - `DefaultOnAdaptive`: major ≥ 5, or family `fable` or `mythos`;
  - `OptInAdaptive`: family `opus` or `sonnet` at 4.6–4.8;
  - `Budget`: everything else, including parse failure.
- The override `adaptive_thinking: Option<bool>` maps `Some(true)` → `OptInAdaptive` and `Some(false)` → `Budget`. It is optional and backward compatible in the config schema.
  - `true` means "send adaptive explicitly". That is correct on both adaptive classes and is the only safe choice for an unknown proxy ID.
  - `false` is documented as rejected by the API on Opus 4.7+ and 5.x.
- Other version predicates read the parsed model, never the override:
  - `rejects_sampling_params`: Opus ≥ 4.7, major ≥ 5, Fable, Mythos.
  - `rejects_forced_tool_choice`: Opus ≥ 5.5, Sonnet ≥ 5.5, Fable ≥ 5.1, Mythos ≥ 5.1.

  They describe API validation, which doesn't change with the thinking mode we choose.
- Alternative: read capabilities from models.dev. Rejected: models.dev has no adaptive/effort flag, and the plugin cannot read the host cache.

### 3. Request shape
```
class               auto                     explicit effort e
DefaultOnAdaptive   (nothing)                thinking{adaptive} + output_config{effort:e}
OptInAdaptive       thinking{adaptive}       thinking{adaptive} + output_config{effort:e}
Budget              (nothing)                thinking{enabled, budget_tokens: fit(b, max_tokens)}

fit(b, m) = clamp(b, 1024, m/2); if m < 2048 -> omit thinking
b = reasoning_budget_tokens or default(e): low 1024, medium 8000, high 16000, max 31999
```
- `AnthropicCompleteRequest` gets `output_config: Option<OutputConfig { effort }>`. `ThinkingConfig` stays as it is.
- Effort strings map one-to-one: low, medium, high, max. `max` is accepted on every adaptive model. Opus 4.5 is the only budget model that accepts effort, and it gets no `output_config`, to keep one rule.
- **Forced tool choice** is applied after the thinking decision:
  - `Budget` + forced → `thinking = None`.
  - `rejects_forced_tool_choice` + forced → `LLMError::InvalidRequest` before any I/O.
- Within one assistant turn every request has the same class, effort and `max_tokens`, so the thinking configuration is stable across a tool loop. Changing it mid-turn makes the API silently disable thinking. `output-limit-recovery` must respect this when it changes options.

### 4. Sampling filter
- One function `sampling_for(model, thinking_sent)` returns the `(temperature, top_p, top_k)` to send:
  - `rejects_sampling_params` → `(None, None, None)`;
  - else if thinking is sent → `(None, top_p.filter(0.95..=1.0), None)`;
  - else the configured values.
- Each dropped value is logged at debug level.
- This replaces the old "force temperature 1" rule. Omitting `temperature` gives the API default, which is 1, so the effect is the same on older models and it is also valid on newer ones.

### 5. Thinking replay: one item per block
- **Streaming:**
  - The provider keeps emitting `Thinking` deltas and a `ThinkingSignature` at block stop, even for empty blocks. An empty block still gets its signature chunk, because the state is created at `content_block_start`.
  - The core legacy accumulator changes in two ways:
    - `append_legacy_reasoning` starts a new reasoning item when the last one already has a signature, or is not the last output item.
    - `append_legacy_signature`, when the last item isn't an unsigned reasoning item, creates an empty reasoning item carrying the signature.

    This makes thinking blocks map one-to-one to reasoning items. It is safe for other providers, which emit at most one signature per reasoning block.
- **Redacted:**
  - A new `StreamChunk::RedactedThinking(String)` is emitted at `content_block_start` of a `redacted_thinking` block.
  - The accumulator stores it as a new reasoning item with `encrypted_content = data` and no text, and marks it with the provider extension `anthropic_block = "redacted_thinking"`, so replay can tell it apart from other providers' encrypted content.
  - Consumers that match `StreamChunk` exhaustively treat it like `ThinkingSignature`: no UI output, counted as content for retry purposes.
- **Non-streaming:** `From<AnthropicCompleteResponse>` builds `ChatOutput` items directly, in content order: reasoning items (text plus signature, or redacted data), message text, and function calls. It no longer goes through `from_projections`.
- **Replay:**
  - signed reasoning → `thinking{thinking: text, signature}`, with no emptiness check;
  - a reasoning item with the redacted marker → `redacted_thinking{data}`;
  - unsigned, unmarked reasoning → skipped, as today.

  Replayed text is the concatenation the API returned. For a single-part item this is exact. Summary and content parts are joined only for items that did not come from Anthropic.
- Alternative: carry the signature inside `Thinking` deltas. Rejected: it changes a chunk every provider and UI consumes.

### 6. Usage mapping stays inside the provider
- Add a provider-local `AnthropicUsage { input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, output_tokens_details: Option<{thinking_tokens}> }` and convert it into `querymt::Usage`.
- Alternative: add a nested-field deserializer to the shared `querymt::Usage`. Rejected: provider-specific nesting would leak into a type every provider shares.
- Streaming keeps emitting `StreamChunk::Usage`. The accumulator's `merge_max` keeps the largest value of each field, so the final `message_delta` count wins. `message_start` carries no thinking count.

### 7. One stop-reason mapping
- The non-streaming conversion calls `Anthropic::map_stop_reason`, so the mapping exists in one place.
- `pause_turn` maps to `Other`, which the agent currently treats as non-terminal "complete unless tool calls". The raw stop reason goes to the debug log.
- On `refusal`, `stop_details.category` is logged at info level when present. It is parsed from both the response and `message_delta`.

## Risks / Trade-offs

- **["auto" turns thinking on for Opus 4.6–4.8 and Sonnet 4.6]** These models think only when asked, so "auto" now raises cost and latency for them, and the response may carry thinking blocks. → Marked BREAKING in the proposal and documented. Users who want no thinking there have no "off" level yet (open question).
- **["auto" on Claude 5.x stays unbounded]** This change does not cap thinking on default-on models: they keep their default effort, `high` on most. → Bounding is `output-limit-recovery`'s job. This change only makes the cost visible through `reasoning_tokens`.
- **[Model-ID parsing misses future naming]** → Parsing fails closed into the budget class, which on a future model may be rejected. The override (`true`) is the escape hatch. Unknown families with major ≥ 5 still classify by version.
- **[`output_config` rejected by some proxies]** → It is sent only for adaptive classes. A proxy ID that can't be parsed is budget by default.
- **[Effort or thinking changes invalidate the prompt cache]** The resolved effort and thinking configuration are rendered into the prompt. → Keep them constant within a session. This change sends the same values on every request of a session. `output-limit-recovery` should prefer per-message effort (beta) where supported, or accept the cache miss.
- **[Replayed empty and redacted blocks increase input tokens on keep-all models]** → This is required for correctness during tool use, and the API filters and bills only the blocks it shows the model.
- **[New `StreamChunk` variant and plugin/host skew]** An older host can't deserialize `RedactedThinking` from a newer plugin. → Plugin and host ship from one workspace version. The variant is additive, and the risk is noted in the release notes.
- **[Accumulator split changes reasoning item counts]** UIs that assumed one reasoning item per reply may show several. → Visible text is unchanged, and the agent already handles multiple reasoning items from structured providers.
- **[Dropping thinking under forced tool choice changes model behaviour]** → This is what the API requires. The request is still valid, and the drop is logged at debug level.
- **[`thinking_tokens` missing from some responses]** → `reasoning_tokens` falls back to 0, as today.

## Open Questions

- Should there be an explicit "off" reasoning level? It needs a per-model table: `disabled` is rejected on Opus 5.5, Fable and Mythos; Sonnet 5.5 uses `between_tools`; on Opus 5 and Haiku 5.5 it is accepted only at `high` effort or below. This is a UI/control-state change and can follow without changing this design.
- Should `xhigh` be added to `ReasoningEffort`? It is supported on Opus 4.7+, every 5.x, Fable and Mythos, but not on Opus/Sonnet 4.6.
