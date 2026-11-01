//! Session-independent speech service.
//!
//! Built from `[speech]` config. Clients select a named profile; this service
//! builds and caches the provider, then calls [`LLMProvider::transcribe`] or
//! [`LLMProvider::speech`].

use crate::config::{SpeechConfig, SpeechOperation, SpeechProfile};
use crate::model::MAX_ATTACHMENT_BYTES;
use base64::Engine as _;
use querymt::LLMProvider;
use querymt::error::LLMError;
use querymt::plugin::host::PluginRegistry;
use querymt::stt::SttRequest;
use querymt::tts::{TtsRequest, VoiceConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Cached speech providers keyed by speech profile id.
pub struct SpeechService {
    config: SpeechConfig,
    registry: Arc<PluginRegistry>,
    providers: Mutex<HashMap<String, Arc<dyn LLMProvider>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeechProfileInfo {
    pub id: String,
    pub operation: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeechDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcription: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListProfilesResponse {
    pub profiles: Vec<SpeechProfileInfo>,
    pub defaults: SpeechDefaults,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscribeResponse {
    pub text: String,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeAudio {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SynthesizeResponse {
    pub audio: TranscribeAudio,
    pub profile: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SpeechError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("provider request failed: {message}")]
    Provider {
        message: String,
        provider: String,
        model: String,
        retryable: bool,
        error: Box<querymt::error::LLMErrorPayload>,
    },
}

impl SpeechService {
    /// Validate config and construct a service.
    ///
    /// # Errors
    ///
    /// Returns an error when profile ids are invalid or a default is missing.
    pub fn new(config: SpeechConfig, registry: Arc<PluginRegistry>) -> Result<Arc<Self>, String> {
        config.validate()?;
        Ok(Arc::new(Self {
            config,
            registry,
            providers: Mutex::new(HashMap::new()),
        }))
    }

    pub fn supports(&self, operation: SpeechOperation) -> bool {
        self.config
            .profiles
            .iter()
            .any(|profile| profile.operation == operation)
    }

    pub fn list_profiles(&self, operation: Option<&str>) -> ListProfilesResponse {
        let profiles = self
            .config
            .profiles
            .iter()
            .filter(|profile| match operation {
                None => true,
                Some(op) => profile.operation.as_str() == op,
            })
            .map(|profile| SpeechProfileInfo {
                id: profile.id.clone(),
                operation: profile.operation.as_str().to_string(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
                voice: profile.voice.clone(),
                voice_description: profile.voice_description.clone(),
            })
            .collect();
        ListProfilesResponse {
            profiles,
            defaults: SpeechDefaults {
                transcription: self.config.default_transcription.clone(),
                synthesis: self.config.default_synthesis.clone(),
            },
        }
    }

    /// Transcribe `audio` with the named profile, or the configured default.
    ///
    /// # Errors
    ///
    /// Returns [`SpeechError::InvalidRequest`] for unknown profiles or invalid
    /// audio, and [`SpeechError::Provider`] when the STT backend fails.
    pub async fn transcribe(
        &self,
        profile_id: Option<&str>,
        audio: &TranscribeAudio,
        language: Option<&str>,
        filename: Option<&str>,
    ) -> Result<TranscribeResponse, SpeechError> {
        let profile = self.resolve_profile(profile_id, SpeechOperation::Transcription)?;
        let bytes = decode_audio(audio)?;
        let provider = self.provider_for(profile).await?;

        let mut request = SttRequest::new()
            .audio(bytes)
            .mime_type(audio.mime_type.clone())
            .model(profile.model.clone());
        if let Some(language) = language {
            request = request.language(language);
        }
        if let Some(filename) = filename {
            request = request.filename(filename);
        }

        let response = provider
            .transcribe(&request)
            .await
            .map_err(|error| provider_error(profile, error))?;

        Ok(TranscribeResponse {
            text: response.text,
            profile: profile.id.clone(),
            language: response.language,
        })
    }

    /// Synthesize `text` with the named profile, or the configured default.
    ///
    /// # Errors
    ///
    /// Returns [`SpeechError::InvalidRequest`] for unknown profiles or empty
    /// text, and [`SpeechError::Provider`] when the TTS backend fails.
    pub async fn synthesize(
        &self,
        profile_id: Option<&str>,
        text: &str,
        voice: Option<&str>,
        format: Option<&str>,
        speed: Option<f32>,
        language: Option<&str>,
    ) -> Result<SynthesizeResponse, SpeechError> {
        if text.trim().is_empty() {
            return Err(SpeechError::InvalidRequest(
                "synthesis text must be non-empty".into(),
            ));
        }
        let profile = self.resolve_profile(profile_id, SpeechOperation::Synthesis)?;
        let provider = self.provider_for(profile).await?;

        let mut request = TtsRequest::new().text(text).model(profile.model.clone());
        if let Some(voice_config) = resolve_voice_config(profile, voice) {
            request = request.voice_config(voice_config);
        }
        if let Some(format) = nonempty(format) {
            request = request.format(format);
        }
        if let Some(speed) = speed {
            request = request.speed(speed);
        }
        if let Some(language) = nonempty(language) {
            request = request.language(language);
        }

        let response = provider
            .speech(&request)
            .await
            .map_err(|error| provider_error(profile, error))?;
        if response.audio.is_empty() {
            return Err(provider_error(
                profile,
                LLMError::ProviderError("provider returned empty audio".into()),
            ));
        }

        Ok(SynthesizeResponse {
            audio: TranscribeAudio {
                data: base64::engine::general_purpose::STANDARD.encode(&response.audio),
                mime_type: response
                    .mime_type
                    .unwrap_or_else(|| "audio/wav".to_string()),
            },
            profile: profile.id.clone(),
        })
    }

    fn resolve_profile(
        &self,
        profile_id: Option<&str>,
        operation: SpeechOperation,
    ) -> Result<&SpeechProfile, SpeechError> {
        let id = match profile_id {
            Some(id) if !id.is_empty() => id,
            Some(_) => {
                return Err(SpeechError::InvalidRequest(
                    "speech profile id must be non-empty".into(),
                ));
            }
            None => match operation {
                SpeechOperation::Transcription => {
                    self.config.default_transcription.as_deref().ok_or_else(|| {
                        SpeechError::InvalidRequest(
                            "profile is required because no default transcription profile is configured"
                                .into(),
                        )
                    })?
                }
                SpeechOperation::Synthesis => {
                    self.config.default_synthesis.as_deref().ok_or_else(|| {
                        SpeechError::InvalidRequest(
                            "profile is required because no default synthesis profile is configured"
                                .into(),
                        )
                    })?
                }
            },
        };
        let profile = self
            .config
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .ok_or_else(|| SpeechError::InvalidRequest(format!("unknown speech profile '{id}'")))?;
        if profile.operation != operation {
            return Err(SpeechError::InvalidRequest(format!(
                "speech profile '{id}' does not support {}",
                operation.as_str()
            )));
        }
        Ok(profile)
    }

    async fn provider_for(
        &self,
        profile: &SpeechProfile,
    ) -> Result<Arc<dyn LLMProvider>, SpeechError> {
        {
            let cache = self
                .providers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(existing) = cache.get(&profile.id) {
                return Ok(existing.clone());
            }
        }

        let built = self.build_provider(profile).await?;

        let mut cache = self
            .providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(cache.entry(profile.id.clone()).or_insert(built).clone())
    }

    async fn build_provider(
        &self,
        profile: &SpeechProfile,
    ) -> Result<Arc<dyn LLMProvider>, SpeechError> {
        let mut builder = self
            .registry
            .builder(profile.provider.clone())
            .model(profile.model.clone());
        if let Some(api_key) = &profile.api_key {
            builder = builder.api_key(api_key.clone());
        }
        if let Some(base_url) = &profile.base_url {
            builder = builder.base_url(base_url.clone());
        }
        let provider = builder
            .build()
            .await
            .map_err(|error| provider_error(profile, error))?;
        Ok(Arc::from(provider))
    }
}

fn provider_error(profile: &SpeechProfile, error: LLMError) -> SpeechError {
    SpeechError::Provider {
        message: error.to_string(),
        provider: profile.provider.clone(),
        model: profile.model.clone(),
        retryable: error.is_retryable(),
        error: Box::new(error.to_payload()),
    }
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn resolve_voice_config(profile: &SpeechProfile, voice: Option<&str>) -> Option<VoiceConfig> {
    if let Some(voice) = nonempty(voice).or(nonempty(profile.voice.as_deref())) {
        return Some(VoiceConfig::preset(voice));
    }
    nonempty(profile.voice_description.as_deref()).map(VoiceConfig::design)
}

fn decode_audio(audio: &TranscribeAudio) -> Result<Vec<u8>, SpeechError> {
    if !audio.mime_type.starts_with("audio/") {
        return Err(SpeechError::InvalidRequest(format!(
            "unsupported audio MIME type: {}",
            audio.mime_type
        )));
    }
    if audio.data.is_empty() {
        return Err(SpeechError::InvalidRequest("audio data is empty".into()));
    }

    let max_encoded_bytes = MAX_ATTACHMENT_BYTES.div_ceil(3).saturating_mul(4);
    if audio.data.len() > max_encoded_bytes {
        return Err(SpeechError::InvalidRequest(format!(
            "audio exceeds the maximum of {MAX_ATTACHMENT_BYTES} decoded bytes"
        )));
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(audio.data.as_bytes())
        .map_err(|error| {
            SpeechError::InvalidRequest(format!("invalid base64 audio data: {error}"))
        })?;
    if bytes.is_empty() {
        return Err(SpeechError::InvalidRequest("audio data is empty".into()));
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(SpeechError::InvalidRequest(format!(
            "audio is {} bytes; maximum is {MAX_ATTACHMENT_BYTES} bytes",
            bytes.len()
        )));
    }
    Ok(bytes)
}
