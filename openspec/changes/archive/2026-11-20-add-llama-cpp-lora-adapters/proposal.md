## Why

The llama-cpp provider can only run a GGUF model as published. Users who fine-tune with LoRA have to merge the adapter into a full model copy (one copy per fine-tune, several GB each), or switch to llama-server. llama.cpp already loads GGUF LoRA adapters at runtime and applies them per context with a strength (`--lora`, `--lora-scaled`). The pinned `llama-cpp-2` (0.1.161) exposes this, so the provider can offer it without a new native dependency.

## What Changes

- New llama-cpp provider option `lora`: a list of adapters, each with a `path` (local GGUF path or Hugging Face `<repo>:<selector>`, resolved like `model`) and an optional `scale` (finite float, default `1.0`).
- Adapters are loaded once with the base model and applied to every generation context: plain, streaming, tool-call and multimodal generation, and the target side of speculative (MTP) decoding.
- A `scale` of `0` keeps the adapter loaded but does not apply it, so output matches the base model.
- Validation runs before the model is loaded. A non-finite scale, a missing adapter file, a Hugging Face repo without a selector, or more adapters than supported fails provider configuration with an error that names the adapter.
- An adapter that does not match the base model (different architecture or tensor shapes, or a file that is not a LoRA adapter) fails provider configuration with an error that names the adapter path.
- The ordered list of adapter paths becomes part of the model cache key, so changing it reloads the model. Scales are applied per generation and stay out of the key, so delegates that share a model can use different scales without a reload.
- At most **one** adapter per provider in this change. The `llama-cpp-2` wrapper replaces the whole adapter set on every call (`LlamaContext::lora_adapter_set` passes `n_adapters = 1`). The option is still a list, so lifting the limit later does not change the config shape.
- Public Rust API: `qmt_llama_cpp::LlamaCppConfig` gains `pub lora: Vec<LoraAdapterConfig>`. New exports: `qmt_llama_cpp::LoraAdapterConfig` (`#[non_exhaustive]`, built with `LoraAdapterConfig::new(path)` and `.with_scale(scale)`) and `qmt_llama_cpp::LoraScale` (a finite `f32`, validated on construction and during deserialization). **BREAKING** for code that builds `LlamaCppConfig` with a struct literal: it must add `lora: Vec::new()`.
- Docs: a `lora` row and a "LoRA adapters" section in `docs/docs/core/available-providers.md`. A CHANGELOG `Unreleased` entry under Added and under Breaking.

## Non-goals

- Stacking several adapters, or a different adapter set per request. Both need a multi-adapter call in `llama-cpp-2` (an upstream PR is listed as a follow-up in tasks, outside this change). Lifting the one-adapter limit is a follow-up change.
- Choosing an adapter or scale per request through `GenerationOptions`.
- Activated LoRA (aLoRA) invocation-token handling. `llama-cpp-2` does not expose it, so an aLoRA file is applied as a plain LoRA (documented).
- Control vectors (`llama_set_adapter_cvec`).
- Applying adapters to the MTP draft model or to the multimodal projector.
- Counting adapter memory in the startup memory advisory.
- Converting PEFT/safetensors adapters to GGUF. Users run llama.cpp's `convert_lora_to_gguf.py`.

## Capabilities

### New Capabilities
- `llama-cpp-lora-adapters`: configuring, validating, loading and applying LoRA adapters in the llama-cpp provider, and how adapters affect model reuse.

### Modified Capabilities
<!-- none: no existing spec covers llama-cpp model loading. The in-flight change add-llama-cpp-rpc-mesh-placement introduces llama-cpp-device-placement; this change does not modify its requirements. -->

## Impact

- Packages: `qmt-llama-cpp` only (`crates/providers/llama-cpp`). Code changes live under the `native`/`common` builds; no feature flag is added or changed. `cargo tree -i qmt-llama-cpp --workspace` shows no workspace dependents.
- Dependencies: `thiserror` (already a workspace dependency, and already compiled through `llama-cpp-2`) becomes a direct dependency of `qmt-llama-cpp`.
- Provider config (JSON/TOML, `[providers.config]` or `[agent.parameters]`): new optional key `lora`. Existing configs parse unchanged. Unknown keys inside an adapter entry are rejected.
- Public Rust API: the **BREAKING** `LlamaCppConfig` field and the new `LoraAdapterConfig` type, as above.
- Unaffected: hooks, middleware, ACP, Extism plugin contract, session DB schema, Python bindings and typeshare types. The provider is native-only, so wasm32-wasip1 builds are not involved.
- No querymt-desktop UI follow-up is required. The provider config schema (`config_schema`) gains the `lora` field automatically.
- External: an upstream PR to `llama-cpp-2` (multi-adapter set; adapter setter taking `&self`) is a follow-up, not part of this change. This change does not depend on it.
