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
use std::any::Any;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Mutex;

const AUDIO_ONLY_ERR: &str = "qmt-izwi is an audio provider and supports only STT/TTS";

pub(crate) struct IzwiProvider {
    cfg: IzwiConfig,
    runtime: Arc<RuntimeService>,
    async_rt: Arc<tokio::runtime::Runtime>,
    tts_lock: Mutex<()>,
}

impl IzwiProvider {
    pub(crate) fn new(cfg: IzwiConfig) -> Result<Self, LLMError> {
        let runtime = RuntimeService::new(cfg.engine_config()).map_err(map_izwi_error)?;
        let async_rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                LLMError::ProviderError(format!("failed to initialize izwi runtime: {err}"))
            })?;
        Ok(Self {
            cfg,
            runtime: Arc::new(runtime),
            async_rt: Arc::new(async_rt),
            tts_lock: Mutex::new(()),
        })
    }

    pub(crate) fn list_models() -> Vec<String> {
        ModelVariant::all()
            .iter()
            .copied()
            .filter(|variant| {
                variant.is_enabled()
                    && (variant.is_tts()
                        || variant.is_asr()
                        || variant.is_lfm2()
                        || variant.is_voxtral())
            })
            .map(|variant| variant.dir_name().to_string())
            .collect()
    }

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

    fn run_on_izwi_runtime<T, F>(&self, fut: F) -> Result<T, LLMError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, LLMError>> + Send + 'static,
    {
        let rt = Arc::clone(&self.async_rt);
        std::thread::spawn(move || rt.block_on(fut))
            .join()
            .map_err(join_panic_to_error)?
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

        if let Some(voice) = req.voice.as_deref().or(self.cfg.default_voice()) {
            gen_cfg.options.speaker = Some(voice.to_string());
            gen_cfg.options.voice = Some(voice.to_string());
        }

        GenerationRequest::new(req.text.clone()).with_config(gen_cfg)
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
        let language = req.language.clone();
        let variant = resolve_asr_model_variant(Some(model.as_str()));
        let audio_base64 = base64::engine::general_purpose::STANDARD.encode(&req.audio);
        let runtime = Arc::clone(&self.runtime);
        let auto_download = self.cfg.auto_download;

        let transcription = self.run_on_izwi_runtime(async move {
            Self::ensure_model_loaded(&runtime, auto_download, variant).await?;
            runtime
                .asr_transcribe(&audio_base64, Some(model.as_str()), language.as_deref())
                .await
                .map_err(map_izwi_error)
        })?;

        Ok(stt::SttResponse {
            text: transcription.text,
        })
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

        let _lock = self.tts_lock.lock().await;
        let (audio, mime_type) = self.run_on_izwi_runtime(async move {
            Self::ensure_model_loaded(&runtime, auto_download, variant).await?;
            let output = runtime.generate(generation).await.map_err(map_izwi_error)?;
            let encoder = runtime.audio_encoder().await;
            let audio = encoder
                .encode(&output.samples, audio_format)
                .map_err(map_izwi_error)?;
            Ok((audio, AudioEncoder::content_type(audio_format).to_string()))
        })?;

        Ok(tts::TtsResponse {
            audio,
            mime_type: Some(mime_type),
        })
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

fn join_panic_to_error(payload: Box<dyn Any + Send + 'static>) -> LLMError {
    let panic_message = if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else {
        "unknown panic payload".to_string()
    };

    LLMError::ProviderError(format!("izwi runtime thread panicked: {panic_message}"))
}

#[cfg(test)]
mod tests {
    use super::parse_audio_format;
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
}
