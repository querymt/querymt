## 1. Core output contract

- [x] 1.1 Add provider-neutral ordered chat output/item/part types, status, provenance, and opaque extensions; verify serde fixtures preserve interleaving, annotations, refusal, phase, unknown fields, and encrypted-only reasoning.
- [x] 1.2 Add raw-argument function items with separate item/call IDs and fallible parsing helpers; verify invalid JSON remains byte-exact and cannot become an executable empty-object call.
- [x] 1.3 Add the defaulted structured ChatResponse accessor and non-recursive normalization/projection helpers; verify a legacy mock provider still works through dyn LLMProvider without new trait implementations.
- [x] 1.4 Add optional structured output to ChatMessage and update response conversions, constructors, and workspace literals; verify old JSON histories decode and structured output projects/replays only once.
- [x] 1.5 Add role/projection consistency validation and explicit replace/clear helpers; verify stale projected edits fail while deliberate portable edits succeed.
- [x] 1.6 Add optional function strictness without changing legacy serialization defaults; verify omitted strictness remains omitted for existing Chat Completions tools.

- [x] 1.7 Add the mime workspace/core dependency and QueryMT MediaType wrapper with fallible parsing, parsed accessors, string serde/schema/bindings, and semantic comparison; test parameter roundtrips, invalid syntax, and wildcard rejection without relying on dependency serialization.
- [x] 1.8 Add reusable typed media kinds/sources and normalization helpers for output and tool results; test required inline MIME, data URL defaults/conflicts, optional URL/file-reference MIME, filename/detail preservation, and legacy loading versus explicit normalization failures. Verify no implicit fetch or byte sniffing.
- [x] 1.9 Add typed display projections and a generic attachment fallback contract; test valid unfamiliar MIME values within an existing kind and explicit later recognition of opaque fixtures without duplicate canonical items or execution.

## 2. Streaming contract and accumulation

- [x] 2.1 Extend StreamChunk with structured metadata, indexed lifecycle/delta events, and complete item snapshots; migrate exhaustive matches and verify event serde round trips.
- [x] 2.2 Implement structured-versus-legacy accumulation and compatibility projection; verify mixed event representations yield one canonical text/call sequence and no duplicate usage.
- [x] 2.3 Implement authoritative snapshot reconciliation and conflict detection; verify repeated identical completions are idempotent and conflicting identities fail.
- [x] 2.4 Wire terminal validation and attempt-local reset into accumulation; verify premature EOF, incomplete output, failure, and retry fixtures never dispatch unfinished calls or mix attempts.

## 3. Transport contracts

- [x] 3.1 Extend Extism request/response DTOs and generated adapters with structured history/events; verify lossless serialized plugin round trips and legacy payload defaults.
- [x] 3.2 Extend remote-provider DTOs and forwarding paths with canonical output; verify a streamed and non-streamed remote fixture retain equivalent items.
- [x] 3.3 Introduce item-aware contract version advertisement and pre-generation peer checks; verify older peers reject structured operation explicitly while legacy operations still work.
- [x] 3.4 Update native plugin compatibility checks and release metadata for the changed Rust contracts; verify mismatched plugins are rejected under the supported loading contract.
- [x] 3.5 Confirm the HTTP adapter/parser framing boundary and add split UTF-8/JSON/SSE integration fixtures; verify arbitrary transport chunking yields the same semantic events without duplicate framing layers.

## 4. Agent history and projections

- [x] 4.1 Carry canonical output through LlmResponse and both execution paths using the shared accumulator; verify ordered items survive transition_call_llm without flattening.
- [x] 4.2 Add a canonical-output message part and update assistant persistence/reload conversion; verify SQLite reload preserves encrypted-only reasoning, unknown items, and exact raw arguments without duplicate projected parts.
- [x] 4.3 Gate local function execution on successful response validation and deduplicate by call identity; verify a mixed structured/legacy stream dispatches each valid call once and opaque actions never enter the executor.
- [x] 4.4 Add provenance-aware target projection including endpoint identity and consistent call/result mapping; verify model/provider/endpoint switches do not leak opaque state or mutate stored originals.
- [x] 4.5 Update hooks, chains, history edits, pruning, and compaction to maintain or intentionally clear structured authority; verify edits cannot resurrect stale payloads and compaction leaves no dangling call/result dependencies.
- [x] 4.6 Update bindings and legacy export projections; verify structured binding serialization is lossless and portable exports are documented/tested as lossy.
- [x] 4.7 Redact continuation state from Debug/display, telemetry, and ordinary exports while preserving authorized storage; verify sentinel opaque secrets are absent from captured logs but present after storage reload.
- [x] 4.8 Add A -> B -> A replay tests including intervening B turns, persistence/reload, and edited/compacted A dependency groups; verify native A state is retained but never sent to B, B turns project portably on return, and excluded A state is never resurrected.
- [x] 4.9 Verify MIME strings, parameters, media sources, filename/detail, and provider-reference scope survive storage, bindings, Extism, and remote roundtrips; verify unresolved references do not imply available renderable bytes.

## 5. OpenAI Responses codec

- [x] 5.1 Add explicit OpenAI API mode configuration with Chat Completions as the default; verify config schema and request snapshots preserve existing behavior and select Responses only when requested.
- [x] 5.2 Add Responses wire types and input-valid ordered replay conversion in qmt-openai; verify mixed reasoning/message/call fixtures retain ordering and unsafe unknown continuation fails rather than silently dropping.
- [x] 5.3 Implement stateless request construction and reserved extra-body conflict checks; verify store=false, encrypted reasoning include, no previous-response/conversation references, and errors for retention overrides.
- [x] 5.4 Map instructions, token limit, text.format, reasoning effort, and supported sampling controls; verify request snapshots and explicit unsupported-parameter errors.
- [x] 5.5 Implement flattened function definitions, named tool choice, explicit non-strict default, and strict-schema validation; verify optional properties are never silently changed and incompatible strict schemas fail.
- [x] 5.6 Serialize rich function outputs using call_id and ordered text/image/file parts; verify distinct item/call IDs and unsupported-media failures without placeholders.
- [x] 5.7 Implement non-streaming response normalization, status/error mapping, and exclusive usage accounting; verify annotated/refusal output, multiple calls, encrypted reasoning, incomplete causes, and provider failures.
- [x] 5.8 Implement request-local semantic Responses SSE parsing with complete item snapshots; verify response.completed, failed/incomplete events, framing-only DONE markers, duplicate snapshots, and premature EOF against the streaming spec.

- [x] 5.9 Validate media support using parsed MIME values and source/content-position rules; test rich image/file metadata preservation, unsupported endpoint formats, and cross-origin provider-reference rejection.
- [x] 5.10 Add unsupported built-in media output fixtures such as image_generation_call; verify opaque retention, no fabricated typed message media or local execution, and explicit unsupported required replay errors.

## 6. Provider reuse and acceptance

- [x] 6.1 Reuse the codec in Codex with provider-owned authentication, instructions, streaming requirements, and error policy; verify its existing fixtures plus item-preserving replay tests pass.
- [x] 6.2 Reuse compatible codec behavior in xAI without importing OpenAI-only options; verify existing endpoint/header/model behavior and rich tool-output fixtures pass.
- [x] 6.3 Add end-to-end native, Extism, and remote tests for response -> persist -> reload -> tool result -> second request; verify exact raw arguments, order, reasoning continuation, IDs, and absence of duplicates.
- [x] 6.4 Run focused core/provider/agent/binding tests and the applicable workspace feature/build matrix; record successful commands and any environment-limited checks, including legacy Chat Completions regressions.
- [x] 6.5 Document opt-in configuration, source/plugin compatibility changes, portable downgrade workflow, and excluded stateful/built-in-tool features; verify examples use existing chat methods and no new provider supertrait.
