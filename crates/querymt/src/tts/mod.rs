use serde::{Deserialize, Serialize};

/// How the TTS engine should determine the output voice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceConfig {
    /// Use a named preset speaker (e.g. "alloy" (OpenAI), "Vivian" (Qwen3-TTS), "af_heart" (Kokoro), etc.).
    Preset { name: String },

    /// Clone a voice from a reference audio clip and its transcript.
    Clone {
        /// Raw reference audio bytes (5-30 s, single speaker).
        reference_audio: Vec<u8>,
        /// Transcript of the reference audio.
        reference_text: String,
    },

    /// Generate a voice from a natural-language description.
    Design {
        /// e.g. "A warm female voice, mid-30s, slight British accent".
        description: String,
    },
}

impl VoiceConfig {
    /// Convenience constructor for a named preset speaker.
    pub fn preset(name: impl Into<String>) -> Self {
        Self::Preset { name: name.into() }
    }

    /// Convenience constructor for voice cloning.
    pub fn clone_voice(reference_audio: Vec<u8>, reference_text: impl Into<String>) -> Self {
        Self::Clone {
            reference_audio,
            reference_text: reference_text.into(),
        }
    }

    /// Convenience constructor for voice design from a description.
    pub fn design(description: impl Into<String>) -> Self {
        Self::Design {
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TtsRequest {
    pub text: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Structured voice configuration (preset, clone, or design).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_config: Option<VoiceConfig>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl TtsRequest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn voice_config(mut self, voice_config: VoiceConfig) -> Self {
        self.voice_config = Some(voice_config);
        self
    }

    pub fn format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }

    pub fn speed(mut self, speed: f32) -> Self {
        self.speed = Some(speed);
        self
    }

    pub fn language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TtsResponse {
    pub audio: Vec<u8>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}
