# Verification Record

Task 6.4 — focused core/provider/agent/binding tests and the applicable
workspace feature/build matrix.

## Successful commands

### Core

- `cargo test -p querymt --lib` → 178 passed, 0 failed
- `cargo check -p querymt --no-default-features` → 0 errors
- `cargo check -p querymt-extism-macros` → 0 errors

### Providers

- `cargo test -p qmt-openai --features native --lib` → 83 passed, 0 failed
- `cargo test -p qmt-codex --features native --lib` → 54 passed, 0 failed
- `cargo test -p querymt-remote --lib` → 81 passed, 0 failed
- Feature matrix, `native` and `extism` for `qmt-openai`, `qmt-codex`, `qmt-xai`
  → 0 errors
- `cargo check -p <provider> --features native` for `qmt-anthropic`, `qmt-groq`,
  `qmt-deepseek`, `qmt-mistral`, `qmt-openrouter`, `qmt-moonshot`, `qmt-alibaba`,
  `qmt-zai`, `qmt-kimi-code` → 0 errors
- `cargo test -p <provider> --features native --lib` → all passed
  (anthropic 30, groq 4, deepseek 1, openrouter 10, kimi-code 21; others 0 tests)

### Agent

- `cargo test -p querymt-agent --lib` → 2107 passed, 0 failed, 3 ignored

## Environment-limited checks

- `cargo check -p querymt-py`: build-script failure from `pyo3-ffi` because the
  system interpreter is Python 3.9.6 while the crate requires `abi3-py310`
  (≥ 3.10). Environmental; unrelated to this change.
- `cargo check -p querymt-mobile-ffi`: pre-existing compile errors unrelated to
  this change. The working tree does not modify `querymt-mobile-ffi`
  (`git diff --name-only HEAD` excludes it).

## Pre-existing failures (not caused by this change)

- `qmt-xai` `parse_list_models_*` (3 tests): stale expectations versus
  `XAI_ADDITIONAL_LIST_MODELS`, which already included `grok-4.5` at `HEAD`
  (commit `4a56b10e`). Diff confirms this change does not touch
  `parse_list_models`, `XAI_ADDITIONAL_LIST_MODELS`, or
  `is_supported_xai_list_model`.

## Legacy Chat Completions regressions

- `qmt-openai` Chat Completions snapshot tests assert the default mode keeps the
  `/chat/completions` endpoint, the `messages` body, and omitted strictness
  semantics; all pass.

---

# Task 8.x — legacy `Content` removal, canonical reasoning persistence

## What changed

### `crates/querymt`: the legacy `Content` enum is no longer public

`Content` was moved from `chat/mod.rs` into `chat/migration.rs` and is now
`pub(crate)`. Only the migration decoders may read it. Removed from the public
API:

- the `Content` enum itself (9 variants) and every public builder
  (`text`, `image`, `image_url`, `pdf`, `audio`, `thinking`, `tool_use`,
  `tool_result`, `resource_link`, `tool_result_error`)
- the hand-written `PartialEq`, `Eq`, and `Display` impls
- the legacy projection helpers: `ChatOutput::portable_content`,
  `portable_content_with`, `MediaPart::portable_content`,
  `ChatInputPart::input_from_legacy`, `input_parts_from_legacy`,
  `legacy_content_to_input_part`, `ToolResultPart::from_legacy_blocks`,
  and the private `legacy_input_part` shim
- `ChatMessage::from_user(Vec<Content>)` / `from_assistant(Vec<Content>)`,
  replaced by `from_user_parts(Vec<ChatInputPart>)` and
  `from_assistant_output(ChatOutput)`
- `ChatMessageBuilder::tool_result` now takes canonical
  `Vec<ToolResultPart>` instead of `Vec<Content>`

Retained as **`pub(crate)` migration-only** decoders:

- `ChatOutput::to_legacy_projection(preserve_signatures)`
- `MediaPart::legacy_projection()`
- `normalize_tool_result_media(&Content)`

### `crates/agent`: persisted reasoning widened to canonical form

Task instruction: widen the persisted reasoning part so it carries full
canonical reasoning rather than the lossy `(content, signature)` pair.

`MessagePart::Reasoning` changed from

```rust
Reasoning { content: String, signature: Option<String>, time_ms: Option<u64> }
```

to

```rust
Reasoning { item: querymt::chat::ChatReasoningItem, time_ms: Option<u64> }
```

so that `id`, `summary`, `content`, `encrypted_content`, `signature`, `status`,
and `extensions` all survive a SQLite round trip. Previously an encrypted
reasoning continuation could not be persisted at all.

Added `deserialize_reasoning_item` (`crates/agent/src/model.rs`): a
`deserialize_with` migration decoder on the `item` field. It detects the legacy
flat shape by the absence of the item-only fields (`summary`, `id`,
`encrypted_content`) and lifts it into a canonical item with its text in
`content`. **Old rows therefore keep loading unchanged.**

Added `ChatReasoningItem::visible_text()` in `crates/querymt/src/chat/output.rs`
for display/token-estimation text, deliberately excluding encrypted continuation
and signatures.

## On-disk compatibility

- **Backward (old data → new binary): verified by construction.** The legacy flat
  `{"content": ..., "signature": ...}` reasoning row is decoded by
  `deserialize_reasoning_item` and normalizes to canonical input/output.
- **Forward (new data → old binary): intentionally NOT compatible.** A row written
  after this change carries the canonical item shape and cannot be read by an
  older binary. This is an accepted one-way migration, recorded here rather than
  silently assumed. No `MessagePart` schema version field is introduced by this
  change.

## Scope additions made during implementation

These were not authorized by the change tasks and are recorded so they are
visible rather than absorbed:

1. `convert_prompt_blocks` (public in `querymt-agent`) changed its return type
   from `Vec<Content>` to `Vec<ChatInputPart>`. Unavoidable once `Content` became
   private; it is a public API change to `querymt-agent`.
2. `PromptContentError` gained two variants (`InvalidMediaType`, `InvalidMedia`)
   to carry `MediaPart`/`MediaType` construction failures that previously could
   not occur on this path.
3. The `blocks` intermediate in `AgentMessage::to_chat_message_with_output_target`
   was removed entirely; text, prompts, attachments, tool results, hook context,
   snapshots and compaction text now accumulate directly into `ChatInputPart`.
   This deleted the provably dead `blocks.extend(output.portable_content_with(true))`
   (its result was discarded by the early `return` in the `structured_output`
   branch) and, with it, the redundant `signed_reasoning` re-derivation from
   reasoning re-lowered into legacy blocks.

## Verification (this session)

- `cargo check -p querymt --lib` → 0 errors
- `cargo check -p querymt --lib --no-default-features` → 0 errors
- `cargo check -p querymt --lib --all-features` → 0 errors
- `cargo test -p querymt --lib` → **183 passed, 0 failed**
- `cargo check -p querymt-agent --lib` → 0 errors
- `cargo test -p querymt-agent --lib` → **2129 passed, 0 failed, 3 ignored**

## Task 8.6 closure

The caller-migration gate is satisfied: repository source search finds no use of
`querymt::chat::Content` outside the private migration decoder. The remaining
legacy-only implementation was removed:

- `Content` has no builders or manual `PartialEq`, `Eq`, or `Display`
  implementations; the private DTO derives equality only for migration-boundary
  consistency checks.
- `Content::to_media_part`, `ChatOutput::to_legacy_projection`,
  `MediaPart::legacy_projection`, `MediaPart::legacy_display_block`,
  `ChatMessage::output_projects_to`, and the test-only raw-argument decoder were
  deleted.
- Legacy projection needed to validate transitional serialized records now lives
  entirely inside `chat/migration.rs`; it is not exposed on canonical values.
- Superseded legacy projection/media tests were removed. Migration fixtures still
  cover old user, assistant, media, call, resource-link, and rich tool-result
  decoding.

Task 8.6 is complete. Tasks 8.2, 8.7, and 8.8 remain open.

## Python and CLI migration (corrected)

The prior claim that Python was blocked or unmigrated was stale. A suitable
interpreter is present at `/opt/homebrew/bin/python3` (Python 3.14.7), and both
bindings and CLI now compile against canonical types:

- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo check -p querymt-py` → 0 errors
  (one existing dead-code warning for `PyBlock::Generated`).
- `cargo check -p querymt-cli` → 0 errors (one existing dead-code warning).

The Python migration uses the agreed **Option B breaking change**: generated
`thinking` and `tool_use` blocks are no longer accepted in ordinary
`message.content`; callers must provide canonical assistant `output`. Python
input media and tool results normalize to canonical `ChatInputPart` /
`ToolResultPart`, and provider responses expose canonical output JSON. This
intentionally drops source-level compatibility rather than recreating the
removed recursive Rust representation.

## Dependent-crate migration status

Every provider **library** now compiles against the privatized `Content`:

| Crate | Result |
| --- | --- |
| `qmt-openai` | 90 passed, 0 failed |
| `qmt-codex` | 56 passed, 0 failed |
| `qmt-xai` | 14 passed, 3 failed (pre-existing `parse_list_models_*`) |
| `qmt-kimi-code` | 21 passed, 0 failed |
| `qmt-deepseek` | 1 passed, 0 failed |
| `qmt-llama-cpp` | 65 lib + 5 + 6 passed, 0 failed |
| `querymt` | 183 passed, 0 failed |
| `querymt-agent` | 2129 passed, 0 failed, 3 ignored |

Migration work done in the providers: removed the dead legacy helpers in
`openai/src/api.rs` (`collect_image_message_contents`,
`content_text_with_fallbacks`, `tool_result_text`); converted
`ChatMessageBuilder::tool_result` call sites to canonical `Vec<ToolResultPart>`;
converted `ChatMessage::from_assistant(output.portable_content())` sites to
`from_assistant_output(output.clone())`; dropped the now-private `Content`
imports from openai, anthropic, google, ollama, xai, codex, llama-cpp.

All 16 provider crates now compile with `--lib --tests` (0 errors), and
`qmt-llama-cpp` also passes `--all-targets` with no `Content::` references
remaining anywhere in it.

### llama-cpp

`qmt-llama-cpp` keeps no dependency on the legacy enum. Its test fixtures now
use a local `Block` enum (`src/messages.rs`, `src/multimodal.rs`) that models only
what the tests need, plus explicit converters into canonical
`ChatInputPart`/`ToolResultPart`:

- `Block::Text` / `Block::Image` / `Block::ImageUrl` -> text or attachment parts.
- `Block::Thinking` / `Block::ToolUse` -> **dropped** from input projection; they
  have no ordinary-input representation. `assistant_msg` still maps them into
  `ChatOutput` items, which is where generated semantics belong.
- `Block::ToolResult` carries `id`, `name`, `is_error`, and inner blocks.

`tests/multimodal_test.rs` and `examples/vision_chat.rs` were migrated to
canonical parts (the example needed only an unused-import removal, since its
builder `.image()/.text()` methods still exist).

`querymt-py` and `querymt-cli` are migrated and compile successfully.

## Public API note

- `ChatReasoningItem::visible_text() -> String` remains the display/token-estimation
  accessor and excludes encrypted continuation and signatures.
- No public compatibility predicate or method returns or reconstructs legacy
  `Content` values.

## Task 8.2 canonical serialization closure

All QueryMT-owned message writers now emit only canonical payloads. The custom
`ChatMessage` serializer writes exactly one of `input` or `output`; persistence
writes canonical `MessagePart` variants; Extism and remote request DTOs inherit
that serializer; Python inputs normalize before crossing into Rust.

Legacy load-then-save coverage now verifies:

- old user text/image records save as canonical text/attachment input parts;
- mixed assistant reasoning/text/call/media records save as one canonical output;
- rich old tool results save as bounded canonical result parts;
- Extism and remote DTOs accept old message records and reserialize without
  `content`;
- Python legacy-shaped input dictionaries normalize and serialize canonically;
- a hand-written SQLite legacy flat-reasoning row reloads and saves with canonical
  reasoning content while retaining visible text, signature, and timing.

Provider API fields named `content` remain intentionally unchanged because they
are external wire schemas, not the removed QueryMT recursive message model.

Focused task 8.2 verification:

- `cargo test -p querymt chat::migration::tests --lib` → **6 passed**
- `cargo test -p querymt plugin::extism_impl::interface::item_aware_tests --lib` → **3 passed**
- `cargo test -p querymt-remote provider_protocol::item_aware_tests --lib` → **4 passed**
- `cargo test -p querymt-agent session::sqlite_storage::tests::legacy_reasoning_row_resaves_as_canonical_persistence --lib` → **1 passed**
- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo test -p querymt-py --lib` → **6 passed**

Task 8.2 is complete.

## Task 8.3 provider codec closure

All 17 provider crates compile their libraries and tests against canonical chat
inputs/outputs. Source audit finds request codecs matching `ChatInputPart` and
`ChatOutputItem` directly in OpenAI, Codex, xAI, Anthropic, Google, Ollama,
llama.cpp, MRS, and Kimi; compatibility providers delegate through those
canonical codecs. No provider imports or matches the removed QueryMT `Content`
type (the remaining bare `Content` names are provider-local wire enums or HTTP
headers).

Existing fixtures cover the required acceptance categories:

- text and inline/URI attachments across OpenAI Responses, Codex, xAI,
  Anthropic, Google, llama.cpp, and MRS;
- bounded rich tool-result text/media in OpenAI, Codex, Anthropic, Ollama,
  Google, llama.cpp, and xAI;
- canonical assistant output replay in OpenAI Responses, Codex, and xAI;
- explicit unsupported continuation/media errors in OpenAI Responses, Codex,
  xAI, MRS, and text-only llama.cpp paths.

Verification:

- one combined `cargo check --lib --tests` over all 17 `qmt-*` provider crates
  completed with 0 errors (warnings only).
- provider source search for legacy QueryMT content variants/imports found no
  matches.

Task 8.3 is complete.

## Task 8.4 non-provider consumer closure

Source audit confirms MCP conversion, agent model/history conversion, hooks,
verification, UI attachment handling, tools, CLI, and examples consume canonical
`ChatInputPart`, `ToolResultPart`, and `ChatOutput` values. Matches for `Content`
in the agent are ACP protocol `ContentBlock`/`Content`, ordinary prose, or HTTP
headers—not the removed QueryMT recursive enum. QueryMT examples and CLI contain
no legacy chat-content references.

Verification:

- `cargo check -p querymt --lib --examples` → 0 errors
- `cargo check -p querymt-cli` → 0 errors
- `cargo check -p querymt-agent --lib --tests --examples` → 0 errors
- `cargo test -p querymt-agent hooks:: --lib` → **26 passed, 1 ignored**
- `cargo test -p querymt-agent verification::service::tests --lib` → **17 passed**
- MCP adapter source uses bounded `ToolResultPart`; its attempted name-filtered
  test command selected no tests, while the containing `querymt` library suite
  remains covered by the 178-pass core run.

Task 8.4 is complete.

## Task 8.5 binding closure

The Python public API now exposes canonical message and part builders only:

- `user_message(input)` and `assistant_message(output)` emit exclusive canonical
  payload keys;
- `text_part`, `inline_attachment`, `url_attachment`, and `tool_result` construct
  canonical input, media, and bounded result shapes;
- `ChatResponse.output` exposes the lossless canonical output JSON;
- the public `ContentBlock` class, `response.content`, and separate image,
  image-URL, PDF, audio, resource-link, thinking, and tool-use builders were
  removed under the accepted Option B breaking change;
- old Python dictionaries using `content` remain read-compatible internally and
  normalize once, but are not produced or documented by the binding.

The README and all Python examples use canonical builders. Binding source search
finds no public legacy block class/builders; the sole old `image` tag is an
internal compatibility test fixture.

Verification:

- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo test -p querymt-py --lib` → **6 passed**
- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo check -p querymt-agent-py --lib --tests` → 0 errors
- `python3 -m compileall -q crates/py/querymt-py/examples` → success
- `git diff --check` → clean

Task 8.5 is complete.

## Task 8.7 old-history closure

A comprehensive unchanged legacy history fixture now covers text, inline image,
image URL, PDF, audio, resource link metadata, rich tool results, assistant
reasoning/signature, text, and function calls in one load/resave/reload flow. It
verifies canonical-only save shapes, bounded results, one authoritative assistant
output, semantic equality after canonical reload, exactly-once portable text/call
projection, and explicit removal of the hidden signature from lossy export.

This complements existing focused coverage for malformed MIME and non-object call
arguments, SQLite legacy-row reload/resave, structured persistence and second
request replay, raw invalid-argument provider replay, stale duplicate rejection,
cross-target continuation redaction, deduplication, and export redaction.

Verification:

- `cargo test -p querymt chat::migration::tests --lib` → **7 passed**
- `cargo test -p querymt-agent session::sqlite_storage::tests --lib` → **59 passed**
- `cargo test -p querymt-agent model::tests::output_part_ --lib` → **5 passed**
- `cargo test -p querymt-agent export::turns::tests::materialized_turns_are_lossy_projections_without_continuation_state --lib` → **1 passed**
- `cargo test -p qmt-openai responses_replays_raw_invalid_arguments_byte_exact --lib` → **1 passed**
- `cargo test -p qmt-openai responses_request_rejects_stale_portable_projection --lib` → **1 passed**

Task 8.7 is complete.

## Current focused verification

- `cargo test -p querymt --lib` → **178 passed, 0 failed**
- `cargo check -p qmt-openai --features native --lib --tests` → 0 errors
- `cargo check -p qmt-codex --features native --lib --tests` → 0 errors
- `cargo check -p qmt-xai --features native --lib --tests` → 0 errors
- `cargo check -p querymt-remote --lib --tests` → 0 errors
- `cargo check -p querymt-agent --lib --tests` → 0 errors
- `git diff --check` → clean
- Source search for removed projection/media helpers → no matches
- Source search for legacy content variants → matches only private
  `crates/querymt/src/chat/migration.rs`

## Task 8.8 final matrix (local portion)

Successful locally applicable checks:

- `cargo test -p querymt --lib` → **179 passed**
- `cargo test -p querymt-remote --lib --tests` → **82 passed**
- `cargo test -p querymt-agent --lib` → **2133 passed, 3 ignored**
- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo test -p querymt-py --lib` → **6 passed**
- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo test -p querymt-agent-py --lib` → **1 passed**
- `cargo check -p querymt --all-features --all-targets` → 0 errors
- `cargo check -p querymt-agent --all-targets` and `cargo check -p querymt-cli` → 0 errors
- `cargo test -p qmt-llama-cpp --lib --tests` → **65 + 5 + 6 passed**
- `cargo test -p qmt-mrs --lib` → **9 passed**, including the local model integration test
- `cargo test -p qmt-izwi --lib --tests` → **3 + 4 passed**
- HTTP provider tests with `--no-default-features --features native` passed through
  OpenAI (**91**), Codex (**56**), Anthropic (**30**), Google (**12**), Ollama
  (**15**), Kimi (**21**), OpenRouter (**10**), and the smaller compatibility
  crates before reaching the known xAI failures.

Expected/non-change failures observed:

- Running HTTP provider tests with default features on this native host attempts
  to link Extism guest imports (`alloc`, `input_load_u8`, `qmt_http_request`, etc.).
  This is a target-mode mismatch; native checks use `--no-default-features
  --features native`, while guest verification belongs on the WASM target.
- xAI native tests still have the same three pre-existing `parse_list_models_*`
  expectation failures caused by the additional `grok-4.5` model.
- `cargo check -p querymt-remote --example share_openai` reproduces the pre-existing
  feature-gating failure described below.

Task 8.8 remains open pending the externally run WASM/provider-guest matrix. All
other tasks are complete (59/60).

## Remaining verification limits

- `cargo check -p querymt-remote --example share_openai` still fails because
  `bootstrap_mesh_runtime` is feature-gated out and `ProviderShare::register_on_mesh`
  is unavailable in the default example build. The prior-session stash check
  established this as pre-existing at `HEAD`; `--lib --tests` succeeds.
- WASM targets were not rebuilt in this environment. Their generated/build output
  still requires external verification before task 8.8 can close.

## Pre-existing, unchanged by this work

- `cargo check -p querymt-mobile-ffi` has pre-existing compile errors
  (`RpcRequest` missing; `provider_lock`/`session_bridge` fields missing) that
  block any `--workspace` build. Unrelated to this change and untouched by it.

## Post-implementation simplification pass

The follow-up audit removed redundant core APIs and storage while preserving the
shipped transitional history and Extism wire formats:

- `{ content, output }` history decoding is isolated and documented as decode-only
  compatibility; load/save tests verify canonical reserialization and stale
  projection rejection.
- stream completion tracks completed indexes without cloning every completed item,
  and `finish` moves owned item state into the result.
- unused display/projection wrappers, dead construction/accumulator errors, and the
  ambiguous allocating `input_parts` compatibility alias were removed. Workspace
  callers now choose borrowed `input()` or explicit `portable_input_parts()`.
- message role/payload validation is shared by construction, replacement, and
  deserialization. Existing `From<ChatOutput>` conveniences remain supported.
- Extism response/chunk values keep one in-memory authority while custom serde
  preserves the released flattened fields until the contract version changes.

Legacy stream accumulation remains intentionally supported: OpenAI Chat
Completions, Anthropic, Codex, Google, llama.cpp, MRS, and compatibility providers
still emit legacy `StreamChunk` variants. Deleting dual-mode accumulation requires
a coordinated provider migration and is not safe as an isolated core cleanup.

Verification:

- `cargo test -p querymt --lib --no-default-features` -> **112 passed**
- `cargo test -p querymt --lib plugin::extism_impl:: --features extism_host --no-default-features` -> **14 passed**
- `cargo check -p querymt --all-features --all-targets` -> 0 errors
- `cargo check -p querymt-agent --all-targets` -> 0 errors (existing warnings)
- `cargo check -p querymt-remote --all-targets --features kameo-mesh` -> 0 errors
- `PYO3_PYTHON=/opt/homebrew/bin/python3 cargo check -p querymt-py --lib` -> 0 errors
- affected provider `--all-targets` check (OpenAI, Anthropic, Google, Codex,
  Ollama, xAI, Kimi, llama.cpp, MRS) -> 0 errors (existing warnings)
- `cargo fmt --all -- --check` and `git diff --check` -> clean

A final redundancy pass then:

- corrected hook token estimation to count one authoritative payload instead of
  adding structured output and its portable projection;
- replaced hook/OpenAI input projection allocations with borrowed canonical input;
- collapsed OpenAI Responses normalization to return `ChatOutput` directly and
  removed duplicate owned/Cow output DTOs;
- normalized remote responses into one in-memory `ChatOutput` while preserving the
  released flattened response fields through custom serialization;
- removed the unnecessary Extism chunk decode DTO; and
- centralized item-aware contract detection/versioning in `querymt::chat`, with
  transport modules retaining their existing re-exported names.

Focused verification:

- hook token-estimation regression tests -> **2 passed**
- OpenAI native library suite -> **93 passed**
- remote item-aware protocol suite -> **4 passed**
- Extism-focused suite -> **14 passed**
- QueryMT, agent, remote, and OpenAI all-target checks -> 0 errors (existing warnings)
