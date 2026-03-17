# Available Providers

QueryMT ships with 15 providers split across two types: **WASM** (API-based, cloud services) and **Native** (local inference, runs models on your hardware). This page covers how to configure each type, how to pick the right build variant for your hardware, and provides copy-pasteable configuration recipes.

---

## Provider Repository (`repo.query.mt`)

QueryMT maintains an always-up-to-date provider repository at **[`https://repo.query.mt`](https://repo.query.mt)**.

- **[`latest.json`](https://repo.query.mt/latest.json)** — updated on every push to `main` (default)
- **[`stable.json`](https://repo.query.mt/stable.json)** — updated on every tagged release

If no providers config exists at `~/.qmt/providers.toml` (or `.json` / `.yaml`), QueryMT automatically fetches `latest.json` and caches it to `~/.qmt/providers.json` on first run.

```sh
# Refresh to the latest provider list
qmt update
```

Override the source URL:

```sh
QMT_PROVIDERS_URL=https://repo.query.mt/stable.json qmt update
```

!!! note
    The provider repository currently covers **WASM providers only**. Native providers must be configured manually in your providers config.

---

## WASM Providers (API-based)

WASM providers call remote APIs. They are sandboxed WebAssembly modules that run on any platform and require no local hardware beyond an internet connection. All 12 WASM providers are listed in the provider repository, so **you don't need to configure them manually** unless you want to pin a version or customise settings.

| Name | Description | API Key Env Var |
|---|---|---|
| `anthropic` | Anthropic Claude models | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI GPT / o-series models | `OPENAI_API_KEY` |
| `codex` | OpenAI Codex / coding-optimised models | `OPENAI_API_KEY` |
| `google` | Google Gemini models | `GEMINI_API_KEY` |
| `mistral` | Mistral AI models | `MISTRAL_API_KEY` |
| `groq` | Groq-hosted models (OpenAI-compatible) | `GROQ_API_KEY` |
| `ollama` | Ollama local server (OpenAI-compatible) | — |
| `openrouter` | OpenRouter multi-provider gateway | `OPENROUTER_API_KEY` |
| `alibaba` | Alibaba Cloud Qwen models (OpenAI-compatible) | `DASHSCOPE_API_KEY` |
| `moonshot` | Moonshot AI Kimi models (OpenAI-compatible) | `MOONSHOT_API_KEY` |
| `kimi-code` | Kimi Code specialised coding model | `MOONSHOT_API_KEY` |
| `xai` | xAI Grok models (OpenAI-compatible) | `XAI_API_KEY` |

### Minimal WASM configuration

You only need to specify a provider if you want to override defaults or pin a version. Most users can skip this entirely.

=== "anthropic"
    ```toml
    [[providers]]
    name = "anthropic"
    path = "oci://ghcr.io/querymt/anthropic:latest"
    ```

=== "openai"
    ```toml
    [[providers]]
    name = "openai"
    path = "oci://ghcr.io/querymt/openai:latest"
    ```

=== "google"
    ```toml
    [[providers]]
    name = "google"
    path = "oci://ghcr.io/querymt/google:latest"
    ```

=== "mistral"
    ```toml
    [[providers]]
    name = "mistral"
    path = "oci://ghcr.io/querymt/mistral:latest"
    ```

=== "groq"
    ```toml
    [[providers]]
    name = "groq"
    path = "oci://ghcr.io/querymt/groq:latest"
    ```

=== "ollama"
    ```toml
    [[providers]]
    name = "ollama"
    path = "oci://ghcr.io/querymt/ollama:latest"
    ```

=== "openrouter"
    ```toml
    [[providers]]
    name = "openrouter"
    path = "oci://ghcr.io/querymt/openrouter:latest"
    ```

=== "alibaba"
    ```toml
    [[providers]]
    name = "alibaba"
    path = "oci://ghcr.io/querymt/alibaba:latest"
    ```

=== "moonshot"
    ```toml
    [[providers]]
    name = "moonshot"
    path = "oci://ghcr.io/querymt/moonshot:latest"
    ```

=== "kimi-code"
    ```toml
    [[providers]]
    name = "kimi-code"
    path = "oci://ghcr.io/querymt/kimi-code:latest"
    ```

=== "xai"
    ```toml
    [[providers]]
    name = "xai"
    path = "oci://ghcr.io/querymt/xai:latest"
    ```

=== "codex"
    ```toml
    [[providers]]
    name = "codex"
    path = "oci://ghcr.io/querymt/codex:latest"
    ```

---

## Native Providers (local inference)

Native providers run LLMs **locally on your machine**. They are distributed as platform-specific shared libraries (`.so` / `.dylib` / `.dll`) packaged into OCI images. Unlike WASM providers, QueryMT resolves native providers at download time by inspecting the OCI image index for a manifest that matches your OS and architecture. If a match is found, the native library is downloaded; if not, QueryMT falls back to the WASM variant if one exists.

There are three native providers:

| Name | Underlying engine | Description |
|---|---|---|
| `llama-cpp` | [llama.cpp](https://github.com/ggerganov/llama.cpp) | GGUF models, vision support, broad hardware compatibility |
| `izwi` | [izwi-core](https://github.com/agentem-ai/izwi) | Efficient local inference with Flash Attention |
| `mrs` | [mistral.rs](https://github.com/EricLBuehler/mistral.rs) | High-performance inference for Mistral and compatible architectures |

!!! warning "Native providers must be configured manually"
    Native providers are not included in the `repo.query.mt` provider repository. You must add them to your `~/.qmt/providers.toml` explicitly.

### Feature tags

Each native provider is published with multiple OCI tags corresponding to different hardware backends. The tag you specify in `path` controls which binary is downloaded.

| Tag suffix | Hardware | Availability |
|---|---|---|
| `latest` / `latest-default` | CPU only | Linux (x86_64, arm64), macOS (x86_64, arm64), Windows (x86_64) |
| `latest-metal` | macOS Metal GPU | macOS (x86_64, arm64) |
| `latest-accelerate` | macOS Accelerate framework | macOS (x86_64, arm64) — izwi, mrs only |
| `latest-vulkan` | Vulkan GPU | Linux (x86_64, arm64), Windows (x86_64) — llama-cpp only |
| `latest-cuda12.8` | NVIDIA CUDA 12.8 | Linux (x86_64, arm64), Windows (x86_64) — llama-cpp only |
| `latest-cuda12.8-sm80` | NVIDIA CUDA SM 80 | Linux x86_64 — izwi, mrs only |
| `latest-cuda12.8-sm86` | NVIDIA CUDA SM 86 | Linux x86_64 — izwi, mrs only |
| `latest-cuda12.8-sm87` | NVIDIA CUDA SM 87 (Jetson Orin) | Linux arm64 — izwi, mrs only |
| `latest-cuda12.8-sm89` | NVIDIA CUDA SM 89 | Linux x86_64 — izwi, mrs only |
| `latest-cuda12.8-sm120` | NVIDIA CUDA SM 120 | Linux x86_64 — izwi, mrs only |

Version-pinned variants follow the same pattern with the crate version prepended: `0.1.0-metal`, `0.1.0-cuda12.8-sm89`, etc.

!!! note "llama-cpp CUDA vs. izwi/mrs CUDA"
    `llama-cpp` uses a single combined `cuda12.8` build that covers all NVIDIA GPUs via JIT compilation at runtime — no SM selection needed. `izwi` and `mrs` compile Flash Attention kernels ahead of time, which requires a specific SM architecture to be selected at build time.

### NVIDIA GPU to SM architecture

Use this table to find the correct `cuda12.8-smXX` suffix for your GPU.

| GPU | Architecture | SM | Tag suffix |
|---|---|---|---|
| RTX 3050 / 3060 / 3070 / 3080 / 3090 | Ampere | 86 | `cuda12.8-sm86` |
| RTX 3050 Ti (laptop) | Ampere | 86 | `cuda12.8-sm86` |
| A10 / A40 | Ampere | 86 | `cuda12.8-sm86` |
| A30 / A100 | Ampere | 80 | `cuda12.8-sm80` |
| RTX 4060 / 4070 / 4070 Ti / 4080 / 4090 | Ada Lovelace | 89 | `cuda12.8-sm89` |
| RTX 4060 Ti / 4070 Super / 4080 Super | Ada Lovelace | 89 | `cuda12.8-sm89` |
| L4 / L40 / L40S | Ada Lovelace | 89 | `cuda12.8-sm89` |
| RTX 5070 / 5070 Ti / 5080 / 5090 | Blackwell | 120 | `cuda12.8-sm120` |
| B100 / B200 / GB200 | Blackwell | 120 | `cuda12.8-sm120` |
| Jetson AGX Orin / Orin NX / Orin Nano | Ampere (aarch64) | 87 | `cuda12.8-sm87` |

!!! tip "Not sure which SM your GPU is?"
    Run `nvidia-smi --query-gpu=compute_cap --format=csv,noheader` to print your GPU's compute capability (e.g. `8.9` = SM 89).

### Choosing a feature tag

```
What OS are you on?
├── macOS Apple Silicon (M1/M2/M3/M4)  → latest-metal
├── macOS Intel
│   ├── izwi / mrs                     → latest-accelerate
│   └── llama-cpp                      → latest-default
├── Linux
│   ├── NVIDIA GPU
│   │   ├── llama-cpp                  → latest-cuda12.8
│   │   └── izwi / mrs                 → latest-cuda12.8-sm{XX}  (see table above)
│   ├── AMD / Intel GPU                → latest-vulkan  (llama-cpp only)
│   └── CPU only                       → latest  (or latest-default)
└── Windows
    ├── NVIDIA GPU                     → latest-cuda12.8  (llama-cpp only)
    └── CPU only                       → latest
```

---

## Provider reference

### llama-cpp

Wraps [llama.cpp](https://github.com/ggerganov/llama.cpp). Supports GGUF models, vision/multimodal models, and streaming. Broadest hardware support of the three native providers.

**Supported platforms:**

| OS | Architecture | CPU | Metal | Accelerate | Vulkan | CUDA 12.8 |
|---|---|:---:|:---:|:---:|:---:|:---:|
| Linux | x86_64 | ✓ | — | — | ✓ | ✓ |
| Linux | arm64 | ✓ | — | — | ✓ | ✓ |
| macOS | x86_64 | — | ✓ | — | — | — |
| macOS | arm64 | — | ✓ | — | — | — |
| Windows | x86_64 | ✓ | — | — | — | ✓ |

**Configuration:**

=== "macOS (Metal)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest-metal"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 99
    ```

=== "Linux / Windows (CPU)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 0
    ```

=== "Linux (CUDA)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest-cuda12.8"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 99
    flash_attention = "enabled"
    ```

=== "Linux (Vulkan)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest-vulkan"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 99
    ```

=== "Windows (CUDA)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest-cuda12.8"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 99
    flash_attention = "enabled"
    ```

**Key config options:**

| Option | Type | Description |
|---|---|---|
| `model` | string | Path to GGUF model file, or `owner/repo:filename` HuggingFace reference |
| `n_ctx` | integer | Context window size (default: model native size) |
| `n_gpu_layers` | integer | Layers to offload to GPU. `0` = CPU only, `99` = all layers |
| `flash_attention` | string | `"auto"` \| `"enabled"` \| `"disabled"` |
| `kv_cache_type_k` | string | KV cache key quantization: `"f16"`, `"q8_0"`, `"q4_0"` |
| `kv_cache_type_v` | string | KV cache value quantization: `"f16"`, `"q8_0"`, `"q4_0"` |
| `max_tokens` | integer | Maximum tokens to generate (default: 256) |
| `temperature` | float | Sampling temperature. `0` = greedy |
| `mmproj_path` | string | Path to multimodal projection file (vision models only) |

For vision model configuration and the full option reference see the [llama-cpp provider README](https://github.com/querymt/querymt/blob/main/crates/providers/llama-cpp/README.md).

---

### izwi

Wraps [izwi-core](https://github.com/agentem-ai/izwi). Efficient local inference with Flash Attention support. CUDA builds are compiled per SM architecture for maximum performance.

**Supported platforms:**

| OS | Architecture | CPU | Metal | Accelerate | CUDA 12.8 |
|---|---|:---:|:---:|:---:|:---:|
| Linux | x86_64 | ✓ | — | — | SM 80, 86, 89, 120 (+ Flash Attn) |
| Linux | arm64 | ✓ | — | — | SM 87 (+ Flash Attn) |
| macOS | x86_64 | — | ✓ | ✓ | — |
| macOS | arm64 | — | ✓ | ✓ | — |
| Windows | x86_64 | ✓ | — | — | — |

**Configuration:**

=== "macOS (Metal)"
    ```toml
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-metal"

    [providers.config]
    model = "/path/to/model"
    ```

=== "macOS (Accelerate)"
    ```toml
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-accelerate"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux / Windows (CPU)"
    ```toml
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 30xx / A10 / A40)"
    ```toml
    # SM 86: RTX 3060, 3070, 3080, 3090, A10, A40
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm86"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 40xx / L4 / L40)"
    ```toml
    # SM 89: RTX 4060, 4070, 4080, 4090, L4, L40, L40S
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm89"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — A100)"
    ```toml
    # SM 80: A30, A100
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm80"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 50xx / Blackwell)"
    ```toml
    # SM 120: RTX 5070, 5080, 5090, B100, B200
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm120"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux arm64 (CUDA — Jetson Orin)"
    ```toml
    # SM 87: Jetson AGX Orin, Orin NX, Orin Nano
    [[providers]]
    name = "izwi"
    path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm87"

    [providers.config]
    model = "/path/to/model"
    ```

---

### mrs

Wraps [mistral.rs](https://github.com/EricLBuehler/mistral.rs). High-performance inference for Mistral and compatible architectures. CUDA x86_64 builds include both Flash Attention and cuDNN.

**Supported platforms:**

| OS | Architecture | CPU | Metal | Accelerate | CUDA 12.8 |
|---|---|:---:|:---:|:---:|:---:|
| Linux | x86_64 | ✓ | — | — | SM 80, 86, 89, 120 (+ Flash Attn + cuDNN) |
| Linux | arm64 | ✓ | — | — | SM 87 (+ cuDNN, no Flash Attn) |
| macOS | x86_64 | — | ✓ | ✓ | — |
| macOS | arm64 | — | ✓ | ✓ | — |
| Windows | x86_64 | ✓ | — | — | — |
| Windows | arm64 | ✓ | — | — | — |

**Configuration:**

=== "macOS (Metal)"
    ```toml
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-metal"

    [providers.config]
    model = "/path/to/model"
    ```

=== "macOS (Accelerate)"
    ```toml
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-accelerate"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux / Windows (CPU)"
    ```toml
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 30xx / A10 / A40)"
    ```toml
    # SM 86: RTX 3060, 3070, 3080, 3090, A10, A40
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm86"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 40xx / L4 / L40)"
    ```toml
    # SM 89: RTX 4060, 4070, 4080, 4090, L4, L40, L40S
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm89"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — A100)"
    ```toml
    # SM 80: A30, A100
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm80"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux (CUDA — RTX 50xx / Blackwell)"
    ```toml
    # SM 120: RTX 5070, 5080, 5090, B100, B200
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm120"

    [providers.config]
    model = "/path/to/model"
    ```

=== "Linux arm64 (CUDA — Jetson Orin)"
    ```toml
    # SM 87: Jetson AGX Orin, Orin NX, Orin Nano
    # Note: cuDNN included; Flash Attention not available on aarch64
    [[providers]]
    name = "mrs"
    path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm87"

    [providers.config]
    model = "/path/to/model"
    ```

---

## Complete configuration recipes

Drop one of these into `~/.qmt/providers.toml` and adjust paths as needed.

### Cloud APIs only

No config needed — `repo.query.mt` handles this automatically on first run. If you want explicit control:

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
```

### Cloud + local llama-cpp on macOS Apple Silicon

```toml
[[providers]]
name = "openai"
path = "oci://ghcr.io/querymt/openai:latest"

[[providers]]
name = "anthropic"
path = "oci://ghcr.io/querymt/anthropic:latest"

[[providers]]
name = "llama-cpp"
path = "oci://ghcr.io/querymt/llama-cpp:latest-metal"

[providers.config]
model = "/path/to/model.gguf"
n_ctx = 8192
n_gpu_layers = 99
flash_attention = "auto"
```

### Fully local on Linux with NVIDIA RTX 40-series

```toml
# llama-cpp: single CUDA build, all SM supported at runtime
[[providers]]
name = "llama-cpp"
path = "oci://ghcr.io/querymt/llama-cpp:latest-cuda12.8"

[providers.config]
model = "/path/to/model.gguf"
n_ctx = 8192
n_gpu_layers = 99
flash_attention = "enabled"

# izwi: SM 89 for RTX 4060/4070/4080/4090
[[providers]]
name = "izwi"
path = "oci://ghcr.io/querymt/izwi:latest-cuda12.8-sm89"

[providers.config]
model = "/path/to/model"

# mrs: SM 89 for RTX 4060/4070/4080/4090
[[providers]]
name = "mrs"
path = "oci://ghcr.io/querymt/mrs:latest-cuda12.8-sm89"

[providers.config]
model = "/path/to/model"
```

### Fully local on macOS Apple Silicon

```toml
[[providers]]
name = "llama-cpp"
path = "oci://ghcr.io/querymt/llama-cpp:latest-metal"

[providers.config]
model = "/path/to/model.gguf"
n_ctx = 8192
n_gpu_layers = 99

[[providers]]
name = "izwi"
path = "oci://ghcr.io/querymt/izwi:latest-metal"

[providers.config]
model = "/path/to/model"

[[providers]]
name = "mrs"
path = "oci://ghcr.io/querymt/mrs:latest-metal"

[providers.config]
model = "/path/to/model"
```

### Fully local on Linux (CPU only)

```toml
[[providers]]
name = "llama-cpp"
path = "oci://ghcr.io/querymt/llama-cpp:latest"

[providers.config]
model = "/path/to/model.gguf"
n_ctx = 4096
n_gpu_layers = 0

[[providers]]
name = "mrs"
path = "oci://ghcr.io/querymt/mrs:latest"

[providers.config]
model = "/path/to/model"
```

---

## Further reading

- [Plugin Configuration](../plugins/configuration.md) — full `providers.toml` field reference, OCI signature verification, JSON/YAML format
- [OCI Plugins](../plugins/oci_plugins.md) — how OCI image resolution and caching works
- [llama-cpp provider README](https://github.com/querymt/querymt/blob/main/crates/providers/llama-cpp/README.md) — vision models, full config reference, troubleshooting
