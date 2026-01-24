# Plugin Interface Specification

To be compatible with QueryMT, a plugin must conform to a specific interface. The interface differs depending on whether you are building a **Native Plugin** (a shared library) or an **Extism Plugin** (a Wasm module).

---

## 1. Native Plugin Interface

Native plugins are Rust `cdylib` crates that implement traits from the `querymt` library and export a factory function with a C ABI. This allows the host to load the library and construct a provider instance with type safety and high performance.

### Core Traits

Your plugin must implement one of the following factory traits:

-   **`querymt::plugin::http::HTTPLLMProviderFactory`**: The **recommended** and simplest approach for providers that communicate over HTTP. The host will wrap this in an adapter to handle the async HTTP calls.
-   **`querymt::plugin::LLMProviderFactory`**: A more advanced, fully async trait for providers that have non-standard communication needs or do not use HTTP.

### Exported Factory Function

Your library **must** export one of the following C-ABI functions. The host looks for these in order.

1.  **`plugin_http_factory`** (if you implement `HTTPLLMProviderFactory`)
    ```rust
    use querymt::plugin::http::{HTTPLLMProviderFactory, HTTPFactoryCtor};

    struct MyFactory;
    // ... impl HTTPLLMProviderFactory for MyFactory ...

    #[no_mangle]
    pub unsafe extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
        Box::into_raw(Box::new(MyFactory))
    }
    ```

2.  **`plugin_factory`** (if you implement `LLMProviderFactory`)
    ```rust
    use querymt::plugin::{LLMProviderFactory, FactoryCtor};

    struct MyAsyncFactory;
    // ... impl LLMProviderFactory for MyAsyncFactory ...

    #[no_mangle]
    pub unsafe extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
        Box::into_raw(Box::new(MyAsyncFactory))
    }
    ```

### `HTTPLLMProviderFactory` Trait Methods

When using the recommended HTTP-based approach, you need to implement these methods:

-   `name() -> &str`: Returns the display name of the provider.
-   `config_schema() -> Value`: Returns a `serde_json::Value` representing the JSON schema for the plugin's configuration.
-   `from_config(&Value) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn Error>>`: Validates the user's configuration and creates an instance of your provider struct that implements the `HTTPLLMProvider` trait.
-   `list_models_request(&Value) -> Result<Request<Vec<u8>>, LLMError>`: Constructs an `http::Request` to fetch the list of available models.
-   `parse_list_models(Response<Vec<u8>>) -> Result<Vec<String>, Box<dyn Error>>`: Parses the `http::Response` from the models list request into a vector of model names.
-   `api_key_name() -> Option<String>`: (Optional) Returns the name of an environment variable for an API key.

Your provider struct created in `from_config` will then need to implement `HTTPChatProvider`, `HTTPEmbeddingProvider`, and `HTTPCompletionProvider`. See the [Native Plugin Development Guide](development.md#developing-native-plugins) for a full example.

---

## 2. Extism (Wasm) Plugin Interface

Extism plugins are Wasm modules that export a set of functions that the host calls. Data is passed between the host and plugin as JSON-encoded byte arrays. The `impl_extism_http_plugin!` macro can generate all of these exports for you.

### Core Exported Functions

1.  **`name() -> String`**: Returns the human-readable name of the plugin.
2.  **`api_key_name() -> Option<String>`**: Returns the name of an environment variable for an API key.
3.  **`config_schema() -> String`**: Returns a JSON string representing the JSON Schema for the plugin's configuration.
4.  **`from_config(config: Json<YourConfigType>) -> Result<Json<YourConfigType>, Error>`**: Validates the plugin-specific configuration.
5.  **`list_models(config: Json<serde_json::Value>) -> Result<Json<Vec<String>>, Error>`**: Dynamically lists available models, usually by making an HTTP request from within the Wasm module.
6.  **`base_url() -> String`**: Returns the default base URL for the provider, used by the host to configure network access for the sandbox.

### LLM Operation Functions

These functions handle the core LLM tasks. Their inputs are wrapper structs that bundle the configuration with the request data.

7.  **`chat(input: ExtismChatRequest<YourConfigType>) -> Result<Json<ExtismChatResponse>, Error>`**: Handles chat completion requests.
8.  **`embed(input: ExtismEmbedRequest<YourConfigType>) -> Result<Json<Vec<Vec<f32>>>, Error>`**: Generates embeddings.
9.  **`complete(input: ExtismCompleteRequest<YourConfigType>) -> Result<Json<CompletionResponse>, Error>`**: Handles text completion.

### Optional Speech Functions

These exports are optional. If absent, the host will treat the capability as unsupported.

10. **`transcribe(input: ExtismSttRequest<YourConfigType>) -> Result<Json<ExtismSttResponse>, Error>`**: Speech-to-text transcription.

See [Data Structures](data_structures.md) for details on the `Extism*` request/response types.
