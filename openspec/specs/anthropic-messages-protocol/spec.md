# Anthropic Messages Protocol Specification

## Purpose

Defines how QueryMT turns generic reasoning and generation settings into Anthropic Messages API requests, how it replays Anthropic thinking content, and how it reports Anthropic usage and stop reasons back in generic terms.

## Requirements

### Requirement: Model IDs are normalised before classification
Model IDs SHALL be normalised before parsing: lower-cased, with any routing or provider prefix up to and including `anthropic.` or `anthropic/` removed (for example `us.anthropic.` or `anthropic/`), and any version suffix such as `-v1:0`, `@20250219` or a trailing `-YYYYMMDD` date removed. Both `claude-{family}-{major}[-{minor}]` and the legacy `claude-{major}[-{minor}]-{family}` forms SHALL be recognised. `claude-mythos-preview` SHALL be recognised as a Mythos model.

#### Scenario: Bedrock-style ID
- **WHEN** the model ID is `us.anthropic.claude-opus-4-7-v1:0`
- **THEN** it is classified the same as `claude-opus-4-7`

#### Scenario: Vertex-style ID
- **WHEN** the model ID is `claude-sonnet-4-6@20260101`
- **THEN** it is classified the same as `claude-sonnet-4-6`

### Requirement: Models are classified by thinking behaviour
Each model SHALL fall into exactly one class:
- **default-on adaptive:** any Claude model with major version 5 or later, and any Fable or Mythos model, including Mythos Preview;
- **opt-in adaptive:** Opus or Sonnet 4.6, 4.7 or 4.8;
- **budget:** every other parseable Claude model, and every model ID that can't be parsed.

An explicit provider configuration flag SHALL override the class: on forces opt-in adaptive, off forces budget.

#### Scenario: Version-based classification
- **WHEN** the model ID is `claude-sonnet-5-5`, `claude-haiku-5-5` or `claude-mythos-preview`
- **THEN** the model is default-on adaptive
- **WHEN** the model ID is `claude-opus-4-8` or `claude-sonnet-4-6-20260101`
- **THEN** the model is opt-in adaptive
- **WHEN** the model ID is `claude-sonnet-4-5` or `claude-3-7-sonnet-20250219`
- **THEN** the model is budget

#### Scenario: Unknown model ID with override
- **WHEN** the model ID is `my-proxy-model` and the configuration sets adaptive thinking on
- **THEN** the model is opt-in adaptive

### Requirement: Automatic reasoning follows the model class
When no explicit reasoning effort is set (session effort "auto"):
- requests for default-on adaptive models SHALL include no `thinking` configuration and no effort, leaving the API defaults in place;
- requests for opt-in adaptive models SHALL include `thinking.type = "adaptive"` with no budget and no effort;
- requests for budget models SHALL include no `thinking` configuration.

#### Scenario: Auto effort on a default-on adaptive model
- **WHEN** a request for `claude-sonnet-5-5` is built with no reasoning effort
- **THEN** the request body contains no `thinking` field and no `output_config` field

#### Scenario: Auto effort on an opt-in adaptive model
- **WHEN** a request for `claude-opus-4-8` is built with no reasoning effort
- **THEN** the request body contains `thinking.type = "adaptive"`, no `budget_tokens` and no `output_config`

#### Scenario: Auto effort on a budget model
- **WHEN** a request for `claude-sonnet-4-5` is built with no reasoning effort
- **THEN** the request body contains no `thinking` field

### Requirement: Explicit effort maps to the model's thinking mode
An explicit reasoning effort (`low`, `medium`, `high`, `max`) SHALL produce the following:
- **adaptive models (either class):** `thinking.type = "adaptive"` plus `output_config.effort` at the same level.
- **budget models:** `thinking.type = "enabled"` with a budget taken from the configured budget, or from the effort's default budget (low 1,024, medium 8,000, high 16,000, max 31,999), then fitted to `max_tokens`. Budget models SHALL NOT receive `output_config`.

#### Scenario: High effort on an adaptive model
- **WHEN** effort `high` is set for `claude-opus-5`
- **THEN** the request contains `thinking.type = "adaptive"` and `output_config.effort = "high"`

#### Scenario: Medium effort on a budget model
- **WHEN** effort `medium` is set for `claude-sonnet-4-5` with `max_tokens` 32,000 and no explicit budget
- **THEN** the request contains `thinking.type = "enabled"` with an 8,000-token budget and no `output_config`

### Requirement: Thinking budgets leave room for the answer
A thinking budget SHALL be at most half of the request's `max_tokens` and at least 1,024 tokens. When `max_tokens` is below 2,048, the request SHALL omit thinking rather than send a budget the API rejects.

#### Scenario: Max effort under a 32k cap
- **WHEN** effort `max` is set for a budget model and `max_tokens` is 32,000
- **THEN** the request's `budget_tokens` is 16,000

#### Scenario: Configured budget above half the cap
- **WHEN** `reasoning_budget_tokens` is 20,000 for a budget model and `max_tokens` is 32,000
- **THEN** the request's `budget_tokens` is 16,000

#### Scenario: Cap too small for thinking
- **WHEN** effort `low` is set for a budget model and `max_tokens` is 1,500
- **THEN** the request contains no `thinking` field

### Requirement: Newer models receive no sampling parameters
On Opus 4.7 or later, and on every Claude 5 or later, Fable or Mythos model (identified by parsed version, not by the override flag), requests SHALL NOT include `temperature`, `top_p` or `top_k`, whether or not thinking is sent. Each omitted value SHALL be logged at debug level.

#### Scenario: Configured temperature on a newer model
- **WHEN** temperature 0.2 and top_p 0.9 are configured for `claude-sonnet-5-5`
- **THEN** the request contains no `temperature` and no `top_p`

### Requirement: Sampling on other models is compatible with thinking
On all other models, a request with a thinking configuration SHALL NOT include `temperature` or `top_k`, and SHALL include `top_p` only when the configured value is between 0.95 and 1 inclusive. Requests without thinking SHALL keep the configured sampling values. Each omitted value SHALL be logged at debug level.

#### Scenario: Out-of-range top_p with thinking
- **WHEN** effort `high` and top_p 0.9 are set for `claude-sonnet-4-5`
- **THEN** the request contains enabled thinking and no `top_p`

#### Scenario: Auto adaptive thinking on an opt-in adaptive model
- **WHEN** temperature 0.2 is configured for `claude-sonnet-4-6` with no reasoning effort
- **THEN** the request contains adaptive thinking and no `temperature`

#### Scenario: Budget model without thinking
- **WHEN** temperature 0.2 is configured for `claude-sonnet-4-5` with no reasoning effort
- **THEN** the request temperature is 0.2

### Requirement: Forced tool use is compatible with thinking
A forced tool choice is `tool_choice` `any` or a named tool.
- **Budget models:** a forced tool choice SHALL remove the thinking configuration from that request.
- **Opus 5.5 or later, Sonnet 5.5 or later, Fable 5.1 or later, Mythos 5.1 or later:** a forced tool choice SHALL be rejected before sending, with an invalid-request error naming the model.
- **Other adaptive models:** forced tool use SHALL be sent unchanged.

#### Scenario: Forced tool on a budget model
- **WHEN** effort `high` and `tool_choice = any` are set for `claude-sonnet-4-5`
- **THEN** the request contains `tool_choice.type = "any"` and no `thinking` field

#### Scenario: Forced tool on a model that rejects it
- **WHEN** `tool_choice` names a tool for `claude-sonnet-5-5`
- **THEN** request building fails with an invalid-request error and nothing is sent

### Requirement: Thinking blocks are preserved one-to-one for replay
Each `thinking` block in a response SHALL become its own reasoning item, carrying its own signature, in the order and position the API returned it, in both streaming and non-streaming responses. When assistant history is replayed, every reasoning item that has a signature SHALL be sent back as a `thinking` block with that signature and its original text, unchanged, even when the text is empty. Reasoning items without a signature SHALL NOT be sent.

#### Scenario: Omitted thinking in a tool-use turn
- **WHEN** a response contains a thinking block with empty text and signature `S`, followed by a `tool_use` block, and the next request replays that turn with its tool result
- **THEN** the replayed assistant message contains a `thinking` block with `thinking = ""` and `signature = "S"` before the `tool_use` block

#### Scenario: Multiple thinking blocks
- **WHEN** a response contains two thinking blocks with signatures `S1` and `S2` around a progress update
- **THEN** the reply yields two reasoning items, and replay sends two `thinking` blocks with `S1` and `S2` in the original order

#### Scenario: Non-streaming response
- **WHEN** a non-streaming response contains a thinking block with signature `S`
- **THEN** the resulting reasoning item carries signature `S`

### Requirement: Redacted thinking blocks round-trip unchanged
Each `redacted_thinking` block in a response, streaming or non-streaming, SHALL be kept as a reasoning item holding its opaque `data`, with no visible text, in its original position. On replay it SHALL be sent back as a `redacted_thinking` block with the same `data`. Redacted content SHALL NOT appear in visible reasoning text or logs.

#### Scenario: Redacted block in a tool-use turn
- **WHEN** a response contains a `redacted_thinking` block with data `D` before a `tool_use` block, and the turn is replayed
- **THEN** the replayed assistant message contains `{type: "redacted_thinking", data: "D"}` before the `tool_use` block

### Requirement: Thinking tokens are reported as reasoning tokens
Usage SHALL report the provider's thinking-token count (`output_tokens_details.thinking_tokens`) as `reasoning_tokens`, included in `output_tokens` and not double-counted. Streaming usage SHALL end with the final values. When no count is reported, `reasoning_tokens` SHALL be 0.

#### Scenario: Streaming response with thinking
- **WHEN** the final `message_delta` reports `output_tokens = 32000` and `thinking_tokens = 31950`
- **THEN** the response usage has `output_tokens = 32000` and `reasoning_tokens = 31950`

#### Scenario: Non-streaming response with thinking
- **WHEN** a non-streaming response reports `thinking_tokens = 500`
- **THEN** the response usage has `reasoning_tokens = 500`

### Requirement: Stop reasons map to generic finish reasons
Anthropic stop reasons SHALL map to finish reasons as follows:
- `end_turn` and `stop_sequence` → stop;
- `tool_use` → tool calls;
- `max_tokens` and `model_context_window_exceeded` → length;
- `refusal` → content filter;
- `pause_turn` → other;
- any unknown value → unknown.

Streaming and non-streaming paths SHALL use the same mapping. For `refusal`, the reported `stop_details.category` SHALL be logged when present.

#### Scenario: Context window exhausted
- **WHEN** a response ends with `model_context_window_exceeded`
- **THEN** the finish reason is length

#### Scenario: Refusal
- **WHEN** a response ends with `refusal`
- **THEN** the finish reason is content filter
