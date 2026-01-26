use crate::{error::LLMError, stt};
use http::{Request, Response};

pub trait HTTPSttProvider: Send + Sync {
    fn stt_request(&self, req: &stt::SttRequest) -> Result<Request<Vec<u8>>, LLMError>;
    fn parse_stt(&self, resp: Response<Vec<u8>>) -> Result<stt::SttResponse, LLMError>;
}
