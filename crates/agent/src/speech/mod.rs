//! Session-independent speech service.
//!
//! Built from `[speech]` config. Clients select a named profile; this service
//! builds and caches the provider, then calls [`LLMProvider::transcribe`] or
//! [`LLMProvider::speech`]. Requests are limited to four in flight and five minutes;
//! synthesis accepts at most 16 KiB of text and returns at most `MAX_ATTACHMENT_BYTES`.

use crate::config::{SpeechConfig, SpeechOperation, SpeechProfile};
use crate::model::MAX_ATTACHMENT_BYTES;
use crate::session::provider_config::{ProviderConfigMode, resolve_provider_config};
use base64::Engine as _;
use querymt::LLMProvider;
use querymt::error::LLMError;
use querymt::plugin::host::{PluginRegistry, ProviderResolver};
use querymt::stt::SttRequest;
use querymt::tts::{TtsRequest, VoiceConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OnceCell, Semaphore};
use typeshare::typeshare;

const MAX_CONCURRENT_REQUESTS: usize = 4;
const MAX_SYNTHESIS_TEXT_BYTES: usize = 16 * 1024;
const SPEECH_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
type ProviderCell = Arc<OnceCell<Arc<dyn LLMProvider>>>;

/// Cached speech providers keyed by speech profile id.
pub struct SpeechService {
    config: SpeechConfig,
    registry: Arc<PluginRegistry>,
    providers: Mutex<HashMap<String, ProviderCell>>,
    requests: Semaphore,
}

#[typeshare]
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

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct SpeechDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcription: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct ListProfilesResponse {
    pub profiles: Vec<SpeechProfileInfo>,
    pub defaults: SpeechDefaults,
}

#[typeshare]
#[derive(Debug, Clone, Serialize)]
pub struct TranscribeResponse {
    pub text: String,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeAudio {
    pub data: String,
    pub mime_type: String,
}

#[typeshare]
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
            requests: Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }))
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
        let _permit = self.admit(profile)?;
        let bytes = decode_audio(audio)?;
        self.run(profile, async {
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
        })
        .await
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
        if text.len() > MAX_SYNTHESIS_TEXT_BYTES {
            return Err(SpeechError::InvalidRequest(format!(
                "synthesis text exceeds {MAX_SYNTHESIS_TEXT_BYTES} bytes"
            )));
        }
        if speed.is_some_and(|speed| !speed.is_finite() || speed <= 0.0) {
            return Err(SpeechError::InvalidRequest(
                "speed must be finite and positive".into(),
            ));
        }
        let profile = self.resolve_profile(profile_id, SpeechOperation::Synthesis)?;
        let _permit = self.admit(profile)?;
        self.run(profile, async {
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

            if response.audio.len() > MAX_ATTACHMENT_BYTES {
                return Err(provider_error(
                    profile,
                    LLMError::ProviderError(format!(
                        "synthesized audio exceeds {MAX_ATTACHMENT_BYTES} bytes"
                    )),
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
        })
        .await
    }

    fn admit(
        &self,
        profile: &SpeechProfile,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, SpeechError> {
        self.requests.try_acquire().map_err(|_| {
            provider_error(
                profile,
                LLMError::RateLimited {
                    message: "speech service is busy".into(),
                    retry_after_secs: None,
                },
            )
        })
    }

    async fn run<T>(
        &self,
        profile: &SpeechProfile,
        request: impl std::future::Future<Output = Result<T, SpeechError>>,
    ) -> Result<T, SpeechError> {
        tokio::time::timeout(SPEECH_REQUEST_TIMEOUT, request)
            .await
            .map_err(|_| {
                provider_error(
                    profile,
                    LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::Timeout,
                        message: "speech request timed out".into(),
                    },
                )
            })?
    }

    pub(crate) fn clear_provider_cache(&self) {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
        let cell = self
            .providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(profile.id.clone())
            .or_default()
            .clone();
        cell.get_or_try_init(|| self.build_provider(profile))
            .await
            .cloned()
    }

    async fn build_provider(
        &self,
        profile: &SpeechProfile,
    ) -> Result<Arc<dyn LLMProvider>, SpeechError> {
        let binding = self
            .registry
            .resolve(&profile.provider)
            .await
            .map_err(|error| provider_error(profile, error))?;
        let params = profile
            .base_url
            .as_ref()
            .map(|base_url| serde_json::json!({"base_url": base_url}));
        let resolved = resolve_provider_config(
            &binding,
            &querymt::LLMParams::default(),
            ProviderConfigMode::Speech {
                model: &profile.model,
                params: params.as_ref(),
                api_key_override: profile.api_key.as_deref(),
            },
        )
        .await
        .map_err(|error| provider_error(profile, error.into()))?;
        resolved
            .build_provider(&binding)
            .map_err(|error| provider_error(profile, error))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{MockLlmProvider, SharedLlmProvider, TestProviderFactory};
    use querymt::plugin::LLMProviderFactory;
    use querymt::plugin::host::{PluginLoader, PluginType, ProviderConfig, ProviderPlugin};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingLoader {
        calls: Arc<AtomicUsize>,
        factory: Arc<TestProviderFactory>,
    }

    #[async_trait::async_trait]
    impl PluginLoader for CountingLoader {
        fn supported_type(&self) -> PluginType {
            PluginType::Wasm
        }
        async fn load_plugin(
            &self,
            _: ProviderPlugin,
            _: &ProviderConfig,
        ) -> Result<Arc<dyn LLMProviderFactory>, LLMError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            if call == 0 {
                return Err(LLMError::AuthError("first attempt fails".into()));
            }
            Ok(self.factory.clone())
        }
    }

    fn fixture() -> (Arc<SpeechService>, tempfile::TempDir, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("mock.wasm");
        std::fs::write(&plugin, "").unwrap();
        let path = dir.path().join("providers.toml");
        std::fs::write(
            &path,
            format!(
                "[[providers]]\nname = 'mock'\npath = '{}'\n",
                plugin.display()
            ),
        )
        .unwrap();
        let mut registry = PluginRegistry::from_path(&path).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        registry.register_loader(Box::new(CountingLoader {
            calls: calls.clone(),
            factory: Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
                MockLlmProvider::new(),
                vec![],
            ))),
        }));
        let config: SpeechConfig = toml::from_str(
            "[[profiles]]\nid='tts'\nprovider='mock'\nmodel='test'\noperation='synthesis'\n",
        )
        .unwrap();
        (
            SpeechService::new(config, Arc::new(registry)).unwrap(),
            dir,
            calls,
        )
    }

    #[tokio::test]
    async fn speech_provider_initialization_is_single_flight_and_retries_failure() {
        let (service, _dir, calls) = fixture();
        let profile = &service.config.profiles[0];
        assert!(service.provider_for(profile).await.is_err());
        let (first, second) =
            tokio::join!(service.provider_for(profile), service.provider_for(profile));
        let first = first.unwrap();
        assert!(Arc::ptr_eq(&first, &second.unwrap()));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "one failed attempt, one successful initialization"
        );
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let config = crate::agent::agent_config_builder::AgentConfigBuilder::new(
            service.registry.clone(),
            storage,
            querymt::LLMParams::default(),
        )
        .with_speech_service(service.clone())
        .build();
        config.invalidate_provider_cache().await;
        assert!(!Arc::ptr_eq(
            &first,
            &service.provider_for(profile).await.unwrap()
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn speech_deadline_drops_work_and_admission_is_bounded() {
        let (service, _dir, _) = fixture();
        let profile = &service.config.profiles[0];
        let permits: Vec<_> = (0..service.requests.available_permits())
            .map(|_| service.admit(profile).unwrap())
            .collect();
        let error = service
            .synthesize(Some("tts"), "hello", None, None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SpeechError::Provider {
                retryable: true,
                ..
            }
        ));
        drop(permits);
        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel::<()>();
        let error = service
            .run::<()>(profile, async {
                let _drop_tx = drop_tx;
                let _permit = service.admit(profile).unwrap();
                std::future::pending().await
            })
            .await
            .unwrap_err();
        assert!(
            drop_rx.await.is_err(),
            "deadline must drop the request future"
        );
        assert!(matches!(
            error,
            SpeechError::Provider {
                retryable: true,
                ..
            }
        ));
        assert!(service.admit(profile).is_ok());
    }

    #[cfg(feature = "api")]
    #[tokio::test]
    async fn speech_disconnect_cancels_in_flight_requests() {
        use std::future::IntoFuture;
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        let router = axum::Router::new().route(
            "/audio/{operation}",
            axum::routing::post(move || {
                let notify = notify.clone();
                async move {
                    notify.notify_one();
                    std::future::pending::<String>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            axum::serve(listener, router).into_future(),
        ));
        let registry = PluginRegistry::empty();
        registry.register_static(Arc::new(
            querymt::plugin::adapters::HTTPFactoryAdapter::new(qmt_openai::create_http_factory()),
        ));
        let config: SpeechConfig = toml::from_str(&format!(
            "[[profiles]]\nid='tts'\nprovider='openai'\nmodel='tts-1'\noperation='synthesis'\napi_key='test'\nbase_url='{base_url}'\n[[profiles]]\nid='stt'\nprovider='openai'\nmodel='whisper-1'\napi_key='test'\nbase_url='{base_url}'\n"
        )).unwrap();
        let registry = Arc::new(registry);
        let service = SpeechService::new(config, registry.clone()).unwrap();
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let agent = Arc::new(crate::LocalAgentHandle::from_config(Arc::new(
            crate::agent::agent_config_builder::AgentConfigBuilder::new(
                registry,
                storage,
                querymt::LLMParams::default(),
            )
            .with_speech_service(service.clone())
            .build(),
        )));
        for (method, params) in [
            (
                "querymt/speech/synthesize",
                serde_json::json!({"profile":"tts", "text":"hello"}),
            ),
            (
                "_querymt/speech/transcribe",
                serde_json::json!({"profile":"stt", "audio":{"data":"aGVsbG8=", "mimeType":"audio/wav"}}),
            ),
        ] {
            let (mut connection, _messages) =
                crate::acp::websocket::EmbeddedAcpConnection::open(agent.clone(), None).await;
            connection
                .receive_json(
                    &serde_json::json!({"jsonrpc":"2.0", "id":1,"method":method,"params":params})
                        .to_string(),
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .unwrap();
            assert_eq!(
                service.requests.available_permits(),
                MAX_CONCURRENT_REQUESTS - 1
            );
            connection.close().await;
            tokio::time::timeout(Duration::from_secs(2), async {
                while service.requests.available_permits() != MAX_CONCURRENT_REQUESTS {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("disconnect must release speech admission");
        }
        drop(server);
    }
}
