# Getting Started with Plugin Development

Developing an Extism plugin for QueryMT involves creating a WebAssembly (Wasm) module that implements a specific interface. Rust is a recommended language due to its strong Wasm support and the availability of helper crates like `extism-pdk` and QueryMT's own plugin utilities.

## Prerequisites

- **Rust Toolchain**: Install Rust from [rust-lang.org](https://www.rust-lang.org/).
- **Wasm Target**: Add the Wasm target: `rustup target add wasm32-wasip1`.
- **Extism PDK**: Familiarize yourself with the [Extism PDK for Rust](https://extism.org/docs/category/pdk-for-rust).

## Project Setup

1.  **Create a new Rust library project**:
    ```bash
    cargo new my_llm_plugin --lib
    cd my_llm_plugin
    ```

2.  **Update `Cargo.toml`**:
    Add necessary dependencies. For an HTTP-based plugin using QueryMT helpers:

    ```toml
    [package]
    name = "my_llm_plugin"
    version = "0.1.0"
    edition = "2021"

    [lib]
    crate-type = ["cdylib"] # Important for Wasm shared libraries

    [dependencies]
    extism-pdk = "1.0.0" # Or latest version
    serde = { version = "1.0", features = ["derive"] }
    serde_json = "1.0"
    schemars = "0.8" # For generating JSON schemas for config

    # QueryMT dependency (assuming it's published or path-based)
    # Replace with actual path or crates.io version when available
    querymt = { path = "../../..", features = ["extism_plugin"] }
    # or querymt = { version = "x.y.z", features = ["extism_plugin"] }

    [profile.release]
    lto = true
    opt-level = 'z' # Optimize for size
    strip = true
    ```
    *Note: The `querymt` dependency path/version needs to be adjusted based on your project structure or if it's published to crates.io. Ensure the `extism_plugin_impl` feature is enabled.*

## Implementing the Plugin

The easiest way to create an HTTP-based LLM plugin is by using the `impl_extism_http_plugin!` macro provided by QueryMT. This macro scaffolds most of the required boilerplate.

### Example: Simple HTTP Plugin

```rust
// src/lib.rs
use querymt::plugin::extism_impl::impl_extism_http_plugin;
use querymt::chat::http::HTTPChatProvider; // Traits for HTTP provider logic
use querymt::completion::http::HTTPCompletionProvider;
use querymt::embedding::http::HTTPEmbeddingProvider;
use querymt::plugin::http::HTTPLLMProviderFactory; // Trait for factory logic
use querymt::{CompletionRequest, CompletionResponse, ChatMessage, ChatResponse, Tool, ToolCall};
use serde::{Serialize, Deserialize};
use schemars::JsonSchema; // For config schema generation
use std::collections::HashMap; // For http::Request headers

// 1. Define your plugin's configuration structure
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct MyPluginConfig {
    pub api_key: String,
    pub model_name: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

fn default_base_url() -> String {
    "https://api.examplellm.com/v1".to_string()
}

// Implement default for config if needed by the factory or for simplicity
impl Default for MyPluginConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(), // Typically fetched from env var specified by api_key_name
            model_name: Some("default-model".to_string()),
            base_url: default_base_url(),
        }
    }
}

// 2. Implement the necessary provider traits for your config
// These traits define how to construct HTTP requests and parse responses.

// This is a marker struct for your factory implementation
struct MyPluginFactory;

impl HTTPLLMProviderFactory for MyPluginFactory {
    // Implement methods like api_key_name(), list_models_request(), parse_list_models()
    // For example:
    fn api_key_name(&self) -> Option<String> {
        Some("EXAMPLE_LLM_API_KEY".to_string())
    }

    fn list_models_request(&self, cfg: &serde_json::Value) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> {
        let config: MyPluginConfig = serde_json::from_value(cfg.clone())?;
        let request = http::Request::builder()
            .method("GET")
            .uri(format!("{}/models", config.base_url))
            .header("Authorization", format!("Bearer {}", config.api_key))
            .body(Vec::new())?;
        Ok(request)
    }

    fn parse_list_models(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        // Parse the HTTP response body (e.g., JSON) into Vec<String>
        // Example:
        // let body = serde_json::from_slice::<serde_json::Value>(resp.body())?;
        // let models = body["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        // Ok(models)
        Ok(vec!["example-model-1".to_string(), "example-model-2".to_string()])
    }
}

impl HTTPChatProvider for MyPluginConfig {
    // Implement chat_request() and parse_chat()
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> {
        let body = serde_json::json!({
            "model": self.model_name.as_deref().unwrap_or("default-model"),
            "messages": messages,
            // Add tools if your API supports them
        });
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body)?)?;
        Ok(request)
    }

    fn parse_chat(
        &self,
        resp: http::Response<Vec<u8>>,
    ) -> Result<Box<dyn ChatResponse>, querymt::error::LLMError> {
        // Parse the HTTP response and adapt it to Box<dyn ChatResponse>
        // Example:
        // let body = serde_json::from_slice::<serde_json::Value>(resp.body())?;
        // let text = body["choices"][0]["message"]["content"].as_str().map(String::from);
        // Ok(Box::new(querymt::plugin::extism_impl::ExtismChatResponse { text, tool_calls: None, thinking: None }))
        struct MyChatResponse { text: Option<String> }
        impl ChatResponse for MyChatResponse {
            fn text(&self) -> Option<String> { self.text.clone() }
            fn tool_calls(&self) -> Option<Vec<ToolCall>> { None }
            fn thinking(&self) -> Option<String> { None }
        }
        Ok(Box::new(MyChatResponse { text: Some(String::from_utf8_lossy(resp.body()).to_string()) }))
    }
}

// Implement HTTPEmbeddingProvider and HTTPCompletionProvider similarly if needed...
impl HTTPEmbeddingProvider for MyPluginConfig {
    fn embed_request(&self, inputs: &[String]) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> {
        // ... construct embedding request ...
        let body = serde_json::json!({ "inputs": inputs });
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body)?)?;
        Ok(request)
    }

    fn parse_embed(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, querymt::error::LLMError> {
        // ... parse embedding response ...
        // Example: let embeddings = serde_json::from_slice(resp.body())?;
        // Ok(embeddings)
        Ok(serde_json::from_slice(resp.body())?)
    }
}

impl HTTPCompletionProvider for MyPluginConfig {
    fn complete_request(&self, req: &CompletionRequest) -> Result<http::Request<Vec<u8>>, querymt::error::LLMError> {
        // ... construct completion request ...
        let body = serde_json::json!({ "prompt": req.prompt });
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("{}/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body)?)?;
        Ok(request)
    }

    fn parse_complete(&self, resp: http::Response<Vec<u8>>) -> Result<CompletionResponse, querymt::error::LLMError> {
        // ... parse completion response ...
        // Example: let completion_response = serde_json::from_slice(resp.body())?;
        // Ok(completion_response)
        Ok(serde_json::from_slice(resp.body())?)
    }
}


// 3. Use the macro to export all necessary Extism functions
impl_extism_http_plugin!(
    config = MyPluginConfig,  // Your config struct
    factory = MyPluginFactory, // Your factory struct
    name = "My Example LLM HTTP Plugin" // Display name for the plugin
);
```
This example uses placeholder implementations for parsing. You'll need to adapt them to the specific API of the LLM provider you are integrating.

## Building the Plugin

Compile your Rust library to Wasm:
```bash
cargo build --target wasm32-wasip1 --release
```
The Wasm file will be located at `target/wasm32-wasip1/release/my_llm_plugin.wasm`. This is the file you'll configure QueryMT to load.

## Debuging the Plugin

In case you would like to have the logging information printed to stdout/stderr you need to enable the wasi output for the plugin by setting the following
environmential variable `EXTISM_ENABLE_WASI_OUTPUT=1`.

Next, learn more about the [Plugin Interface](interface_spec.md) and [Helper Macros](helper_macros.md).
