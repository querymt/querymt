# Tasks

## 1. Flat-chunk pending calls open only on explicit start (option 1)

- [x] 1.1 In `crates/querymt/src/chat/streaming.rs`, stop `ToolUseInputDelta` from inserting into `pending_legacy_calls`. Log a warn-level contract message with the index only (no argument text) when the index was never started. Verify with a new unit test: an orphan delta for index 1 plus a completed call 0 and a `ToolCalls` stop finishes `Completed` with one call.
- [x] 1.2 Keep the existing tests `legacy_terminal_rejects_unfinished_tool_call` and `legacy_length_stop_with_unfinished_tool_call_is_incomplete` passing. Add a test that a started call followed by deltas and a `ToolCalls` stop with no completion still fails with `IncompleteToolCalls([1])`. Verify with `cargo nextest run -p querymt streaming`.
- [x] 1.3 Confirm that every flat-chunk provider (openai chat-completions, mrs, kimi-code, openrouter, groq, google, llama-cpp, codex flat path) emits `ToolUseStart` before or alongside the first `ToolUseInputDelta` for each index. Verify by reading each emission site and by running each provider's existing streaming tests (`cargo nextest run -p qmt-<provider>`). Record any provider that does not.

## 2. Anthropic parser emits structured events (option 3)

- [x] 2.1 Emit `ResponseMetadata { status: InProgress, usage }` on `message_start`. Verify with a parser test that the first chunk of any stream is structured metadata carrying the start usage.
- [x] 2.2 Map `text` blocks to `ItemStarted(Message)` + `MessagePartStarted` + `MessagePartDelta`, and `content_block_stop` to an `ItemCompleted` message snapshot. Verify with a parser test that pushes the chunks through `ChatStreamAccumulator` and checks that the finished text equals the concatenated deltas.
- [x] 2.3 Map `thinking` blocks to a reasoning item with `ReasoningPartDelta`, with the buffered signature in the `ItemCompleted` snapshot. Map `redacted_thinking` to immediate start and completion with opaque data. Verify that the existing thinking and redacted-thinking stream tests are migrated to the accumulated `ChatOutput` and still pass, including the empty-text-with-signature case.
- [x] 2.4 Map `tool_use` blocks to `ItemStarted(FunctionCall)` + `FunctionArgumentsDelta`. At stop, the `ItemCompleted` snapshot carries the raw arguments, `{}` for no deltas, and the invalid-input extension for non-object input (reuse `AnthropicToolUseState::into_complete` logic). Verify with tests for parallel calls at indexes 2 and 3, empty input, and non-object input.
- [x] 2.5 Map any other block type to an `Opaque` item. The parser buffers its deltas and emits `ItemCompleted(Opaque)` at stop. Verify with a test: a `server_tool_use` block at index 1 streaming `input_json_delta`, a `tool_use` at index 2 and a `tool_use` stop finish `Completed`, with exactly one function call.
- [x] 2.6 Map the `message_delta` stop reason to `ResponseTerminal` with the final overlaid usage. On a length stop, first emit invalid `ItemCompleted` for each open tool block in index order, then an `Incomplete` terminal with `LENGTH_CUTOFF_DETAIL`. A late `content_block_stop` emits nothing. Verify with tests for an open block at `max_tokens`, a late stop after interruption, a call completed before the limit staying valid, and final usage with a thinking split.
- [x] 2.7 Remove all flat-chunk emission from `AnthropicStreamParser`, and confirm that the `error` event still returns a classified `LLMError`. Verify with `rg "StreamChunk::(Text|Thinking|ToolUse|Done|Usage)" crates/providers/anthropic/src` showing no non-test matches in the parser, and with `cargo nextest run -p qmt-anthropic`.

## 3. Consumer verification

- [x] 3.1 Add an agent execution test with a scripted Anthropic structured stream (text + thinking + two tool calls). Check that both calls are dispatched exactly once and that the assistant message stored in history matches today's non-streaming shape. Verify with `cargo nextest run -p querymt-agent transitions`.
- [x] 3.2 Add an agent test that replays the failing session shape (an opaque block plus a local call with a `tool_use` stop). Check that the run completes with no `llm_retry_wait` event.
- [x] 3.3 Check that the UI projection emits assistant text and thinking deltas for an Anthropic structured stream (thinking under one part id per reasoning item). Verify with an agent event assertion test.
- [x] 3.4 Check that an Anthropic `max_tokens` stop with an open tool block reaches output-limit recovery: no retry wait, one `INVALID_JSON` result with split guidance, and the stop type is the model token limit. Verify by running the existing output-limit-recovery agent tests with an Anthropic structured fixture.
- [x] 3.5 Check that `cli/src/chat.rs` and the Python bindings render text from an Anthropic stream. Verify by running `cargo nextest run -p querymt-cli` and `cargo nextest run -p querymt-py` (or the package's stream conversion test).
- [x] 3.6 Check that a structured Anthropic stream through the remote mesh relay terminates on `ResponseTerminal`. Verify with the existing remote provider host tests, using an Anthropic-shaped structured fixture.

## 4. Integration

- [x] 4.1 Run `cargo clippy --workspace --all-targets` and `cargo nextest run`, with provider crates built under `--features native --no-default-features` and `querymt-mobile-ffi` excluded (pre-existing breakage, see notes.md), and confirm that both pass.
- [x] 4.2 Run a live smoke test against Anthropic with a prompt that triggers parallel tool calls and extended thinking. Confirm that the turn completes with no accumulator warnings, and record the result in the change notes.
- [x] 4.3 Add a release note that remote mesh peers must run a version with item-aware streaming to consume Anthropic streams from an upgraded host. Release notes are generated from PR titles (`release.yml`), so the note must also go in the PR title/description and the release body.

## 5. Review follow-ups

- [x] 5.1 Accumulator: track `started_legacy_calls` and `warned_orphan_calls`. Warn once per orphan index; log a delta after completion at debug level without the missing-start warning. Verify with tests: many orphan deltas for one index still complete, and a delta after `ToolUseComplete` leaves the call unchanged.
- [x] 5.2 Parser: check the `terminated` guard before `message_start`, and ignore a second `message_start` with a warning. Verify with tests for a duplicate `message_start` before and after the terminal.
- [x] 5.3 Parser: at opaque block stop, buffered input that parses as a JSON object replaces `payload.input`; otherwise keep it as `payload.input_json`. Verify with the server-tool test and a non-object input test.
- [x] 5.4 Parser: log at debug level (index, block kind, delta type only) when a delta produces nothing for a modelled block.
- [x] 5.5 Add a parser test for consecutive text blocks producing two message items.
- [x] 5.6 Add a checked-in Anthropic structured-event fixture produced from a fixed SSE transcript, with a `qmt-anthropic` test asserting the parser output equals it. Make the agent and remote relay tests load the fixture instead of hand-built events.
- [x] 5.7 Record the Codex pre-start delta finding for task 1.3 in notes.md.
