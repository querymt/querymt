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
| `latest-rocm7.2` | AMD ROCm 7.2 | Linux x86_64 only - llama-cpp only; GPU runtime validation pending |
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

### AMD ROCm compatibility

The `llama-cpp` `latest-rocm7.2` variant is built on Ubuntu 24.04 with **ROCm 7.2.1**, matching [llama.cpp's Linux ROCm Docker build](https://github.com/ggml-org/llama.cpp/blob/59d5fad7c111d30a547d690b69235ed9e05c035d/.devops/rocm.Dockerfile). One binary contains all of the upstream build's GPU targets; no per-GPU tag is needed:

| GPU family | Compiled targets |
|---|---|
| Instinct (CDNA) | `gfx908`, `gfx90a`, `gfx942` |
| RDNA 2 | `gfx1030` |
| RDNA 3 | `gfx1100`, `gfx1101`, `gfx1102` |
| Ryzen APUs (RDNA 3.5) | `gfx1150`, `gfx1151` |
| RDNA 4 | `gfx1200`, `gfx1201` |

!!! warning "Initial ROCm release: GPU runtime validation pending"
    CI compiles the provider and checks ROCm linkage without an AMD GPU. Actual inference, performance, and model/backend combinations have not yet been validated on AMD hardware. The targets listed here describe compiled coverage, not a tested hardware support guarantee.

The OCI artifact contains the provider library only, **not** ROCm or GPU drivers. Use Ubuntu 24.04 or a compatible environment with the ROCm 7.2.1 HIP runtime, hipBLAS, and rocBLAS (including its GPU kernel data) installed. Follow [AMD's ROCm 7.2.1 installation instructions](https://rocm.docs.amd.com/projects/install-on-linux/en/docs-7.2.1/install/install-methods/package-manager/package-manager-ubuntu.html) and check the GPU/driver requirements for your hardware. The dynamic linker must be able to resolve `libamdhip64.so`, `libhipblas.so`, `librocblas.so`, and their dependencies. Nonstandard installations, including NixOS, require an environment providing those libraries; the downloaded artifact is not self-contained.

The process also needs permission to access the AMD GPU (`/dev/kfd` and the appropriate `/dev/dri` render node). GPUs outside the compiled list can use `latest-vulkan` or a source build with a custom target list. This initial ROCm variant is not published for Windows or Linux arm64.

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
│   ├── AMD GPU + ROCm 7.2 (x86_64)     → latest-rocm7.2 (llama-cpp; validation pending)
│   ├── Other AMD / Intel GPU          → latest-vulkan  (llama-cpp only)
│   └── CPU only                       → latest  (or latest-default)
└── Windows
    ├── NVIDIA GPU                     → latest-cuda12.8  (llama-cpp only)
    └── CPU only                       → latest
```

---

## Provider reference

### anthropic

Claude models through the Anthropic Messages API. Requests are shaped per model, so the same session settings are valid on every Claude generation.

#### Model classes and "auto" effort

The model ID (including Bedrock, Vertex and router forms such as `us.anthropic.claude-opus-4-7-v1:0`, `claude-sonnet-4-6@20260101` or `anthropic/claude-sonnet-5-5`) is parsed into family and version and falls into one class:

| Class | Models | "auto" (no effort set) | Explicit effort (`low`, `medium`, `high`, `max`) |
|---|---|---|---|
| Default-on adaptive | Claude 5 or later, Fable, Mythos (including Mythos Preview) | No `thinking`, no effort: the API defaults apply | `thinking: {type: "adaptive"}` + `output_config.effort` |
| Opt-in adaptive | Opus and Sonnet 4.6 to 4.8 | `thinking: {type: "adaptive"}` | `thinking: {type: "adaptive"}` + `output_config.effort` |
| Budget | Everything older, and IDs that can't be parsed | No `thinking` | `thinking: {type: "enabled", budget_tokens}` |

!!! warning "Behaviour change for Opus 4.6-4.8 and Sonnet 4.6"
    "auto" now turns adaptive thinking **on** for these models. This raises cost and latency, and responses may contain thinking blocks. There is no "off" reasoning level yet.

!!! note "\"auto\" on Claude 5.x does not bound thinking"
    Default-on models keep their API default effort (`high` on most models), and nothing in the request limits how much they think. The documented controls are `max_tokens` and `effort`. Thinking spend is reported as `reasoning_tokens` in usage so it is visible, but it is not capped by this provider.

#### Effort and budget fitting

On adaptive models the effort is sent unchanged as `output_config.effort`. On budget models the budget is `reasoning_budget_tokens` when configured, otherwise a default per effort (low 1,024, medium 8,000, high 16,000, max 31,999). It is then fitted to the request: at least 1,024 and at most half of `max_tokens`. When `max_tokens` is below 2,048 thinking is omitted. Budget models never receive `output_config`.

#### Sampling parameters

- On Opus 4.7 or later and on every Claude 5 or later, Fable or Mythos model, `temperature`, `top_p` and `top_k` are **never sent**.
- On other models, whenever thinking is sent, `temperature` and `top_k` are omitted and `top_p` is sent only if it is between 0.95 and 1.
- Otherwise the configured values are sent.

Each dropped value is logged at debug level.

#### Forced tool choice

A forced `tool_choice` is `any` or a named tool.

- Budget models: thinking is dropped from that request.
- Opus 5.5 or later, Sonnet 5.5 or later, Fable 5.1 or later, Mythos 5.1 or later: the API rejects forced tool choice, so the request fails locally with an invalid-request error.
- Other adaptive models: sent unchanged.

#### Thinking replay, usage and stop reasons

Each thinking block becomes its own reasoning item with its own signature. Signed blocks are replayed unchanged, even when their text is empty (the default `display: "omitted"` on Opus 4.7+ and Claude 5). `redacted_thinking` blocks are kept and replayed with their opaque data. `output_tokens_details.thinking_tokens` is reported as `reasoning_tokens`. Stop reasons map as: `end_turn`/`stop_sequence` to stop, `tool_use` to tool calls, `max_tokens`/`model_context_window_exceeded` to length, `refusal` to content filter, `pause_turn` to other.

#### `adaptive_thinking` override

An optional provider config field overrides the class derived from the model ID:

```toml
[providers.anthropic]
adaptive_thinking = true   # force opt-in adaptive thinking
```

- `true` sends adaptive thinking explicitly. This is the right choice for a proxy or custom model ID that can't be parsed but fronts an adaptive model.
- `false` forces budget thinking. The API **rejects** budget thinking on Opus 4.7+ and every Claude 5 or later model.

The sampling and forced-tool-choice rules always use the parsed model ID and ignore this override.

#### Strict tool use (`strict_tools`)

Strict tool use makes the API enforce a tool's input schema during sampling. One optional provider setting controls it:

```toml
[providers.anthropic]
strict_tools = ["edit", "multiedit", "replace_symbol"]
```

A tool is eligible when its definition sets `strict: true` or its unprefixed name is in `strict_tools` (names are matched before the OAuth `mcp_` prefix is applied). The provider itself has no default list: when `strict_tools` is unset or empty, no tool is selected by name, but tools whose definitions explicitly set `strict: true` stay eligible. The QueryMT agent fills in `["edit", "multiedit", "replace_symbol"]` for Anthropic when you have not set `strict_tools`; set `strict_tools = []` to turn that off. Builtin tool definitions never set `strict`, so OpenAI Responses behaviour is unchanged.

Eligible tools are sent with `"strict": true` only when every check passes:

- **Model.** Opus and Sonnet 4.5 or later, Haiku 4.5 or later, and all Fable and Mythos models. Older or unrecognised model IDs send no `strict` field and the original schemas (logged at debug level).
- **Schema.** Strict mode accepts a subset of JSON Schema, so the provider converts a *copy* of each schema: fixed-property objects are closed (`additionalProperties: false`), and numeric limits, string-length limits, `maxItems` and `minItems` above 1 move into the property description, for example `(minimum: 0)`. Required and optional properties are unchanged. The agent still validates calls against the original schema. A tool whose schema cannot be converted safely (external or recursive references, dictionaries or open objects, `pattern`, unsupported formats, `oneOf`/`allOf`/`not` and other unknown keywords) is sent non-strict with its original schema and a warning naming the tool and the unsupported feature. Other eligible tools stay strict. This applies to explicit and name-selected strictness alike.
- **Budgets.** Counted over the successfully converted tools only: at most 20 strict tools, 24 optional properties and 16 union properties (`anyOf` or type arrays). If any limit is exceeded, *every* tool in that request is sent non-strict with its original schema and a warning names the limit.

Strict tools work the same over API key and OAuth: OAuth strict requests were verified against the live API on `claude-sonnet-4-5` and `claude-sonnet-5-5` (see `scripts/verify-anthropic-strict.sh`).

### llama-cpp

Wraps [llama.cpp](https://github.com/ggerganov/llama.cpp). Supports GGUF models, vision/multimodal models, and streaming. Broadest hardware support of the three native providers.

**Supported platforms:**

| OS | Architecture | CPU | Metal | Accelerate | Vulkan | CUDA 12.8 | ROCm 7.2 |
|---|---|:---:|:---:|:---:|:---:|:---:|:---:|
| Linux | x86_64 | ✓ | — | — | ✓ | ✓ | Build only* |
| Linux | arm64 | ✓ | — | — | ✓ | ✓ | — |
| macOS | x86_64 | — | ✓ | — | — | — | — |
| macOS | arm64 | — | ✓ | — | — | — | — |
| Windows | x86_64 | ✓ | — | — | — | ✓ | — |

*ROCm is published with compiled target coverage; GPU runtime validation is pending. See [AMD ROCm compatibility](#amd-rocm-compatibility).

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

=== "Linux (ROCm, x86_64)"
    ```toml
    [[providers]]
    name = "llama-cpp"
    path = "oci://ghcr.io/querymt/llama-cpp:latest-rocm7.2"

    [providers.config]
    model = "/path/to/model.gguf"
    n_ctx = 4096
    n_gpu_layers = 99
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
