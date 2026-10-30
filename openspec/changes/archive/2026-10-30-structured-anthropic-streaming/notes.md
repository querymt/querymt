# Implementation Notes

## Flat-provider start ordering (task 1.3)

All flat providers emit `ToolUseStart` before or alongside the first
`ToolUseInputDelta`, with one exception:

- codex `handle_function_call_arguments_delta`
  (`crates/providers/codex/src/api.rs:1469`) emits `ToolUseInputDelta` before
  `ToolUseStart` when `response.output_item.added` did not carry the call id and
  name (`emit_tool_start` is a no-op until both are known). The call is still
  assembled correctly at `response.output_item.done` (start, delta, complete),
  and `ensure_no_incomplete_tool_calls` guards the terminal. The accumulator logs
  one orphan warning per such index. Fixing Codex is out of scope.

## Live smoke test (task 4.2)

A real streamed `/v1/messages` request (extended thinking enabled, budget 1024, one
`get_weather` tool, prompt asking for Paris and Rome in parallel) was captured and
replayed through `AnthropicStreamParser` and `ChatStreamAccumulator`.

- Parser output was structured events only (no flat chunks).
- Finished `Completed` with 4 items in block order: reasoning (signature present),
  text message, and two calls (`get_weather` Paris, `get_weather` Rome) with raw
  JSON arguments. Finish reason `ToolCalls`; usage input 607, output 105,
  reasoning 83 (split from the inclusive output count).
- No accumulator errors or warnings.

Scope: this exercised the real wire format through the provider parser and the
accumulator. It did not run a full agent turn against the live API.

## Release note (task 4.3)

Anthropic streaming now emits item-aware structured events (`StreamChunk::Structured`)
instead of flat chunks. Remote mesh peers must run a version that includes
item-aware streaming (2026-09-22 or later) to consume Anthropic streams from an
upgraded host; older clients cannot accumulate structured events.

Also fixed: a flat-chunk tool-argument delta for an index that was never started no
longer fails the response with `completed legacy response has unfinished tool call
indexes`. Unmodelled Anthropic blocks (for example `server_tool_use`) are retained
as non-executable opaque items.

## Validation results (task 4.1)

Run with `PYO3_PYTHON=python3.14` (system Python 3.9 is below the abi3-py310 floor).
`querymt-mobile-ffi` is skipped by decision (its test target implements the removed
`chat_with_tools` and does not compile, independent of this change).

Passing:
- `cargo clippy --workspace --all-targets --exclude querymt-mobile-ffi`: no errors.
- `cargo nextest run -p querymt-agent`: 3563/3563.
- `cargo nextest run -p querymt -p querymt-remote -p querymt-cli -p querymt-py -j 1`: 442/442.
- `cargo nextest run -p querymt-remote` alone: all pass.
- Provider libs with `--features native --no-default-features` (anthropic 133, openai 122,
  codex 65, kimi-code 21, openrouter 10, groq 4, google 17): all pass.

Remaining, unrelated to this change:
- `querymt-remote iroh_config::tests::custom_primary_dns_works_with_fallback_disabled`
  fails only when the four packages run in parallel (`Request timed out`). The test
  resolves against a one-shot local UDP nameserver with a 2 s timeout; under parallel
  load the reply misses that window. It passes alone, with `-j 1`, and fails the same way
  with this change stashed (where two `iroh_diagnostics` relay tests also fail).
- Default-feature provider test builds (`qmt-openai` etc.) fail to link extism host
  symbols, so a literal `cargo nextest run --workspace` does not build on this machine.
  CI tests providers with `--features native --no-default-features`.
- `qmt-mrs` tests that build a provider time out (model download); mrs streaming has no
  unit tests.
