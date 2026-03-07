# QueryMT Examples Setup

This guide explains how to prepare `providers.toml` so you can run examples in `crates/querymt/examples`.

## Requirements

- Run commands from the workspace root.
- Install the wasm target (needed for local provider builds):

```sh
rustup target add wasm32-wasip1
```

## Create `providers.toml` with GHCR providers

You can start with OCI-hosted providers from GHCR:

```toml
[[providers]]
name = "openai"
path = "oci://ghcr.io/querymt/openai:latest"

[[providers]]
name = "anthropic"
path = "oci://ghcr.io/querymt/anthropic:latest"

[[providers]]
name = "google"
path = "oci://ghcr.io/querymt/google:latest"

[[providers]]
name = "groq"
path = "oci://ghcr.io/querymt/groq:latest"

[[providers]]
name = "xai"
path = "oci://ghcr.io/querymt/xai:latest"

[[providers]]
name = "ollama"
path = "oci://ghcr.io/querymt/ollama:latest"
```

You can also copy and trim `providers.example.toml` from the repository root.

## Run examples with this config

Most examples in this folder read `PROVIDER_CONFIG` and default to `providers.toml` in the current directory.

```sh
PROVIDER_CONFIG=./providers.toml \
OPENAI_API_KEY="your-key" \
cargo run -p querymt --example openai_example
```

```sh
PROVIDER_CONFIG=./providers.toml \
ANTHROPIC_API_KEY="your-key" \
cargo run -p querymt --example anthropic_example
```

```sh
PROVIDER_CONFIG=./providers.toml \
OLLAMA_URL="http://127.0.0.1:11434" \
OLLAMA_MODEL="qwen3:0.6b" \
cargo run -p querymt --example ollama_example
```

Note: `stt_example` and `tts_example` take `providers.toml` as the first positional argument instead of `PROVIDER_CONFIG`.

## Build providers locally and use local wasm files

Examples in this crate register `ExtismLoader` only, so use wasm plugins (`.wasm`) for local builds.

Build one or more providers locally:

```sh
cargo build -p qmt-openai -p qmt-anthropic -p qmt-google --target wasm32-wasip1 --release
```

Then reference local wasm artifacts in `providers.toml`:

```toml
[[providers]]
name = "openai"
path = "target/wasm32-wasip1/release/qmt_openai.wasm"

[[providers]]
name = "anthropic"
path = "target/wasm32-wasip1/release/qmt_anthropic.wasm"

[[providers]]
name = "google"
path = "target/wasm32-wasip1/release/qmt_google.wasm"
```

Common crate/package to wasm output mapping:

- `qmt-openai` -> `qmt_openai.wasm`
- `qmt-anthropic` -> `qmt_anthropic.wasm`
- `qmt-google` -> `qmt_google.wasm`
- `qmt-groq` -> `qmt_groq.wasm`
- `qmt-ollama` -> `qmt_ollama.wasm`
- `qmt-xai` -> `qmt_xai.wasm`

## Troubleshooting

- `Provider '...' not found`: `name` in `providers.toml` must match `.provider("...")` in the example.
- `Local file not found at path`: run from repo root or use absolute paths in `providers.toml`.
- `No registered loader for plugin type 'Native'`: these examples do not register `NativeLoader`; use wasm plugin paths.
- `google_embedding_example` currently fails at runtime because embedding is not fully implemented in the Google provider.
