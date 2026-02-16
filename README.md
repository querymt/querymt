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

    // 2. Build the LLM provider
    let llm = LLMBuilder::new()
        .provider("openai")
        .model("gpt-5-mini")
        .api_key("your-api-key")
        .build(&registry)
        .await?;

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

## Agent (`qmt-agent`)

The `qmt-agent` crate (in `crates/agent`) is the high-level agent runtime for QueryMaTe.

If you're new, the easiest way to try it is the `coder_agent` example at `crates/agent/examples/coder_agent.rs`.
It loads an agent from a TOML config file and can run in two modes:

- `--stdio`: runs as an ACP stdio server (great for integrations and tooling)
- `--dashboard`: runs with a local web dashboard for interactive use

### Quick start

From the workspace root:

```bash
cd crates/agent

# ACP stdio mode
cargo run --example coder_agent --features dashboard -- --stdio

# Dashboard mode (default http://127.0.0.1:3000)
cargo run --example coder_agent --features dashboard -- --dashboard

# Dashboard mode on a custom address
cargo run --example coder_agent --features dashboard -- --dashboard=0.0.0.0:8080
```

By default it reads config from `examples/confs/coder_agent.toml`.
You can also pass your own config path before the mode flag.

## Documentation

For everything else, refer to [docs.query.mt](https://docs.query.mt).
