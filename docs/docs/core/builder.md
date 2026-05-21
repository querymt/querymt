# Building Providers (LLMBuilder)

QueryMT uses a fluent builder pattern, embodied by the `querymt::builder::LLMBuilder`, to simplify the configuration and instantiation of `LLMProvider` instances. This approach allows you to chain configuration methods in a readable and expressive way.

The `LLMBuilder` is defined in `crates/querymt/src/builder.rs`.

## Core Functionality

The `LLMBuilder` allows you to set various common and provider-specific parameters before finally building the `LLMProvider`.

### Common Configuration Options:

*   **`provider(name: String)`**: Specifies the name of the LLM provider to use (e.g., "openai", "anthropic", "my-custom-plugin"). This name is used to look up the corresponding `LLMProviderFactory` from a `PluginRegistry`.
*   **`api_key(key: String)`**: Sets the API key for authentication with the provider.
*   **`base_url(url: String)`**: Sets a custom base URL, often used for self-hosted models or proxies.
*   **`model(model_id: String)`**: Specifies the model identifier to use (e.g., "gpt-4", "claude-2.1").
*   **`max_tokens(tokens: u32)`**: Sets the maximum number of tokens the LLM should generate in its response.
*   **`temperature(temp: f32)`**: Controls the randomness of the output (typically 0.0 to 1.0).
*   **`system(prompt: String)`**: Provides a system-level prompt or instructions to guide the LLM's behavior.
*   **`timeout_seconds(seconds: u64)`**: Sets a timeout for requests to the LLM provider.
*   **`stream(enable: bool)`**: Enables or disables streaming responses (if supported by the provider).
*   **`top_p(p: f32)`**, **`top_k(k: u32)`**: Parameters for nucleus and top-k sampling.
*   **`embedding_encoding_format(format: String)`**, **`embedding_dimensions(dims: u32)`**: Configuration for embedding generation.
*   **`schema(schema: impl Into<StructuredOutputFormat>)`**: Defines a JSON schema for structured output from the LLM. The parameter can be any type that implements `Into<StructuredOutputFormat>`.
*   **`validator(func: F)`**: Sets a custom validation function for LLM responses.
*   **`validator_attempts(attempts: usize)`**: Sets the number of retries if validation fails.
*   **`add_tool(tool: T)`**: Registers a tool (an implementation of `CallFunctionTool`) to be made available to the LLM. See [Tools & Function Calling](./tools.md).
*   **`tool_choice(choice: ToolChoice)`**: Specifies how the LLM should use tools (e.g., auto, force specific tool).
*   **`enable_parallel_tool_use(enable: bool)`**: Allows the model to call multiple tools in parallel, if supported.
*   **`reasoning(enable: bool)`**: Enables the model to output its reasoning or "thinking" steps.
*   **`reasoning_effort(effort: ReasoningEffort)`**: Specifies the desired level of reasoning effort (`Low`, `Medium`, `High`).
*   **`reasoning_budget_tokens(tokens: u32)`**: Allocates a token budget for the model's reasoning process.
*   **`parameter(key: String, value: Value)`**: Allows setting arbitrary provider-specific parameters.

### Building the Provider

*   **`build(self) -> Result<Box<dyn LLMProvider>, LLMError>`** on a registry-bound builder:
    This is the final step when the builder came from `registry.builder(...)`.
    1.  It retrieves the appropriate `LLMProviderFactory` from the bound `PluginRegistry` based on the `provider` name set earlier.
    2.  It loads provider defaults from `[providers.config]` for that provider.
    3.  It injects an environment API key when the provider advertises one via `api_key_name()` and no explicit `api_key` was set.
    4.  It overlays explicit builder settings on top of those defaults, so builder setters win.
    5.  It prunes the merged configuration based on the schema provided by the factory, so only relevant options are passed.
    6.  It calls `factory.from_config()` with the pruned configuration to get a base `LLMProvider`.
    7.  If tools were added via `add_tool()`, it wraps the base provider in a `querymt::tool_decorator::ToolEnabledProvider`.
    8.  If a `validator` was set, it further wraps the provider in a `querymt::validated_llm::ValidatedLLM`.
    9.  Returns the fully configured `LLMProvider` instance, boxed as a trait object.

*   **`build_with(self, registry: &PluginRegistry) -> Result<Box<dyn LLMProvider>, LLMError>`** on an unbound builder:
    This is the equivalent final step for builders created via `querymt::provider(...)` or `LLMBuilder::new()`.

## Example Usage (Conceptual)

```rust
use querymt::plugin::host::PluginRegistry; // Assuming you have a registry instance
// Assume GetWeatherTool is an impl CallFunctionTool
// use my_tools::GetWeatherTool;

async fn setup_llm_provider(registry: &PluginRegistry) -> Result<Box<dyn querymt::LLMProvider>, querymt::error::LLMError> {
    let llm = registry
        .builder("openai") // Or your plugin's name
        .model("gpt-4-turbo")
        .api_key("YOUR_OPENAI_API_KEY")
        .temperature(0.7)
        .max_tokens(500)
        .system("You are a helpful assistant.")
        // .add_tool(GetWeatherTool) // Example of adding a tool
        // .tool_choice(ToolChoice::Auto)
        .validator(|response_text| {
            if response_text.to_lowercase().contains("sorry") {
                Err("Response should not be apologetic.".to_string())
            } else {
                Ok(())
            }
        })
        .validator_attempts(2)
        .build()
        .await?;
```


The `LLMBuilder` provides a convenient and type-safe way to configure diverse LLM providers with a wide range of options, abstracting away the underlying factory and wrapping logic.
