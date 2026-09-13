## Context

See proposal.md for motivation and scope. The relevant existing boundaries are:

- `crates/querymt/src/chat/mod.rs:736`: ChatResponse exposes flattened accessors; conversion at line 746 reconstructs thinking, text, and calls and replaces invalid arguments with an empty object.
- `crates/querymt/src/chat/mod.rs:798`: StreamChunk has no message/item boundaries for text or reasoning. Existing generation signatures at line 874 already provide the required transport abstraction.
- `crates/querymt/src/chat/http.rs:17`: HTTPChatProvider separates request construction from parsing and needs no new protocol-specific supertrait.
- `crates/providers/openai/src/lib.rs:181`: OpenAI delegates to Chat Completions request and parsing helpers.
- `crates/providers/codex/src/api.rs:531` and `crates/providers/xai/src/lib.rs:581`: existing Responses converters collect message content before tool items, losing interleaving. Codex streaming at line 818 handles several semantic events but flattens reasoning.
- `crates/agent/src/middleware/state.rs:277` and `crates/agent/src/agent/execution/transitions.rs:912`: execution stores flattened fields and reconstructs assistant parts.
- `crates/agent/src/model.rs:472`: provider/model provenance already gates thinking signatures. Message parts are stored as JSON by `crates/agent/src/session/sqlite_storage/session_store.rs:294`.

Protocol reference: https://developers.openai.com/api/docs/guides/function-calling?api-mode=responses

## Goals / Non-Goals

**Goals:** A lossless structured path within existing chat interfaces, testable stateless tool-turn replay, and explicit compatibility boundaries. Legacy projections remain useful but never become a second source of truth.

**Non-Goals:** New provider hierarchies, remote session state, automatic protocol selection, arbitrary built-in/custom-tool execution, or byte-identical JSON envelopes. Raw function argument strings must be exact; other preserved JSON data is semantically lossless, not whitespace/key-order identical.

## Decisions

### 1. Evolve data contracts, not generation traits

Keep LLMProvider composition and ChatProvider/HTTPChatProvider generation signatures unchanged. Add a defaulted object-safe `ChatResponse::output() -> Option<&ChatOutput>` accessor returning None for legacy implementations. Item-aware concrete responses implement existing accessors as projections of their output. A normalization helper synthesizes a marked legacy projection when structured output is absent; do not use mutually recursive default accessors.

Alternative rejected: ResponseProvider plus another request/stream API would duplicate a generation capability and spread protocol-specific trait obligations across providers and adapters.

### 2. Provider-neutral ordered output

Add chat output types, preferably in a focused submodule of `querymt::chat`. Names are provisional, behavior is normative:

- ChatOutput: response ID, ordered items, response status, usage, finish reason, provenance, and versioned provider extensions.
- Message item: item ID, role, optional phase/status, ordered parts containing text with annotations, refusal, and supported media; preserve unknown parts.
- Reasoning item: item ID, separate summary/content parts, optional encrypted continuation and signature. An empty visible summary does not make the item empty.
- Function-call item: separate item ID and call ID, name, raw arguments, status, extensions. Parse arguments on demand rather than store an independently mutable parsed copy.
- Opaque item: original type and semantic JSON payload scoped to its originating protocol. It is never implicitly executable.

Response items describe generated output. Tool results remain input Content::ToolResult blocks with rich ordered content and call IDs; no second general request model is required. Provider wire types stay outside querymt core.

Alternative rejected: adding only fields to ToolCall or overloading Thinking.signature cannot preserve multiple reasoning items, message boundaries, and unknown siblings.

### 3. ChatMessage is a turn envelope

Add optional structured output to ChatMessage using serde default/omission for old histories. An assistant turn can contain several protocol items. Its content is a deterministic portable projection of output, not additional input to replay.

Conversion from ChatResponse preserves output and derives content. Structured serializers expand items once. Legacy serializers use the portable projection. Validate role and projection consistency at request boundaries: stale content/output disagreement returns an explicit error rather than silently ignoring caller edits. Provide helpers to replace output and regenerate content, or intentionally clear output before editing portable content. Audit hooks, chains, cache helpers, and agent conversion for this rule.

Alternative rejected: nesting response message items inside Content blurs message-versus-part hierarchy. A sidecar detached from a turn makes persistence, reorder, and edit association fragile. The chosen field does cost source compatibility for public struct literals.

### 4. Structured streaming with one accumulator

Extend StreamChunk with structured output metadata and indexed item lifecycle/content delta events. Include content/summary indexes where applicable, not only tool indexes. Retain legacy Text/Thinking/tool events as display/execution projections for old consumers.

The shared accumulator selects structured mode when structured metadata appears before semantic output; structured events alone determine canonical history in that mode. It must not append legacy projection events to canonical items. In legacy mode it synthesizes limited output from legacy events. Completed item snapshots replace provisional state; repeated consistent completions are idempotent and conflicting ones are errors. Usage is normalized once rather than summed across projections/snapshots.

Provider parsers own request-local protocol state. Decode SSE across arbitrary byte/UTF-8/frame boundaries using the existing framing layer where available; verify that boundary before adding any second framing layer. Response completion reconciles final items without replaying their visible deltas. Emit final items/metadata and usage before Done. Failed/incomplete terminals and premature EOF are not successful completion; preserve partial output/status without dispatching unfinished calls. Item completion alone does not authorize the agent to execute tools before response-level terminal validation. Retries reset attempt-local accumulation.

Alternative rejected: a separate response stream would duplicate APIs; only a final output snapshot would preserve replay but leave live item indexing unavailable.

### 5. Persistence and replay are protocol-aware

Carry canonical output through Extism responses, stream DTOs, remote providers, and agent LlmResponse. Store it in a dedicated agent message part so existing JSON part storage can retain it. Existing display parts may remain as projections but replay must select the canonical part exactly once. Bindings expose structured data; legacy exports explicitly project and do not claim lossless roundtrip.

Provenance includes provider, protocol, model, and endpoint identity (no credentials). Same-provider/protocol/model/endpoint replay retains compatible opaque state. On a model or endpoint switch, default to portable projection; a future explicit compatibility rule can relax this. Do not mutate stored originals when projecting to another target. Call-ID remapping, if required by a target, must apply consistently to calls and results.

Persistence retains unknown fields/items; replay is NOT blind raw-output passthrough. The codec preserves input-valid fields and validates unknown items against supported replay rules. If an opaque item is required for continuation but cannot safely be serialized, fail explicitly instead of silently dropping it. Compaction replaces complete dependency groups (reasoning/calls/results) with a portable summary, clearing affected opaque continuation. No hidden raw sidecar may bypass a user edit or compaction.

Older JSON payloads decode with absent output. Old peers do not automatically understand new variants: advertise a versioned item-aware transport capability and reject structured operation when a peer lacks it. Legacy sessions remain supported. Native Rust plugins require coordinated rebuild/version checks; serde defaults are not ABI compatibility. Sensitive opaque state must be redacted from Debug/display, telemetry, and ordinary exports while retained in authorized history storage.

### 6. Explicit Responses mode and reusable codec

Add OpenAI config selection `api = "chat_completions" | "responses"`; omission retains Chat Completions. Both use existing HTTP provider methods. No fallback after sending a Responses request. Add a codec module in qmt-openai for Responses wire types and mapping; xAI already depends on qmt-openai, and provider-specific wrappers keep authentication, headers, instructions, supported fields, error classification, and backend restrictions. Confirm dependency direction before reuse; no new protocol crate is required by this plan.

Responses requests use store=false, request encrypted reasoning via include where supported, and replay local items rather than previous_response_id or conversation IDs. Reject conflicting extra-body values for protocol-owned fields rather than allowing duplicate keys or changing retention/continuation mode. Map max_tokens to max_output_tokens, JSON schema to text.format, reasoning effort to reasoning.effort, and system config to instructions. Exclude unsupported sampling fields explicitly instead of inheriting Codex/xAI quirks.

Function definitions use the Responses flattened shape. Add optional strictness to the shared function definition: legacy Chat Completions omission stays omitted, while Responses omission is serialized as false to preserve QueryMT's existing non-strict intent. Explicit strict=true requires a compatible schema; validate and reject incompatible schemas rather than silently making optional fields required. Named tool choice is protocol-specific. Function outputs correlate using call_id, preserve rich part order, support text/images/files where valid, and reject unsupported media instead of placeholders or silent omission.

Reasoning summaries and encrypted content remain separate. Preserve refusal and annotation data. Normalize usage to QueryMT's exclusive token categories to avoid double-counting cached/reasoning tokens. Completed responses with local calls project to ToolCalls; otherwise Stop. Incomplete max-output-token/content-filter responses retain the corresponding terminal cause and partial output; they are not blanket retryable failures. Unknown causes and provider failures retain structured details. Unsupported provider actions requiring local participation fail explicitly and never enter the function executor.

## Risks / Trade-offs

- Public struct fields and enum variants break some Rust source usage -> migrate workspace literals/matches, document release impact, and add constructors/helpers.
- Two representations can diverge -> validate projections, provide edit helpers, and test hooks/history rewrite paths.
- Larger serialized histories -> keep opaque data scoped, avoid duplicate canonical payloads, and test storage/transport limits; do not truncate continuation silently.
- Opaque state can leak or cross endpoints -> redact logs and gate replay on conservative provenance checks.
- Shared codec can accidentally unify incompatible backends -> provider-owned policy hooks and separate OpenAI/Codex/xAI fixture suites.
- Protocol evolution introduces unknown types -> preserve on disk, reject unsupported required replay/actions, and never execute arbitrary opaque items.

## Migration Plan

1. Land core types/accessor/conversion and source migrations with old serialized histories accepted.
2. Land structured stream accumulation and versioned transport support before enabling item-aware providers.
3. Migrate agent persistence, edits, compaction, and bindings so no intermediate boundary flattens output.
4. Implement opt-in OpenAI Responses and migrate reusable Codex/xAI codec behavior with parity fixtures.
5. Run native/plugin/remote and second-request replay acceptance tests; release with documented Rust/native-plugin compatibility changes.

Rollback means disabling Responses mode and selecting an explicit portable-history projection for existing sessions, not deleting canonical history. Older binaries that cannot read new message parts require a version guard or an explicit lossy export; they must not be promised transparent downgrade compatibility.
