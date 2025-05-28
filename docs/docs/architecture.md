# Architecture

QueryMT is designed with a modular and extensible architecture to provide a flexible foundation for interacting with Large Language Models. Understanding its key components will help you leverage the library effectively.

## Core Abstractions

At the heart of QueryMT are several key traits that define the contract for LLM interactions:

*   **`LLMProvider`**: This is the central trait that all LLM provider implementations must conform to. It combines capabilities for chat, text completion, and embeddings into a single, unified interface. It also includes methods for listing available tools and calling them.
    *   Source: `crates/querymt/src/lib.rs`

*   **`HTTPLLMProvider`**: A specialized trait for LLM providers that are accessed over HTTP. It defines methods for constructing HTTP requests and parsing HTTP responses for chat, completion, and embedding operations.
    *   Source: `crates/querymt/src/lib.rs`

## Provider Adapters

*   **`LLMProviderFromHTTP`**: This struct acts as an adapter, allowing an `HTTPLLMProvider` (which handles the raw HTTP logic) to be used as a full-fledged `LLMProvider`. It takes care of calling the outbound HTTP mechanism and then parsing the response.
    *   Source: `crates/querymt/src/adapters.rs`

## Building and Configuring Providers

*   **`LLMBuilder`**: QueryMT provides a fluent builder pattern (`LLMBuilder`) for configuring and instantiating `LLMProvider` instances. This builder allows you to set various options such as the model, API keys, temperature, custom parameters, and register tools. It uses a `ProviderRegistry` to find the appropriate factory for the selected provider.
    *   Source: `crates/querymt/src/builder.rs`

## Tool and Function Calling

QueryMT has robust support for LLM tool usage (often called function calling):

*   **`Tool` / `FunctionTool`**: These structs define the schema of tools that an LLM can use.
*   **`CallFunctionTool`**: A trait that your host-side functions must implement to be callable by the LLM. It includes a method to describe the tool and a method to execute it.
*   **`ToolEnabledProvider`**: A decorator that wraps an existing `LLMProvider` and injects tool-calling capabilities. It manages a registry of `CallFunctionTool` implementations and handles the interaction logic when an LLM decides to call a tool.
    *   Source: `crates/querymt/src/tool_decorator.rs`

## Plugin System

A core strength of QueryMT is its plugin system, enabling easy addition of new LLM providers:

*   **`LLMProviderFactory`**: A trait that plugin authors implement. Its primary role is to create an `LLMProvider` instance from a given configuration. It also provides metadata like the plugin name and configuration schema.
*   **`HTTPLLMProviderFactory`**: A specialized factory for plugins that expose an `HTTPLLMProvider`.
*   **`ProviderRegistry`**: A trait for a system that can discover and manage `LLMProviderFactory` instances. QueryMT provides implementations for:
    *   **Native Plugins:** (`NativeProviderRegistry`) Loads plugins from shared libraries (`.so`, `.dll`, `.dylib`).
    *   **WASM Plugins via Extism:** (`ExtismProviderRegistry`) Loads plugins compiled to WebAssembly and executed via Extism, offering sandboxing and portability.
    *   Sources: `crates/querymt/src/plugin/mod.rs`, `crates/querymt/src/plugin/native.rs`, `crates/querymt/src/plugin/extism_impl/host/registry.rs`

## Outbound HTTP Communication

*   The `outbound.rs` module provides a common function (`call_outbound`) for making HTTP requests. This is used by HTTP-based providers and adapters. It's designed to work in both native environments (using `reqwest`) and potentially WASM environments (e.g., using Spin SDK, though the WASM path might be a placeholder or specific to a target).
    *   Source: `crates/querymt/src/outbound.rs`

## Error Handling

*   QueryMT defines a comprehensive `LLMError` enum to represent various issues that can occur, such as HTTP errors, authentication problems, provider-specific errors, and plugin issues.
    *   Source: `crates/querymt/src/error.rs`

## High-Level Flow

1.  An application uses `LLMBuilder` to configure a desired LLM provider (e.g., "openai", "custom-plugin").
2.  The builder consults a `ProviderRegistry` (like `NativeProviderRegistry` or `ExtismProviderRegistry`) to find the `LLMProviderFactory` for the specified provider.
3.  The factory's `from_config` method is called, which instantiates an `LLMProvider` (possibly an `HTTPLLMProvider` wrapped by `LLMProviderFromHTTP`).
4.  If tools are added via the builder, the base provider is wrapped in a `ToolEnabledProvider`.
5.  If a validator is set, the provider might be further wrapped in a `ValidatedLLM`.
6.  The application can then use the resulting `LLMProvider` instance to perform chat, completion, or embedding operations.
7.  For HTTP-based providers, requests are made via `call_outbound`.
8.  If the LLM decides to use a tool, `ToolEnabledProvider` routes the call to the appropriate registered `CallFunctionTool`.

This layered and decoupled design makes QueryMT adaptable and easy to extend.
