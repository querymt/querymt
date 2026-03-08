use crate::config::IzwiConfig;
use async_trait::async_trait;
use base64::Engine;
use izwi_core::audio::{AudioEncoder, AudioFormat};
use izwi_core::{
    Error as IzwiError, GenerationConfig, GenerationRequest, ModelVariant, RuntimeService,
    parse_tts_model_variant, resolve_asr_model_variant,
};
use querymt::chat::{ChatMessage, ChatProvider, ChatResponse, Tool};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use querymt::{LLMProvider, stt, tts};
use std::sync::Arc;

const AUDIO_ONLY_ERR: &str = "qmt-izwi is an audio provider and supports only STT/TTS";

pub(crate) struct IzwiProvider {
    cfg: IzwiConfig,
    /// The izwi engine runtime.  All async izwi-core calls are dispatched
    /// onto [`izwi_rt`] via `spawn` so that izwi-core's internal
    /// `tokio::spawn` / `reqwest::Client` usage lands on a runtime it owns.
    runtime: Arc<RuntimeService>,
    /// Dedicated tokio runtime for izwi-core.
    ///
    /// izwi-core internally uses `tokio::spawn` (step-driver loop),
    /// `tokio::task::spawn_blocking` (model weight loading) and
    /// `reqwest::Client` (which captures a `Handle` at build time).
    /// All of these require a reactor to be present on the current thread.
    ///
    /// We run every izwi-core future on this runtime via
    /// `izwi_rt.spawn(fut).await` so the caller's own tokio runtime is
    /// never blocked and izwi-core always has the reactor it needs.
    izwi_rt: Arc<tokio::runtime::Runtime>,
}

impl IzwiProvider {
    pub(crate) fn new(cfg: IzwiConfig) -> Result<Self, LLMError> {
        let izwi_rt = build_izwi_runtime()?;
        // RuntimeService::new() is sync but its internals (reqwest::Client)
        // need a tokio reactor on the current thread.  Enter the runtime
        // context so Handle::current() succeeds.
        let runtime = {
            let _guard = izwi_rt.enter();
            RuntimeService::new(cfg.engine_config()).map_err(map_izwi_error)?
        };
        Ok(Self {
            cfg,
            runtime: Arc::new(runtime),
            izwi_rt: Arc::new(izwi_rt),
        })
    }

    /// Build a provider, reusing a cached `RuntimeService` if the cache key matches.
    ///
    /// `RuntimeService` is the expensive resource (engine threads, model manager).
    /// The cache stores it keyed on [`RuntimeCacheKey`] so multiple provider
    /// instances (e.g. different models) share the same runtime when their
    /// engine-level config is identical.  If a request arrives for a different
    /// engine config, the old runtime is evicted.
    pub(crate) fn new_with_cache(
        cfg: IzwiConfig,
        cache: &std::sync::Mutex<Option<CachedRuntime>>,
    ) -> Result<Self, LLMError> {
        let key = RuntimeCacheKey::from_config(&cfg);

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(cached) = guard.as_ref() {
            if cached.key == key {
                log::debug!("izwi RuntimeService cache hit");
                return Ok(Self {
                    cfg,
                    runtime: Arc::clone(&cached.runtime),
                    izwi_rt: Arc::clone(&cached.izwi_rt),
                });
            }
            log::info!("izwi RuntimeService cache evict (config changed)");
        }

        // Drop the guard before expensive runtime construction to avoid
        // holding the mutex longer than necessary.
        drop(guard);

        let izwi_rt = Arc::new(build_izwi_runtime()?);
        let runtime = {
            let _guard = izwi_rt.enter();
            Arc::new(RuntimeService::new(cfg.engine_config()).map_err(map_izwi_error)?)
        };

        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedRuntime {
            key,
            runtime: Arc::clone(&runtime),
            izwi_rt: Arc::clone(&izwi_rt),
        });

        Ok(Self {
            cfg,
            runtime,
            izwi_rt,
        })
    }

    /// Return all enabled audio model names from the static catalog.
    ///
    /// Included families:
    /// - TTS: Qwen3-TTS, Kokoro (`is_tts`)
    /// - ASR: Qwen3-ASR, Parakeet, Whisper (`is_asr`)
    /// - Audio-LM: LFM2.5-Audio (`is_lfm2`)
    /// - Realtime: Voxtral (`is_voxtral`)
    ///
    /// Excluded (for now): chat/GGUF models, diarization, forced-aligner.
    pub(crate) fn list_models() -> Vec<String> {
        ModelVariant::all()
            .iter()
            .copied()
            .filter(|v| {
                v.is_enabled() && (v.is_tts() || v.is_asr() || v.is_lfm2() || v.is_voxtral())
            })
            .map(|v| v.dir_name().to_string())
            .collect()
    }

    // TODO: Consider eager model loading at construction time (matching the
    // llama-cpp / mrs pattern). This would fail-fast on misconfigured
    // providers and remove the first-request latency spike from model
    // download + load. For now we keep lazy loading since the model catalog
    // is large and the provider may serve both TTS and STT with different
    // models.

    /// Dispatch an async izwi-core future onto the dedicated runtime and
    /// await its result.  This ensures izwi-core's internal `tokio::spawn`
    /// calls land on a runtime that will outlive the request.
    async fn on_izwi_rt<T, F>(&self, fut: F) -> Result<T, LLMError>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, LLMError>> + Send + 'static,
    {
        self.izwi_rt
            .spawn(fut)
            .await
            .map_err(|err| LLMError::ProviderError(format!("izwi task failed: {err}")))?
    }

    fn resolve_tts_model(&self, req: &tts::TtsRequest) -> String {
        req.model
            .clone()
            .unwrap_or_else(|| self.cfg.default_tts_model().to_string())
    }

    fn resolve_stt_model(&self, req: &stt::SttRequest) -> String {
        req.model
            .clone()
            .unwrap_or_else(|| self.cfg.default_stt_model().to_string())
    }

    fn build_generation_request(&self, req: &tts::TtsRequest) -> GenerationRequest {
        let mut gen_cfg = GenerationConfig::default();

        if let Some(speed) = req.speed.or(self.cfg.speed) {
            gen_cfg.options.speed = speed;
        }

        let mut gen_req = GenerationRequest::new(req.text.clone()).with_config(gen_cfg);

        // Map language hint.
        gen_req.language = req.language.clone();

        // Map structured voice configuration.
        match req.voice_config.as_ref() {
            Some(tts::VoiceConfig::Preset { name }) => {
                // izwi-core uses `speaker` for some model families (Kokoro)
                // and `voice` for others (Qwen3-TTS). Set both so the right
                // one is picked regardless of model.
                gen_req.config.options.speaker = Some(name.clone());
                gen_req.config.options.voice = Some(name.clone());
            }
            Some(tts::VoiceConfig::Clone {
                reference_audio,
                reference_text,
            }) => {
                // izwi-core expects base64-encoded reference audio.
                gen_req.reference_audio =
                    Some(base64::engine::general_purpose::STANDARD.encode(reference_audio));
                gen_req.reference_text = Some(reference_text.clone());
            }
            Some(tts::VoiceConfig::Design { description }) => {
                gen_req.voice_description = Some(description.clone());
            }
            None => {
                // Fall back to provider-level default voice if configured.
                if let Some(voice) = self.cfg.voice.as_deref() {
                    gen_req.config.options.speaker = Some(voice.to_string());
                    gen_req.config.options.voice = Some(voice.to_string());
                }
            }
        }

        gen_req
    }

    fn resolve_audio_format(&self, req: &tts::TtsRequest) -> Result<AudioFormat, LLMError> {
        let format = req
            .format
            .as_deref()
            .unwrap_or_else(|| self.cfg.default_audio_format());
        parse_audio_format(format)
    }
}

#[async_trait]
impl ChatProvider for IzwiProvider {
    async fn chat_with_tools(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        Err(LLMError::NotImplemented(AUDIO_ONLY_ERR.into()))
    }
}

#[async_trait]
impl CompletionProvider for IzwiProvider {
    async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(AUDIO_ONLY_ERR.into()))
    }
}

#[async_trait]
impl EmbeddingProvider for IzwiProvider {
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(AUDIO_ONLY_ERR.into()))
    }
}

#[async_trait]
impl LLMProvider for IzwiProvider {
    async fn transcribe(&self, req: &stt::SttRequest) -> Result<stt::SttResponse, LLMError> {
        if req.audio.is_empty() {
            return Err(LLMError::InvalidRequest(
                "STT request audio is empty".into(),
            ));
        }

        let model = self.resolve_stt_model(req);
        let variant = resolve_asr_model_variant(Some(model.as_str()));
        let audio_base64 = base64::engine::general_purpose::STANDARD.encode(&req.audio);
        let language = req.language.clone();
        let runtime = Arc::clone(&self.runtime);
        let auto_download = self.cfg.auto_download;

        self.on_izwi_rt(async move {
            ensure_model_loaded(&runtime, auto_download, variant).await?;
            let transcription = runtime
                .asr_transcribe(&audio_base64, Some(model.as_str()), language.as_deref())
                .await
                .map_err(map_izwi_error)?;
            Ok(stt::SttResponse {
                text: transcription.text,
            })
        })
        .await
    }

    async fn speech(&self, req: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        if req.text.trim().is_empty() {
            return Err(LLMError::InvalidRequest("TTS request text is empty".into()));
        }

        let model = self.resolve_tts_model(req);
        let variant = parse_tts_model_variant(&model).map_err(|err| {
            LLMError::InvalidRequest(format!("Unsupported TTS model '{model}': {err}"))
        })?;

        let audio_format = self.resolve_audio_format(req)?;
        let generation = self.build_generation_request(req);
        let runtime = Arc::clone(&self.runtime);
        let auto_download = self.cfg.auto_download;

        self.on_izwi_rt(async move {
            ensure_model_loaded(&runtime, auto_download, variant).await?;
            let output = runtime.generate(generation).await.map_err(map_izwi_error)?;
            let encoder = runtime.audio_encoder().await;
            let audio = encoder
                .encode(&output.samples, audio_format)
                .map_err(map_izwi_error)?;
            let mime_type = AudioEncoder::content_type(audio_format).to_string();
            Ok(tts::TtsResponse {
                audio,
                mime_type: Some(mime_type),
            })
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Factory-level runtime cache
// ---------------------------------------------------------------------------

/// Cache key for `RuntimeService` — only engine-level params that affect
/// the runtime's behaviour. Per-request settings (model, voice, speed)
/// are not included since they don't require a new runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeCacheKey {
    pub models_dir: String,
    pub max_batch_size: usize,
    pub max_sequence_length: usize,
    pub chunk_size: usize,
    pub kv_cache_dtype: String,
    pub kv_page_size: usize,
    pub use_metal: bool,
    pub num_threads: usize,
}

impl RuntimeCacheKey {
    fn from_config(cfg: &IzwiConfig) -> Self {
        let engine = cfg.engine_config();
        Self {
            models_dir: engine.models_dir.to_string_lossy().into_owned(),
            max_batch_size: engine.max_batch_size,
            max_sequence_length: engine.max_sequence_length,
            chunk_size: engine.chunk_size,
            kv_cache_dtype: engine.kv_cache_dtype.clone(),
            kv_page_size: engine.kv_page_size,
            use_metal: engine.use_metal,
            num_threads: engine.num_threads,
        }
    }
}

/// A cached `RuntimeService` + its dedicated tokio runtime,
/// shared across provider instances.
pub(crate) struct CachedRuntime {
    pub key: RuntimeCacheKey,
    pub runtime: Arc<RuntimeService>,
    pub izwi_rt: Arc<tokio::runtime::Runtime>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a multi-threaded tokio runtime for izwi-core.
fn build_izwi_runtime() -> Result<tokio::runtime::Runtime, LLMError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("izwi-worker")
        .build()
        .map_err(|err| LLMError::ProviderError(format!("failed to build izwi runtime: {err}")))
}

/// Ensure a model variant is loaded, optionally downloading it first.
async fn ensure_model_loaded(
    runtime: &RuntimeService,
    auto_download: bool,
    variant: ModelVariant,
) -> Result<(), LLMError> {
    match runtime.load_model(variant).await {
        Ok(()) => Ok(()),
        Err(IzwiError::ModelNotFound(_)) if auto_download => {
            runtime
                .download_model(variant)
                .await
                .map_err(map_izwi_error)?;
            runtime.load_model(variant).await.map_err(map_izwi_error)
        }
        Err(err) => Err(map_izwi_error(err)),
    }
}

fn parse_audio_format(raw: &str) -> Result<AudioFormat, LLMError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "wav" => Ok(AudioFormat::Wav),
        "pcm" | "s16le" | "raw_i16" => Ok(AudioFormat::RawI16),
        "raw_f32" | "f32" => Ok(AudioFormat::RawF32),
        other => Err(LLMError::InvalidRequest(format!(
            "Unsupported izwi TTS format '{other}'. Supported: wav, pcm, raw_f32"
        ))),
    }
}

fn map_izwi_error(err: IzwiError) -> LLMError {
    match err {
        IzwiError::InvalidInput(message) => LLMError::InvalidRequest(message),
        IzwiError::ModelNotFound(message) => LLMError::InvalidRequest(message),
        other => LLMError::ProviderError(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{IzwiProvider, parse_audio_format};
    use izwi_core::ModelVariant;
    use izwi_core::audio::AudioFormat;

    #[test]
    fn parses_audio_formats() {
        assert!(matches!(
            parse_audio_format("wav").expect("wav should parse"),
            AudioFormat::Wav
        ));
        assert!(matches!(
            parse_audio_format("pcm").expect("pcm should parse"),
            AudioFormat::RawI16
        ));
        assert!(matches!(
            parse_audio_format("raw_f32").expect("raw_f32 should parse"),
            AudioFormat::RawF32
        ));
    }

    #[test]
    fn list_models_returns_audio_families_only() {
        let models = IzwiProvider::list_models();
        assert!(!models.is_empty(), "model list should not be empty");

        // Verify expected families are represented
        assert!(
            models.iter().any(|m| m.contains("TTS")),
            "should include TTS models"
        );
        assert!(
            models.iter().any(|m| m.contains("ASR")),
            "should include ASR models"
        );
        assert!(
            models.iter().any(|m| m.contains("Kokoro")),
            "should include Kokoro TTS"
        );

        // Verify excluded families are absent
        for model in &models {
            let variant: ModelVariant =
                serde_json::from_value(serde_json::Value::String(model.clone()))
                    .unwrap_or_else(|_| panic!("model name '{model}' should deserialize"));
            assert!(
                !variant.is_chat(),
                "chat model '{model}' should not appear in audio-only listing"
            );
            assert!(
                !variant.is_diarization(),
                "diarization model '{model}' should not appear"
            );
            assert!(
                !variant.is_forced_aligner(),
                "forced-aligner model '{model}' should not appear"
            );
        }
    }

    #[test]
    fn list_models_excludes_disabled_variants() {
        let models = IzwiProvider::list_models();
        for model in &models {
            let variant: ModelVariant =
                serde_json::from_value(serde_json::Value::String(model.clone()))
                    .unwrap_or_else(|_| panic!("model name '{model}' should deserialize"));
            assert!(
                variant.is_enabled(),
                "disabled variant '{model}' should not appear"
            );
        }
    }
}
