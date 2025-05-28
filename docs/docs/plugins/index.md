# QueryMT Extism Plugins Overview

Welcome to the documentation for QueryMT's Extism plugin system! This system allows you to extend QueryMT's capabilities by integrating various Large Language Model (LLM) providers through WebAssembly (Wasm) plugins built with [Extism](https://extism.org/).

## Why Extism Plugins?

- **Sandboxing**: Plugins run in a secure Wasm sandbox, isolating them from the host system.
- **Portability**: Wasm plugins can be written in various languages (Rust, Go, C++, etc.) and run on any platform supporting Wasm and Extism.
- **Dynamic Loading**: Add or update LLM providers without recompiling QueryMT.
- **Simplified Integration**: QueryMT provides helper macros and interfaces to streamline plugin development, especially for HTTP-based providers.

## Core Concepts

- **Plugin**: A Wasm module that implements the QueryMT LLM provider interface. It exposes a set of functions (e.g., `chat`, `embed`) that the host (QueryMT) can call.
- **Host**: QueryMT itself, which loads, configures, and interacts with these Wasm plugins.
- **Interface**: A defined contract (set of functions and data structures) that plugins must adhere to. This ensures consistent interaction between the host and plugins.
- **Configuration**: Plugins can have their own specific configurations, managed by the host and passed to the plugin during initialization and calls.

This documentation will guide you through:

- **Using existing plugins**: How to configure QueryMT to load and use LLM provider plugins.
- **Developing new plugins**: How to create your own LLM provider plugins compatible with QueryMT.

Navigate through the sections to learn more:

- **[Host Usage](configuration.md)**: Learn how to configure and use plugins in QueryMT.
- **[Plugin Development](development.md)**: Dive into creating your own Extism plugins for QueryMT.
- **[OCI Plugins](oci_plugins.md)**: Information about using plugins distributed via OCI registries.
