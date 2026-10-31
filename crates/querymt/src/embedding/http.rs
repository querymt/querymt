use http::{Request, Response};

use crate::error::{LLMError, classify_http_status};

pub trait HTTPEmbeddingProvider: Send + Sync {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError>;

    fn classify_embed_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_http_status(
            response.status().as_u16(),
            response.headers(),
            response.body(),
        )
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError>;
}
