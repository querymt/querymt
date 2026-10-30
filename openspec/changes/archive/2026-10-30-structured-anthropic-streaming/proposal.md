# Proposal

## Why

Session `01a18903-0e37-75be-b9b3-ea5472410b2c` failed every turn with `completed legacy response has unfinished tool call indexes: [N]` and burned three retries per turn. The stream accumulator rebuilds tool-call open/closed state from flat chunks, and it opens a pending call on an argument delta alone. The Anthropic parser forwards `input_json_delta` for any content block, including blocks it never registered as tool calls. The parser knows such a block is not a tool call and the accumulator does not, so the turn fails.

The underlying cause is that Anthropic streams through the flat "legacy" chunk format, even though its content blocks map one-to-one onto the item-aware format already used by OpenAI, Codex and xAI. That format opens items only on explicit starts and closes them on explicit completions.

## What Changes

- **Flat-chunk accumulation (option 1):** a flat-chunk tool call is pending only after an explicit tool-call start. An argument delta for an index that was never started no longer opens a pending call; it is reported as a diagnostic and does not fail the response. The terminal check for calls that started but never completed is kept.
- **Anthropic streaming migrates to item-aware events (option 3):** the Anthropic provider emits structured events instead of flat chunks.
  - `message_start` becomes response metadata.
  - Each content block becomes one output item, keyed by its block index, with explicit start and completion.
  - `message_delta` with a stop reason becomes the semantic response terminal.
- Content blocks the provider does not model (for example `server_tool_use`) become non-executable opaque items instead of being dropped. Their deltas can no longer be mistaken for local tool calls.
- A `max_tokens` stop becomes an incomplete response terminal carrying the length finish reason. Open tool blocks are still completed once, marked invalid, before that terminal.
- Non-streaming Anthropic responses are unchanged.

No wire-format change: `StreamChunk::Structured` already crosses the Extism plugin boundary, the remote mesh relay and the Python bindings.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `item-aware-chat-streaming`: adds the rule that flat-chunk tool calls become pending only on an explicit start, and that orphan argument deltas do not fail a response.
- `anthropic-messages-protocol`: adds requirements for item-aware streaming: block-to-item mapping, unknown-block handling, and terminal mapping, including length stops.

## Impact

- `crates/querymt/src/chat/streaming.rs`: flat-chunk pending-call tracking.
- `crates/providers/anthropic/src/lib.rs`: `AnthropicStreamParser` and its stream tests.
- Agent streaming path (`crates/agent/src/agent/execution/transitions.rs`): no intended change. It already handles structured providers, but needs verification for Anthropic (UI text and reasoning projection, length recovery, partial-attempt usage).
- Remote mesh: a host running this change sends structured events. A client older than the item-aware streaming work (2026-09-22) cannot accumulate them. Newer clients can.
- Out of scope: retry classification of `IncompleteToolCalls`, and migrating other flat-chunk providers.
