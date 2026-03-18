# QueryMaTe
[![Discord](https://img.shields.io/badge/Chat-Join%20Discord-7289da?logo=discord&logoColor=white)](https://discord.gg/vArq2xssXt)
[![Codecov](https://img.shields.io/codecov/c/github/querymt/querymt)](https://app.codecov.io/gh/querymt/querymt)
[![GitHub contributors](https://img.shields.io/github/contributors/querymt/querymt)](https://github.com/querymt/querymt/graphs/contributors)
[![Visual Studio Marketplace Version](https://img.shields.io/visual-studio-marketplace/v/querymt.vscode-querymt?label=VS%20Marketplace)](https://marketplace.visualstudio.com/items?itemName=querymt.vscode-querymt)

A unified interface for Large Language Models (LLMs) with support for various providers via a plugin-based architecture.

## Installation

Install the `qmt` and `coder_agent` binaries:

```bash
curl -sSf https://query.mt/install.sh | sh
```

Nightly channel:

```bash
curl -sSf https://query.mt/install.sh | sh -s -- --nightly
```

Windows PowerShell:

```powershell
irm https://query.mt/install.ps1 | iex
```

Windows nightly channel:

```powershell
$env:QMT_CHANNEL='nightly'; irm https://query.mt/install.ps1 | iex
```

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

For a full examples-oriented setup (GHCR providers, local wasm provider builds, and run commands), see `crates/querymt/examples/README.md`.

## Agent

The `querymt-agent` crate (in `crates/agent`) is the high-level agent runtime for QueryMaTe.

If you're new, the easiest way to try it is the `qmtcode` example at `crates/agent/examples/qmtcode.rs`.
It loads an agent from a TOML config file and can run in two modes:

- `--stdio`: runs as an ACP stdio server (great for integrations and tooling)
- `--dashboard`: runs with a local web dashboard for interactive use

### Quick start

From the workspace root:

```bash
cd crates/agent

# ACP stdio mode
cargo run --example qmtcode --features dashboard -- --stdio

# Dashboard mode (default http://127.0.0.1:3000)
cargo run --example qmtcode --features dashboard -- --dashboard

# Dashboard mode on a custom address
cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080
```

By default it reads config from `examples/confs/coder_agent.toml`.
You can also pass your own config path before the mode flag.

### Generate shared types

From the workspace root:

```bash
scripts/generate-types.sh
```

This regenerates TypeScript (and Swift when the sibling iOS repo exists) typeshare outputs.

### macOS Silicon releases

macOS Silicon users who download the `qmtcode` release binary need to clear the quarantine flag before running it:

```bash
xattr -dr com.apple.quarantine qmtcode
```

## Documentation

For everything else, refer to [docs.query.mt](https://docs.query.mt).
