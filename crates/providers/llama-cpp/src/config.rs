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
    /// Top-k sampling.
    pub top_k: Option<u32>,
    /// System prompt to prepend to chat requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<String>,
    /// Override model context length.
    pub n_ctx: Option<u32>,
    /// Batch size for llama.cpp decoding.
    pub n_batch: Option<u32>,
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
