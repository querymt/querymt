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

