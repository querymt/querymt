# Data Structures

These are the primary data structures used in the interface between the QueryMT host and Extism plugins. They are typically serialized to/from JSON.

Many of these types are defined in `querymt::chat`, `querymt::completion`, and `querymt::plugin::extism_impl::interface`.

## Core Request/Response Wrappers

These structures are used as the direct input/output for the main plugin functions (`chat`, `embed`, `complete`). They bundle the plugin-specific configuration (`cfg`) with the actual request payload.

-   **`querymt::plugin::extism_impl::ExtismChatRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration (type `C` is your plugin's config struct).
    -   `messages: Vec<ChatMessage>`: The history of chat messages.
    -   `tools: Option<Vec<Tool>>`: Optional list of tools the model can use.

-   **`querymt::plugin::extism_impl::ExtismEmbedRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration.
    -   `inputs: Vec<String>`: List of texts to embed.

-   **`querymt::plugin::extism_impl::ExtismCompleteRequest<C>`**
    -   `cfg: C`: Plugin-specific configuration.
    -   `req: CompletionRequest`: The core completion request.

-   **`querymt::plugin::extism_impl::ExtismChatResponse`** (implements `querymt::chat::ChatResponse`)
    -   `text: Option<String>`: The main textual response from the LLM.
    -   `tool_calls: Option<Vec<ToolCall>>`: If the LLM decides to call tools.
    -   `thinking: Option<String>`: Optional intermediate "thinking" messages.
    -   `usage: Option<Usage>`: Optional token usage statistics.

## Common Data Types

These are used within the request/response wrappers.

-   **`querymt::chat::ChatMessage`**
    -   `role: ChatRole`: Enum (`User`, `Assistant`).
    -   `message_type: MessageType`: An enum that determines the message content type.
    -   `content: String`: The primary text content of the message.
    -   **Note on Tool Calls**: Tool-related information is carried within the `message_type` field.
        -   `MessageType::ToolUse(Vec<ToolCall>)`: Used in an `Assistant` message to indicate the model's decision to call tools.
        -   `MessageType::ToolResult(Vec<ToolCall>)`: Used in a `User` message to provide the results of tool executions back to the model. In this case, the `arguments` field of each `ToolCall` contains the JSON string result of the function.

-   **`querymt::chat::Tool`**
    -   `tool_type: String`: Typically `"function"`.
    -   `function: FunctionTool`: Describes the function tool.

-   **`querymt::chat::FunctionTool`**
    -   `name: String`: Name of the function.
    -   `description: String`: Description of the function.
    -   `parameters: serde_json::Value`: JSON schema defining the function's parameters.

-   **`querymt::ToolCall`**
    -   `id: String`: Unique ID for the tool call.
    -   `call_type: String`: Typically `"function"`.
    -   `function: FunctionCall`: Details of the function to be called.

-   **`querymt::FunctionCall`**
    -   `name: String`: Name of the function called.
    -   `arguments: String`: JSON string of arguments for the function (or the result of the function, in a `ToolResult` message).

-   **`querymt::completion::CompletionRequest`**
    -   `prompt: String`: The prompt for completion.
    *   `suffix: Option<String>`
    -   `max_tokens: Option<u32>`
    -   `temperature: Option<f32>`

-   **`querymt::completion::CompletionResponse`**
    -   `text: String`: The completed text.

For plugin developers using Rust, these types are readily available. When interacting via raw JSON, ensure your data conforms to these structures. The `config_schema()` export of a plugin should define the structure for the generic `C` (plugin configuration) part of the `Extism*Request` types.
