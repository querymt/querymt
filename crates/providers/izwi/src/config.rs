use izwi_core::EngineConfig;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_TTS_MODEL: &str = "Qwen3-TTS-12Hz-0.6B-Base-4bit";
pub const DEFAULT_STT_MODEL: &str = "Qwen3-ASR-0.6B";
pub const DEFAULT_AUDIO_FORMAT: &str = "wav";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IzwiConfig {
    /// Default TTS model name from the izwi catalog.
    /// Used when the request does not specify a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_model: Option<String>,

    /// Default STT model name from the izwi catalog.
    /// Used when the request does not specify a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stt_model: Option<String>,

    /// Default voice/speaker hint for TTS (model-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,

    /// Default output audio format (`wav`, `pcm`, `raw_f32`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// Default speech speed multiplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,

    /// Whether to automatically download models that are not locally available.
    #[serde(default)]
    pub auto_download: bool,

    // -- Engine-level settings (affect RuntimeService construction) ----------
    /// Directory where izwi stores downloaded models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_batch_size: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sequence_length: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache_dtype: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_page_size: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_metal: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_threads: Option<usize>,
}

impl Default for IzwiConfig {
    fn default() -> Self {
        Self {
            tts_model: None,
            stt_model: None,
            voice: None,
            format: None,
            speed: None,
            auto_download: false,
            models_dir: None,
            max_batch_size: None,
            max_sequence_length: None,
            chunk_size: None,
            kv_cache_dtype: None,
            kv_page_size: None,
            use_metal: None,
            num_threads: None,
        }
    }
}

impl IzwiConfig {
    pub fn engine_config(&self) -> EngineConfig {
        let mut cfg = EngineConfig::default();

        if let Some(models_dir) = self
            .models_dir
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            cfg.models_dir = PathBuf::from(models_dir);
        }
        if let Some(max_batch_size) = self.max_batch_size {
            cfg.max_batch_size = max_batch_size;
        }
        if let Some(max_sequence_length) = self.max_sequence_length {
            cfg.max_sequence_length = max_sequence_length;
        }
        if let Some(chunk_size) = self.chunk_size {
            cfg.chunk_size = chunk_size;
        }
        if let Some(ref kv_cache_dtype) = self.kv_cache_dtype {
            cfg.kv_cache_dtype = kv_cache_dtype.clone();
        }
        if let Some(kv_page_size) = self.kv_page_size {
            cfg.kv_page_size = kv_page_size;
        }
        if let Some(use_metal) = self.use_metal {
            cfg.use_metal = use_metal;
        }
        if let Some(num_threads) = self.num_threads {
            cfg.num_threads = num_threads;
        }

        cfg
    }

    pub fn default_tts_model(&self) -> &str {
        self.tts_model.as_deref().unwrap_or(DEFAULT_TTS_MODEL)
    }

    pub fn default_stt_model(&self) -> &str {
        self.stt_model.as_deref().unwrap_or(DEFAULT_STT_MODEL)
    }

    pub fn default_audio_format(&self) -> &str {
        self.format.as_deref().unwrap_or(DEFAULT_AUDIO_FORMAT)
    }
}
