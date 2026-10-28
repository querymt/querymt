# Proposal

## Why

In the 7 days from 2026-10-21 to 2026-10-28, sessions using the `anthropic` provider had 151 failed tool calls. 108 of them were rejected before running with `invalid tool arguments`. The model's intent was usually clear, but the arguments did not match the schema:

- `multiedit` `edits` or `replace_symbol` `replacements` sent as a JSON string, or with the wrapper left out (about 60 calls).
- `search_text` `exclude` sent as a string instead of an array (25 calls).
- snake_case keys sent to the three camelCase tools, e.g. `new_string` or `new_str` instead of `newString` (20 calls).
- numbers sent as strings, e.g. `offset: "466, "` (3 calls).

Each failure costs a model turn and tokens, and 12 of the retries failed again. The causes are on our side:

- `edit`, `multiedit` and `replace_symbol` use camelCase keys, while the other ~28 builtin tools use snake_case.
- `search_text` takes `include` as a string but `exclude` as an array.
- Validation errors echo whole escaped arguments and report only the first problem.
- The Anthropic provider drops the `strict` tool flag, so Anthropic's schema-constrained tool input (strict tool use) can never be used.

## What Changes

- **BREAKING: One canonical naming change.** The parameters of `edit`, `multiedit` and `replace_symbol` become snake_case (`file_path`, `old_string`, `new_string`, `replace_all`, `new_text`, `expected_hash`) in one release. CamelCase names and `old_str`/`new_str` remain accepted aliases into those canonical names, including inside nested items. There is no intermediate reverse mapping. Calls supplying conflicting spellings are rejected. Pre-tool hooks receive canonical names.
- **Shape repairs, driven by the schema.** Only cases with one reading are repaired: a JSON string parsed into an array or object, a number string turned into an integer, and a single edit or replacement wrapped into its array. Normalization handles combined mistakes, including aliases inside decoded arrays and wrapped items, and is idempotent. Broken JSON is never repaired. Every alias or repair is recorded in telemetry.
- **Raw and effective arguments have distinct owners.** Immutable model-emitted arguments remain in assistant output and stored history for provider replay. Effective arguments are normalized before hooks and again after any hook update, then used for validation, execution, snapshots, execution events and tool-result argument records. File-access scheduling must account for the effective arguments, including hook-modified targets, so hooks cannot introduce concurrent conflicting writes.
- **Clearer validation errors.** All validation errors are reported, each with its path and the expected type. Echoed values are shortened. Known mistakes get a hint.
- `search_text` `exclude` accepts a string or an array.
- **BREAKING:** builtin tool schemas set `additionalProperties: false`, so unknown keys are rejected instead of silently ignored.
- `question` `header` values longer than 12 characters survive argument validation and are truncated to 12 characters before presentation instead of being rejected.
- **Anthropic provider:**
  - sends `strict` for strict tools only after checked conversion to the subset Anthropic accepts; unsupported schemas follow an explicit failure or non-strict fallback policy, and fallback retains the original schema;
  - enforces Anthropic's strict-mode limits and falls back to non-strict when they are exceeded;
  - checks that the model supports strict tool use;
  - adds a `strict_tools` setting listing which tools are sent strict (default `edit`, `multiedit`, `replace_symbol`; empty list turns name-based strictness off);
  - reports invalid or truncated streamed tool input as a distinct error, carrying invalid-input metadata through stream assembly and execution preparation so an interrupted call cannot run merely because its buffered JSON happens to parse.
- Builtin tool definitions are not marked strict in the agent, so the OpenAI Responses rule that rejects strict tools with optional properties is unaffected. Strictness for the edit tools is chosen by the Anthropic provider setting.
- Adds a repeatable tool-failure report with bounded baseline and post-release windows, total call counts and failure rates. Successful repair counts are reported separately through an explicitly identified telemetry source. The 108 historical schema failures are a baseline to reproduce, not a deterministic post-release acceptance criterion.

## Capabilities

### New Capabilities
- `tool-argument-handling`: how tool arguments are normalized, validated and reported before a tool runs, with builtin-only aliases and wrapping and generic schema-driven value repairs. Covers:
  - the parameter naming convention;
  - schema rules for builtins;
  - repair telemetry;
  - raw versus effective argument ownership and malformed-input rejection;
  - error message format.

### Modified Capabilities
- `anthropic-messages-protocol`: the strict tool flag is sent to the API, schemas are converted for strict tools, model support and request limits are enforced, and invalid streamed tool input is handled.
- `tool-batch-scheduling`: file conflicts are detected from effective arguments, including aliases, repairs and hook-modified targets, so normalization or hooks cannot bypass same-file ordering.

## Impact

- **`crates/agent`:**
  - `agent/execution/tool_calls.rs`: argument parsing, hook input and `validate_tool_arguments`.
  - `agent/execution/transitions.rs` and middleware tool-call state: execution preparation, invalid-input metadata propagation and scheduling of effective arguments.
  - `tools/registry.rs`: `file_access`.
  - the `Tool` trait: alias declarations.
  - builtins: `edit`, `multiedit`, `replace_symbol`, `search_text`, `question`, plus `additionalProperties` on every builtin schema.
  - `delegation/summarizer.rs`, `middleware/parallel_edit_guard.rs`, `agent/execution_tests.rs`.
- **`crates/providers/anthropic`:** `lib.rs` (`AnthropicTool`, request building, stream parser, history replay), `model.rs` (model support check) and `factory.rs` (setting).
- **`crates/querymt`:** no change to `FunctionTool`; its existing `strict` field is used as is. Verify that stream assembly retains invalid-input extensions and avoid losing them when preparing executable calls.
- **External:**
  - `querymt-desktop` (`session-tool-args.ts`, `session-tool-read-output.ts`) must read both naming styles from stored history.
  - User pre-tool hooks that read `filePath`/`oldString`/`newString` must switch to the snake_case names.
- **Operations:**
  - changing tool schemas invalidates prompt caches once;
  - strict schemas add compile latency on first use (Anthropic caches them for 24 hours).
