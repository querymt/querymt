# Plugin Development

This guide provides instructions for developing both **Native** and **Extism (Wasm)** plugins for QueryMT.

---

## Developing Native Plugins

Native plugins offer the best performance by running as shared libraries directly within the host process. They are recommended for trusted, performance-critical integrations.

### 1. Prerequisites

-   Rust Toolchain: Install Rust from [rust-lang.org](https://www.rust-lang.org/).

### 2. Project Setup

1.  **Create a new Rust library project**:
    ```bash
    cargo new my_native_plugin --lib
    cd my_native_plugin
    ```

2.  **Update `Cargo.toml`**:
    Configure the crate to build a dynamic system library (`cdylib`).

    ```toml
    [package]
    name = "my_native_plugin"
    version = "0.1.0"
    edition = "2021"

    [lib]
    crate-type = ["cdylib"] # Important for shared libraries

    [dependencies]
    querymt = { path = "../../..", features = ["http-client"] } # Adjust path
    serde = { version = "1.0", features = ["derive"] }
    serde_json = "1.0"
    schemars = "0.8"
    http = "0.2"
    # Add any other dependencies your provider needs
    ```
    *Note: The `querymt` dependency does **not** use the `extism_plugin` feature.*

### 3. Implementing the Plugin

You will implement the `HTTPLLMProviderFactory` trait and export it via the `plugin_http_factory` function.

```rust
// src/lib.rs
use querymt::plugin::http::{HTTPLLMProviderFactory, HTTPFactoryCtor};
use querymt::chat::http::HTTPChatProvider;
use querymt::completion::http::HTTPCompletionProvider;
use querymt::embedding::http::HTTPEmbeddingProvider;
use querymt::{
    HTTPLLMProvider, CompletionRequest, CompletionResponse,
    ChatMessage, ChatResponse, Tool, ToolCall, LLMError
};
use serde::{Serialize, Deserialize};
use schemars::{JsonSchema, schema_for};
use std::collections::HashMap;
use std::error::Error;

// 1. Define your plugin's configuration structure
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct MyPluginConfig {
    pub api_key: String,
    pub model_name: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}
fn default_base_url() -> String { "https://api.examplellm.com/v1".to_string() }

// 2. Define your provider struct. It holds the config.
#[derive(Clone)]
pub struct MyProvider {
    config: MyPluginConfig,
}

// 3. Implement the core HTTP provider traits for your provider struct.
// This defines how to build requests and parse responses.
impl HTTPChatProvider for MyProvider {
    // ... implement chat_request() and parse_chat() ...
    fn chat_request(&self, messages: &[ChatMessage], _tools: Option<&[Tool]>) -> Result<http::Request<Vec<u8>>, LLMError> { /* ... */ Ok(http::Request::default()) }
    fn parse_chat(&self, resp: http::Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, Box<dyn Error>> { /* ... */ Ok(Box::new(querymt::completion::CompletionResponse{text:"...".into()})) }
}
impl HTTPEmbeddingProvider for MyProvider { /* ... */
    fn embed_request(&self, inputs: &[String]) -> Result<http::Request<Vec<u8>>, LLMError> { Ok(http::Request::default()) }
    fn parse_embed(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, Box<dyn Error>> { Ok(vec![]) }
}
impl HTTPCompletionProvider for MyProvider { /* ... */
    fn complete_request(&self, req: &CompletionRequest) -> Result<http::Request<Vec<u8>>, LLMError> { Ok(http::Request::default()) }
    fn parse_complete(&self, resp: http::Response<Vec<u8>>) -> Result<CompletionResponse, Box<dyn Error>> { Ok(CompletionResponse{text:"...".into()}) }
}

// This blanket impl turns your provider struct into an HTTPLLMProvider
impl HTTPLLMProvider for MyProvider {}

// 4. Implement the factory, which knows how to create your provider.
pub struct MyFactory;

impl HTTPLLMProviderFactory for MyFactory {
    fn name(&self) -> &str {
        "My Native HTTP Plugin"
    }

    fn config_schema(&self) -> serde_json::Value {
        serde_json::to_value(schema_for!(MyPluginConfig)).unwrap()
    }

    fn from_config(&self, cfg: &serde_json::Value) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn Error>> {
        let config: MyPluginConfig = serde_json::from_value(cfg.clone())?;
        let provider = MyProvider { config };
        Ok(Box::new(provider))
    }

    // Implement list_models_request, parse_list_models, api_key_name...
    fn list_models_request(&self, cfg: &serde_json::Value) -> Result<http::Request<Vec<u8>>, LLMError> { Ok(http::Request::default()) }
    fn parse_list_models(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<String>, Box<dyn Error>> { Ok(vec![]) }
}

// 5. Export the factory constructor function. This is the entry point for the host.
#[no_mangle]
pub unsafe extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(MyFactory))
}
```

### 4. Building the Plugin

Compile your Rust library into a shared object:
```bash
cargo build --release
```
The compiled library will be at `target/release/libmy_native_plugin.so` (or `.dll`/`.dylib`). This is the file you configure in `plugins.toml`.

---

## Developing Extism (Wasm) Plugins

Extism plugins provide security and portability by running in a Wasm sandbox.

### 1. Prerequisites

-   Rust Toolchain: Install Rust from [rust-lang.org](https://www.rust-lang.org/).
-   Wasm Target: Add the Wasm target: `rustup target add wasm32-wasip1`.

### 2. Project Setup

1.  **Create a new Rust library project**:
    ```bash
    cargo new my_extism_plugin --lib
    cd my_extism_plugin
    ```

2.  **Update `Cargo.toml`**:
    ```toml
    [package]
    name = "my_extism_plugin"
    # ...

    [lib]
    crate-type = ["cdylib"]

    [dependencies]
    extism-pdk = "1.0.0"
    serde = { version = "1.0", features = ["derive"] }
    serde_json = "1.0"
    schemars = "0.8"
    querymt = { path = "../../..", features = ["extism_plugin"] } # Note the feature

    [profile.release]
    lto = true
    opt-level = 'z'
    strip = true
    ```

### 3. Implementing the Plugin

The easiest way to create an HTTP-based Wasm plugin is using the `impl_extism_http_plugin!` macro. The implementation logic for the HTTP traits is identical to the native plugin example, but it's all wrapped in the macro.

```rust
// src/lib.rs
use querymt::plugin::extism_impl::impl_extism_http_plugin;
use querymt::chat::http::HTTPChatProvider;
use querymt::plugin::http::HTTPLLMProviderFactory;
// ... other trait imports

// 1. Define your config struct (same as native example)
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone, Debug)]
pub struct MyPluginConfig { /* ... */ }
// ... with default_base_url() function

// 2. Implement the HTTP provider traits for your config struct
impl HTTPChatProvider for MyPluginConfig { /* ... */ }
// ... HTTPEmbeddingProvider, HTTPCompletionProvider ...

// 3. Create a marker struct for your factory logic
struct MyPluginFactory;

// 4. Implement the factory trait for the marker struct
impl HTTPLLMProviderFactory for MyPluginFactory { /* ... */ }

// 5. Use the macro to export all necessary Extism functions
impl_extism_http_plugin!(
    config = MyPluginConfig,
    factory = MyPluginFactory,
    name = "My Example Extism HTTP Plugin"
);
```

### 4. Building the Plugin

Compile your Rust library to Wasm:
```bash
cargo build --target wasm32-wasip1 --release
```
The Wasm file will be at `target/wasm32-wasip1/release/my_extism_plugin.wasm`. This is the file you configure in `plugins.toml`.
