# Design

## Context

See proposal.md for motivation. Relevant current state:

- `ChatStreamAccumulator` (`crates/querymt/src/chat/streaming.rs`) has two modes. The mode is chosen by the first chunk: a `ResponseMetadata` event selects structured mode, and anything else selects flat ("legacy") mode.
  - Flat mode tracks `pending_legacy_calls`. Both `ToolUseStart` and `ToolUseInputDelta` insert an index; only `ToolUseComplete` removes it.
  - Structured mode tracks `structured_items` / `completed_items` and opens items only on `ItemStarted`.
- `AnthropicStreamParser` (`crates/providers/anthropic/src/lib.rs:1863-2028`) already keeps per-block state (`tool_state_buffer`, `thinking_state_buffer`). However:
  - It ignores unknown block types at `content_block_start`.
  - It emits `ToolUseInputDelta` for every `partial_json`, even for blocks that have no tool state.
  - It completes open tool blocks as invalid only on a length stop.
- OpenAI, Codex and xAI already emit structured events. The agent (`transitions.rs`) projects UI text and reasoning from `MessagePartDelta` / `ReasoningPartDelta`, dispatches calls from the finalized `ChatOutput`, and treats `ChatStreamFinish::Incomplete` as non-retryable.
- `StreamChunk::Structured` already serializes across Extism, the remote relay (`chunk_is_terminal` covers `ResponseTerminal`) and Python.
- The accumulator's `FunctionArgumentsDelta` requires a function-call item. No delta variant targets opaque items.

## Goals / Non-Goals

**Goals:**
- Make the orphan-delta failure impossible in flat mode for every flat-chunk provider (option 1).
- Move Anthropic streaming onto the structured path, so block boundaries come from the provider's own events (option 3).
- Keep the current Anthropic guarantees: thinking signatures one-to-one, redacted round-trip, invalid-input marking, interrupted blocks completed exactly once, and usage semantics.

**Non-Goals:**
- Changing the retry classification of accumulator errors.
- Migrating other flat-chunk providers.
- Removing flat mode, or renaming `StreamChunk` variants (that would change the plugin and Python API).
- Replaying opaque server-tool blocks to Anthropic in later requests. Replay keeps today's behaviour: unknown items are skipped.

## Decisions

### D1. Flat mode: only `ToolUseStart` opens a pending call
`ToolUseInputDelta` for an index without a start is logged at warn level (index only, never argument text) and otherwise ignored. A delta for a started index changes nothing in the pending set.

The accumulator tracks started indexes separately from pending ones (`started_legacy_calls`), so it can tell "never started" apart from "started and already completed". An orphan index warns once per attempt (`warned_orphan_calls`); a delta after completion is logged at debug level. Both sets reset with the attempt.

*Alternative:* treat orphan deltas as a protocol error. Rejected because it reproduces today's turn failure for a case where no executable call exists.

*Check:* OpenAI-compatible parsers (openai, mrs, kimi-code, openrouter, groq) emit a start only once the name is known. If a provider sent argument deltas before the name, those deltas would now be ignored by the pending set. Calls are still assembled from `ToolUseComplete`, so nothing is lost. Task 1.3 verifies that each flat provider emits a start before or alongside its first delta.

### D2. Anthropic emits structured events only
The parser emits only `StreamChunk::Structured(..)`. It does not interleave flat projections. This matches the "one event representation" requirement.

| Anthropic event | Structured event |
|---|---|
| `message_start` | `ResponseMetadata { status: InProgress, usage: start usage }` |
| `content_block_start` text | `ItemStarted(Message, empty Text part)` + `MessagePartStarted { content_index: 0 }` |
| `text_delta` | `MessagePartDelta { content_index: 0, Text }` |
| `content_block_start` thinking | `ItemStarted(Reasoning)` + `ReasoningPartStarted { Content, 0 }` |
| `thinking_delta` | `ReasoningPartDelta { Content, 0 }` |
| `signature_delta` | buffered in the parser |
| `content_block_start` redacted_thinking | `ItemStarted` + immediate `ItemCompleted` (reasoning with `encrypted_content` and the redacted extension) |
| `content_block_start` tool_use | `ItemStarted(FunctionCall { call_id, name, arguments: "" })` |
| `input_json_delta` on tool_use | `FunctionArgumentsDelta` (raw text also buffered) |
| `content_block_stop` | `ItemCompleted` with the authoritative snapshot (text, reasoning + signature, call + invalid flag) |
| `message_delta` usage | buffered; final usage goes on the terminal |
| `message_delta` stop_reason | close open tool blocks as invalid if the stop is length, then `ResponseTerminal` |
| `error` | `Err(LLMError)` as today |

`output_index` is the Anthropic block index, so output order matches content order.

Snapshot construction reuses the item constructors of the non-streaming path (`redacted_reasoning_item`, the same reasoning/message/call shapes). Item granularity follows content blocks: consecutive text blocks stay separate items and empty blocks are retained, whereas non-streaming merges consecutive text with `\n` and skips empty blocks. Replay is unaffected (empty text and unsigned empty reasoning are dropped at replay), but `ChatOutput::text()` joins streamed blocks without a separator. Merging is rejected because it would break the one-block-one-item index mapping, and changing non-streaming is out of scope.

*Alternative:* keep flat chunks and only fix the parser filter. Rejected as the long-term fix: tool-call boundary state would still be duplicated between parser and accumulator.

### D3. Unknown blocks become opaque items, buffered in the parser
At `content_block_start` for any other type, the parser emits `ItemStarted(Opaque { original_type, payload: block json })`. It buffers any `input_json_delta` or other delta payload itself, because no structured delta targets opaque items. At `content_block_stop` it emits `ItemCompleted(Opaque)` carrying the start payload plus the buffered input. The agent never dispatches opaque items.

The completed payload matches the non-streaming block shape: buffered input that parses as a JSON object replaces `payload.input`; input that does not parse is kept verbatim as `payload.input_json` (which then takes precedence over `input`). Only input deltas and text deltas are accumulated; other delta types (for example `citations_delta`) are dropped, as in non-streaming. A delta that produces nothing for a modelled block is logged at debug level with index, block kind and delta type only.

*Alternative:* add an `OpaqueDelta` event. Rejected: it changes the plugin and Python surface for no consumer benefit.

### D4. Terminal status from the stop reason
- `max_tokens` / `model_context_window_exceeded` (mapped to length) produce `ResponseTerminal { status: Incomplete, finish_reason: Length, detail: LENGTH_CUTOFF_DETAIL }`. Open tool blocks get `ItemCompleted` with the invalid extension first, exactly once. A late `content_block_stop` finds no state and emits nothing.
- All other stop reasons produce `Completed`.
- Open non-tool blocks at a completed terminal are left open, so the accumulator reports `IncompleteItems`. That is a genuine protocol violation.

The agent's structured `Incomplete` path must reach output-limit recovery with finish reason length. Task 3.4 verifies this; the existing spec already requires length handling regardless of status.

### D5. Usage
Usage is reported on `ResponseMetadata` at start and on `ResponseTerminal` at the end. The final value uses the same overlay semantics as today (`AnthropicUsage::overlay`, reasoning split). If an attempt is abandoned mid-stream, it still settles with the start usage via the accumulator output.

### D6. Parser robustness
The `terminated` guard is checked before every event type, including `message_start`. A second `message_start` within one response is ignored with a warning instead of emitting a second metadata event; failing the stream would turn a harmless protocol oddity into a retry.

### D7. Shared stream fixture
Consumer tests (agent, remote relay) load a checked-in Anthropic structured-event fixture instead of hand-built events. A parser test in `qmt-anthropic` asserts that a fixed SSE transcript produces exactly that fixture, so a parser change cannot leave consumer tests validating a stale shape. The agent does not depend on `qmt-anthropic`.

## Risks / Trade-offs

- [A remote host on this version streams to a client older than item-aware streaming] → That client cannot parse `Structured`. Mesh peers must be upgraded together, as for the 2026-09-22 release. This goes in the release notes.
- [UI projection differences: text from `MessagePartDelta` instead of `StreamChunk::Text`] → Already exercised by OpenAI/xAI. Agent-level tests with an Anthropic fixture cover text and thinking display (task 3.3).
- [The CLI and Python consumers render only flat chunks] → `cli/src/chat.rs` already handles structured deltas, and Python passes structured events through. Task 3.5 checks both.
- [Thinking signature arrives only at block stop] → It is carried in the `ItemCompleted` snapshot, which is authoritative and replaces the provisional item.
- [Option 1 masks a provider bug that drops starts] → The warning log keeps it visible. The calls are still not executed, because no `ToolUseComplete` arrives.
- [Codex can emit argument deltas before its start] → Calls are still assembled at `output_item.done`; the orphan warning fires once per index. Recorded in notes.md; fixing Codex is out of scope.
- [Release notes are generated from PR titles] → The mesh-compatibility note must be in the PR title/description and the release body, not only in notes.md.

## Migration Plan

1. Land option 1 on its own. It is small and independently fixes the observed failure for Anthropic as it is today.
2. Land the Anthropic structured parser together with its tests.
3. Rollback: revert the parser commit. Option 1 remains.
