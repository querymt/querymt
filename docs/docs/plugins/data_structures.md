# Data Structures

These are the primary data structures used in the interface between the QueryMT host and Extism plugins. They are typically serialized to/from JSON.

Many of these types are defined in `querymt::chat`, `querymt::completion`, and `querymt::plugin::extism_impl::interface`.

## Core Request/Response Wrappers

These structures are used as the direct input/output for the main plugin functions (`chat`, `embed`, `complete`). They bundle the plugin-specific configuration (`cfg`) with the actual request payload.

-   **`ExtismChatRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration (type `C` is your plugin's config struct).
    -   `messages: Vec<ChatMessage>`: The history of chat messages.
    -   `tools: Option<Vec<Tool>>`: Optional list of tools the model can use.

-   **`ExtismEmbedRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration.
    -   `inputs: Vec<String>`: List of texts to embed.

-   **`ExtismCompleteRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration.
    -   `req: CompletionRequest`: The core completion request.

-   **`ExtismChatResponse`** (implements `querymt::chat::ChatResponse`)
    -   `text: Option<String>`: The main textual response from the LLM.
    -   `tool_calls: Option<Vec<ToolCall>>`: If the LLM decides to call tools.
    -   `thinking: Option<String>`: Optional intermediate "thinking" messages.

## Common Data Types

These are used within the request/response wrappers.

-   **`ChatMessage`** (`querymt::chat::ChatMessage`)
    -   `role: ChatRole`: Enum (`System`, `User`, `Assistant`, `Tool`).
    -   `content: Option<String>`: Text content of the message.
    -   `name: Option<String>`: Name of the speaker (especially for `Tool` role).
    -   `tool_calls: Option<Vec<ToolCall>>`: For `Assistant` role, if it calls tools.
    -   `tool_call_id: Option<String>`: For `Tool` role, the ID of the tool call it's responding to.

-   **`Tool`** (`querymt::chat::Tool`)
    -   `type: ToolType`: Enum (currently only `Function`).
    -   `function: ToolFunction`: Describes the function tool.

-   **`ToolFunction`** (`querymt::chat::ToolFunction`)
    -   `name: String`: Name of the function.
    -   `description: Option<String>`: Description of the function.
    -   `parameters: serde_json::Value`: JSON schema defining the function's parameters.

-   **`ToolCall`** (`querymt::ToolCall`)
    -   `id: String`: Unique ID for the tool call.
    -   `type: ToolType`: Enum (currently only `Function`).
    -   `function: ToolCallFunction`: Details of the function to be called.

-   **`ToolCallFunction`** (`querymt::ToolCallFunction`)
    -   `name: String`: Name of the function called.
    -   `arguments: String`: JSON string of arguments for the function.

-   **`CompletionRequest`** (`querymt::completion::CompletionRequest`)
    -   `prompt: String`: The prompt for completion.
    -   `max_tokens: Option<u32>`
    -   `temperature: Option<f32>`
    -   `stop_sequences: Option<Vec<String>>`
    -   *(Other common completion parameters)*

-   **`CompletionResponse`** (`querymt::completion::CompletionResponse`)
    -   `text: String`: The completed text.
    -   *(Other relevant fields, e.g., finish reason, tokens used)*

For plugin developers using Rust, these types are readily available. When interacting via raw JSON, ensure your data conforms to these structures. The `config_schema()` export of a plugin should define the structure for the generic `C` (plugin configuration) part of the `Extism*Request` types.

