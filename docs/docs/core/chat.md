# Chat Interactions

QueryMT provides comprehensive support for chat-based interactions with Large Language Models, enabling you to build conversational AI applications.

## Key Components

*   **`ChatMessage`**: Represents a single message in a conversation. Key attributes include:
    *   `role`: Indicates who sent the message (e.g., `ChatRole::User`, `ChatRole::Assistant`).
    *   `message_type`: Specifies the nature of the content (e.g., `MessageType::Text`, `MessageType::Image`, `MessageType::ToolUse`, `MessageType::ToolResult`).
    *   `content`: The actual content of the message (often text, but can be related to other types like image metadata or tool call details).
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`ChatResponse`**: A trait representing the LLM's response to a chat request. It provides methods to access:
    *   `text()`: The textual content of the LLM's reply.
    *   `tool_calls()`: A list of `ToolCall` objects if the LLM decided to use one or more tools.
    *   `thinking()`: Optional "thoughts" or reasoning steps from the model, if supported and enabled.
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`BasicChatProvider`**: A trait that LLM providers implement to support fundamental chat functionality. It has a single method:
    *   `chat(&self, messages: &[ChatMessage])`: Sends a list of messages to the LLM and returns a `ChatResponse`.
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`ToolChatProvider`**: Extends `BasicChatProvider` to include support for tools (function calling). It has one primary method:
    *   `chat_with_tools(&self, messages: &[ChatMessage], tools: Option<&[Tool]>)`: Sends messages along with a list of available tools the LLM can use.
    *   Source: `crates/querymt/src/chat/mod.rs`

## How It Works

1.  **Construct Messages:** Your application assembles a sequence of `ChatMessage` objects representing the conversation history. This typically starts with a system prompt (as a `User` message, or handled specially by the provider) followed by alternating `User` and `Assistant` messages.
2.  **Initiate Chat:** You call the `chat` or `chat_with_tools` method on an `LLMProvider` instance, passing the message history and optionally, a list of available tools.
3.  **Provider Interaction:** The `LLMProvider` (or its underlying implementation like `HTTPLLMProvider`) formats the request according to the specific LLM's API, sends it, and receives the raw response.
4.  **Parse Response:** The provider parses the raw response into an object implementing `ChatResponse`.
5.  **Handle Response:** Your application processes the `ChatResponse`:
    *   If `text()` is present, it's the LLM's textual reply.
    *   If `tool_calls()` is present, the LLM wants to execute one or more functions. Your application needs to:
        *   Execute these functions.
        *   Send the results back to the LLM as new `ChatMessage`s (typically with `MessageType::ToolResult`).
        *   Continue the chat loop.

## Example Flow (Conceptual)

```rust
// Assuming 'llm_provider' is an instance of Box<dyn LLMProvider>
// and 'my_tools' is a Vec<Tool>

let messages = vec![
    ChatMessage::user().content("What's the weather like in London?").build(),
];

// Chat with tool capabilities
let response = llm_provider.chat_with_tools(&messages, Some(&my_tools)).await?;

if let Some(tool_calls) = response.tool_calls() {
    // LLM wants to call a tool (e.g., a weather API function)
    // ... execute tools and get results ...
    let mut new_messages = messages.clone();
    new_messages.push(ChatMessage::assistant().tool_use(tool_calls.clone()).build()); // Model's decision to use tool
    // for each tool_call_result:
    // new_messages.push(ChatMessage::user().tool_result(tool_call_id, result_string).build()); // Actual tool result

    // Send results back to LLM
    let final_response = llm_provider.chat_with_tools(&new_messages, Some(&my_tools)).await?;
    println!("Final answer: {}", final_response.text().unwrap_or_default());
} else if let Some(text) = response.text() {
    println!("LLM response: {}", text);
}
```

QueryMT's chat system is designed to be flexible, supporting simple Q&A, complex multi-turn dialogues, and sophisticated interactions involving external tools. The `Tool` and `ToolChoice` mechanisms provide fine-grained control over how LLMs can utilize functions.

