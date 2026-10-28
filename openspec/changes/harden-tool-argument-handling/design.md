# Design

## Context

See proposal.md (Why) for the failure data. Current state relevant to the approach:

- `execute_tool_call_inner` (`crates/agent/src/agent/execution/tool_calls.rs`) parses `call.function.arguments` into a `Value`, runs pre-tool hooks (which may replace the input), then calls `validate_tool_arguments`, which looks up the schema (builtin registry, then MCP tool state, then provider tools), caches a `jsonschema` validator and returns only the first error's `Display` text.
- `ToolRegistry::file_access` (`crates/agent/src/tools/registry.rs`) parses the raw argument string a second time for batch scheduling and calls the tool's `file_access`. `edit` and `multiedit` call `single_path_write_access(args, "filePath", cwd)`.
- Builtin schemas are inline `json!` values in each tool's `definition()`. Only `edit`, `multiedit` and `replace_symbol` use camelCase; none sets `additionalProperties`.
- `FunctionTool` already has `strict: Option<bool>`. The OpenAI provider passes it through; the Anthropic `AnthropicTool` struct has no `strict` field, so it is dropped.
- The Anthropic stream parser emits `ToolUseComplete` with whatever was accumulated in `arguments_buffer`, without a parse check. History replay replaces unparseable arguments with `{}`.
- Stored history and `querymt-desktop` contain camelCase edit arguments.

## Goals / Non-Goals

**Goals:**
- A single effective argument representation per prepared call, shared by validation, file-access scheduling and execution, without rewriting raw provider output.
- Repairs limited to readings that are unambiguous from the schema.
- Provider-specific strictness lives in the provider; the agent keeps validating against the full original schema.

**Non-Goals:**
- Repairing syntactically broken JSON inside string values.
- Aliasing parameter names or wrapping single items for MCP or provider-supplied tools. They receive only the generic schema-driven value repair (Decision 3).
- Enabling `eager_input_streaming` (only the safeguard for invalid input is added).
- Rewriting stored history to the new names.

## Decisions

### 1. Normalisation is a separate step owned by the agent, driven by the tool
Add to the agent `Tool` trait two defaulted methods:
- `arg_aliases(&self) -> &'static [ArgAlias]`, where `ArgAlias { scope: AliasScope, from: &'static str, to: &'static str }` and `AliasScope` is `TopLevel` or `ArrayItems("edits")`.
- `wrap_single_item(&self) -> Option<WrapRule>`, where `WrapRule { array: "edits", fields: &["old_string", "new_string", "replace_all"], required: &["old_string", "new_string"] }`.

A new module `crates/agent/src/tools/args.rs` provides `normalize_arguments(tool: Option<&dyn Tool>, schema: Option<&Value>, args: &mut Value) -> Result<Vec<Repair>, ArgConflict>`. Normalisation recursively decodes schema-declared containers before visiting their contents, normalises property names at each object scope, repairs scalar values and then visits child schemas. When a builtin permits an omitted wrapper, its item aliases also apply to the flat item fields before the wrapping rule checks required fields; the resulting item is then processed using the array item schema. Thus stringified `edits` containing `oldString`, and flat replacements containing `newText`, both reach canonical nested arguments.

Alias declarations always point into the final snake_case schema. Supplying multiple spellings for the same property is rejected rather than choosing a winner. Re-running normalisation on a normalised value produces the same value and no further repairs. Repairs are recorded only for changes actually applied. Container decoding, aliases and wrapping have combined regression tests, not only isolated rule tests.

*Alternatives:* a pre-tool hook (rejected: user hooks run in that slot and must see normalised input); `#[serde(alias)]` on tool structs (rejected: builtins read `Value` directly, and validation and scheduling run before deserialisation).

### 2. Prepare effective arguments before scheduling and carry them into execution
Introduce an execution-preparation stage with an owned prepared-call representation containing effective arguments, hook decisions and context contributions, validation outcome, and invalid-input status. Parse and reject malformed or provider-marked invalid input before invoking hooks. Normalise valid input before hooks; run pre-tool hooks exactly once per call; normalise any `updated_input` again and validate the final value. Blocked or invalid calls become ordered error results without file access or tool invocation.

Compute file access and plan execution waves from the final effective arguments, not a separately parsed raw copy. Execution consumes the prepared call and does not repeat its pre-tool hooks or argument preparation. Registry file-access helpers and `ParallelEditGuard` must not make definitive conflict decisions from raw arguments: share the prepared accesses with conflict checks or move those checks after preparation. Pure normalisation on a copy is insufficient when a hook changes a target.

Prepare calls within execution-barrier-delimited segments, rather than eagerly preparing the whole turn. Do not prepare calls beyond a stateful or clarification boundary until that boundary has completed and execution may continue. Keep preparation in model order and preserve cancellation, clarification short-circuiting, hook notice/context delivery and exactly one result per call. Calls skipped because of cancellation or clarification do not invoke pre-tool hooks. A regression test redirects two initially disjoint edits to one target through hooks and verifies they cannot execute concurrently.

Raw model-emitted arguments remain immutable in canonical assistant output and stored assistant history for provider replay. Effective arguments feed validation, scheduling, permission checks, tool invocation, snapshot association, `ToolCallStart`, progress records, post-tool hooks and tool-result argument records. These execution records persist effective arguments; they do not replace the raw assistant call. Desktop rendering and history consumers must distinguish the two representations and accept historical camelCase data.

*Alternatives:* rewrite raw middleware calls (rejected: changes provider replay); normalise a second copy in `file_access` (rejected: cannot account for hook-modified targets); run hooks again immediately before invocation (rejected: duplicates user-visible effects and can change the planned access).

### 3. Value repair is schema-driven and applies to all tools
Value repair needs only the schema, so it runs for builtin and MCP tools alike. It walks `properties` (and `items` for arrays) and only changes a value when its JSON type differs from a single declared `type`:
- schema `array`/`object`, value string → `serde_json::from_str`; keep only on type match.
- schema `integer`, value string → trim whitespace and trailing commas, `parse::<i64>`.
- schema `array` with `items.type: string`, value string → wrap in a one-element array (covers `search_text.exclude` and similar MCP patterns).
- `anyOf`/`oneOf`/type arrays → no repair.
- schema `string` → never touched.

Aliases and wrapping stay builtin-only because they encode a tool's own meaning.

### 4. One snake_case migration with permanent compatibility aliases
Builtin schemas move to `file_path`, `old_string`, `new_string`, `replace_all`, `new_text`, `expected_hash`. Aliases cover the camelCase names and the `old_str`/`new_str` short forms. Schema changes, alias declarations, tool readers, file-access declarations and dependent consumers land together; there is no release that maps snake_case back into camelCase. Tool implementations read only the canonical names. A unit test iterates the registry's builtin definitions and checks property names recursively.

*Alternative:* keep camelCase and alias snake_case. Rejected: every other builtin is snake_case and observed drift is entirely toward snake_case.

### 5. Closed schemas plus alias-aware validation
Every builtin object schema gets `additionalProperties: false`. Since normalisation runs before validation, aliases never reach the validator. A test checks all builtin schemas are closed.

Remove `maxLength: 12` from `question.header`'s validation schema while retaining the concise-header guidance in its description. Inside `QuestionTool::call`, truncate the header to the first 12 Unicode scalar values before presentation. Do not introduce a generic string-truncation repair. Test an overlong multibyte header through agent validation and execution, as well as directly through the tool.

### 6. Validation error format
`validate_tool_arguments` returns a structured error built from `validator.iter_errors()`: one line per error, `<instance_path>: <kind>` (e.g. `/edits: expected array, got string`). Values are shortened to 80 characters with `…`. Hints are selected by error kind and context:
- type error, expected array/object, got string → "must be a JSON array/object, not a string containing JSON";
- additional property whose snake/camel conversion or known short form matches a declared name → "did you mean `x`?".

### 7. Telemetry
A counter `querymt.tool.args_repaired` with attributes `tool` and `rule` (`alias`, `wrap`, `json_string`, `int_string`, `scalar_to_array`), and a span attribute `querymt.tool.args_repairs` listing applied rules. Implemented through the existing `genai`/OpenTelemetry helpers used in `tool_calls.rs`. Record mutations from initial and hook-output normalisation without counting scheduling inspections as repairs. Export a per-call repaired indicator and execution outcome so the metrics backend can distinguish repaired calls that succeed from repaired calls that still fail; rule counters alone do not count successful repaired calls.

The failure report accepts explicit inclusive start and exclusive end timestamps, provider/model filters and deployment boundaries. Reproduce the historical baseline using `[2026-10-21T00:00:00Z, 2026-10-28T00:00:00Z)` and document any difference from the reported counts rather than forcing a match. Report total calls, failure counts and rates, and repeated failures. Use session execution records for failure/volume data and the exported per-call telemetry for repair outcomes, documenting backend queries and retention. If repair telemetry is unavailable, mark those figures unavailable rather than inferring them from failed calls. Compare a full seven-day post-release window containing no pre-release calls with the bounded baseline; a three-day report is preliminary only. Production improvement is observational, not a deterministic implementation acceptance gate.

### 8. Anthropic strict tools
- `AnthropicTool` gains `strict: Option<bool>` (skipped when `None`) and owns its `input_schema` as `Cow<Value>` so a converted schema can be sent.
- `strict_schema::convert(&Value) -> Result<Value, UnsupportedSchema>` (new module in the provider) works on a copy, closes fixed-property objects, removes supported-to-relax constraints and appends them to the description as `(minimum: 0)`-style text. It checks the complete supported subset, including references, recursion, formats, patterns, composition and object extensibility. Resolve supported local references with cycle detection; unsupported or external references, recursive schemas, dictionary objects and other constructs that cannot be converted safely produce `UnsupportedSchema`. Do not silently close dictionary schemas or discard unknown validation keywords. The original schema remains authoritative for local validation.
- `strict_schema::budget(&[Value]) -> Result<(), StrictLimit>` counts strict tools, optional properties and union properties across the request.
- `ClaudeModel::supports_strict_tools()` in `model.rs`.
- A `strict_tools: Option<Vec<String>>` field on `Anthropic`, read in `factory.rs`; `None` means the default `["edit", "multiedit", "replace_symbol"]`.
- Request building: a tool is strict when `tool.function.strict == Some(true)` or its unprefixed name is in `strict_tools` (matched before `prefix_tool_name`). Convert each eligible schema with the checked converter. If conversion fails, send that tool non-strict with its original, unmodified schema and a warning identifying the tool and unsupported feature, without logging argument contents. Budget checks run over the tools that remain strict. If the model check or the budget fails, send all tools non-strict with their original schemas and log (debug for model, warn for budget). Test explicit `strict: true` as well as name-selected tools under this Anthropic-specific fallback policy.
- The agent does **not** set `strict` on builtin definitions. The existing `openai-responses-protocol` spec requires a schema error when `strict=true` meets a schema with optional properties; marking the edit tools strict in the agent would break OpenAI Responses. Keeping the choice in the Anthropic provider avoids changing that spec.

*Alternative:* mark builtins strict and make OpenAI fall back to non-strict. Rejected: it changes an accepted OpenAI requirement for an Anthropic-only benefit.

### 9. Invalid streamed tool input
The Anthropic stream parser validates the accumulated buffer at `content_block_stop` (`serde_json::from_str::<Map>`), preserving the existing empty-buffer `{}` representation for a normally completed zero-argument call. On failure it still emits `ToolUseComplete`, with an extension marker `anthropic.invalid_input = true`. When `max_tokens` terminates a response with open tool blocks, flush each open block as exactly one marked `ToolUseComplete` before emitting `Done`, even when its buffer is valid JSON. Remove flushed state so a later stop cannot duplicate completion. Preserve the raw buffer for diagnostics.

Stream assembly retains this extension on the canonical function-call item. Execution preparation reads canonical function-call metadata before the lossy `to_tool_call` projection and carries an explicit invalid-input status alongside raw arguments into middleware/prepared-call state. Do not try to recover invalidity from argument syntax alone. The agent rejects marked input, malformed JSON and non-object top-level arguments before hooks, permissions or tool invocation, returning one error result `{"INVALID_JSON": "<raw, at most 2000 characters>"}`. For all providers this replaces the current parse-failure-to-`{}` fallback. End-to-end tests cover metadata surviving assembly and execution projection, including a `max_tokens`-interrupted block whose buffer is syntactically valid.

Keep immutable raw assistant output for diagnostics and provider replay. Verify Anthropic history encoding produces a protocol-valid representation for malformed calls and their error results without treating a replay placeholder as executable input; replay fallback must not erase the raw diagnostic record.

## Risks / Trade-offs

- [Repairs hide model weaknesses] → telemetry counter by rule; the failure report tracks repaired vs failed.
- [Breaking rename for user hooks matching `filePath`] → changelog entry; hooks receive normalised (snake_case) input consistently.
- [Desktop renders stored camelCase history] → desktop reads both forms; tests on recorded fixtures.
- [Name-based strictness drifts from tool renames] → the default list uses canonical builtin names, and a provider test asserts each default name exists among the agent's builtin tools.
- [Prompt cache invalidation and strict-schema compile latency] → ship schema changes in one release; Anthropic caches compiled grammars 24h.
- [OAuth endpoint may reject `strict`] → manual verification task; `strict_tools = []` disables name-selected strictness, and a provider-level OAuth capability gate disables all strictness (including explicit flags) if verification fails.
- [Closed schemas reject keys some model habitually sends] → aliases cover observed drift; error hints point to the right name.

## Migration Plan

1. Capture the bounded baseline and establish the telemetry queries. Generic normalisation machinery can be developed independently, but does not ship an intermediate canonical naming convention.
2. Release canonical snake_case schemas, compatibility aliases, recursive repairs, prepared-call scheduling, error handling, closed schemas and `search_text`/`question` changes together with dependent consumers and the desktop update. Include the invalid-input metadata path and a changelog entry for hook authors. Verify the complete compatibility and scheduling test suite before release.
3. Release Anthropic strict support with the default `strict_tools` list after OAuth and API-key verification. If OAuth verification fails, disable strictness for that authentication path, including explicitly requested strictness, and record why.
4. Capture preliminary three-day reports and a complete seven-day post-release window for each deployment. Compare rates and call volumes against the bounded baseline; report repair outcomes from the telemetry backend separately.

Rollback: strictness selected by name can be disabled with `strict_tools = []`; explicit `FunctionTool.strict` requests require disabling their callers or reverting provider strict support. Roll back the canonical naming release as a coordinated unit with hook/desktop compatibility, preserving raw assistant history and accepting both historical spellings. Do not swap alias direction in isolation while leaving schemas, readers or scheduling on the other convention.

## Open Questions

- Exact OpenTelemetry attribute names, if the project has a naming convention for custom metrics beyond `querymt.*`.
