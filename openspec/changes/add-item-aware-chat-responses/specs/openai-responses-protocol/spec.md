## Purpose

Support the OpenAI Responses protocol through existing chat operations with explicit selection and complete stateless function-turn replay.

## ADDED Requirements

### Requirement: Protocol selection is explicit and backward compatible
OpenAI configuration SHALL support explicit Chat Completions and Responses modes. Omitted selection SHALL retain Chat Completions behavior. Responses mode SHALL use the Responses endpoint for both streaming and non-streaming operations and SHALL NOT silently fall back to another protocol after failure.

#### Scenario: Existing configuration
- **WHEN** a caller omits protocol selection
- **THEN** requests retain the existing Chat Completions endpoint and body semantics.

#### Scenario: Explicit Responses request fails
- **WHEN** a Responses request fails at an OpenAI-compatible endpoint
- **THEN** the original classified error is surfaced without a second Chat Completions request.

### Requirement: Responses uses local stateless continuation
Responses mode SHALL send store=false, request encrypted reasoning content where the endpoint supports it, and replay compatible local items in order. Stateful previous-response and conversation references SHALL be outside this mode. Conflicting extra request fields SHALL be rejected instead of overriding storage or continuation policy.

#### Scenario: Tool-loop continuation
- **WHEN** a caller submits history with an earlier reasoning item, function call, and corresponding result
- **THEN** the request contains their valid input representations in order with store=false and no remote conversation reference.

#### Scenario: Conflicting retention override
- **WHEN** extra request configuration attempts to enable storage or supply a previous response ID
- **THEN** request construction reports a configuration conflict before sending the request.

### Requirement: Request controls use Responses wire semantics
The system SHALL map system instructions, output token limits, structured output formats, and reasoning effort to their Responses equivalents. Unsupported parameters SHALL be rejected with an explicit error instead of silently ignored or copied from a different backend's contract.

#### Scenario: Structured-output request
- **WHEN** a request supplies instructions, an output token limit, JSON schema, and reasoning effort
- **THEN** those values are represented by instructions, max_output_tokens, text.format, and reasoning.effort respectively, subject to model support.

### Requirement: Function definitions and choices preserve intent
Function tools SHALL use the Responses flattened function-definition shape and protocol-correct named tool choice. Responses tools without an explicit strictness setting SHALL serialize strict=false; explicit strict=true SHALL require a compatible schema rather than silently altering optional-property semantics. Chat Completions SHALL preserve its existing omission behavior when strictness is unspecified.

#### Scenario: Named non-strict function
- **WHEN** a caller selects a named function and does not specify strictness
- **THEN** Responses receives the flattened definition with strict=false and the Responses named-choice shape.

#### Scenario: Invalid strict schema
- **WHEN** a caller requests strict=true with a schema incompatible with strict validation
- **THEN** request construction returns a schema error instead of making formerly optional fields required.

### Requirement: Tool results retain correlation and media order
Function outputs SHALL correlate by call_id, not output item ID. Text, supported image, and supported file output parts SHALL preserve their order. Unsupported media SHALL cause an explicit unsupported-content error rather than placeholders or silent omission.

#### Scenario: Mixed rich output
- **WHEN** a tool result contains text, an image, and a supported file for a call with different item and call IDs
- **THEN** the function output references the call ID and retains the three parts in their original order.

#### Scenario: Unsupported output media
- **WHEN** a tool result contains media unsupported by the configured endpoint
- **THEN** construction fails explicitly without substituting a textual description.

### Requirement: Media conversion distinguishes portable parts from provider items
Responses media conversion SHALL consume validated media types and preserve MIME parameter semantics, source form, filenames, and supplied detail metadata in canonical history. Wire serialization SHALL use only fields valid for the selected endpoint and content position. Provider file references SHALL NOT be forwarded to incompatible origins as if they were portable URLs. Unsupported built-in media item kinds SHALL remain opaque until an explicit codec supports their semantics, irrespective of whether they contain recognizable MIME or encoded bytes.

#### Scenario: Typed media survives a tool-result roundtrip
- **WHEN** a rich tool result containing typed inline image and file parts is persisted, reloaded, and serialized for a supported Responses endpoint
- **THEN** its MIME metadata, sources, filenames, supplied detail, and part order survive in canonical history
- **AND** the wire request uses the corresponding protocol-valid image/file fields and call ID.

#### Scenario: Unsupported built-in image output
- **WHEN** a response contains an image_generation_call for which no typed codec is implemented
- **THEN** the complete item is retained as opaque data rather than fabricated message content
- **AND** it is not dispatched to the local function executor; any required unsupported replay or client action fails explicitly.

### Requirement: Responses status and usage retain meaning
Responses parsing SHALL preserve structured items, refusal data, annotations, terminal cause, and provider failure details. Token counts SHALL normalize to QueryMT's non-overlapping cached-input, ordinary-input, reasoning-output, and ordinary-output categories. Completed responses with supported local calls SHALL indicate pending tool execution; completed responses without such calls SHALL indicate stop. Unsupported provider actions requiring local participation SHALL fail explicitly and never enter the local function executor.

#### Scenario: Completed function response
- **WHEN** a successful response contains two local function calls plus reasoning and text
- **THEN** both calls remain available for execution, ordered output remains intact, and cached/reasoning tokens are counted once.

#### Scenario: Incomplete or failed response
- **WHEN** a response is incomplete or failed
- **THEN** the system retains its partial output and terminal cause or classified error
- **AND** it does not report successful stop or dispatch incomplete calls.

### Requirement: Compatible providers retain their own policies
Shared Responses parsing and conversion SHALL preserve each provider's authentication, headers, supported options, instructions, and error policies. OpenAI SHALL support ordinary non-streaming JSON responses independently of Codex's streaming-only backend requirements.

#### Scenario: OpenAI non-streaming alongside Codex
- **WHEN** OpenAI and Codex use shared protocol conversion
- **THEN** OpenAI non-streaming chat still parses a JSON response
- **AND** Codex retains its required streaming request and provider-specific authentication behavior.
