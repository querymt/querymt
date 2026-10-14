# QueryMaTe
[![Discord](https://img.shields.io/badge/Chat-Join%20Discord-7289da?logo=discord&logoColor=white)](https://discord.gg/vArq2xssXt)
[![Codecov](https://img.shields.io/codecov/c/github/querymt/querymt)](https://app.codecov.io/gh/querymt/querymt)
[![GitHub contributors](https://img.shields.io/github/contributors/querymt/querymt)](https://github.com/querymt/querymt/graphs/contributors)
[![Visual Studio Marketplace Version](https://img.shields.io/visual-studio-marketplace/v/querymt.vscode-querymt?label=VS%20Marketplace)](https://marketplace.visualstudio.com/items?itemName=querymt.vscode-querymt)

A unified interface for Large Language Models (LLMs) with support for various providers via a plugin-based architecture.

## Installation

Install the `qmt` and `qmtcode` binaries:

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

### Prompt Cache Hints

`ChatMessageBuilder::cache(CacheHint::Ephemeral { ttl_seconds })` requests a reusable
prefix ending at that message. Configured instructions accept the same metadata;
legacy strings and string arrays remain supported. For example, in an agent config:

```toml
system = [
  { file = "prompts/base.md", cache = { ephemeral = { ttl_seconds = 1800 } } },
  "Changing instructions belong after the stable prefix."
]
```

In the agent runtime, plain system strings and `{ file = "..." }` entries receive
provider-default cache hints for Anthropic, Codex, and OpenAI Responses. Explicit
hints/TTLs are preserved. Each instruction section is a prefix boundary, so a
changing later section does not remove the earlier stable boundary. The standalone
provider/CLI API still requires explicit hints; these defaults are agent policy.
Responses currently defaults to `30m`, not unlimited session retention. Codex's
unsupported translation remains disabled, so annotations do not guarantee reuse.

Programmatic callers can pass `querymt::params::SystemPrompt::with_cache(text, hint)`
to `.system(...)`. File references are resolved by the agent, not provider plugins.
Providers translate hints; unsupported endpoints may skip them. Responses supports
only a 1,800-second (`30m`) request-wide minimum lifetime: applied hints must agree
with the provider's configured TTL. Omit `ttl_seconds` for the provider default.
A lifetime is not a deletion deadline, and cache hits are not guaranteed.

On supported OpenAI Responses models, hints become explicit prefix breakpoints;
request-wide mode remains configurable with `extra_body.prompt_cache_options`.
Unknown endpoints require the `supports_prompt_cache_breakpoints` override after
support is verified. ChatGPT Codex translation is implemented but its activation is
commented out with a FIXME because that backend rejects these controls. DEBUG logs
under `qmt_codex` explain skipped hints. No prewarming is performed.

The resolved `LLMParams.system`, `OpenAI.system`, and `Codex.system` Rust fields now
use `Vec<SystemPrompt>`; callers assigning those fields directly should convert
strings with `.into()`. Existing `.system("text")` builder calls remain supported.
The providers' configuration-trait text getters use `Cow` to support both legacy
string storage and structured instructions; `system_parts()` preserves the hints.

#### Opt-In Codex Live Test

Ordinary `cargo test` runs ignore live inference. The reusable diagnostic is also
protected by `QMT_RUN_LIVE_CODEX=1`, even when running ignored tests. Build the CLI
and Codex WASM plugin and authenticate first, then explicitly run:

```sh
QMT_RUN_LIVE_CODEX=1 \
QMT_TEST_QMT_BIN="$PWD/target/debug/qmt" \
QMT_TEST_PROVIDER_CONFIG="$HOME/.qmt/providers.toml" \
QMT_TEST_CODEX_MODEL=gpt-5.6-sol \
cargo test -p qmt-codex --no-default-features --features api \
  --test live_cache live_codex_cache_usage -- --ignored --nocapture
```

The config must load the patched plugin. This diagnostic makes three live requests,
consumes account quota, and prints raw usage; a cache miss is not a test failure.
Do not enable the opt-in in routine CI. No live requests are made by unit tests.

## Agent

The `querymt-agent` crate (in `crates/agent`) is the high-level agent runtime for QueryMaTe.

If you're new, the easiest way to try it is the `qmtcode` example at `crates/agent/examples/qmtcode.rs`.
It loads an agent from a TOML config file and supports several transports:

- `--acp`: runs as an ACP stdio server for integrations and tooling
- `--acp-ws`: runs a standalone ACP WebSocket server at `/acp/ws`
- `--api`: exposes ACP at `/acp/ws` and SFT export at `/api/export/sft`
- `--dashboard`: serves the embedded Svelte dashboard over same-origin `/acp/ws`

The transport flags are mutually exclusive.

### Quick start

Run these commands from the workspace root. The `dashboard` feature automatically resolves and builds the pinned querymt-desktop UI; this source build requires Node.js and npm.

```bash
# ACP stdio mode
cargo run --example qmtcode -- --acp

# Dashboard mode (default http://127.0.0.1:3000)
cargo run --example qmtcode --features dashboard -- --dashboard

# Dashboard mode on a custom address
cargo run --example qmtcode --features dashboard -- --dashboard=0.0.0.0:8080
```

For advanced use, `QMT_UI_DIST` can select a prebuilt local directory/archive, while a plain `QMT_UI_REVISION` selects a Git SHA, tag, or ref to build instead of the pinned revision.

By default it reads config from `examples/confs/coder_agent.toml`.
You can also pass your own config path before the mode flag.

### macOS Silicon releases

macOS Silicon users who download the `qmtcode` release binary need to clear the quarantine flag before running it:

```bash
xattr -dr com.apple.quarantine qmtcode
```

## Documentation

For everything else, refer to [docs.query.mt](https://docs.query.mt).
