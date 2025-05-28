# Plugin Interface Specification

QueryMT Extism plugins must export a specific set of functions to be compatible with the host system. If you use the `impl_extism_http_plugin!` macro, these are generated for you. However, understanding them is useful.

All interaction with the plugin happens by calling these exported Wasm functions. Data is typically passed as JSON-encoded byte arrays.

## Core Exported Functions

1.  **`name() -> String`**
    -   **Input**: None
    -   **Output**: `String` - The human-readable name of the plugin.
    -   **Description**: Returns the display name of the LLM provider.

2.  **`api_key_name() -> Option<String>`**
    -   **Input**: None
    -   **Output**: `Option<String>` - The name of an environment variable the plugin expects for an API key.
    -   **Description**: If the plugin requires an API key passed via an environment variable, this function should return the name of that variable (e.g., "OPENAI_API_KEY"). The host can use this to inform the user or to potentially pre-populate configurations. If no such environment variable is used (e.g., API key is part of the `config` struct), return `None` or an empty string.

3.  **`config_schema() -> String`**
    -   **Input**: None
    -   **Output**: `String` - A JSON string representing the JSON schema for the plugin's specific configuration.
    -   **Description**: Provides a schema for the `cfg` field within `ExtismChatRequest`, `ExtismEmbedRequest`, and `ExtismCompleteRequest`, and for the configuration passed to `from_config` and `list_models`. The host can use this for validation or generating UI for plugin configuration.
        The `impl_extism_http_plugin!` macro generates this from your `$Config` struct if it derives `schemars::JsonSchema`.

4.  **`from_config(config: Json<YourConfigType>) -> Result<Json<YourConfigType>, Error>`**
    -   **Input**: `Json<YourConfigType>` - Plugin-specific configuration as a JSON object. `YourConfigType` is the concrete type for this plugin's config.
    -   **Output**: `Json<YourConfigType>` - The validated configuration (usually the same as input), or an error if validation fails.
    -   **Description**: Called by the host to validate the plugin-specific configuration. The plugin should deserialize the input, perform any necessary checks (e.g., required fields, value ranges), and return the configuration if valid. The `impl_extism_http_plugin!` macro handles basic deserialization; you can add custom validation logic if needed by implementing `HTTPLLMProviderFactory::from_config` on your factory struct.

5.  **`list_models(config: Json<serde_json::Value>) -> Result<Json<Vec<String>>, Error>`**
    -   **Input**: `Json<serde_json::Value>` - Plugin-specific configuration as a JSON object.
    -   **Output**: `Json<Vec<String>>` - A list of model names available through this provider with the given configuration.
    -   **Description**: Dynamically lists available models. For HTTP plugins, this usually involves making an API call.

6.  **`base_url() -> String`**
    -   **Input**: None
    -   **Output**: `String` - The default base URL for the provider.
    -   **Description**: Returns a default base URL associated with the LLM provider (e.g., "https://api.openai.com/v1"). This can be used by the host to set up network permissions (allowed hosts).

## LLM Operations Functions

These functions handle the core LLM tasks. Their inputs are wrapper structs (`ExtismChatRequest`, etc.) that include both the plugin-specific configuration (`cfg`) and the actual request data. These structs are serialized as JSON by the host and deserialized by the plugin (handled by `BinaryCodec` and `extism-pdk`).

7.  **`chat(input: ExtismChatRequest<YourConfigType>) -> Result<Json<ExtismChatResponse>, Error>`**
    -   **Input**: `ExtismChatRequest<YourConfigType>` (serialized as JSON bytes by host). Contains:
        -   `cfg: YourConfigType`
        -   `messages: Vec<ChatMessage>`
        -   `tools: Option<Vec<Tool>>`
    -   **Output**: `Json<ExtismChatResponse>` (serialized to JSON bytes by plugin). Contains:
        -   `text: Option<String>`
        -   `tool_calls: Option<Vec<ToolCall>>`
        -   `thinking: Option<String>`
    -   **Description**: Handles chat completion requests.

8.  **`embed(input: ExtismEmbedRequest<YourConfigType>) -> Result<Json<Vec<Vec<f32>>>, Error>`**
    -   **Input**: `ExtismEmbedRequest<YourConfigType>` (serialized as JSON bytes). Contains:
        -   `cfg: YourConfigType`
        -   `inputs: Vec<String>`
    -   **Output**: `Json<Vec<Vec<f32>>>` (serialized to JSON bytes).
    -   **Description**: Generates embeddings for a list of input strings.

9.  **`complete(input: ExtismCompleteRequest<YourConfigType>) -> Result<Json<CompletionResponse>, Error>`**
    -   **Input**: `ExtismCompleteRequest<YourConfigType>` (serialized as JSON bytes). Contains:
        -   `cfg: YourConfigType`
        -   `req: CompletionRequest`
    -   **Output**: `Json<CompletionResponse>` (serialized to JSON bytes).
    -   **Description**: Handles standard text completion requests.

See [Data Structures](data_structures.md) for details on the request/response types.

