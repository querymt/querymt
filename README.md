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

## Release channels and versioning

`qmt` and `qmtcode` ship on two channels. Both report a semver `--version`, and
version order always matches code order.

| Channel | Release | Version | Asset names |
|---------|---------|---------|-------------|
| stable  | `vX.Y.Z` tag, latest release | `X.Y.Z` | `qmtcode-vX.Y.Z-<target>.*` |
| nightly | rolling `nightly` prerelease | `<base>-nightly.<UTC YYYYMMDDHHMMSS>` | `qmtcode-nightly-<target>.*` |

**Nightly version.** `<base>` is `[workspace.package].version` from `Cargo.toml`,
unless the stable tag `v<that version>` already exists, in which case it is the
next patch. A nightly therefore sorts above the stable it follows and below the
next stable (`0.5.2` < `0.6.0-nightly.20261105111147` < `0.6.0`), and later
nightlies sort above earlier ones. The commit SHA is not part of the version; it
is in the release notes and `release.json`. The version is computed by
`scripts/release/nightly-version.sh`.

**`release.json`.** Every stable and nightly release uploads a `release.json`
asset: `{ "version", "channel", "commit", "published_at" }`. Consumers such as
querymt-desktop read the version and full commit SHA from it instead of parsing
asset names or notes. It is available at
`releases/latest/download/release.json` and `releases/download/nightly/release.json`.

**Stable gate.** A `vX.Y.Z` tag fails the Release workflow before anything is
built or published when:

- the tag does not equal the workspace version, or
- `X.Y.Z` is at least the published nightly's base version and the tag commit does
  not contain the nightly's commit (`git merge-base --is-ancestor`).

The second rule stops a stable with a higher version but older code, and
therefore fewer database migrations, from being offered to users running the
nightly. Patch releases on an older line (`v0.5.3` while the nightly is
`0.6.0-nightly.*`) sort below the nightly and pass. If the gate fails, tag a
commit that contains the nightly's commit. The nightly's `release.json` must
exist; if the published nightly predates it, run the Nightly workflow once and
re-run the release.

**Nightly publishing.** The rolling release is replaced through a
`nightly-staging` draft: assets are uploaded to the draft first, then the old
`nightly` release and tag are deleted and the draft is published as `nightly`. A
failed upload leaves the previous nightly in place. After publishing, a
`qmtcode-nightly` dispatch with `querymt_sha` and `querymt_version` is sent to
querymt-desktop.

The helper scripts and their tests live in `scripts/release/`
(`scripts/release/test-release-scripts.sh`).

## Documentation

For everything else, refer to [docs.query.mt](https://docs.query.mt).
