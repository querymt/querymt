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
Usage SHALL report `output_tokens_details.thinking_tokens` as `reasoning_tokens` and SHALL subtract it from the provider's inclusive `output_tokens` to produce non-reasoning `output_tokens`, using saturating subtraction. Streaming usage SHALL end with authoritative final values, including downward corrections to provisional non-reasoning counts. When no thinking count is reported, `reasoning_tokens` SHALL be 0.

#### Scenario: Streaming response with thinking
- **WHEN** the final `message_delta` reports `output_tokens = 32000` and `thinking_tokens = 31950`
- **THEN** the response usage has `output_tokens = 50` and `reasoning_tokens = 31950`
- **AND** their sum remains the provider's inclusive 32000 output tokens

#### Scenario: Non-streaming response with thinking
- **WHEN** a non-streaming response reports `output_tokens = 600` and `thinking_tokens = 500`
- **THEN** the response usage has `output_tokens = 100` and `reasoning_tokens = 500`

#### Scenario: Provisional output exceeds final non-reasoning output
- **WHEN** streaming starts with inclusive output 1 and no thinking details, then finishes with inclusive output 100 and thinking 100
- **THEN** final response usage has `output_tokens = 0` and `reasoning_tokens = 100`
- **AND** the provisional ordinary output count does not survive as an extra token

#### Scenario: No thinking detail is available
- **WHEN** a response reports output tokens without `output_tokens_details.thinking_tokens`
- **THEN** usage retains the reported output count and reports zero reasoning tokens

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

### Requirement: Strict tools are sent with the strict flag
A tool is eligible for strictness when its definition sets `strict: true` or its unprefixed name is in the provider's `strict_tools` list. Subject to schema compatibility, model and authentication support, and request limits, eligible tools SHALL include `"strict": true`. Other tools SHALL be sent without a `strict` field.

#### Scenario: Listed tool on a supported model
- **WHEN** `strict_tools` contains `edit`, the model is `claude-sonnet-5-5`, and schema, authentication and budget checks pass
- **THEN** the request's `edit` tool contains `"strict": true`

#### Scenario: Definition marked strict
- **WHEN** an MCP tool definition sets `strict: true`, is not listed in `strict_tools`, and schema, model, authentication and budget checks pass
- **THEN** the request's tool contains `"strict": true`

#### Scenario: Non-strict tool
- **WHEN** the `glob` tool is neither listed nor marked strict
- **THEN** the request's `glob` tool has no `strict` field

#### Scenario: OAuth tool-name prefix
- **WHEN** the provider sends `edit` as `mcp_edit` over OAuth, `strict_tools` contains `edit`, and other eligibility checks pass
- **THEN** the request's `mcp_edit` tool contains `"strict": true`

### Requirement: Strict tool schemas use the subset Anthropic accepts
For each tool sent with `"strict": true`, the provider SHALL send a checked converted schema with closed fixed-property objects. It SHALL move numeric limits, string-length limits, `maxItems` and `minItems` above 1 into descriptions. Conversion SHALL preserve required and optional property semantics. The agent SHALL validate calls against the original schema.

#### Scenario: Numeric limit moved to the description
- **WHEN** a strict tool has property `occurrence` with `minimum: 0` and description `0-based occurrence.`
- **THEN** the sent schema for `occurrence` has no `minimum` and its description mentions a minimum of 0

#### Scenario: Nested objects closed
- **WHEN** a strict tool has an array of objects that does not set `additionalProperties`
- **THEN** the sent item schema has `additionalProperties: false`

#### Scenario: Local validation keeps the limit
- **WHEN** a strict tool's original schema has `minimum: 0` and the model sends `-1`
- **THEN** the agent rejects the call with a validation error

### Requirement: Unsupported strict schemas fall back without mutation
The provider SHALL check schema compatibility before enabling strictness. Unsupported references, recursion, formats, patterns, composition, extensible dictionaries or other unconvertible constructs SHALL cause that tool to be sent non-strict with its original schema. A warning SHALL identify the tool and unsupported feature without argument contents. This policy SHALL apply to explicit and name-selected strictness.

#### Scenario: External reference on an explicitly strict tool
- **WHEN** a definition sets `strict: true` and its schema contains an external reference unsupported by strict mode
- **THEN** that tool is sent without `strict`, its original schema is unchanged, and a warning identifies the reference limitation

#### Scenario: Dictionary values remain expressible
- **WHEN** a name-selected strict tool accepts arbitrary keys through a dictionary schema
- **THEN** it is sent non-strict with its original schema rather than converting the dictionary to an empty closed object

#### Scenario: Partial conversion cannot leak into fallback
- **WHEN** conversion relaxes a numeric constraint before discovering unsupported recursion elsewhere in the same schema
- **THEN** fallback sends the complete original schema, including that numeric constraint

#### Scenario: Compatible tools remain eligible
- **WHEN** one eligible tool has an unsupported schema and another has a compatible schema within the remaining request limits
- **THEN** only the unsupported tool falls back and the compatible tool remains strict

### Requirement: Strict tool use requires a supporting model
Strict tool use SHALL be sent only for Opus and Sonnet 4.5 or later, Haiku 4.5 or later, and all Fable and Mythos models. For other or unrecognised models, tools SHALL be sent without `strict` and the omission SHALL be logged at debug level.

#### Scenario: Older model
- **WHEN** a tool is marked strict and the model is `claude-sonnet-4-0`
- **THEN** the request's tool has no `strict` field and uses its original unmodified schema

#### Scenario: Unrecognised model
- **WHEN** a tool is marked strict and the model ID cannot be classified
- **THEN** the request's tool has no `strict` field and uses its original unmodified schema

### Requirement: Strict tool limits are enforced before sending
Before sending, the provider SHALL count eligible, successfully converted strict tools (limit 20), their optional properties (limit 24), and their properties using `anyOf` or a type array (limit 16). If any limit is exceeded, all tools in that request SHALL be sent without `strict` using their original unmodified schemas, and a warning naming the exceeded limit SHALL be logged.

#### Scenario: Too many optional properties
- **WHEN** the strict tools in a request declare 25 optional properties in total
- **THEN** no tool contains `strict`, every tool uses its original schema, and a warning names the optional-property limit

#### Scenario: Within limits
- **WHEN** three strict tools declare 6 optional properties in total
- **THEN** all three are sent with `"strict": true`

### Requirement: Strict tools are configured per provider
The Anthropic provider SHALL accept a `strict_tools` setting: a list of tool names. The provider SHALL have no built-in list: an unset or empty list SHALL select no tool by name, without overriding explicit `strict: true`. The agent SHALL default `strict_tools` to `["edit", "multiedit", "replace_symbol"]` for Anthropic only when the option is not configured. All strictness requests SHALL remain subject to schema, model and budget checks.

#### Scenario: Provider default is empty
- **WHEN** `strict_tools` is not configured on the provider and the `edit` definition does not set `strict`
- **THEN** the request's `edit` tool has no `strict` field

#### Scenario: Agent default list
- **WHEN** the agent builds an Anthropic configuration without `strict_tools`
- **THEN** the configuration contains `["edit", "multiedit", "replace_symbol"]`, and a user-supplied value (including an empty list) is left unchanged

#### Scenario: Disabled by configuration
- **WHEN** `strict_tools = []` and the `edit` definition does not set `strict`
- **THEN** the request's `edit` tool has no `strict` field

### Requirement: OAuth strictness matches API-key strictness
Strict tool use over OAuth was verified against the live endpoint, so OAuth requests SHALL follow the same strictness rules as API-key requests. There SHALL be no separate OAuth capability gate.

#### Scenario: Explicit strictness over OAuth
- **WHEN** a definition explicitly requests `strict: true` on an OAuth request and all other eligibility checks pass
- **THEN** the OAuth request's tool contains `"strict": true`

### Requirement: Invalid streamed tool input is reported distinctly
A streamed tool call SHALL NOT run if its completed input is not a JSON object or `max_tokens` interrupts its block, even when the buffer parses. Invalid status SHALL survive stream assembly and execution preparation. Before hooks or permissions, the call SHALL receive one error result containing `{"INVALID_JSON": "<raw input>"}`, with the diagnostic shortened to at most 2,000 Unicode scalar values.

#### Scenario: Truncated tool input
- **WHEN** a stream ends with `max_tokens` while a `tool_use` block's input is `{"file_path": "a.rs", "old_str`
- **THEN** the tool does not run and its error result contains an `INVALID_JSON` key holding that input

#### Scenario: Empty tool input
- **WHEN** a `tool_use` block completes with no input deltas
- **THEN** the input is treated as `{}`

#### Scenario: Interrupted but syntactically valid input
- **WHEN** `max_tokens` interrupts an open tool block whose buffer is `{"file_path":"a.rs"}`
- **THEN** invalid status survives assembly and execution preparation, hooks and the tool do not run, and the call receives one `INVALID_JSON` error

### Requirement: Interrupted blocks complete exactly once before termination
On a `max_tokens` stop, each still-open tool block SHALL emit exactly one completion carrying its raw input and invalid status before stream termination. A later block-stop event SHALL NOT emit a second completion for the same block. Normally completed blocks SHALL NOT be invalidated by a later `max_tokens` stop.

#### Scenario: Multiple open blocks at the token limit
- **WHEN** `max_tokens` stops a response with two open tool blocks
- **THEN** both marked completions precede termination and each call receives exactly one result

#### Scenario: Late stop after interruption
- **WHEN** a block-stop event arrives for a tool block already completed as interrupted
- **THEN** no duplicate completion or result is produced

#### Scenario: Completed block before token limit
- **WHEN** a valid tool block completes normally before a later `max_tokens` stop
- **THEN** that completed call is not marked invalid merely because the response reached its token limit

### Requirement: Replayed tool calls with unparseable arguments are logged
When replaying an assistant tool call whose stored arguments do not parse to a JSON object, the provider SHALL send `input: {}` as today and log a warning with the call ID. This protocol placeholder SHALL NOT overwrite raw stored arguments or convert rejected input into an executable call.

#### Scenario: Unparseable stored arguments
- **WHEN** history contains a function call with arguments `{"edits": [` and the conversation is replayed
- **THEN** the replayed `tool_use` block has `input: {}`, a warning with that call ID is logged, and raw stored arguments and the matching error result remain intact

### Requirement: Cache usage retains write-duration detail
Anthropic usage SHALL retain uncached `input_tokens`, cache reads, aggregate cache-creation tokens, and returned five-minute/one-hour cache-creation counts in streaming and non-streaming output. Write-duration counts SHALL be a breakdown of aggregate writes, not additional input tokens. Missing detail SHALL remain distinguishable from a returned zero count.

#### Scenario: Mixed cache-write durations
- **WHEN** usage reports 12 uncached input tokens, 30 cache reads, and 300 cache writes split into 200 five-minute and 100 one-hour tokens
- **THEN** canonical usage retains all counts and the total input count is 342
- **AND** both write-duration counts survive local output storage and remote provider transport

#### Scenario: Aggregate writes without duration detail
- **WHEN** usage reports 300 cache-creation tokens without a `cache_creation` breakdown
- **THEN** canonical usage retains 300 aggregate writes with unknown duration detail
- **AND** it does not invent 300 five-minute writes based on request configuration

#### Scenario: Streaming preserves start-event cache usage
- **WHEN** a start event reports uncached input, cache reads, and write-duration details, and a later output-usage event omits those input fields
- **THEN** final output retains the start event's cache information while using authoritative final output counts
- **AND** repeated events do not add duplicate cache tokens

#### Scenario: Cache-write detail disagrees with its aggregate
- **WHEN** the write-duration counts do not sum to reported aggregate writes
- **THEN** the available counts remain identifiable as inconsistent for accounting
- **AND** the provider path does not silently claim a complete cache-write estimate
