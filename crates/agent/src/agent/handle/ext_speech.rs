use super::utils::{ext_json_response, format_prefixed_error_chain};
use super::*;
use crate::error::AgentError;
use crate::speech::{SpeechError, TranscribeAudio};
use serde::Deserialize;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListProfilesRequest {
    session_id: Option<String>,
    profile_id: Option<String>,
    #[serde(default)]
    operation: Option<String>,
}

#[typeshare]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscribeRequest {
    session_id: Option<String>,
    profile_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    audio: TranscribeAudio,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

#[typeshare]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SynthesizeRequest {
    session_id: Option<String>,
    profile_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    #[serde(default)]
    language: Option<String>,
}

impl LocalAgentHandle {
    async fn scoped_speech_service(
        &self,
        session_id: Option<&str>,
        profile_id: Option<&str>,
    ) -> Result<Option<Arc<crate::speech::SpeechService>>, Error> {
        match (session_id, profile_id) {
            (Some(_), Some(_)) => {
                Err(Error::invalid_params().data("provide sessionId or profileId, not both"))
            }
            // Profile construction futures are large; keep them off the ACP request stack.
            (Some(session_id), None) => {
                match Box::pin(self.bound_session_handle(session_id)).await? {
                    Some((handle, _)) => Ok(handle
                        .as_any()
                        .downcast_ref::<LocalAgentHandle>()
                        .and_then(|host| host.config.speech_service())),
                    None => Ok(self.config.speech_service()),
                }
            }
            (None, Some(profile_id)) => {
                let profiles = self
                    .profiles()
                    .ok_or_else(|| Error::invalid_params().data("profiles are not configured"))?;
                let requested_profile = profile_id.to_owned();
                // Boxing the future does not isolate its poll stack from ACP dispatch.
                // A separate task also keeps cold startup off that deep call chain.
                let load = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                    profiles.runtime_for_profile(&requested_profile).await
                }));
                let runtime = load
                    .await
                    .map_err(|error| Error::internal_error().data(format!("Speech profile task failed: {error}")))?
                    .map_err(|error| {
                        Error::invalid_params().data(serde_json::json!({
                            "message": format_prefixed_error_chain("Failed to load profile", &error),
                            "profileId": profile_id,
                        }))
                    })?;
                Ok(runtime.agent().handle().config.speech_service())
            }
            (None, None) => Ok(self.config.speech_service()),
        }
    }

    pub(super) async fn handle_ext_speech_list_profiles(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: ListProfilesRequest =
            serde_json::from_str(req.params.get()).map_err(|error| {
                Error::invalid_params().data(format!("invalid speech listProfiles params: {error}"))
            })?;
        let service = self
            .scoped_speech_service(parsed.session_id.as_deref(), parsed.profile_id.as_deref())
            .await?;
        let response = service
            .map(|service| service.list_profiles(parsed.operation.as_deref()))
            .unwrap_or(crate::speech::ListProfilesResponse {
                profiles: Vec::new(),
                defaults: crate::speech::SpeechDefaults {
                    transcription: None,
                    synthesis: None,
                },
            });
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_speech_transcribe(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: TranscribeRequest =
            serde_json::from_str(req.params.get()).map_err(|error| {
                Error::invalid_params().data(format!("invalid speech transcribe params: {error}"))
            })?;
        let service = self
            .scoped_speech_service(parsed.session_id.as_deref(), parsed.profile_id.as_deref())
            .await?
            .ok_or_else(Error::method_not_found)?;
        let response = service
            .transcribe(
                parsed.profile.as_deref(),
                &parsed.audio,
                parsed.language.as_deref(),
                parsed.filename.as_deref(),
            )
            .await
            .map_err(speech_error)?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_speech_synthesize(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: SynthesizeRequest =
            serde_json::from_str(req.params.get()).map_err(|error| {
                Error::invalid_params().data(format!("invalid speech synthesize params: {error}"))
            })?;
        let service = self
            .scoped_speech_service(parsed.session_id.as_deref(), parsed.profile_id.as_deref())
            .await?
            .ok_or_else(Error::method_not_found)?;
        let response = service
            .synthesize(
                parsed.profile.as_deref(),
                &parsed.text,
                parsed.voice.as_deref(),
                parsed.format.as_deref(),
                parsed.speed,
                parsed.language.as_deref(),
            )
            .await
            .map_err(speech_error)?;
        ext_json_response(&response)
    }
}

fn speech_error(error: SpeechError) -> Error {
    match error {
        SpeechError::InvalidRequest(message) => Error::invalid_params().data(message),
        SpeechError::Provider {
            message,
            provider,
            model,
            retryable,
            error,
        } => AgentError::ProviderFailure {
            message,
            provider: Some(provider),
            model: Some(model),
            retryable,
            error,
        }
        .into(),
    }
}
