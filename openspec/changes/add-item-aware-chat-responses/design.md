## Context

See proposal.md for motivation and scope. The relevant existing boundaries are:

- `crates/querymt/src/chat/mod.rs:736`: ChatResponse exposes flattened accessors; conversion at line 746 reconstructs thinking, text, and calls and replaces invalid arguments with an empty object.
- `crates/querymt/src/chat/mod.rs:798`: StreamChunk has no message/item boundaries for text or reasoning. Existing request-side provider composition already provides the required transport abstraction, while response return types can be migrated independently.
- `crates/querymt/src/chat/http.rs:17`: HTTPChatProvider separates request construction from parsing and needs no new protocol-specific supertrait.
- `crates/providers/openai/src/lib.rs:181`: OpenAI delegates to Chat Completions request and parsing helpers.
- `crates/providers/codex/src/api.rs:531` and `crates/providers/xai/src/lib.rs:581`: existing Responses converters collect message content before tool items, losing interleaving. Codex streaming at line 818 handles several semantic events but flattens reasoning.
- `crates/agent/src/middleware/state.rs:277` and `crates/agent/src/agent/execution/transitions.rs:912`: execution stores flattened fields and reconstructs assistant parts.
- `crates/agent/src/model.rs:472`: provider/model provenance already gates thinking signatures. Message parts are stored as JSON by `crates/agent/src/session/sqlite_storage/session_store.rs:294`.

Protocol reference: https://developers.openai.com/api/docs/guides/function-calling?api-mode=responses

## Goals / Non-Goals

**Goals:** Canonical input and output models, testable stateless tool-turn replay, one authoritative assistant payload, and explicit compatibility boundaries. Portable and UI projections remain available as adapters but never become stored or streamed second sources of truth. Parallel legacy content/media representations are removed from the primary API.

**Non-Goals:** New provider hierarchies, remote session state, automatic protocol selection, arbitrary built-in/custom-tool execution, or byte-identical JSON envelopes. Raw function argument strings must be exact; other preserved JSON data is semantically lossless, not whitespace/key-order identical.

## Decisions

### 1. Return canonical output directly

Keep `LLMProvider` composition and request-side chat abstractions, but change chat generation results to `ChatOutput`. Text, visible reasoning, supported local calls, usage, and finish reason are inherent projection methods on `ChatOutput`; providers do not independently implement flattened accessors that can disagree with canonical items. Providers without native item semantics adapt their wire response into a limited `ChatOutput` at the provider boundary.

This is an intentional Rust source break. The crate's small user base makes migration cheaper now than preserving an object-safe legacy response trait that allocates projections, duplicates behavior, and permits inconsistent results. A temporary compatibility adapter may exist outside the primary API, but it must not shape provider contracts or canonical storage.

Alternative rejected: adding a defaulted structured accessor to the flattened response trait preserves source compatibility but creates two authorities and makes transport capability detection depend on whether callers happened to expose the optional accessor. `ResponseProvider` plus another request API is also rejected because it duplicates a generation capability and spreads protocol-specific trait obligations.

### 2. Provider-neutral ordered output

Add chat output types, preferably in a focused submodule of `querymt::chat`. Names are provisional, behavior is normative:

- ChatOutput: response ID, ordered items, response status, usage, finish reason, provenance, and versioned provider extensions.
- Message item: item ID, role, optional phase/status, ordered parts containing text with annotations, refusal, and supported media; preserve unknown parts.
- Reasoning item: item ID, separate summary/content parts, optional encrypted continuation and signature. An empty visible summary does not make the item empty.
- Function-call item: separate item ID and call ID, name, raw arguments, status, extensions. Parse arguments on demand rather than store an independently mutable parsed copy.
- Opaque item: original type and semantic JSON payload scoped to its originating protocol. It is never implicitly executable.

Response items describe generated output. Reasoning and function calls are output-only concepts; they are not also general input-content variants. Provider wire types stay outside querymt core.

Add a focused input model:

- `ChatInputPart`: text, validated attachment, or correlated tool result.
- `ToolResult`: call ID, optional function name, error state, and ordered result parts.
- `ToolResultPart`: text or validated attachment; it is deliberately nonrecursive and cannot contain reasoning, function calls, or nested results.

Provider request codecs consume the canonical message payload directly. They map output items for native replay and input parts for user/tool input instead of matching a shared recursive enum containing both generated and supplied semantics.

Alternative rejected: retaining `Content::{Thinking, ToolUse, ToolResult, Image, ImageUrl, Pdf, Audio, ResourceLink}` keeps output and input semantics mixed, preserves invalid MIME strings, and forces every provider and binding to support two media models. Adding only fields to ToolCall or overloading Thinking.signature cannot preserve multiple reasoning items, message boundaries, and unknown siblings.

### 2a. Validated structured values and crate-backed media types

Use the `mime` crate behind a QueryMT-owned `MediaType(mime::Mime)` newtype, not an ad hoc MIME parser or an unrestricted public string. The wrapper owns fallible parsing, Display/AsRef<str>, parsed type/subtype/suffix/parameter access, equality, string-based serde, string JSON schema, and binding conversions. Preserve MIME parameter semantics; normalized spelling is permitted and byte-identical MIME strings are not required. Reject wildcards for concrete attachment types. The crate validates syntax, not endpoint support or the actual contents of bytes.

Use one reusable attachment type with a broad kind (image, audio, video, document, other), optional MediaType, a typed source (inline bytes, data URL, general URI, or provider file reference), optional filename/description/detail, and scoped provider extensions. The final public name should communicate that it also represents non-media resource links; `AttachmentPart` is preferred over `MediaPart`. Reuse it for ordinary input, tool-result parts, and normalized output rather than inventing a variant per format or context. Expose known attachments to renderers as typed parts; a generic attachment fallback handles valid formats without a specialized renderer. Adding another format within an existing kind requires codec/capability and possibly renderer changes, not another core enum variant. A genuinely new source or modality can still require schema/version and binding updates.

Keep invariant-bearing fields private and use constructors/builders or validated mutation. Implement custom deserialization through the same validation path so serde cannot construct states rejected by ordinary APIs. Apply this rule at least to media source/type combinations and any structured value whose fields have cross-field identity or role invariants.

Inline bytes require a MediaType. Data URLs use their parsed declared media type (or the standard default when omitted); a separately supplied type must agree, including parameter semantics. General URIs and provider file references may omit MIME metadata when unavailable. A URI source is not assumed to be HTTP and preserves resource-link metadata. Do not fetch resources or sniff bytes implicitly. Preserve source form, optional filename/description/detail, and MIME parameters across storage and transport. Provider file references remain origin-scoped and do not become portable URIs; an unresolved reference is not a promise of immediately renderable bytes. Treat MIME declarations as untrusted metadata, not authorization to execute content.

Known typed media with a valid but endpoint-unsupported MIME type remains storable and displayable as an attachment; sending it returns an explicit unsupported-content error. Unknown item/part semantics remain opaque even if their JSON happens to contain MIME or base64 fields. Unsupported built-in outputs such as image_generation_call are not automatically promoted to message media. A future explicit codec can derive a typed display projection from a stored opaque item while retaining its original identity, order, and replay payload; this does not introduce duplicate canonical items or local execution.

Apply validated attachments throughout the canonical input/output path. Move the old recursive `Content` shape into a private migration module rather than keeping it public. The migration decoder converts legacy image/audio MIME strings, infers `application/pdf` from the old PDF variant, preserves resource-link URI metadata, and maps tool results to the bounded canonical result model. Migration is role-aware: user turns become canonical input, while the entirety of each assistant turn becomes one `ChatOutput`, including message items for legacy assistant text/attachments and reasoning or function-call items for generated semantics. Invalid legacy metadata produces a path-specific normalization error rather than an invalid attachment. New serializers never emit legacy `Content`. After all callers migrate, delete bidirectional public conversion helpers such as legacy-media normalization and attachment-to-legacy-content projection; explicit legacy export belongs in a boundary adapter.

### 3. ChatMessage has an exclusive authoritative payload

Represent a turn payload as an exclusive form, conceptually `Input(Vec<ChatInputPart>) | Output(ChatOutput)`, with role and cache metadata outside it. User and tool-result turns use canonical input parts. An assistant output can contain several protocol items. Display and cross-origin portable input parts are computed from output when needed, not retained as a second public field.

Keep payload fields private and provide focused constructors, accessors, replacement, and explicit `into_portable`/projection operations. Constructors enforce role/payload constraints. History editors and compaction replace the authoritative payload; they cannot edit a projection while stale continuation remains hidden. Custom deserialization accepts old `{ role, content }` records and the transitional `{ role, content, output }` shape through private DTOs, validates any duplicate representation once at the migration boundary, and normalizes it into the exclusive in-memory form. New serialization writes one canonical authority.

Alternative rejected: public `content` plus optional `output` models an invariant that ordinary mutation can violate and moves failures to request time. Retaining the recursive public Content enum would continue mixing input and generated semantics. A sidecar detached from a turn makes persistence, reorder, and edit association fragile.

### 4. One canonical structured stream and explicit adapters

Use structured response metadata, indexed item lifecycle/content delta events, completed snapshots, and one semantic response terminal as the canonical stream. Include content/summary indexes where applicable, not only tool indexes. Providers emit only canonical events. A separate compatibility adapter projects text, visible reasoning, usage, and completed local calls for legacy UI consumers; those projections are never fed back into canonical accumulation.

The accumulator consumes canonical events by value and `finish(self)` returns an explicit completed, incomplete, or failed outcome carrying the available `ChatOutput` and terminal detail. Consuming finalization prevents post-terminal mutation. Completed item snapshots replace provisional state; repeated consistent completions are idempotent and conflicting ones are errors. Usage is normalized once rather than summed across deltas and snapshots.

Provider parsers own request-local protocol state. Decode SSE across arbitrary byte/UTF-8/frame boundaries using the existing framing layer where available; verify that boundary before adding any second framing layer. Reconcile the final response snapshot before deriving terminal meaning, including whether final calls require tool execution. Failed/incomplete terminals and premature EOF are not successful completion; preserve partial output/status without dispatching unfinished calls. Item completion alone does not authorize execution before response-level validation. Retries reset attempt-local accumulation.

Local, Extism, and remote transports treat the canonical response terminal as terminal for acknowledgement, buffering, phase changes, and receiver shutdown. They must not depend on a projected legacy `Done` event.

Alternative rejected: dual canonical/legacy emissions require mode detection and allow duplicate history or dispatch. A separate response stream would duplicate APIs; only a final output snapshot would preserve replay but leave live item indexing unavailable.

### 5. Persistence and replay are protocol-aware

Carry canonical input parts and output through Extism DTOs, remote providers, and agent LlmResponse/history. Store one authoritative payload so existing JSON storage can retain it without parallel display parts. Bindings expose canonical input, attachment, result, and output types; legacy exports explicitly project at the boundary and do not claim lossless roundtrip. MCP adapters, provider codecs, agent token estimation/pruning, UI attachment handling, examples, and language bindings migrate away from matching legacy variants.

Provenance includes provider, protocol, model, and endpoint identity (no credentials). Same-provider/protocol/model/endpoint replay retains compatible opaque state. On a model or endpoint switch, default to portable projection; a future explicit compatibility rule can relax this. Do not mutate stored originals when projecting to another target. Call-ID remapping, if required by a target, must apply consistently to calls and results.

Switching from origin A to B and back to exactly compatible A is a per-turn projection decision, not a destructive session conversion. Portable requests to B exclude A's opaque state; that state remains only in A's stored canonical turns. On return to A, unchanged A turns use their original validated native representation and B turns use portable projections. Retention makes native replay possible, not universally valid: protocol dependency checks still apply, and edited/compacted groups or unsupported required continuation cannot be restored merely because the target matches. Switching back must never resurrect excluded history.

Persistence retains unknown fields/items; replay is NOT blind raw-output passthrough. The codec preserves input-valid fields and validates unknown items against supported replay rules. If an opaque item is required for continuation but cannot safely be serialized, fail explicitly instead of silently dropping it. Compaction replaces complete dependency groups (reasoning/calls/results) with a portable summary, clearing affected opaque continuation. No hidden raw sidecar may bypass a user edit or compaction.

Older JSON payloads decode through custom migration into the canonical model. Old peers do not automatically understand new variants: advertise a versioned item-aware transport capability and reject operations whose concrete ordering, identity, opaque, or continuation semantics require it. Do not require that capability merely because an in-memory result uses `ChatOutput`; a basic output that projects losslessly can use an explicit legacy transport adapter. Native Rust plugins require coordinated rebuild/version checks; serde defaults are not ABI compatibility. Sensitive opaque state must be redacted from Debug/display, telemetry, and ordinary exports while retained in authorized history storage.

### 6. Explicit Responses mode and reusable codec

Add OpenAI config selection `api_mode = "chat_completions" | "responses"`; omission retains Chat Completions. Both use existing HTTP provider methods. No fallback after sending a Responses request. Add a codec module in qmt-openai for Responses wire types and mapping; xAI already depends on qmt-openai, and provider-specific wrappers keep authentication, headers, instructions, supported fields, error classification, and backend restrictions. Confirm dependency direction before reuse; no new protocol crate is required by this plan.

Responses requests use store=false, request encrypted reasoning via include where supported, and replay local items rather than previous_response_id or conversation IDs. Reject conflicting extra-body values for protocol-owned fields rather than allowing duplicate keys or changing retention/continuation mode. Map max_tokens to max_output_tokens, JSON schema to text.format, reasoning effort to reasoning.effort, and system config to instructions. Exclude unsupported sampling fields explicitly instead of inheriting Codex/xAI quirks.

Function definitions use the Responses flattened shape. Add optional strictness to the shared function definition: legacy Chat Completions omission stays omitted, while Responses omission is serialized as false to preserve QueryMT's existing non-strict intent. Explicit strict=true requires a compatible schema; validate and reject incompatible schemas rather than silently making optional fields required. Named tool choice is protocol-specific. Function outputs correlate using call_id, preserve rich part order, support text/images/files where valid, and reject unsupported media instead of placeholders or silent omission.

Reasoning summaries and encrypted content remain separate. Preserve refusal and annotation data. Capture unknown sibling fields on recognized wire items/parts into scoped extensions and retain complete unknown item/part objects as opaque payloads; do not rely on a literal wire field named `raw`. Normalize usage to QueryMT's exclusive token categories to avoid double-counting cached/reasoning tokens. Reconcile final output snapshots before classifying completed responses: any supported local call discovered there yields ToolCalls; otherwise Stop. Incomplete max-output-token/content-filter responses and provider failures retain available partial output plus terminal cause or classified error. Unsupported provider actions requiring local participation fail explicitly and never enter the function executor.

## Risks / Trade-offs

- Canonical return types, removal of public `Content`, private payloads, and canonical stream events break Rust source usage -> migrate all provider codecs, agents, adapters, bindings, examples, and callers in one release; document the major impact and provide focused boundary projection adapters.
- Old and transitional persisted representations may disagree -> validate once during custom deserialization, reject ambiguous stale records, and serialize only one authority thereafter.
- Larger serialized histories -> keep opaque data scoped, avoid duplicate canonical payloads, and test storage/transport limits; do not truncate continuation silently.
- Opaque state can leak or cross endpoints -> redact logs and gate replay on conservative provenance checks.
- Shared codec can accidentally unify incompatible backends -> provider-owned policy hooks and separate OpenAI/Codex/xAI fixture suites.
- Protocol evolution introduces unknown types -> preserve on disk, reject unsupported required replay/actions, and never execute arbitrary opaque items.

## Migration Plan

1. Land canonical `ChatInputPart`, `ToolResult`, attachment, `ChatOutput`, and exclusive message payload types with private legacy-history DTOs.
2. Migrate every provider request codec, MCP adapter, agent/history path, token estimator, binding, plugin/remote DTO, example, and workspace caller to canonical input/output types; serialize only the canonical shape and remove public legacy conversions.
3. Land canonical structured streaming, explicit legacy/UI adapters, consuming outcomes, and local/remote terminal recognition before enabling item-aware providers.
4. Implement opt-in OpenAI Responses with lossless unknown-field capture, final-snapshot-first terminal classification, and partial failure outcomes; migrate reusable Codex/xAI codec behavior with parity fixtures.
5. Delete the public recursive `Content` API and superseded normalization/projection helpers, then run old-history migration, native/plugin/remote, provider, binding, and second-request replay acceptance tests. Release as a documented Rust/native-plugin API break with persisted-data migration support.

Rollback means disabling Responses mode and selecting an explicit portable-history projection for existing sessions, not deleting canonical history. Older binaries that cannot read the new canonical message shape require a version guard or an explicit lossy export; they are not promised source, ABI, or transparent downgrade compatibility.
