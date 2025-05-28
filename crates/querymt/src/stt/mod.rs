use crate::error::LLMError;
use async_trait::async_trait;

/// Trait implemented by all speech to text backends
///
/// This trait defines the interface for speech-to-text conversion services.
/// Implementors must provide functionality to convert audio data into text.
#[async_trait]
pub trait SpeechToTextProvider: Send + Sync {
    /// Transcribe the given audio bytes into text
    ///
    /// # Arguments
    ///
    /// * `audio` - A vector of bytes containing the audio data to transcribe
    ///
    /// # Returns
    ///
    /// * `Result<String, LLMError>` - On success, returns the transcribed text as a String.
    ///   On failure, returns an LLMError describing what went wrong.
    async fn transcribe(&self, audio: Vec<u8>) -> Result<String, LLMError>;

    #[allow(unused)]
    async fn transcribe_file(&self, file_path: &str) -> Result<String, LLMError> {
        Err(LLMError::ProviderError(
            "Phind does not implement speech to text endpoint yet.".into(),
        ))
    }
}
