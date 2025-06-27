# Welcome to QueryMT

QueryMT is a versatile Rust library designed to provide a unified and extensible interface for interacting with various Large Language Models (LLMs). Whether you're working with commercial APIs like OpenAI and Anthropic, or self-hosted models, QueryMT aims to simplify development by abstracting away provider-specific details.

## Core Capabilities

QueryMT offers a comprehensive suite of features for LLM interactions:

*   **Chat Interactions:** Engage in conversational AI, similar to platforms like ChatGPT. QueryMT supports multi-turn dialogues, system prompts, and user/assistant roles.
*   **Text Completion:** Generate text based on prompts, suitable for tasks like summarization, translation, or creative writing.
*   **Embeddings Generation:** Convert text into numerical vector representations, crucial for semantic search, clustering, and other machine learning tasks.
*   **Tool Usage & Function Calling:** Enable LLMs to interact with external systems and data sources by defining and calling functions based on the conversation context.
*   **Extensible Plugin System:** Easily add support for new LLM providers or custom logic through native shared libraries or sandboxed WebAssembly (WASM) modules via Extism.
*   **Prompt Chaining:** Orchestrate complex workflows by linking multiple LLM calls, potentially across different providers.
*   **Response Evaluation:** Compare and score responses from multiple LLM providers in parallel to select the best result based on custom criteria.
*   **MCP Integration:** Connect with Model Context Protocol (MCP) servers to leverage external tools and services.

## Key Benefits

*   **Unified Interface:** Write code once and interact with multiple LLM backends using a consistent API.
*   **Flexibility:** Choose from a variety of LLM providers, or implement your own.
*   **Extensibility:** The plugin architecture allows for easy expansion and customization.
*   **Sandboxing (WASM):** Safely run untrusted LLM provider logic using Extism-based WASM plugins.
*   **Robustness:** Features like response validation and error handling improve the reliability of LLM applications.
*   **Developer-Friendly:** Designed with Rust's strengths in performance and safety, offering a fluent builder pattern for configuration.

This documentation will guide you through the architecture, core concepts, and various features of QueryMT, helping you leverage its power for your LLM-based projects.
