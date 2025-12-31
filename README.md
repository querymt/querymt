# QueryMaTe

A unified interface for Large Language Models (LLMs) with support for various providers via a plugin-based architecture.

## Installation

Add `querymt` to your `Cargo.toml` with the `extism_host` feature:

```toml
[dependencies]
querymt = { version = "0.2", features = ["extism_host"] }
```

## Basic Usage

```rust
use querymt::builder::LLMBuilder;
use querymt::chat::ChatMessage;
use querymt::plugin::{extism_impl::host::ExtismLoader, host::PluginRegistry};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize registry and register the Wasm loader
    let mut registry = PluginRegistry::from_path("providers.toml")?;
    registry.register_loader(Box::new(ExtismLoader));
    registry.load_all_plugins().await;

    // 2. Build the LLM provider
    let llm = LLMBuilder::new()
        .provider("openai")
        .model("gpt-5-mini")
        .api_key("your-api-key")
        .build(&registry)?;

    // 3. Send a message
    let response = llm.chat(&[
        ChatMessage::user().content("Hello!").build()
    ]).await?;

    println!("{}", response);
    Ok(())
}
```

### Configuration (`providers.toml`)

```toml
[[providers]]
name = "openai"
path = "oci://ghcr.io/querymt/openai:latest"
```

## Documentation

For everything else, refer to [docs.query.mt](https://docs.query.mt).
