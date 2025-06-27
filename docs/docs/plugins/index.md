# QueryMT Plugins Overview

Welcome to the documentation for QueryMT's plugin system. This system allows you to extend QueryMT's capabilities by integrating various Large Language Model (LLM) providers through two distinct plugin types: **Native** and **Extism (Wasm)**.

## Two Types of Plugins

QueryMT supports a flexible plugin architecture to meet different needs for performance, security, and portability.

### 1. Native Plugins

-   **Mechanism**: Native plugins are dynamic shared libraries (`.so`, `.dll`, `.dylib`) that are loaded directly by the QueryMT host.
-   **Language**: Primarily developed in Rust to directly implement the required provider traits.
-   **Pros**:
    -   **Maximum Performance**: Code runs natively with no sandbox overhead, enabling direct memory access and function calls.
    -   **Simplicity**: For Rust developers, this is a very direct integration path.
-   **Cons**:
    -   **No Sandboxing**: The plugin runs with the same permissions as the host application, posing a potential security risk if the plugin is not from a trusted source.
    -   **Platform-Dependent**: A library compiled for `linux-x86_64` will not run on `darwin-aarch64`.

### 2. Extism (Wasm) Plugins

-   **Mechanism**: Plugins are WebAssembly (Wasm) modules that run inside a secure sandbox provided by [Extism](https://extism.org/).
-   **Language**: Can be developed in any language that compiles to Wasm (e.g., Rust, Go, C++, Zig).
-   **Pros**:
    -   **Security**: The Wasm sandbox isolates the plugin from the host system, preventing unauthorized file or network access.
    -   **Portability**: A single `.wasm` file can run on any platform and architecture supported by the host.
    -   **Dynamic Loading**: Add or update LLM providers without recompiling QueryMT.
-   **Cons**:
    -   **Performance Overhead**: Communication between the host and plugin involves serialization (typically to JSON) and crossing the sandbox boundary, which introduces latency compared to a native call.

## Core Concepts

-   **Plugin**: A self-contained unit (either a shared library or a Wasm module) that implements a provider interface.
-   **Host**: QueryMT itself, which loads, configures, and interacts with plugins via the `PluginRegistry`.
-   **Interface**: A defined contract that plugins must adhere to. This differs for Native and Extism plugins but is abstracted away by the host.
-   **Configuration**: A central file (e.g., `plugins.toml`) where you define all plugins—both native and Extism—that QueryMT should load.

This documentation will guide you through:

-   **[Configuration](configuration.md)**: How to configure QueryMT to load and use any type of plugin.
-   **[Plugin Interface](interface_spec.md)**: The technical specifications for both Native and Extism plugin interfaces.
-   **[Plugin Development](development.md)**: How to create your own LLM provider plugins.
-   **[OCI Plugins](oci_plugins.md)**: How to distribute and consume plugins using OCI registries.
