## 1. Core output contract

- [x] 1.1 Add provider-neutral ordered chat output/item/part types, status, provenance, and opaque extensions; verify serde fixtures preserve interleaving, annotations, refusal, phase, unknown fields, and encrypted-only reasoning.
- [x] 1.2 Add raw-argument function items with separate item/call IDs and fallible parsing helpers; verify invalid JSON remains byte-exact and cannot become an executable empty-object call.
- [x] 1.3 Add the defaulted structured ChatResponse accessor and non-recursive normalization/projection helpers; verify a legacy mock provider still works through dyn LLMProvider without new trait implementations. Superseded by task 7.1.
- [x] 1.4 Add optional structured output to ChatMessage and update response conversions, constructors, and workspace literals; verify old JSON histories decode and structured output projects/replays only once. Superseded by task 7.2.
- [x] 1.5 Add role/projection consistency validation and explicit replace/clear helpers; verify stale projected edits fail while deliberate portable edits succeed. Superseded by task 7.2.
- [x] 1.6 Add optional function strictness without changing legacy serialization defaults; verify omitted strictness remains omitted for existing Chat Completions tools.

- [x] 1.7 Add the mime workspace/core dependency and QueryMT MediaType wrapper with fallible parsing, parsed accessors, string serde/schema/bindings, and semantic comparison; test parameter roundtrips, invalid syntax, and wildcard rejection without relying on dependency serialization.
- [x] 1.8 Add reusable typed media kinds/sources and normalization helpers for output and tool results; test required inline MIME, data URL defaults/conflicts, optional URL/file-reference MIME, filename/detail preservation, and legacy loading versus explicit normalization failures. Verify no implicit fetch or byte sniffing.
- [x] 1.9 Add typed display projections and a generic attachment fallback contract; test valid unfamiliar MIME values within an existing kind and explicit later recognition of opaque fixtures without duplicate canonical items or execution.

## 2. Streaming contract and accumulation

- [x] 2.1 Extend StreamChunk with structured metadata, indexed lifecycle/delta events, and complete item snapshots; migrate exhaustive matches and verify event serde round trips.
- [x] 2.2 Implement structured-versus-legacy accumulation and compatibility projection; verify mixed event representations yield one canonical text/call sequence and no duplicate usage. Superseded by task 7.4.
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

## 7. Canonical API migration and review remediation

- [x] 7.1 Replace flattened `ChatResponse` generation results with direct canonical `ChatOutput` results across core traits, HTTP adapters, providers, plugins, remote clients, bindings, and agents; verify all workspace providers compile and projection tests cover text, reasoning, calls, usage, and finish reason.
- [x] 7.2 Add private `Input(Vec<ChatInputPart>) | Output(ChatOutput)` message payloads with role-aware constructors and explicit portable conversion; verify stale transitional duplicate records fail and callers cannot independently mutate projected input and structured output.
- [x] 7.3 Add canonical text, unified attachment, correlated `ToolResult`, and bounded nonrecursive `ToolResultPart` input types; use typed general URI and provider-file sources plus filename/description/detail metadata, and verify generated reasoning, function calls, and nested tool results cannot be constructed as ordinary input.
- [x] 7.4 Consolidate provider streaming on canonical structured events, add an explicit legacy/UI projection adapter, and make accumulator finalization consuming with completed/incomplete/failed outcomes carrying available output; verify no provider emits dual canonical and compatibility events.
- [x] 7.5 Treat canonical structured response terminals as terminal throughout remote relay acknowledgement, buffering, lifecycle, replay, and receiver shutdown; verify a remote stream containing no legacy `Done` closes promptly and reaches its completed or failed phase.
- [x] 7.6 Capture unknown sibling fields on recognized Responses items/parts and complete payloads for unknown parts; verify non-streaming and streaming fixtures round-trip new known-item fields and arbitrary unknown content objects without null substitution or execution.
- [x] 7.7 Reconcile final Responses snapshots before deriving finish reason; verify a function call appearing only in the final snapshot yields `ToolCalls` and is dispatched exactly once after successful response-level validation.
- [x] 7.8 Return partial canonical output with incomplete and failed streaming/non-streaming outcomes while retaining classified causes; verify partial items remain inspectable and unfinished calls are never executable.
- [x] 7.9 Replace transport checks based on `output.is_some()` or a public representation marker with checks for concrete item-aware fidelity requirements; verify normalized portable output can cross a legacy adapter while identities, ordering, opaque state, and continuation require version negotiation.
- [x] 7.10 Make invariant-bearing canonical input/output values constructor-driven and validate deserialization through the same path; verify inline attachments without MIME and conflicting data-URL MIME fail equally through direct construction, mutation, and serde.
- [x] 7.11 Remediate public API review findings: keep portable assistant turns valid and correlated, preserve ordered assistant builder media/calls, make MIME failure explicit, provide borrowed canonical accessors and validated execution projections, remove the caller-set representation marker, and verify cross-origin replay strips continuation without flattening calls.
- [x] 7.12 Gate local chain execution on completed responses, normalize complete legacy agent assistant turns into one output, enforce provenance in direct Responses request builders, and reject unsupported Chat Completions attachment kinds instead of emitting placeholders.

## 8. Legacy content removal and workspace cleanup

- [x] 8.1 First migrate the tool-result boundary (`LLMProvider::call_tool`, agent execution state, and post-tool hooks) to canonical bounded `ToolResultPart` values; then move the old recursive `Content` representation into private migration/export DTOs and implement path- and role-aware normalization for every legacy text, image, image-URL, PDF, audio, resource-link, thinking, tool-use, and tool-result variant. Verify user turns become canonical input, mixed assistant turns become one `ChatOutput`, unchanged old fixtures load successfully, and malformed MIME/source metadata returns explicit errors. Public `Content` removal remains deferred to 8.6.
- [x] 8.2 Serialize only canonical message payloads, input parts, attachments, tool results, and output items across persistence, Extism, remote DTOs, and bindings; verify loading then saving every legacy fixture removes the old representation without losing supported semantics.
- [x] 8.3 Migrate OpenAI, Codex, xAI, Anthropic, Google, Ollama, llama.cpp, MRS, and other provider request codecs to consume canonical input parts and output items directly; verify provider fixtures cover text, inline/URI attachments, rich tool results, native assistant replay, and explicit unsupported-content errors.
- [x] 8.4 Migrate MCP conversion, agent model/history conversion, pruning and token estimation, hooks, verification, UI attachment handling, tool implementations, CLI paths, and examples away from legacy variant matches; verify focused crate tests and examples compile against only canonical public types.
- [x] 8.5 Migrate Python and other language bindings plus public schemas/builders to canonical input, attachment, result, and output types; verify round-trip binding tests expose no legacy image/PDF/audio/thinking/tool-use content variants.
- [x] 8.6 Delete the public recursive `Content` enum, its builders and manual equality/display implementations, public legacy media normalization/projection helpers, and superseded tests; verify repository source search finds legacy names only in private migration/export modules and migration fixtures.
- [x] 8.7 Add end-to-end old-history tests covering all legacy variants, assistant reasoning/calls, rich and invalid tool results, persistence reload, canonical save, provider replay, and explicit lossy export; verify no duplicate history or hidden continuation is introduced.
- [ ] 8.8 Run focused core, all provider, remote actor, Extism, MCP, agent persistence/pruning, binding, example, and old-history migration suites plus the applicable workspace build matrix; record commands and verify all review regressions and legacy Chat Completions wire behavior.
