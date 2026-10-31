# Spec Delta

## ADDED Requirements

### Requirement: Streaming responses use item-aware events
Streaming Anthropic responses SHALL be emitted as item-aware structured events only, with no interleaved flat chunks. Response metadata, including the initial usage from `message_start`, SHALL precede the first item. Non-streaming responses SHALL be unchanged.

#### Scenario: Text-only response
- **WHEN** a stream contains `message_start`, one text block and a `message_delta` with `end_turn`
- **THEN** consumers receive response metadata, one message item started and completed, and a completed response terminal with finish reason stop
- **AND** no flat text or done chunk is emitted

#### Scenario: Duplicate message start
- **WHEN** a stream contains a second `message_start`, before or after the terminal
- **THEN** no second metadata event is emitted and the response outcome is unaffected

### Requirement: Content blocks map to output items by block index
Each content block SHALL become exactly one output item whose output index is the block index. The item SHALL start at `content_block_start` and complete at `content_block_stop` with an authoritative snapshot: `text` as an assistant message, `thinking` as a reasoning item with its signature, `redacted_thinking` as a reasoning item with its opaque data, and `tool_use` as a function call with its raw accumulated input.

#### Scenario: Interleaved thinking, text and parallel tool calls
- **WHEN** a response streams a thinking block at index 0, text at 1, and tool calls at 2 and 3
- **THEN** the completed output has four items in index order, the reasoning item carries its signature, and each call carries its own id, name and arguments

#### Scenario: Consecutive text blocks
- **WHEN** a response streams text blocks at index 0 and 1
- **THEN** the completed output has two message items, one per block

#### Scenario: Tool block with no input deltas
- **WHEN** a `tool_use` block starts and stops with no input deltas
- **THEN** its completed function call has arguments `{}`

### Requirement: Unmodelled content blocks are retained as non-executable items
A content block of any type other than `text`, `thinking`, `redacted_thinking` or `tool_use` SHALL become an opaque item recording its original type, with its input deltas accumulated into that item. Accumulated input that parses as a JSON object SHALL be recorded as the item's `input`; otherwise it SHALL be kept verbatim as `input_json`. Such an item SHALL NOT be dispatched as a local tool call and SHALL NOT fail a completed response.

#### Scenario: Server tool block streams input
- **WHEN** a response contains a `server_tool_use` block at index 1 that streams `input_json_delta` events, followed by a local `tool_use` block at index 2 and a `tool_use` stop
- **THEN** the response completes, only the call at index 2 is dispatched, and index 1 is an opaque item of type `server_tool_use`
- **AND** the opaque item's `input` is the parsed streamed input

### Requirement: Stop reasons determine the response terminal status
A `message_delta` carrying a stop reason SHALL produce one response terminal with the mapped finish reason and final usage. Stops mapped to length SHALL produce an incomplete terminal carrying the output-token-limit cause; all other stop reasons SHALL produce a completed terminal. A stream `error` event SHALL be reported as a classified provider error, not a terminal.

#### Scenario: Token limit with an open tool block
- **WHEN** `max_tokens` stops a response while a `tool_use` block is open
- **THEN** that block completes once, marked invalid, before an incomplete terminal with finish reason length
- **AND** the reply is handled by output-limit recovery without a retry wait

#### Scenario: Tool-use stop
- **WHEN** a response ends with `tool_use` after all tool blocks have stopped
- **THEN** the terminal is completed with finish reason tool calls and the final usage
