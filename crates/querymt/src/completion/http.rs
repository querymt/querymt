use crate::{
    completion::{CompletionRequest, CompletionResponse},
    error::LLMError,
};
use http::{Request, Response};

pub trait HTTPCompletionProvider: Send + Sync {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError>;
    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError>;
}
