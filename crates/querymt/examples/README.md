# QueryMT Examples

These examples demonstrate the `querymt` core library API: chat completion, vision, embeddings, tool calling, prompt chains, evaluation, and more.

Each example requires a `providers.toml` file and, for cloud providers, the relevant API key(s).

## Available Examples

### Basic Chat

| Example | Provider | Description |
|---------|----------|-------------|
| `openai_example` | OpenAI | Multi-turn chat with GPT-4o Mini |
| `anthropic_example` | Anthropic | Chat about Rust concurrency |
| `google_example` | Google | Chat about async/await in Rust |
| `groq_example` | Groq | Chat about quantum computing |
| `xai_example` | xAI | Multi-turn chat with Grok |
| `ollama_example` | Ollama | Chat with a local model |

### Vision

| Example | Provider | Description |
|---------|----------|-------------|
| `openai_vision_example` | OpenAI | Describe images from URL and local file |
| `anthropic_vision_example` | Anthropic | Describe a local image |
| `google_vision_example` | Google | Describe a local image |

### PDF

| Example | Provider | Description |
|---------|----------|-------------|
| `google_pdf_example` | Google | Analyze a local PDF |

### Reasoning / Thinking

| Example | Provider | Description |
|---------|----------|-------------|
| `openai_reasoning_example` | OpenAI | Reasoning effort with GPT-5.2 |
| `anthropic_thinking_example` | Anthropic | Extended thinking with reasoning budget |

### Embeddings

| Example | Provider | Description |
|---------|----------|-------------|
| `openai_embedding_example` | OpenAI | Generate text embeddings |
| `google_embedding_example` | Google | Generate text embeddings (not yet implemented) |

### Structured Output

| Example | Provider | Description |
|---------|----------|-------------|
| `openai_structured_output_example` | OpenAI | JSON schema-constrained output |
| `google_structured_output_example` | Google | JSON schema-constrained output |
| `xai_structured_output_example` | xAI | JSON schema-constrained output |
| `ollama_structured_output_example` | Ollama | JSON schema-constrained output |

### Tool Calling

| Example | Provider | Description |
|---------|----------|-------------|
| `tool_calling_example` | OpenAI | Single-provider tool calling with weather function |
| `unified_tool_calling_example` | Multiple | Multi-provider tool calling with multiple scenarios |

### Chains and Pipelines

| Example | Provider | Description |
|---------|----------|-------------|
| `chain_example` | OpenAI | 4-step prompt chain with a single provider |
| `multi_backend_example` | Multiple | Multi-provider chain (OpenAI -> Anthropic -> Groq) |
| `groq_claude_pipeline_example` | Groq + Anthropic | Two-step pipeline with response transform |

### Evaluation

| Example | Provider | Description |
|---------|----------|-------------|
| `evaluation_example` | Multiple | Score responses across providers with custom functions |
| `evaluator_parallel_example` | Multiple | Parallel evaluation across providers |

### Validation

| Example | Provider | Description |
|---------|----------|-------------|
| `validator_example` | Anthropic | JSON validation with automatic retries |

### Audio

| Example | Provider | Description |
|---------|----------|-------------|
| `tts_example` | OpenAI | Text-to-speech (CLI arguments) |
| `stt_example` | OpenAI | Speech-to-text (CLI arguments) |

## Requirements

- Run commands from the workspace root.
- API keys for the cloud providers you want to use (OpenAI, Anthropic, Google, Groq, xAI).
- For Ollama examples, a running Ollama server with the model pulled.
- Install the wasm target (needed for local provider builds):

```sh
rustup target add wasm32-wasip1
```

## Create `providers.toml` with GHCR providers

The easiest way to get started is with OCI-hosted providers from [GHCR](https://github.com/orgs/querymt/packages?repo_name=querymt). The providers listed below are the ones used by examples in this directory; more are available in the registry.

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

## Running examples

Most examples read `PROVIDER_CONFIG` and default to `providers.toml` in the current directory.

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

## Build providers locally

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
