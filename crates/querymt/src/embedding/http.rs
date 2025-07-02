use http::{Request, Response};

use crate::error::LLMError;

pub trait HTTPEmbeddingProvider: Send + Sync {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError>;
    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError>;
}
