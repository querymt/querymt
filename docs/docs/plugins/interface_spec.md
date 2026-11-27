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
-   `account_request(&str) -> Result<Option<Request<Vec<u8>>>, LLMError>`, `parse_account(Response<Vec<u8>>) -> Result<ProviderAccount, LLMError>` and `account_hosts() -> Vec<String>`: (Optional) Report plan, usage limits and credits for the credential in the configuration. See [Account snapshots](#account-snapshots).

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

11. **`speech(input: ExtismTtsRequest<YourConfigType>) -> Result<Json<ExtismTtsResponse>, Error>`**: Text-to-speech synthesis.

### Optional Account Functions

Generated by `impl_extism_http_plugin!`. A plugin built without them answers every account query as unsupported.

12. **`account_request(input: Json<ExtismAccountRequest>) -> Result<Json<Option<SerializableHttpRequest>>, Error>`**: Builds the account request for the resolved configuration, or `null` when this credential has no account data.
13. **`parse_account_response(input: Json<ExtismAccountParseRequest>) -> Result<Json<ProviderAccount>, Error>`**: Maps the success response to a snapshot.
14. **`account_hosts() -> Json<Vec<String>>`**: Hosts besides the base-URL host that account requests may target.

See [Data Structures](data_structures.md) for details on the `Extism*` request/response types.

## 3. Account snapshots

A provider can report what it knows about the credential it runs under: an optional `plan`, a list of usage `limits` and optional `credits`. Clients read the result through the `querymt/auth/account` ACP method.

```json
{
  "plan": "pro",
  "limits": [
    {"id": "primary", "used_percent": 42.0, "window_seconds": 604800, "resets_at": 1795798467, "reached": false},
    {"id": "weekly:Api", "scope": "Api", "used_percent": 15.0, "reached": false}
  ],
  "credits": {"balance": "12.34", "currency": "USD", "unlimited": false}
}
```

-   `used_percent` is never negative and may exceed 100. `resets_at` is Unix seconds (UTC). `reached` is the provider's own blocking flag, or `used_percent >= 100`. `balance` is a decimal string.
-   Build values with `ProviderAccount::new()`, `UsageLimit::new(id, percent)` and `CreditBalance::new()` and their `with_*` methods; the structs are `#[non_exhaustive]`. `querymt::plugin::account::parse_rfc3339_utc` converts UTC RFC 3339 timestamps.
-   **Decline per credential.** Return `Ok(None)` from `account_request` when the configured credential has no account data, for example an API key on a provider that reports usage only for subscription logins. The host performs the request; a non-success status is classified like model listing, so `parse_account` only sees success responses.
-   **Tolerate missing fields.** Account endpoints are often undocumented. Treat every mapped field as optional, so a dropped field removes one value instead of failing the snapshot, and return `LLMError::ResponseFormatError` (with an empty `raw_response`) only for a body that is not the expected JSON shape. Response bodies can contain personal data and must not be echoed in errors.
-   **Hosts.** For Wasm plugins the host issues an account request only to an allowed host (the base-URL host or `[providers.config].allowed_hosts`) or to a host returned by `account_hosts()`. Declare a host there only when the account endpoint is not on the base-URL host; declared hosts are used for account requests only and do not widen what the plugin itself may reach.
