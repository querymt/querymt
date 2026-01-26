use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TtsRequest {
    pub text: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
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

    pub fn voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TtsResponse {
    pub audio: Vec<u8>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}
