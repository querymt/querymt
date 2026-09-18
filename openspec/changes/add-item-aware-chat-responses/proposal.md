## Why

OpenAI Responses support requires preserving ordered output items, reasoning continuation state, and function-call identities across requests, not just changing an endpoint. QueryMT currently flattens these into text, thinking, and tool calls, making persisted tool-turn replay lossy (context: https://github.com/querymt/querymt/issues/737).

## What Changes

- Preserve `LLMProvider` supertraits and existing `ChatProvider` and `HTTPChatProvider` generation signatures; do not introduce `ResponseProvider`.
- Add provider-neutral structured chat output with ordered message, reasoning, function-call, and opaque items, exposed through a defaulted `ChatResponse` accessor.
- Carry authoritative structured assistant output through `ChatMessage`, with existing content as a compatibility projection rather than additional history.
- Extend the existing stream with indexed structured events and a shared accumulator; preserve legacy display events without duplicating history or tool execution.
- Preserve structured output through agent storage, plugin and remote transport, history editing, and compatible replay, including non-destructive A -> B -> A provider switching.
- Add typed media with source/rendering metadata and a QueryMT MediaType wrapper backed by the mime crate, serialized as a MIME string. Preserve unknown media item semantics as opaque data; extending formats does not require a core variant per MIME type.
- Add explicit OpenAI Responses selection, stateless local replay, function tools, rich tool results, and structured-output mapping. Share compatible codec behavior with Codex and xAI without importing their endpoint-specific restrictions.
- **BREAKING**: Adding public message fields and stream enum variants requires Rust struct-literal and exhaustive-match migrations. Native plugins require coordinated rebuilding; serialized transports require explicit compatibility handling.

## Capabilities

### New Capabilities

- `item-aware-chat-output`: Structured output, identity, raw arguments, reasoning state, and projection authority within existing chat interfaces.
- `item-aware-chat-streaming`: Indexed item events, semantic terminal handling, shared accumulation, and duplicate-free compatibility projections.
- `chat-output-roundtrip`: Persistence, plugin/remote transport, compatible replay, edits, compaction, and provenance boundaries.
- `openai-responses-protocol`: Explicit endpoint selection, stateless requests, function tools, structured output, and response/error mapping.

### Modified Capabilities

None. The local `openspec/specs` directory has no existing capability specifications.

## Impact

Core chat types and conversion helpers in `crates/querymt`, OpenAI/Codex/xAI providers, HTTP adapters, Extism interfaces/macros, remote-provider DTOs, agent execution and history storage, bindings, exports, and their tests. This is a coordinated data-contract evolution, not a new provider capability hierarchy.

## Non-goals

No new provider supertrait, stateful `previous_response_id` or Conversations API support, automatic endpoint selection/fallback, complete built-in/custom-tool execution framework, or universal portability of encrypted/provider-opaque state. No implementation changes are part of this planning change.
