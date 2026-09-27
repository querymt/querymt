## Why

OpenAI Responses support requires preserving ordered output items, reasoning continuation state, and function-call identities across requests, not just changing an endpoint. QueryMT currently flattens these into text, thinking, and tool calls, making persisted tool-turn replay lossy (context: https://github.com/querymt/querymt/issues/737).

## What Changes

- Preserve `LLMProvider` composition and existing request-side chat abstractions; do not introduce a protocol-specific `ResponseProvider`.
- **BREAKING**: Make provider-neutral `ChatOutput` the canonical response value returned by chat generation, replacing the flattened `ChatResponse` object contract. Text, visible reasoning, tool calls, usage, and finish reason become projections of that value rather than independently implemented accessors.
- **BREAKING**: Give each assistant turn one authoritative payload: either canonical input parts or structured output. Do not expose independently mutable `content` and `output` fields whose consistency is checked only at request time.
- **BREAKING**: Replace the public recursive `Content` enum with a canonical input-side model for text, validated attachments, and correlated tool results. Generated reasoning and function calls exist only in `ChatOutput`; image, audio, document, URL, and resource-link forms use one validated attachment abstraction rather than parallel variants.
- **BREAKING**: Use one canonical indexed structured event stream and shared consuming accumulator. Legacy/UI chunks are produced by an explicit projection adapter, not emitted alongside canonical events.
- Preserve structured output through agent storage, plugin and remote transport, history editing, and compatible replay, including non-destructive A -> B -> A provider switching. Structured terminal events must terminate remote relays just like legacy terminals.
- Add unified typed attachments with source/rendering metadata and a QueryMT MediaType wrapper backed by the mime crate, serialized as a MIME string. All construction and deserialization must enforce media invariants. Preserve unknown media item semantics as opaque data; extending formats does not require a core variant per MIME type.
- Preserve unknown fields on both known and unknown items/parts, infer terminal meaning after final snapshot reconciliation, and retain partial structured output with incomplete or failed outcomes.
- Add explicit OpenAI Responses selection, stateless local replay, function tools, rich tool results, and structured-output mapping. Share compatible codec behavior with Codex and xAI without importing their endpoint-specific restrictions.
- Retain backward compatibility for supported old serialized histories through private migration DTOs and explicit lossy portable exports, but serialize only the canonical model. Intentionally drop Rust source compatibility, exhaustive-match compatibility, and automatic legacy transport behavior. Native plugins and serialized item-aware transports require coordinated versioning.

## Capabilities

### New Capabilities

- `item-aware-chat-output`: Canonical input parts and structured results, identity, raw arguments, reasoning state, validated attachments, output projections, and exclusive assistant payload authority.
- `item-aware-chat-streaming`: Canonical indexed item events, semantic terminal handling across transports, consuming accumulation outcomes, and explicit compatibility adapters.
- `chat-output-roundtrip`: Persistence, plugin/remote transport, compatible replay, edits, compaction, and provenance boundaries.
- `openai-responses-protocol`: Explicit endpoint selection, stateless requests, function tools, structured output, and response/error mapping.

### Modified Capabilities

None. The local `openspec/specs` directory has no existing capability specifications.

## Impact

Core chat traits and return types, message and input-part models, stream events and adapters, validated attachments, every provider request codec, HTTP adapters, MCP conversion, Extism interfaces/macros, remote-provider DTOs and terminal routing, agent execution, token estimation and history storage, bindings, examples, exports, and their tests. This is a coordinated breaking data-contract evolution, not a new provider capability hierarchy.

## Non-goals

No new provider supertrait, stateful `previous_response_id` or Conversations API support, automatic endpoint selection/fallback, complete built-in/custom-tool execution framework, or universal portability of encrypted/provider-opaque state. Compatibility with old persisted data remains a goal; source compatibility with the pre-item-aware Rust response and streaming APIs does not. No implementation changes are part of this planning change.
