## Why

The llama.cpp provider constrains tool calls with GBNF grammars. The GBNF engine ran over the whole vocabulary (150k-260k tokens) before every sampled token. After the fix that lets string values contain `<`, it took about 0.5 s of CPU per token inside a tool call. Long `write_file`/`edit` calls became CPU-bound and the GPUs sat idle, which made local agent runs and the AREX-2 benchmark unusable. llguidance computes the same masks from a token trie in about 0.3 ms, and checking one sampled token takes under 1 µs. Today only two of the six tool formats have an llguidance grammar. Four formats, Qwen3 JSON, Ministral, LFM2 and FunctionGemma, check argument structure but not the tool's parameter schema.

## What Changes

- **BREAKING (behaviour):** tool-call grammars for local llama.cpp models use llguidance by default. A new provider parameter `grammar_backend` (`llguidance` | `gbnf`, default `llguidance`) selects the engine.
- llguidance grammars for all six tool formats the provider detects: Qwen3 JSON, Qwen XML-function, Ministral, LFM2, FunctionGemma and Gemma 4.
- **BREAKING (behaviour):** tool arguments are schema-aware in every format:
  - parameter names, value types, enums and required parameters follow the tool's JSON schema;
  - undeclared parameters are allowed only when the schema allows additional properties.
  
  Before this change, only the Qwen XML-function format checked the schema; the others accepted any JSON object or keyword list.
- Fallback chain:
  1. If a tool set's schema cannot be compiled, the provider uses a generic llguidance grammar for the format. It is structural only and matches today's GBNF language.
  2. If llguidance cannot be used, or fails during a generation, the provider uses the GBNF grammar for the rest of that request.
  
  Each fallback is logged once per request.
- Grammars are enforced by rejection sampling. The grammar checks the sampled token, and the whole-vocabulary mask runs only when that token is rejected. This applies to every decoding path: streaming, non-streaming, structured output, speculative (MTP) decoding, and tokens forced by the thinking budget.
- Text values (`</parameter>`, `<escape>` and `<|"|>` delimited) accept any text, including `<`, except their own terminator. The GBNF rendering of this rule is narrowed so that every state has at most four alternatives.
- Every format is verified on the tokenizer of a real model that uses it. Missing tokenizers (LFM2, FunctionGemma) and the MTP model (Qwen3.5-0.8B-MTP) are downloaded from Hugging Face at pinned revisions; for tokenizers only the GGUF metadata prefix is fetched.
- Sampling with a fixed seed may produce different output than before, because a rejected draw consumes one extra random number.

## Non-goals

- Grammar-constrained decoding for remote/API providers or for the mistral.rs and izwi native providers.
- Relaxing tool schemas (ignoring `additionalProperties: false`, or treating `enum` as a hint). The grammar enforces what the tool declares.
- Schema validation of values nested inside Gemma 4 objects and arrays beyond the top-level parameter types. Nested values stay structurally constrained only.
- New tool-call formats, or changes to how tool calls are parsed out of model text.
- Moving the standard sampler chain onto the GPU (`backend_sampling`) for grammar-constrained requests.

## Capabilities

### New Capabilities

- `local-tool-call-grammars`: how a local llama.cpp model's output is constrained while it writes tool calls. Covers the engine setting, text-value rules, lazy activation, schema-aware arguments, fallbacks, rejection sampling, decoding-path coverage, grammar overhead, and verification on real model tokenizers fetched from pinned llama.cpp and Hugging Face sources (including a real MTP model).

### Modified Capabilities

(none: no existing spec covers local tool-call grammars. `tool-argument-handling` validates arguments after generation and is unchanged.)

## Impact

- **Packages:** `qmt-llama-cpp` only. `cargo tree -i qmt-llama-cpp --workspace --depth 1` lists no dependents.
- **Feature flags:**
  - llguidance code is under `common`, which is enabled by `native`, `cuda`, `metal`, `rocm` and `vulkan`.
  - Builds without `common` keep GBNF only.
  - The provider is native-only. The wasm32-wasip1 and Extism plugin contracts are unaffected.
- **Public Rust API:**
  - **BREAKING:** `qmt_llama_cpp::LlamaCppConfig` gains a public field `grammar_backend: Option<GrammarBackend>`, so struct literals must add it.
  - New exported enum `qmt_llama_cpp::GrammarBackend`.
- **Config:**
  - New provider parameter `grammar_backend`, accepted in an agent profile's `[agent.parameters]` and in llama-cpp provider parameters.
  - **BREAKING:** the default engine and argument strictness change.
- **Not changed:** hooks, middleware, ACP, session DB, Python bindings, typeshare types.
- **querymt-desktop:** no UI follow-up required. The provider's parameter JSON schema gains one enum field, which generic parameter editors pick up.
- **Dependencies:** none new. `llguidance` and `toktrie` are already optional dependencies of `qmt-llama-cpp` under `common`.
- **Docs and changelog:** `docs/docs/core/available-providers.md` (llama-cpp section) and a CHANGELOG.md "Unreleased" entry.
