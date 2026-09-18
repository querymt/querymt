# LLM Providers

At the core of QueryMT's design is the concept of an **LLM Provider**. A provider represents a specific Large Language Model service or backend that QueryMT can interact with. This could be a commercial API like OpenAI's GPT models, Anthropic's Claude, a self-hosted open-source model, or even a custom model accessed via a proprietary API.

## The `LLMProvider` Trait

The primary interface for all providers is the `LLMProvider` trait (defined in `crates/querymt/src/lib.rs`). This trait unifies the different ways one might interact with an LLM, requiring implementers to support:

*   **Chat Interactions:** Via `BasicChatProvider` and `ToolChatProvider` supertraits.
*   **Text Completion:** Via the `CompletionProvider` supertrait.
*   **Embeddings Generation:** Via the `EmbeddingProvider` supertrait.

It also includes optional methods related to tool usage:
*   `tools()`: Returns a list of tools the provider is aware of or configured with.
*   `call_tool()`: Allows the system to invoke a tool call identified by the LLM.
*   `tool_server_name(&tool_name)`: Returns the server name for a tool if available (e.g., for MCP tools). This is used to dispatch tool calls to the correct MCP server when multiple MCP servers are configured.

By conforming to this trait, different LLM backends can be used interchangeably within QueryMT applications.

## HTTP-Based Providers

Many LLM services are accessed via HTTP APIs. QueryMT provides a specialized trait for these:

*   **`HTTPLLMProvider`**: This trait (defined in `crates/querymt/src/lib.rs`) is implemented by providers that communicate over HTTP. It defines methods for:
    *   Constructing HTTP requests for chat, completion, and embedding operations (e.g., `chat_request`, `complete_request`, `embed_request`).
    *   Parsing HTTP responses back into QueryMT's standard data structures (e.g., `parse_chat`, `parse_complete`, `parse_embed`).

An `HTTPLLMProvider` is typically wrapped by `LLMProviderFromHTTP` (from `crates/querymt/src/adapters.rs`) to make it usable as a full `LLMProvider`. The adapter handles the actual outbound HTTP call and then uses the `HTTPLLMProvider`'s parsing methods.

## Instantiation and Configuration

You don't usually interact with these traits directly to create provider instances. Instead, QueryMT offers:

1.  **`LLMBuilder`**: A fluent interface to configure and build `LLMProvider` instances. You specify the provider name (e.g., "openai"), model, API keys, and other parameters.
2.  **Plugin System**: For providers not built directly into QueryMT, a plugin system allows new providers to be added dynamically. Plugins implement `LLMProviderFactory` or `HTTPLLMProviderFactory` which are then used by the `LLMBuilder` to create provider instances.

This separation of concerns—the core provider traits, HTTP-specific handling, builder for configuration, and a plugin system for extensibility—makes QueryMT a flexible framework for working with a diverse range of LLMs.

## Z.AI Provider Notes

QueryMT includes a `zai` HTTP provider (OpenAI-compatible request/response flow).

- Configure `base_url` in your provider config to select the Z.AI endpoint.
- Default endpoint is the general API: `https://api.z.ai/api/paas/v4/`.
- For GLM Coding Plan scenarios, use the coding endpoint instead: `https://api.z.ai/api/coding/paas/v4/`.
- Model listing is dynamic and uses the configured `base_url`.

Example provider config:

```toml
[[providers]]
name = "zai"
path = "oci://ghcr.io/querymt/zai:latest"

[providers.config]
api_key = "${ZAI_API_KEY}"
model = "glm-5.1"
base_url = "https://api.z.ai/api/paas/v4/"
```

Coding endpoint variant:

```toml
[[providers]]
name = "zai-coding"
path = "oci://ghcr.io/querymt/zai:latest"

[providers.config]
api_key = "${ZAI_API_KEY}"
model = "glm-5.1"
base_url = "https://api.z.ai/api/coding/paas/v4/"
```

## OpenAI Responses API (opt-in)

The `openai` provider can speak either the Chat Completions or the Responses
protocol. Selection is explicit and defaults to Chat Completions, so existing
configurations keep their endpoint and body semantics unchanged.

```toml
[[providers]]
name = "openai"
path = "oci://ghcr.io/querymt/openai:latest"

[providers.config]
api_key = "${OPENAI_API_KEY}"
model = "gpt-5"
api_mode = "responses"   # omit for "chat_completions" (default)
```

`api_mode` accepts `"chat_completions"` (default) or `"responses"`. When omitted
the configuration schema applies the `chat_completions` default, so the field is
never required. Setting `api_mode = "responses"` switches both streaming and
non-streaming operations to `POST /responses`.

### Behavior in Responses mode

- **Stateless local continuation.** Requests always send `store = false`,
  request `reasoning.encrypted_content` via `include`, and never emit
  `previous_response_id` or `conversation`. Ordered reasoning, message, and
  function-call items are replayed locally instead.
- **No fallback.** A failed Responses request surfaces its original classified
  error; it is never retried as a Chat Completions request.
- **Conflicting passthrough is rejected.** Supplying `store`,
  `previous_response_id`, `conversation`, or `include` through `extra_body`
  fails request construction instead of silently overriding continuation or
  retention policy. Unrelated `extra_body` fields still pass through.
- **Control mapping.** `system` → `instructions`, `max_tokens` →
  `max_output_tokens`, `json_schema` → `text.format`, `reasoning_effort` →
  `reasoning.effort`. Controls with no Responses equivalent (for example
  `top_k`) fail explicitly rather than being silently ignored.
- **Function tools.** Definitions use the flattened Responses shape. Unspecified
  strictness serializes `strict = false`; explicit `strict = true` requires a
  schema where every object sets `additionalProperties: false` and lists all
  properties as required, otherwise construction fails rather than silently
  making formerly optional fields required. Named tool choice uses the Responses
  `{ "type": "function", "name": ... }` shape.
- **Function outputs** correlate by `call_id` and preserve ordered
  text/image/file parts. Media the endpoint cannot represent fails explicitly
  instead of being replaced by a placeholder.

### Compatibility and shared codec

The Responses codec is shared with the `codex` and `xai` providers, which keep
their own authentication, headers, instructions, streaming requirements, and
error policies. Codex remains streaming-only and requires its provider-specific
credentials; xAI retains its endpoint, headers, model-based option gating, and
does not import OpenAI-only options.

### Portable (lossy) downgrade

Opaque provider items — such as `image_generation_call` — are retained verbatim
but have no validated input representation. Replaying a turn that requires such
an item fails with an explicit unsupported-continuation error instead of
dropping it. To move a session to a target that cannot replay that state, use an
explicit portable projection, which is **lossy**: visible text and reasoning
summaries are preserved, while encrypted continuation and opaque payloads are
not. Portable exports are display-oriented and cannot restore full continuation.

Because structured output adds public message fields and stream variants, older
QueryMT binaries and native plugins require a coordinated rebuild or an explicit
lossy export; transparent downgrade compatibility is not provided.

### Out of scope

- Stateful `previous_response_id` and the Conversations API.
- Automatic endpoint selection or fallback between protocols.
- A complete built-in/custom-tool execution framework.
- Universal portability of encrypted or provider-opaque state across origins.

