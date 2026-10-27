# Chat Interactions

QueryMT provides comprehensive support for chat-based interactions with Large Language Models, enabling you to build conversational AI applications.

## Key Components

*   **`querymt::chat::ChatMessage`**: Represents a single message in a conversation. Key attributes include:
    *   `role`: Indicates who sent the message (`querymt::chat::ChatRole::User` or `querymt::chat::ChatRole::Assistant`).
    *   `message_type`: Specifies the nature of the content (e.g., `querymt::chat::MessageType::Text`, `Image`, `ToolUse`, `ToolResult`).
    *   `content`: The primary text content of the message.
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`querymt::chat::ChatResponse`**: A trait representing the LLM's response to a chat request. It provides methods to access:
    *   `text()`: The textual content of the LLM's reply.
    *   `tool_calls()`: A list of `querymt::ToolCall` objects if the LLM decided to use one or more tools.
    *   `thinking()`: Optional "thoughts" or reasoning steps from the model, if supported and enabled.
    *   `usage()`: Optional token usage information (`querymt::Usage`).
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`querymt::chat::ChatCall`**: The single request value every chat entry point takes. It carries the `messages`, optional `tools` and per-request `GenerationOptions`. Build it with `ChatCall::new(&messages)`, then chain `.tools(&tools)` / `.maybe_tools(opt)` and `.options(opts)`. A new request parameter is a new field here, never a new provider method.

*   **`querymt::chat::ChatProvider`**: The trait LLM providers implement. It has three methods:
    *   `chat(&self, call: &ChatCall<'_>)`: Sends one request and returns a `ChatOutput`.
    *   `chat_stream(&self, call: &ChatCall<'_>)`: Streams the response as `StreamChunk`s (returns `NotImplemented` unless the provider supports streaming).
    *   `capabilities(&self)`: Reports a `ChatCapabilities { streaming, options }` so callers can check streaming and per-request option support before sending.
    *   Source: `crates/querymt/src/chat/mod.rs`

## How It Works

1.  **Construct Messages:** Your application assembles a sequence of `ChatMessage` objects representing the conversation history. Use `ChatMessage::user()` for user messages and `ChatMessage::assistant()` for assistant messages. This typically starts with alternating `User` and `Assistant` messages.
2.  **Initiate Chat:** You call `chat` (or `chat_stream`) on an `LLMProvider` instance with a `ChatCall` holding the message history and, optionally, the available tools.
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
use querymt::chat::{ChatCall, ChatMessage, ChatMessageBuilder, ChatResponse, MessageType, Tool, ToolCall};
use querymt::LLMProvider;
use serde_json::Value;

// Assuming 'llm_provider' is an instance of Box<dyn LLMProvider> that has tools registered
// and 'my_tools' is a Vec<Tool> describing them.

async fn handle_tool_calling_loop(
    llm_provider: &Box<dyn LLMProvider>,
    initial_messages: Vec<ChatMessage>,
    my_tools: &[Tool],
) -> anyhow::Result<String> {
    let mut messages = initial_messages;

    loop {
        let response = llm_provider
            .chat(&ChatCall::new(&messages).tools(my_tools))
            .await?;

            // Add the assistant's response to the history. It might contain text and/or tool calls.
            let mut msg_builder = ChatMessage::assistant().text(response.text().unwrap_or_default());
            if let Some(tool_calls) = response.tool_calls() {
                for tc in tool_calls {
                    let args: serde_json::Value = serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                    msg_builder = msg_builder.tool_use(tc.id.clone(), tc.function.name.clone(), args);
                }
            }
            if let Some(thinking) = response.thinking() {
                msg_builder = msg_builder.thinking(thinking);
            }
            messages.push(msg_builder.build());

        if let Some(tool_calls) = response.tool_calls() {
            if tool_calls.is_empty() {
                // No tool calls, so the text response is the final answer.
                return Ok(response.text().unwrap_or_default());
            }

            // The model wants to call one or more tools.
            let mut tool_results = Vec::new();
            for call in tool_calls {
                println!("LLM wants to call tool: {} with args: {}", call.function.name, call.function.arguments);

                // In a real app, you would dispatch to your tool execution logic here.
                // The `LLMProvider` trait has a `call_tool` helper for this.
                let args: Value = serde_json::from_str(&call.function.arguments)?;
                let result_str = llm_provider.call_tool(&call.function.name, args).await?;

                // Create a ToolCall struct containing the result.
                // Note: The result is placed in the `arguments` field for transport.
                let result_call = ToolCall {
                    id: call.id,
                    call_type: "function".to_string(),
                    function: querymt::FunctionCall {
                        name: call.function.name,
                        arguments: result_str,
                    },
                };
                tool_results.push(result_call);
            }

            // Add the tool results back to the conversation history.
            messages.push(ChatMessage::user().tool_result(tool_results).build());
            // Loop again to let the model process the tool results.
        } else {
            // No tool calls in the response, so the text is the final answer.
            return Ok(response.text().unwrap_or_default());
        }
    }
}
```

## Per-request generation options

Some settings, such as the output-token cap or the reasoning effort, are normally fixed when a provider is built. `querymt::chat::GenerationOptions` overrides them for a **single** chat request, without rebuilding the provider:

```rust
use querymt::chat::{ChatCall, GenerationOptions, ReasoningEffort};

let options = GenerationOptions::default()
    .with_max_tokens(128_000)
    .with_reasoning_effort(ReasoningEffort::Low);

// `chat_stream` takes the same `ChatCall`.
let output = provider
    .chat(&ChatCall::new(&messages).tools(&tools).options(options))
    .await?;

// The next call, made without options, uses the configured values again.
let output = provider.chat(&ChatCall::new(&messages).tools(&tools)).await?;
```

*   A set field overrides the configured value for that request only. An unset field leaves the configured value in effect.
*   Providers report what they honour through `ChatProvider::capabilities()`; its `options` field is a `GenerationOptionSupport`. Query it before relying on an override. Options a provider does not honour are ignored and the request still succeeds. `GenerationOptionSupport::retain_supported` drops the unsupported overrides from a `GenerationOptions` value.
*   A call without options produces a request identical to one made before options existed.
*   Wrappers (tool and validation decorators, the HTTP adapter, mesh and remote providers, the Extism host) forward the whole `ChatCall`, so options and any future field cannot be dropped silently.
*   Plugins and remote peers built against an older contract are rejected with an actionable "rebuild the plugin" / "upgrade both peers" error rather than silently ignoring options. A remote host answers the contract handshake with the capabilities of the provider it builds for the request.
*   Today the Anthropic provider honours both fields. Other providers report no support until they are updated.

Source: `crates/querymt/src/chat/mod.rs`

QueryMT's chat system is designed to be flexible, supporting simple Q&A, complex multi-turn dialogues, and sophisticated interactions involving external tools. The `Tool` and `ToolChoice` mechanisms provide fine-grained control over how LLMs can utilize functions.
