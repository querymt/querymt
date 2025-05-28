use crate::error::LLMError;
use async_trait::async_trait;

/// Trait implemented by all text to speech backends
///
/// This trait defines the interface for text-to-speech conversion services.
/// Implementors must provide functionality to convert text into audio data.
#[async_trait]
pub trait TextToSpeechProvider: Send + Sync {
    /// Convert the given text into speech audio
    ///
    /// # Arguments
    ///
    /// * `text` - A string containing the text to convert to speech
    ///
    /// # Returns
    ///
    /// * `Result<Vec<u8>, LLMError>` - On success, returns the audio data as a vector of bytes.
    ///   On failure, returns an LLMError describing what went wrong.
    #[allow(unused)]
    async fn speech(&self, text: &str) -> Result<Vec<u8>, LLMError> {
        Err(LLMError::ProviderError(
            "Phind does not implement text to speech endpoint yet.".into(),
        ))
    }
}
