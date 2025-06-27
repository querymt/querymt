# Architecture

QueryMT is designed with a modular and extensible architecture to provide a flexible foundation for interacting with Large Language Models. Understanding its key components will help you leverage the library effectively.

## Core Abstractions

At the heart of QueryMT are several key traits that define the contract for LLM interactions:

*   **`querymt::LLMProvider`**: This is the central trait that all LLM provider implementations must conform to. It combines capabilities for chat, text completion, and embeddings into a single, unified interface by extending `querymt::chat::BasicChatProvider`, `querymt::chat::ToolChatProvider`, `querymt::completion::CompletionProvider`, and `querymt::embedding::EmbeddingProvider`. It also includes methods for listing available tools (`tools()`) and calling them (`call_tool()`).
    *   Source: `crates/querymt/src/lib.rs`

*   **`querymt::HTTPLLMProvider`**: A specialized trait for LLM providers that are accessed over HTTP. It defines methods for constructing HTTP requests and parsing HTTP responses for chat, completion, and embedding operations by extending the `http` sub-traits (e.g., `querymt::chat::http::HTTPChatProvider`).
    *   Source: `crates/querymt/src/lib.rs`

## Provider Adapters

*   **`querymt::adapters::LLMProviderFromHTTP`**: This struct acts as an adapter, allowing an `HTTPLLMProvider` (which handles the raw HTTP logic) to be used as a full-fledged `LLMProvider`. It takes care of calling the outbound HTTP mechanism (`fn@querymt::outbound::call_outbound`) and then parsing the response using the `HTTPLLMProvider` implementation.
    *   Source: `crates/querymt/src/adapters.rs`

## Building and Configuring Providers

*   **`querymt::builder::LLMBuilder`**: QueryMT provides a fluent builder pattern for configuring and instantiating `LLMProvider` instances. This builder allows you to set various options such as the model, API keys, temperature, custom parameters, and register tools. It uses a `querymt::plugin::host::PluginRegistry` to find the appropriate factory for the selected provider.
    *   Source: `crates/querymt/src/builder.rs`

## Tool and Function Calling

QueryMT has robust support for LLM tool usage (often called function calling):

*   **`querymt::chat::Tool` / `querymt::chat::FunctionTool`**: These structs define the schema of tools that an LLM can use.
*   **`querymt::tool_decorator::CallFunctionTool`**: A trait that your host-side functions must implement to be callable by the LLM. It includes a method to describe the tool (`descriptor()`) and a method to execute it (`call()`).
*   **`querymt::tool_decorator::ToolEnabledProvider`**: A decorator that wraps an existing `LLMProvider` and injects tool-calling capabilities. It manages a registry of `CallFunctionTool` implementations and handles the interaction logic when an LLM decides to call a tool.
    *   Source: `crates/querymt/src/tool_decorator.rs`

## Plugin System

A core strength of QueryMT is its plugin system, enabling easy addition of new LLM providers. The system has been unified to support different plugin types through a single registry.

*   **`querymt::plugin::LLMProviderFactory`**: A trait that plugin authors implement. Its primary role is to create an `LLMProvider` instance from a given configuration. It also provides metadata like the plugin name and configuration schema.
*   **`querymt::plugin::http::HTTPLLMProviderFactory`**: A specialized factory for plugins that expose an `HTTPLLMProvider`.

*   **`querymt::plugin::host::PluginRegistry`**: A central registry that discovers, loads, and manages `LLMProviderFactory` instances from a configuration file. It uses different loaders for different plugin types.
    *   **`querymt::plugin::host::PluginLoader`**: A trait for systems that can load a specific `querymt::plugin::host::PluginType`. QueryMT provides implementations for:
        *   **Native Plugins:** (`querymt::plugin::host::native::NativeLoader`) Loads plugins from shared libraries (`.so`, `.dll`, `.dylib`).
        *   **WASM Plugins via Extism:** (`querymt::plugin::extism_impl::host::ExtismLoader`) Loads plugins compiled to WebAssembly and executed via Extism, offering sandboxing and portability.
    *   Sources: `crates/querymt/src/plugin/host/mod.rs`, `crates/querymt/src/plugin/host/native.rs`, `crates/querymt/src/plugin/extism_impl/host/loader.rs`

## Outbound HTTP Communication

*   The `outbound.rs` module provides a common function (`fn@querymt::outbound::call_outbound`) for making HTTP requests. This is used by HTTP-based providers and adapters. It's designed to work in both native environments (using `reqwest`) and potentially WASM environments.
    *   Source: `crates/querymt/src/outbound.rs`

## Error Handling

*   QueryMT defines a comprehensive `querymt::error::LLMError` to represent various issues that can occur, such as HTTP errors, authentication problems, provider-specific errors, and plugin issues.
    *   Source: `crates/querymt/src/error.rs`

## High-Level Flow

1.  An application initializes a `querymt::plugin::host::PluginRegistry` from a configuration file (e.g., `plugins.toml`).
2.  The application registers the desired loaders (e.g., `NativeLoader`, `ExtismLoader`) with the registry.
3.  The registry calls `load_all_plugins()`, which iterates through the configured providers. For each provider, it determines its type (e.g., local Wasm, OCI image, native library) and uses the appropriate `PluginLoader` to load it and create an `LLMProviderFactory`.
4.  The application uses `querymt::builder::LLMBuilder` to configure a desired LLM provider by name (e.g., "openai", "custom-plugin").
5.  The builder's `build()` method is called, passing a reference to the `PluginRegistry`.
6.  The builder looks up the `LLMProviderFactory` from the registry using the provider name.
7.  The factory's `from_config()` method is called, which instantiates an `LLMProvider` (possibly an `HTTPLLMProvider` wrapped by `LLMProviderFromHTTP`).
8.  If tools were added via `add_tool()`, the base provider is wrapped in a `querymt::tool_decorator::ToolEnabledProvider`.
9.  If a validator is set, the provider is further wrapped in a `querymt::validated_llm::ValidatedLLM`.
10. The application can then use the resulting `Box<dyn LLMProvider>` instance to perform chat, completion, or embedding operations.

This layered and decoupled design makes QueryMT adaptable and easy to extend.
