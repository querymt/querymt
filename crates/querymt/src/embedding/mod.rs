use async_trait::async_trait;

use crate::error::LLMError;

pub mod http;

#[async_trait]
pub trait EmbeddingProvider {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError>;
}
