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
    /// System prompt to prepend to chat requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// Enable high-throughput HuggingFace Hub downloads. Uses multiple parallel
    /// connections to saturate high-bandwidth connections (>500MB/s). This will
    /// heavily utilize CPU cores during download. Only recommended for cloud
    /// instances with high CPU and bandwidth.
    pub fast_download: Option<bool>,
    /// Enable thinking/reasoning output from the model.
    /// When true, the template is rendered with thinking support and
    /// `<think>` blocks are parsed into separate reasoning_content.
    /// Defaults to true.
    pub enable_thinking: Option<bool>,
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
