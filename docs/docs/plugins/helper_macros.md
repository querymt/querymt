# Plugin Development Helper Macros

QueryMT provides macros to simplify the development of Extism plugins in Rust, especially for common patterns like HTTP-based LLM providers.

## `impl_binary_codec!($Type)`

-   **Source**: `crates/querymt/src/plugin/extism_impl/wrapper.rs`
-   **Purpose**: Implements the `BinaryCodec` and `FromBytesOwned` traits for a given struct. This enables easy serialization to and deserialization from JSON byte arrays, which is the primary way data is exchanged between the Extism host and plugin.
-   **Usage**:
    ```rust
    use querymt::plugin::extism_impl::impl_binary_codec;
    use serde::{Serialize, Deserialize};

    #[derive(Serialize, Deserialize)]
    struct MyData {
        field: String,
    }
    impl_binary_codec!(MyData);

    #[derive(Serialize, Deserialize)]
    struct MyGenericData<T> {
        data: T,
    }
    impl_binary_codec!(MyGenericData<T>); // For generic types
    ```
-   **Details**:
    -   The macro has two forms: one for non-generic types (e.g., `MyData`) and one for types with a single generic parameter (e.g., `MyGenericData<C>`).
    -   It relies on `serde_json` for serialization/deserialization.
    -   The types `ExtismChatRequest<C>`, `ExtismEmbedRequest<C>`, and `ExtismCompleteRequest<C>` already have this implemented in `querymt`.

## `impl_extism_http_plugin!`

-   **Source**: `crates/querymt/src/plugin/extism_impl/wrapper.rs`
-   **Purpose**: Generates all the necessary Extism plugin exports (`#[plugin_fn]`) for an LLM provider that interacts with an external service via HTTP. This significantly reduces boilerplate code.
-   **Usage**:
    ```rust
    use querymt::plugin::extism_impl::impl_extism_http_plugin;
    use serde::{Serialize, Deserialize};
    use schemars::JsonSchema;
    // ... other necessary imports for your provider traits ...
    # use querymt::chat::http::HTTPChatProvider;
    # use querymt::completion::http::HTTPCompletionProvider;
    # use querymt::embedding::http::HTTPEmbeddingProvider;
    # use querymt::plugin::http::HTTPLLMProviderFactory;
    # use querymt::{CompletionRequest, CompletionResponse, ChatMessage, ChatResponse, Tool, ToolCall};

    #[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
    pub struct MyPluginConfig { /* ... fields ... */
    # pub api_key: String, base_url: String, model_name: Option<String>,
    }
    # impl Default for MyPluginConfig { fn default() -> Self { Self{api_key: "".into(), base_url: "".into(), model_name: None} } }
    # impl HTTPChatProvider for MyPluginConfig { /* ... */ fn chat_request(&self, _: &[ChatMessage], _: Option<&[Tool]>) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> { todo!() } fn parse_chat(&self, _: http::Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, querymt::error::LLMError> { todo!() } }
    # impl HTTPEmbeddingProvider for MyPluginConfig { /* ... */ fn embed_request(&self, _: &[String]) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> { todo!() } fn parse_embed(&self, _: http::Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, querymt::error::LLMError> { todo!() } }
    # impl HTTPCompletionProvider for MyPluginConfig { /* ... */ fn complete_request(&self, _: &CompletionRequest) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> { todo!() } fn parse_complete(&self, _: http::Response<Vec<u8>>) -> Result<CompletionResponse, querymt::error::LLMError> { todo!() } }


    pub struct MyPluginFactory;
    impl HTTPLLMProviderFactory for MyPluginFactory { /* ... */
    # fn name(&self) -> &str { "test" }
    # fn api_key_name(&self) -> Option<String> { None }
    # fn config_schema(&self) -> serde_json::Value { serde_json::Value::Null }
    # fn from_config(&self, _cfg: &serde_json::Value) -> Result<Box<dyn querymt::LLMProvider>, querymt::error::LLMError> { todo!() }
    # fn list_models_request(&self, _cfg: &serde_json::Value) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> { todo!() }
    # fn parse_list_models(&self, _resp: http::Response<Vec<u8>>) -> Result<Vec<String>, Box<dyn std::error::Error>> { todo!() }
    }

    impl_extism_http_plugin!(
        config = MyPluginConfig,         // Your config struct
        factory = MyPluginFactory,       // Your factory struct implementing HTTPLLMProviderFactory
        name = "My Awesome HTTP LLM"     // Display name for the plugin
    );
    ```
-   **Parameters**:
    -   `config = $ConfigType`: The Rust struct you define for this plugin's specific configuration (e.g., `MyPluginConfig`). This struct must derive `Serialize`, `Deserialize`, `JsonSchema`, and `Clone`. It also needs to implement `HTTPChatProvider`, `HTTPEmbeddingProvider`, and `HTTPCompletionProvider` traits from `querymt`.
    -   `factory = $FactoryType`: A unit struct (or any struct) that implements the `HTTPLLMProviderFactory` trait from `querymt`. This factory is responsible for tasks like providing the API key name, constructing HTTP requests for listing models, and parsing their responses.
    -   `name = $PluginNameExpr`: A string expression for the human-readable name of the plugin.
-   **Generated Exports**:
    -   `name()`
    -   `api_key_name()` (delegates to `$FactoryType::api_key_name()`)
    -   `config_schema()` (generates JSON schema from `$ConfigType`)
    -   `from_config()` (validates `$ConfigType`)
    -   `list_models()` (uses `$FactoryType` to make HTTP request and parse response)
    -   `base_url()` (uses `<$ConfigType>::default_base_url()`)
    -   `chat()` (uses `$ConfigType::chat_request()` and `$ConfigType::parse_chat()`)
    -   `embed()` (uses `$ConfigType::embed_request()` and `$ConfigType::parse_embed()`)
    -   `complete()` (uses `$ConfigType::complete_request()` and `$ConfigType::parse_complete()`)
-   **Requirements for `$ConfigType`**:
    -   Must derive `serde::Serialize`, `serde::Deserialize`, `schemars::JsonSchema`, `Clone`.
    -   Must implement `Default` (or provide `default_base_url()` static method if not using `Default` for base_url).
    -   Must implement `querymt::chat::http::HTTPChatProvider`.
    -   Must implement `querymt::embedding::http::HTTPEmbeddingProvider`.
    -   Must implement `querymt::completion::http::HTTPCompletionProvider`.
-   **Requirements for `$FactoryType`**:
    -   Must implement `querymt::plugin::http::HTTPLLMProviderFactory`.

