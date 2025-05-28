# Tools & Function Calling

One of the powerful features of modern Large Language Models is their ability to use "tools" or "call functions." This allows LLMs to interact with external systems, APIs, or data sources to gather information or perform actions, making them much more capable and grounded in real-world data. QueryMT provides robust support for defining and using tools.

## Key Concepts

*   **`Tool`**: A struct representing a tool that the LLM can use. It primarily describes a function.
    *   `tool_type`: Currently, this is typically `"function"`.
    *   `function`: A `FunctionTool` struct detailing the function.
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`FunctionTool`**: Describes a specific function the LLM can call.
    *   `name`: The name of the function.
    *   `description`: A natural language description of what the function does, its parameters, and when to use it. This is crucial for the LLM to understand the tool's purpose.
    *   `parameters`: A `ParametersSchema` defining the expected input arguments for the function, typically in JSON Schema format.
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`ToolCall`**: When an LLM decides to use a tool, its response will include one or more `ToolCall` objects.
    *   `id`: A unique ID for this specific tool call instance.
    *   `call_type`: Usually `"function"`.
    *   `function`: A `FunctionCall` struct.
    *   Source: `crates/querymt/src/lib.rs`

*   **`FunctionCall`**: Details of the function the LLM wants to invoke.
    *   `name`: The name of the function to call.
    *   `arguments`: A string containing the arguments for the function, typically as a JSON object.
    *   Source: `crates/querymt/src/lib.rs`

*   **`ToolChoice`**: An enum that allows you to specify how the LLM should use the provided tools (e.g., `Auto` to let the model decide, `Any` to force tool use, `Tool(name)` to force a specific tool, or `None` to disable tools).
    *   Source: `crates/querymt/src/chat/mod.rs`

*   **`CallFunctionTool`**: A trait that your *host-side Rust code* must implement for each function you want to make available to the LLM.
    *   `descriptor()`: Returns the `Tool` definition (schema) for this function.
    *   `call(&self, args: Value)`: The actual Rust async function that gets executed when the LLM calls this tool. It receives parsed JSON arguments and should return a string result.
    *   Source: `crates/querymt/src/tool_decorator.rs`

*   **`ToolEnabledProvider`**: A decorator struct that wraps an `LLMProvider`. When you register tools using `LLMBuilder::add_tool()`, the builder automatically wraps the base provider with `ToolEnabledProvider`. This wrapper manages the registered tools and handles the two-way communication:
    1.  It passes the tool descriptors to the LLM during a `chat_with_tools` call.
    2.  If the LLM responds with a `ToolCall`, `ToolEnabledProvider` looks up the corresponding `CallFunctionTool` implementation by name and invokes its `call` method.
    *   Source: `crates/querymt/src/tool_decorator.rs`

## Workflow

1.  **Define Tools:**
    *   Implement the `CallFunctionTool` trait for each Rust function you want to expose.
    *   In the `descriptor()` method, accurately describe the function's purpose and parameters using `Tool` and `FunctionTool`.

2.  **Register Tools:**
    *   When building your `LLMProvider` using `LLMBuilder`, use the `add_tool()` method to register instances of your `CallFunctionTool` implementations.

3.  **Chat with Tools:**
    *   Use the `chat_with_tools()` method on the `LLMProvider`. The `ToolEnabledProvider` (if tools were added) will automatically pass the descriptors of registered tools to the LLM.
    *   You can use `ToolChoice` to guide the LLM's tool usage.

4.  **LLM Decides to Call a Tool:**
    *   The LLM, based on the conversation and tool descriptions, might decide to call one or more tools. Its response (via `ChatResponse::tool_calls()`) will contain `ToolCall` objects.

5.  **Application Executes Tool:**
    *   Your application receives the `ToolCall`s.
    *   The `LLMProvider` itself (if it's a `ToolEnabledProvider`) can handle the dispatch via its `call_tool(name, args)` method, or your application can manage this explicitly. This involves:
        *   Parsing the `arguments` string (usually JSON) into the expected types for your Rust function.
        *   Calling the actual Rust function logic.

6.  **Return Tool Result to LLM:**
    *   Construct a new `ChatMessage` with `MessageType::ToolResult`. This message should include the `id` from the original `ToolCall` and the string result of the function execution.
    *   Send this message (along with the conversation history) back to the LLM using `chat_with_tools()`.

7.  **LLM Continues:**
    *   The LLM uses the tool's output to formulate its final response or decide on further actions (which might include calling another tool).

## Example (Conceptual `CallFunctionTool` Implementation)

```rust
use querymt::tool_decorator::CallFunctionTool;
use querymt::chat::{Tool, FunctionTool, ParametersSchema, ParameterProperty};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;

struct GetWeatherTool;

#[async_trait]
impl CallFunctionTool for GetWeatherTool {
    fn descriptor(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: "get_current_weather".to_string(),
                description: "Get the current weather in a given location".to_string(),
                parameters: ParametersSchema {
                    schema_type: "object".to_string(),
                    properties: {
                        let mut props = HashMap::new();
                        props.insert(
                            "location".to_string(),
                            ParameterProperty {
                                property_type: "string".to_string(),
                                description: "The city and state, e.g. San Francisco, CA".to_string(),
                                items: None,
                                enum_list: None,
                            },
                        );
                        props
                    },
                    required: vec!["location".to_string()],
                },
            },
        }
    }

    async fn call(&self, args: Value) -> anyhow::Result<String> {
        let location = args.get("location").and_then(Value::as_str).unwrap_or_default();
        // In a real scenario, call a weather API here
        Ok(json!({ "weather": format!("Sunny in {}", location) }).to_string())
    }
}

// To use it:
// let builder = LLMBuilder::new().provider("some_provider").add_tool(GetWeatherTool);
// let llm = builder.build(&registry)?;
// ... then use llm.chat_with_tools(...) ...
```

Tool usage significantly enhances the capabilities of LLMs, allowing them to perform complex tasks that require external information or actions. QueryMT's system provides a structured way to integrate these tools into your LLM applications.

