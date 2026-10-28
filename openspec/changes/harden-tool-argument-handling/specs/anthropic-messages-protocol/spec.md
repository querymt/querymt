# Spec Delta

## ADDED Requirements

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
- **WHEN** OAuth strict support is verified, the provider sends `edit` as `mcp_edit`, `strict_tools` contains `edit`, and other eligibility checks pass
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
The Anthropic provider SHALL accept a `strict_tools` setting: a list of tool names, defaulting to `["edit", "multiedit", "replace_symbol"]`. An empty list SHALL disable name-based strictness without overriding explicit `strict: true`. All strictness requests SHALL remain subject to schema, model, authentication and budget checks.

#### Scenario: Default list
- **WHEN** `strict_tools` is not configured, the request includes `multiedit`, and all strict eligibility checks pass
- **THEN** the request's `multiedit` tool contains `"strict": true`

#### Scenario: Disabled by configuration
- **WHEN** `strict_tools = []` and the `edit` definition does not set `strict`
- **THEN** the request's `edit` tool has no `strict` field

### Requirement: OAuth strictness requires verified support
Strict tools over OAuth SHALL be enabled only after support is verified for that authentication path. If verification fails, an authentication capability gate SHALL omit strictness for all OAuth tools, including explicit `strict: true`, while preserving original schemas. API-key strictness SHALL remain independently available.

#### Scenario: OAuth strict verification fails
- **WHEN** OAuth strict support is disabled and a definition explicitly requests `strict: true`
- **THEN** the OAuth request omits `strict` and sends the original schema regardless of `strict_tools`

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
