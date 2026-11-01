use super::utils::ext_json_response;
use super::*;
use crate::error::AgentError;
use crate::speech::{SpeechError, TranscribeAudio};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListProfilesRequest {
    #[serde(default)]
    operation: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TranscribeRequest {
    #[serde(default)]
    profile: Option<String>,
    audio: TranscribeAudio,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SynthesizeRequest {
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
    fn speech_service(&self) -> Result<std::sync::Arc<crate::speech::SpeechService>, Error> {
        self.config
            .speech_service()
            .ok_or_else(Error::method_not_found)
    }

    pub(super) async fn handle_ext_speech_list_profiles(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.speech_service()?;
        let parsed: ListProfilesRequest =
            serde_json::from_str(req.params.get()).unwrap_or_default();
        ext_json_response(&service.list_profiles(parsed.operation.as_deref()))
    }

    pub(super) async fn handle_ext_speech_transcribe(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let service = self.speech_service()?;
        let parsed: TranscribeRequest =
            serde_json::from_str(req.params.get()).map_err(|error| {
                Error::invalid_params().data(format!("invalid speech transcribe params: {error}"))
            })?;
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
        let service = self.speech_service()?;
        let parsed: SynthesizeRequest =
            serde_json::from_str(req.params.get()).map_err(|error| {
                Error::invalid_params().data(format!("invalid speech synthesize params: {error}"))
            })?;
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
