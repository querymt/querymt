//! Handlers for audio STT/TTS requests.
//!
//! Builds a provider from the plugin registry using the caller-supplied
//! `provider` + `model` pair (same pattern as chat model selection) and
//! delegates transcription / speech synthesis to it.

use super::super::ServerState;
use super::super::connection::{send_binary, send_error, send_message};
use super::super::messages::{AudioModelInfo, UiServerMessage};
use querymt::LLMProvider;
use std::sync::Arc;
use tokio::sync::mpsc;

// ── Provider resolution ────────────────────────────────────────────────────────

/// Build a provider from the plugin registry by name and configure it with
/// the requested model.
async fn build_provider(
    state: &ServerState,
    provider_name: &str,
    model: &str,
) -> Result<Arc<dyn LLMProvider>, String> {
    let registry = state.agent.config.provider().plugin_registry();
    let factory = registry
        .get(provider_name)
        .await
        .ok_or_else(|| format!("Unknown audio provider: {provider_name}"))?;

    // Build a config JSON with just the model field.
    // Provider-specific configs (e.g. IzwiConfig) use `#[serde(default)]` so
    // unrecognised fields are fine and missing fields get defaults.
    let config_json = serde_json::json!({}).to_string();

    let provider: Arc<dyn LLMProvider> = Arc::from(
        factory
            .from_config(&config_json)
            .map_err(|e| format!("Failed to build provider '{provider_name}': {e}"))?,
    );

    // Verify the provider name is plausible by checking it loaded at all.
    // The `model` is passed per-request via SttRequest/TtsRequest, not at
    // construction time, so no model validation here.
    let _ = model; // used by callers below
    Ok(provider)
}

/// Classify a model name as STT, TTS, or both using naming conventions.
///
/// Known patterns from izwi-core's `ModelVariant`:
/// - ASR / Parakeet / Whisper → STT
/// - TTS / Kokoro → TTS
/// - Voxtral / Audio → both (multimodal audio-LM)
fn classify_audio_model(model: &str) -> (bool, bool) {
    let m = model.to_ascii_lowercase();
    let is_stt = m.contains("asr") || m.contains("parakeet") || m.contains("whisper");
    let is_tts = m.contains("tts") || m.contains("kokoro");
    let is_multi = m.contains("voxtral") || m.contains("audio");
    (is_stt || is_multi, is_tts || is_multi)
}

/// Check which audio-capable providers are available and report to the UI.
///
/// Scans the plugin registry for audio-capable factories (currently `"izwi"`),
/// lists their models, and categorizes each as STT, TTS, or both.
pub async fn handle_audio_capabilities(state: &ServerState, tx: &mpsc::Sender<String>) {
    let registry = state.agent.config.provider().plugin_registry();

    let mut stt_models = Vec::new();
    let mut tts_models = Vec::new();

    // Probe known audio providers.
    // Future: iterate all factories and probe for audio capability.
    for provider_name in &["izwi"] {
        if let Some(factory) = registry.get(provider_name).await {
            match factory.list_models("{}").await {
                Ok(models) => {
                    for model in models {
                        let (is_stt, is_tts) = classify_audio_model(&model);
                        let info = AudioModelInfo {
                            provider: provider_name.to_string(),
                            model: model.clone(),
                        };
                        if is_stt {
                            stt_models.push(info.clone());
                        }
                        if is_tts {
                            tts_models.push(info);
                        }
                    }
                }
                Err(err) => {
                    log::warn!(
                        "audio: failed to list models for provider '{provider_name}': {err}"
                    );
                }
            }
        }
    }

    let _ = send_message(
        tx,
        UiServerMessage::AudioCapabilities {
            stt_models,
            tts_models,
        },
    )
    .await;
}

// ── STT ────────────────────────────────────────────────────────────────────────

/// Handle `Transcribe` — audio bytes arrive from a binary WebSocket frame.
pub async fn handle_transcribe(
    state: &ServerState,
    provider_name: &str,
    model: &str,
    audio: Vec<u8>,
    mime_type: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    if audio.is_empty() {
        let _ = send_error(tx, "Empty audio data".to_string()).await;
        return;
    }

    let provider = match build_provider(state, provider_name, model).await {
        Ok(p) => p,
        Err(msg) => {
            let _ = send_error(tx, msg).await;
            return;
        }
    };

    let req = querymt::stt::SttRequest {
        audio,
        model: Some(model.to_string()),
        mime_type: mime_type.map(String::from),
        ..Default::default()
    };

    match provider.transcribe(&req).await {
        Ok(resp) => {
            let _ = send_message(tx, UiServerMessage::TranscribeResult { text: resp.text }).await;
        }
        Err(err) => {
            log::error!("STT transcription failed: {err}");
            let _ = send_error(tx, format!("Transcription failed: {err}")).await;
        }
    }
}

// ── TTS ────────────────────────────────────────────────────────────────────────

/// TTS-specific parameters for [`handle_speech`].
pub struct SpeechParams<'a> {
    pub provider_name: &'a str,
    pub model: &'a str,
    pub text: &'a str,
    pub voice: Option<&'a str>,
    pub format: Option<&'a str>,
}

/// Handle `Speech` — synthesize text, return audio via a binary WebSocket frame.
///
/// The response is a binary frame with header
/// `{"type":"speech_result","data":{"mime_type":"audio/wav"}}` followed by
/// raw audio bytes.
pub async fn handle_speech(
    state: &ServerState,
    params: &SpeechParams<'_>,
    tx: &mpsc::Sender<String>,
    bin_tx: &mpsc::Sender<Vec<u8>>,
) {
    let SpeechParams {
        provider_name,
        model,
        text,
        voice,
        format,
    } = params;

    if text.trim().is_empty() {
        let _ = send_error(tx, "Empty text for TTS".to_string()).await;
        return;
    }

    let provider = match build_provider(state, provider_name, model).await {
        Ok(p) => p,
        Err(msg) => {
            let _ = send_error(tx, msg).await;
            return;
        }
    };

    let voice_config = voice.map(querymt::tts::VoiceConfig::preset);

    let req = querymt::tts::TtsRequest {
        text: text.to_string(),
        model: Some(model.to_string()),
        voice_config,
        format: format.map(String::from),
        ..Default::default()
    };

    match provider.speech(&req).await {
        Ok(resp) => {
            let mime_type = resp.mime_type.unwrap_or_else(|| "audio/wav".to_string());
            let header = serde_json::json!({
                "type": "speech_result",
                "data": { "mime_type": mime_type }
            })
            .to_string();

            if let Err(err) = send_binary(bin_tx, &header, &resp.audio).await {
                log::error!("Failed to send TTS audio: {err}");
            }
        }
        Err(err) => {
            log::error!("TTS synthesis failed: {err}");
            let _ = send_error(tx, format!("Speech synthesis failed: {err}")).await;
        }
    }
}
