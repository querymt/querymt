use crate::{error::LLMError, tts};
use http::{Request, Response};

pub trait HTTPTtsProvider: Send + Sync {
    fn tts_request(&self, req: &tts::TtsRequest) -> Result<Request<Vec<u8>>, LLMError>;
    fn parse_tts(&self, resp: Response<Vec<u8>>) -> Result<tts::TtsResponse, LLMError>;
}
