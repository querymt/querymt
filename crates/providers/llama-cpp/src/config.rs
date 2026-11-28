use querymt::chat::ReasoningEffort;
use querymt::chat::{GenerationOptions, StructuredOutputFormat};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// Default maximum tokens to generate when not specified.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 256;

/// Flash attention type constants from llama.h
pub(crate) const LLAMA_FLASH_ATTN_TYPE_AUTO: i32 = -1;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_DISABLED: i32 = 0;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_ENABLED: i32 = 1;

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
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
    /// Keep a full-size KV cache for sliding-window attention (SWA) layers.
    ///
    /// Defaults to `false`, as in llama.cpp's own tools: SWA layers only cache
    /// their window, which for models such as Inkling-2 saves a very large
    /// amount of memory at long contexts. `true` lets llama.cpp reuse older
    /// cached prompt prefixes at the cost of a full-size cache on every layer,
    /// and can exhaust GPU memory (`Decode Error -3`).
    pub swa_full: Option<bool>,
    /// Fit the model to free device memory when it is loaded, as `llama-cli`
    /// does by default (llama.cpp `--fit`). Defaults to `false`.
    ///
    /// Chooses `n_gpu_layers`, the tensor split and which expert tensors stay
    /// on the CPU so that the model and its KV cache fit. When `n_ctx` is not
    /// set it also picks the largest context that fits (at least
    /// `fit_min_ctx`). Like llama.cpp, fitting only changes what is still
    /// unset, so it is skipped (with a log message) when `n_gpu_layers`, a
    /// device placement (`devices`, `tensor_split`, `split_mode`,
    /// `main_gpu`) or `speculative` decoding is configured.
    pub fit: Option<bool>,
    /// Smallest context size `fit` may choose when `n_ctx` is not set.
    /// Defaults to 4096.
    pub fit_min_ctx: Option<u32>,
    /// Free memory, in MiB, that `fit` leaves unused on each device.
    /// Defaults to 1024.
    pub fit_margin_mib: Option<u32>,
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
    /// `llguidance` (default) compiles the tool grammar to an llguidance
    /// (Lark) grammar, whose token masks are computed from a token trie and
    /// which follows the tools' parameter schemas. It falls back to the
    /// `gbnf` grammar when it cannot be built or fails mid-request, and in
    /// builds without llguidance. `gbnf` uses llama.cpp's GBNF grammar.
    /// Both are applied by rejection sampling: the grammar checks the
    /// sampled token and is applied to the whole vocabulary only when that
    /// token is rejected.
    pub grammar_backend: Option<GrammarBackend>,
    /// Backend devices the model is loaded onto, by name (case-insensitive),
    /// e.g. `["CUDA0", "CUDA1"]` or `["Vulkan1"]`.
    ///
    /// Names are the ggml device names (see llama.cpp's `--list-devices`).
    /// When omitted, llama.cpp uses every detected GPU. CPU/ACCEL devices do
    /// not need to be listed. The order here defines the order of
    /// `tensor_split` and the meaning of `main_gpu`.
    pub devices: Option<Vec<String>>,
    /// Each device's relative share of the model, parallel to `devices`
    /// (or to llama.cpp's device enumeration order when `devices` is unset).
    ///
    /// Shares are relative and need not sum to one: `[3, 1]` puts 75% of the
    /// model on the first device and 25% on the second. When omitted,
    /// llama.cpp splits in proportion to each device's free memory.
    pub tensor_split: Option<Vec<f32>>,
    /// How the model is split across devices. llama.cpp defaults to `layer`.
    pub split_mode: Option<SplitMode>,
    /// Index into the selected device list of the GPU used for the whole
    /// model (`split_mode = none`) or for intermediate results (`row`).
    /// Defaults to 0.
    pub main_gpu: Option<u32>,
    /// LoRA adapters applied on top of `model`. At most one adapter is
    /// supported. Omit or leave empty to run the base model.
    ///
    /// Changing the adapter paths reloads the model; changing a `scale` does not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lora: Vec<LoraAdapterConfig>,
}

/// Strength of a LoRA adapter: any finite `f32`. `0` disables the adapter and
/// negative values are allowed.
///
/// A `LoraScale` is always finite; the check runs on construction and while
/// deserializing.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Option<f32>", into = "f32")]
pub struct LoraScale(f32);

impl LoraScale {
    /// Creates a scale from a finite value.
    ///
    /// # Errors
    ///
    /// Returns [`LoraScaleError`] when `value` is NaN or infinite.
    ///
    /// # Examples
    ///
    /// ```
    /// use qmt_llama_cpp::LoraScale;
    ///
    /// # fn main() -> Result<(), qmt_llama_cpp::LoraScaleError> {
    /// let scale = LoraScale::new(0.5)?;
    /// assert_eq!(scale.get(), 0.5);
    /// assert!(LoraScale::new(f32::NAN).is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub const fn new(value: f32) -> Result<Self, LoraScaleError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(LoraScaleError { value: Some(value) })
        }
    }

    /// Returns the scale as an `f32`.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// Returns `true` when the scale is zero, so the adapter is not applied.
    #[must_use]
    pub const fn is_disabled(self) -> bool {
        // `0` is a user-written sentinel, not a computed value; `-0.0 == 0.0`.
        self.0 == 0.0
    }
}

impl Default for LoraScale {
    fn default() -> Self {
        Self(1.0)
    }
}

impl TryFrom<f32> for LoraScale {
    type Error = LoraScaleError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// `None` stands for a JSON `null`, which is what a TOML `nan`/`inf` becomes
/// once it reaches the provider as JSON.
impl TryFrom<Option<f32>> for LoraScale {
    type Error = LoraScaleError;

    fn try_from(value: Option<f32>) -> Result<Self, Self::Error> {
        value.map_or(Err(LoraScaleError { value: None }), Self::new)
    }
}

impl From<LoraScale> for f32 {
    fn from(scale: LoraScale) -> Self {
        scale.get()
    }
}

impl JsonSchema for LoraScale {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LoraScale".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "number",
            "default": 1.0,
            "description": "LoRA adapter strength: a finite number. 0 disables the adapter."
        })
    }
}

/// A LoRA scale that is not a finite number.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error(
    "invalid LoRA scale {}: must be a finite number",
    .value.map_or_else(|| "null".to_owned(), |v| v.to_string())
)]
pub struct LoraScaleError {
    /// `None` when the input was `null`.
    value: Option<f32>,
}

/// One LoRA adapter to load with the base model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LoraAdapterConfig {
    /// Local GGUF path or Hugging Face `<repo>:<selector>`, resolved like `model`.
    pub path: String,
    /// Adapter strength. Defaults to `1.0`; `0` keeps the adapter loaded but
    /// does not apply it.
    #[serde(default)]
    pub scale: LoraScale,
}

impl LoraAdapterConfig {
    /// Creates an adapter entry with the default scale of `1.0`.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            scale: LoraScale::default(),
        }
    }

    /// Sets the adapter strength.
    #[must_use]
    pub const fn with_scale(mut self, scale: LoraScale) -> Self {
        self.scale = scale;
        self
    }
}

impl LlamaCppConfig {
    /// Creates a config for `model` with every other option unset.
    ///
    /// Set options with struct-update syntax, which keeps working when new
    /// options are added:
    ///
    /// ```
    /// use qmt_llama_cpp::LlamaCppConfig;
    ///
    /// let cfg = LlamaCppConfig {
    ///     n_ctx: Some(4096),
    ///     max_tokens: Some(256),
    ///     ..LlamaCppConfig::new("/models/base.gguf")
    /// };
    /// assert_eq!(cfg.model, "/models/base.gguf");
    /// assert!(cfg.lora.is_empty());
    /// ```
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }
}

/// How a model is distributed across devices. See [`LlamaCppConfig::split_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SplitMode {
    /// Use a single device (`main_gpu`).
    None,
    /// Split layers and KV cache across devices (llama.cpp default).
    Layer,
    /// Split layers and KV cache across devices, using tensor parallelism
    /// where supported.
    Row,
    /// Experimental tensor parallelism across devices.
    Tensor,
}

impl From<SplitMode> for llama_cpp_2::model::params::LlamaSplitMode {
    fn from(mode: SplitMode) -> Self {
        match mode {
            SplitMode::None => Self::None,
            SplitMode::Layer => Self::Layer,
            SplitMode::Row => Self::Row,
            SplitMode::Tensor => Self::Tensor,
        }
    }
}

/// Grammar engine for tool calls. See [`LlamaCppConfig::grammar_backend`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrammarBackend {
    /// llama.cpp GBNF grammar sampler.
    Gbnf,
    /// llguidance token-trie matcher.
    #[default]
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
    /// Device used for a sidecar model, as an index into the target's
    /// selected `devices` list. A sidecar is small, so it is always loaded on
    /// a single device rather than split. Defaults to the target's `main_gpu`.
    pub main_gpu: Option<u32>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
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
    use super::{GrammarBackend, LlamaCppConfig, LoraScale, SpeculativeConfig, SpeculativeType};

    fn parse_json(lora: &str) -> Result<LlamaCppConfig, serde_json::Error> {
        serde_json::from_str(&format!(r#"{{"model": "m.gguf", "lora": {lora}}}"#))
    }

    /// Keep in sync with the "LoRA adapters" section of
    /// `docs/docs/core/available-providers.md`.
    const DOCUMENTED_LORA_EXAMPLE: &str = r#"
model = "/models/base.gguf"
lora = [{ path = "/models/style-lora.gguf", scale = 0.8 }]
"#;

    #[test]
    fn documented_lora_example_parses() {
        let cfg: LlamaCppConfig = toml::from_str(DOCUMENTED_LORA_EXAMPLE).unwrap();
        assert_eq!(cfg.lora.len(), 1);
        assert_eq!(cfg.lora[0].path, "/models/style-lora.gguf");
        assert_eq!(cfg.lora[0].scale.get(), 0.8);
        let docs = include_str!("../../../../docs/docs/core/available-providers.md");
        assert!(
            docs.contains(r#"lora = [{ path = "/models/style-lora.gguf", scale = 0.8 }]"#),
            "the docs example drifted from DOCUMENTED_LORA_EXAMPLE"
        );
    }

    #[test]
    fn lora_omitted_keeps_the_base_model() {
        let cfg: LlamaCppConfig = serde_json::from_str(r#"{"model": "m.gguf"}"#).unwrap();
        assert!(cfg.lora.is_empty());
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("lora").is_none(), "empty lora must be omitted");
    }

    #[test]
    fn adapter_scale_defaults_to_one() {
        let cfg = parse_json(r#"[{"path": "/models/style.gguf"}]"#).unwrap();
        assert_eq!(cfg.lora[0].scale.get(), 1.0);
        let cfg = parse_json(r#"[{"path": "/models/style.gguf", "scale": -0.5}]"#).unwrap();
        assert_eq!(cfg.lora[0].scale.get(), -0.5);
    }

    #[test]
    fn unknown_adapter_key_rejected() {
        let err = parse_json(r#"[{"path": "/models/style.gguf", "weight": 0.5}]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("weight"), "{err}");
    }

    #[test]
    fn non_finite_adapter_scale_rejected() {
        for literal in ["nan", "inf", "-inf"] {
            let src = format!(
                "model = \"m.gguf\"\nlora = [{{ path = \"/models/style.gguf\", scale = {literal} }}]"
            );
            let err = toml::from_str::<LlamaCppConfig>(&src)
                .unwrap_err()
                .to_string();
            assert!(err.contains("must be a finite number"), "{literal}: {err}");
        }
        let err = parse_json(r#"[{"path": "/models/style.gguf", "scale": null}]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a finite number"), "{err}");
        assert!(LoraScale::new(f32::NAN).is_err());
    }

    #[test]
    fn grammar_backend_rejects_unknown_value() {
        let err = serde_json::from_value::<super::LlamaCppConfig>(serde_json::json!({
            "model": "test.gguf", "grammar_backend": "fast"
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("fast") && err.contains("llguidance"), "{err}");
        let ok: super::LlamaCppConfig = serde_json::from_value(serde_json::json!({
            "model": "test.gguf", "grammar_backend": "gbnf"
        }))
        .unwrap();
        assert_eq!(ok.grammar_backend, Some(GrammarBackend::Gbnf));
        assert_eq!(GrammarBackend::default(), GrammarBackend::Llguidance);
    }

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
            main_gpu: None,
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

    #[test]
    fn memory_options_default_to_unset_and_deserialize() {
        let cfg: LlamaCppConfig = serde_json::from_str(r#"{"model": "m.gguf"}"#).unwrap();
        assert_eq!(cfg.swa_full, None);
        assert_eq!(cfg.fit, None);
        assert_eq!(cfg.fit_min_ctx, None);
        assert_eq!(cfg.fit_margin_mib, None);

        let cfg: LlamaCppConfig = serde_json::from_str(
            r#"{"model": "m.gguf", "swa_full": true, "fit": true,
                "fit_min_ctx": 8192, "fit_margin_mib": 2048}"#,
        )
        .unwrap();
        assert_eq!(cfg.swa_full, Some(true));
        assert_eq!(cfg.fit, Some(true));
        assert_eq!(cfg.fit_min_ctx, Some(8192));
        assert_eq!(cfg.fit_margin_mib, Some(2048));
    }
}
