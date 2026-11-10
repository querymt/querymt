use querymt::chat::ReasoningEffort;
use querymt::chat::{GenerationOptions, StructuredOutputFormat};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default maximum tokens to generate when not specified.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 256;

/// Flash attention type constants from llama.h
pub(crate) const LLAMA_FLASH_ATTN_TYPE_AUTO: i32 = -1;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_DISABLED: i32 = 0;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_ENABLED: i32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct LlamaCppConfig {
    /// Model reference. Supports local GGUF paths and Hugging Face refs `<repo>:<selector>`.
    pub model: String,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature; set to 0 for greedy.
    pub temperature: Option<f32>,
    /// Top-p sampling.
    pub top_p: Option<f32>,
    /// Min-p sampling.
    pub min_p: Option<f32>,
    /// Top-k sampling.
    pub top_k: Option<u32>,
    /// Repeat penalty. Penalizes tokens that have already appeared in the context.
    /// 1.0 = disabled. Typical range: 1.0–1.5.
    pub repeat_penalty: Option<f32>,
    /// Presence penalty. Increases the likelihood of new tokens appearing.
    /// 0.0 = disabled. Typical range: 0.0–2.0.
    pub presence_penalty: Option<f32>,
    /// Frequency penalty. Penalizes tokens proportional to how often they have appeared.
    /// 0.0 = disabled. Typical range: 0.0–2.0.
    pub frequency_penalty: Option<f32>,
    /// Number of last tokens to consider for repeat/presence/frequency penalties.
    /// Matches native llama.cpp semantics: 0 disables penalties and negative values
    /// are clamped to 0. Defaults to 64 when any penalty is set but this is not
    /// explicitly configured.
    pub penalty_last_n: Option<i32>,
    /// System prompt to prepend to chat requests.
    #[serde(
        default,
        deserialize_with = "querymt::params::deserialize_system_vec",
        skip_serializing_if = "Vec::is_empty"
    )]
    #[schemars(schema_with = "querymt::params::schema_for_system_prompts")]
    pub system: Vec<String>,
    /// Override model context length.
    pub n_ctx: Option<u32>,
    /// Batch size for llama.cpp decoding.
    pub n_batch: Option<u32>,
    /// Physical batch size (micro-batch) for llama.cpp prompt processing.
    ///
    /// Vision models (Qwen-VL, LLaVA, etc.) use non-causal attention when
    /// decoding image embeddings, which requires that all image tokens fit in
    /// a single physical batch: `n_ubatch >= <image token count>`.
    ///
    /// When a multimodal projection (mmproj) is active and this value is not
    /// set, the provider automatically uses `n_batch` as `n_ubatch` so that
    /// any image that fits in the logical batch also fits in a single ubatch.
    /// Set this explicitly only when you need fine-grained control.
    pub n_ubatch: Option<u32>,
    /// Threads for evaluation.
    pub n_threads: Option<i32>,
    /// Threads for batch evaluation.
    pub n_threads_batch: Option<i32>,
    /// GPU layers to offload, if supported.
    pub n_gpu_layers: Option<u32>,
    /// RNG seed for sampling.
    pub seed: Option<u32>,
    /// Explicit chat template name (defaults to model's template).
    pub chat_template: Option<String>,
    /// Disable llama.cpp chat template usage and fall back to a simple prompt format.
    pub use_chat_template: Option<bool>,
    /// Control whether to add BOS when tokenizing prompts.
    pub add_bos: Option<bool>,
    /// Logging destination for llama.cpp output.
    pub log: Option<LlamaCppLogMode>,
    /// Enable thinking/reasoning output from the model.
    /// When true, the template is rendered with thinking support and
    /// `<think>` blocks are parsed into separate reasoning_content.
    /// Defaults to true.
    pub enable_thinking: Option<bool>,
    /// Portable reasoning effort. The selected chat template determines the
    /// provider-specific value rendered for this level.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Preserve assistant reasoning from earlier turns in the rendered prompt.
    ///
    /// `None` leaves the behavior to the chat template, while explicit true or
    /// false maps to the compatibility variables used by llama.cpp templates.
    pub preserve_reasoning: Option<bool>,
    /// Maximum tokens the model may spend inside a reasoning block per reply.
    ///
    /// When the budget is spent and the model is still reasoning, the provider
    /// inserts the end-of-reasoning marker so the model has to answer. This
    /// stops runaway reasoning from consuming the whole `max_tokens` cap.
    /// `None` (default) leaves reasoning unbounded. Not applied with
    /// `speculative` decoding, when `json_schema` constrains the output, or to
    /// plain non-streaming `chat()` calls that use `generate()` (including
    /// fallback prompts).
    pub thinking_budget: Option<u32>,
    /// Flash attention policy. Enables flash attention for faster inference
    /// and is required for KV cache quantization. Supported on Metal (Apple
    /// Silicon) and CUDA backends.
    /// Values: "auto" (let llama.cpp decide), "enabled", "disabled".
    /// Defaults to None (llama.cpp default, typically disabled).
    pub flash_attention: Option<FlashAttentionPolicy>,
    /// Quantization type for KV cache keys. Reduces memory usage at the cost
    /// of some precision. Requires flash_attention to be "auto" or "enabled".
    /// Common values: "f16" (default), "q8_0" (half memory), "q4_0" (quarter).
    pub kv_cache_type_k: Option<String>,
    /// Quantization type for KV cache values. Reduces memory usage at the cost
    /// of some precision. Requires flash_attention to be "auto" or "enabled".
    /// Common values: "f16" (default), "q8_0" (half memory), "q4_0" (quarter).
    pub kv_cache_type_v: Option<String>,
    /// Optional path to multimodal projection file (mmproj).
    ///
    /// If not set, the provider will attempt to use built-in vision
    /// capabilities if the model supports it (detected via RopeType::Vision/MRope).
    ///
    /// Set this explicitly for models that require separate projection
    /// files, such as Gemma 3 which uses mmproj-F16.gguf.
    ///
    /// Supports the same path formats as model_path (local paths and hf: prefix).
    ///
    /// To suppress mmproj loading entirely (e.g. to save VRAM when running a
    /// VL model in text-only mode), set `text_only = true` instead.
    pub mmproj_path: Option<String>,
    /// Media marker string for identifying image/audio positions in prompts.
    ///
    /// Defaults to "<__media__>" (llama.cpp default) if not set.
    /// Some models require specific markers:
    /// - Gemma 3: "<start_of_image>"
    /// - Most others: "<__media__>" (default)
    ///
    /// The marker will be automatically inserted into messages containing images.
    pub media_marker: Option<String>,
    /// Number of threads for multimodal projection processing.
    /// Defaults to n_threads if not set.
    pub mmproj_threads: Option<i32>,
    /// Whether to offload multimodal projection to GPU.
    /// Defaults to `true` when `n_gpu_layers` is not set (llama.cpp defaults
    /// to full GPU offload on Metal/CUDA) or is `> 0`, `false` when
    /// `n_gpu_layers = 0` (CPU-only mode).
    pub mmproj_use_gpu: Option<bool>,
    /// Force text-only mode. When `true`, the multimodal projection (mmproj)
    /// will not be loaded even if the model supports vision/audio and an mmproj
    /// file is available (via explicit `mmproj_path` or auto-discovery from the
    /// Hugging Face repo). This avoids the extra VRAM cost of the projector
    /// when you only need text generation from a VL model.
    /// Defaults to `false`.
    pub text_only: Option<bool>,
    /// Model-based speculative decoding.
    ///
    /// Mainline supports `type: "mtp"`. When no sidecar model is configured,
    /// the target GGUF must contain bundled NextN/MTP tensors. Bundled tensors
    /// are only loaded into the target model in that case (upstream keeps
    /// `load_mtp` off by default).
    pub speculative: Option<SpeculativeConfig>,
    /// Offload the standard sampler chain to the backend context.
    ///
    /// Grammar-constrained, MTP, and mutable fallback sampling remain on the CPU path.
    pub backend_sampling: Option<bool>,
    /// Optional structured output schema.
    ///
    /// When set, llama.cpp converts the JSON Schema into a GBNF grammar that
    /// constrains sampling to produce only valid JSON conforming to the schema.
    /// The schema is forwarded to the chat template engine via
    /// `OpenAIChatTemplateParams::json_schema`.
    pub json_schema: Option<StructuredOutputFormat>,
    /// Engine that enforces tool-call grammars.
    ///
    /// `gbnf` (default) uses llama.cpp's GBNF grammar. `llguidance` compiles
    /// the tool grammar to an llguidance (Lark) grammar, whose token masks
    /// are computed from a token trie; it is used for the Qwen XML-function
    /// (`<function=..>`) and Ministral formats and falls back to `gbnf` for
    /// the others. Both are applied by rejection sampling: the grammar checks
    /// the sampled token and is applied to the whole vocabulary only when
    /// that token is rejected.
    pub grammar_backend: Option<GrammarBackend>,
}

/// Grammar engine for tool calls. See [`LlamaCppConfig::grammar_backend`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrammarBackend {
    /// llama.cpp GBNF grammar sampler.
    #[default]
    Gbnf,
    /// llguidance token-trie matcher.
    Llguidance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SpeculativeType {
    /// Multi-token prediction using bundled tensors or a sidecar model.
    Mtp,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct SpeculativeConfig {
    /// Speculative decoding implementation.
    #[serde(rename = "type")]
    pub kind: SpeculativeType,
    /// Optional MTP sidecar model. If omitted, use bundled MTP tensors.
    pub model: Option<String>,
    /// Maximum number of draft tokens proposed per step. Defaults to 3.
    pub n_max: Option<u32>,
    /// Minimum useful draft length. Defaults to 0.
    pub n_min: Option<u32>,
    /// Minimum draft-token probability. Defaults to 0.0.
    pub p_min: Option<f32>,
    /// GPU layers for a sidecar model. Defaults to the target model setting.
    pub n_gpu_layers: Option<u32>,
}

impl SpeculativeConfig {
    /// Whether MTP should use the tensors bundled in the target GGUF.
    ///
    /// True only when no sidecar model is configured; llama-cpp-2 keeps
    /// `load_mtp` disabled by default, so the provider must opt in when the
    /// target model is loaded.
    pub(crate) fn uses_bundled_mtp(&self) -> bool {
        self.model.is_none()
    }

    pub(crate) fn params(&self) -> Result<llama_cpp_2::speculative::MtpSpeculativeParams, String> {
        let n_max = self.n_max.unwrap_or(3);
        let n_min = self.n_min.unwrap_or(0);
        let p_min = self.p_min.unwrap_or(0.0);
        if n_max == 0
            || n_max > i32::MAX as u32
            || n_min > n_max
            || !p_min.is_finite()
            || !(0.0..=1.0).contains(&p_min)
        {
            return Err(
                "speculative decoding requires n_max > 0, n_min <= n_max, and p_min in 0..=1"
                    .into(),
            );
        }
        Ok(llama_cpp_2::speculative::MtpSpeculativeParams {
            n_max: n_max as i32,
            n_min: n_min as i32,
            p_min,
        })
    }
}

impl LlamaCppConfig {
    /// The JSON schema constraining the output, when structured output is on.
    pub(crate) fn structured_schema(&self) -> Option<&serde_json::Value> {
        self.json_schema.as_ref().and_then(|s| s.schema.as_ref())
    }

    /// The reasoning budget to enforce. A schema-constrained grammar is always
    /// active and would reject the forced end-of-reasoning marker, so no budget
    /// is applied when structured output is on.
    pub(crate) fn effective_thinking_budget(&self) -> Option<u32> {
        if self.structured_schema().is_some() {
            None
        } else {
            self.thinking_budget
        }
    }

    /// This config with a request's per-call overrides applied.
    pub(crate) fn with_overrides(&self, options: GenerationOptions) -> Self {
        let mut cfg = self.clone();
        if let Some(max_tokens) = options.max_tokens {
            cfg.max_tokens = Some(max_tokens);
        }
        if let Some(effort) = options.reasoning_effort {
            cfg.reasoning_effort = Some(effort);
        }
        cfg
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LlamaCppLogMode {
    Stderr,
    Tracing,
    Off,
}

/// Flash attention policy for llama.cpp context.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlashAttentionPolicy {
    /// Let llama.cpp decide based on backend capabilities.
    Auto,
    /// Explicitly enable flash attention.
    Enabled,
    /// Explicitly disable flash attention.
    Disabled,
}

#[cfg(test)]
mod tests {
    use super::{SpeculativeConfig, SpeculativeType};

    #[test]
    fn portable_instruction_hints_preserve_text_for_local_models() {
        let config: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf",
            "system": ["legacy", {"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}}]
        }))
        .unwrap();
        assert_eq!(config.system, ["legacy", "stable"]);
    }

    #[test]
    fn request_overrides_replace_configured_cap_and_effort() {
        use querymt::chat::{GenerationOptions, ReasoningEffort};
        let config: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf", "max_tokens": 100, "reasoning_effort": "high"
        }))
        .unwrap();

        let unchanged = config.with_overrides(GenerationOptions::NONE);
        assert_eq!(unchanged.max_tokens, Some(100));
        assert_eq!(unchanged.reasoning_effort, Some(ReasoningEffort::High));

        let overridden = config.with_overrides(
            GenerationOptions::NONE
                .with_max_tokens(4096)
                .with_reasoning_effort(ReasoningEffort::Low),
        );
        assert_eq!(overridden.max_tokens, Some(4096));
        assert_eq!(overridden.reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn structured_schema_disables_thinking_budget() {
        let plain: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf", "thinking_budget": 128
        }))
        .unwrap();
        assert!(plain.structured_schema().is_none());
        assert_eq!(plain.effective_thinking_budget(), Some(128));

        let structured: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf",
            "thinking_budget": 128,
            "json_schema": {
                "name": "out",
                "schema": {"type": "object"},
                "strict": true
            }
        }))
        .unwrap();
        assert!(structured.structured_schema().is_some());
        assert_eq!(structured.effective_thinking_budget(), None);

        let schemaless: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf",
            "thinking_budget": 128,
            "json_schema": {"name": "out", "schema": null, "strict": null}
        }))
        .unwrap();
        assert_eq!(schemaless.effective_thinking_budget(), Some(128));
    }

    fn config(n_max: u32, n_min: u32, p_min: f32) -> SpeculativeConfig {
        SpeculativeConfig {
            kind: SpeculativeType::Mtp,
            model: None,
            n_max: Some(n_max),
            n_min: Some(n_min),
            p_min: Some(p_min),
            n_gpu_layers: None,
        }
    }

    #[test]
    fn speculative_params_validate_bounds() {
        assert!(config(3, 0, 0.0).params().is_ok());
        assert!(config(0, 0, 0.0).params().is_err());
        assert!(config(3, 4, 0.0).params().is_err());
        assert!(config(3, 0, -0.1).params().is_err());
        assert!(config(3, 0, 1.1).params().is_err());
        assert!(config(i32::MAX as u32 + 1, 0, 0.0).params().is_err());
    }
}
