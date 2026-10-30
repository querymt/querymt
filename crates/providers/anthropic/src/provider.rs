//! `querymt` HTTP provider trait implementations.

use crate::config::Anthropic;
use crate::error::classify_anthropic_http_error;
use crate::stream::AnthropicStreamParser;
use crate::wire::response::AnthropicCompleteResponse;
use http::{Request, Response};
use querymt::HTTPLLMProvider;
use querymt::auth::ApiKeyResolver;
use querymt::chat::http::{ChatStreamParser, HTTPChatProvider, PreparedChatRequest};
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, GenerationOptionSupport, Tool};
use querymt::completion::http::HTTPCompletionProvider;
use querymt::completion::{CompletionRequest, CompletionResponse};
use querymt::embedding::http::HTTPEmbeddingProvider;
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

impl HTTPChatProvider for Anthropic {
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE
            .with_streaming(true)
            .with_options(GenerationOptionSupport::ALL)
    }

    fn prepare_chat_request(
        &self,
        call: &ChatCall<'_>,
        stream: bool,
    ) -> Result<PreparedChatRequest, LLMError> {
        let mut cfg = if call.options.is_empty() {
            self.clone()
        } else {
            self.with_generation_options(&call.options)
        };
        if stream {
            cfg.stream = Some(true);
        }
        let names = cfg.declared_tool_names(call.tools)?;
        let request = cfg.build_chat_request(call.messages, call.tools)?;
        Ok(PreparedChatRequest {
            request,
            context: serde_json::to_value(names)?,
        })
    }

    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_anthropic_http_error(response)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>, context: Value) -> Result<ChatOutput, LLMError> {
        let mut json_resp: AnthropicCompleteResponse = serde_json::from_slice(resp.body())
            .map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse Anthropic response JSON: {e}"),
                raw_response: String::from_utf8_lossy(resp.body()).into_owned(),
            })?;

        // Strip tool prefix from tool names in response (for OAuth)
        if self.is_oauth() {
            let declared = self.tool_names_from_context(context)?;
            for content in &mut json_resp.content {
                if let Some(ref mut name) = content.name {
                    *name = Self::strip_tool_prefix(name, Some(&declared));
                }
            }
        }

        Ok(json_resp.into())
    }

    fn chat_stream_parser(&self, context: Value) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(AnthropicStreamParser {
            oauth: self.is_oauth(),
            tool_names: self.tool_names_from_context(context)?,
            blocks: BTreeMap::new(),
            usage: None,
            final_usage: false,
            metadata_sent: false,
            terminated: false,
        }))
    }
}

impl HTTPCompletionProvider for Anthropic {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        unimplemented!()
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        unimplemented!()
    }
}

impl HTTPEmbeddingProvider for Anthropic {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported".to_string(),
        ))
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported".to_string(),
        ))
    }
}

impl HTTPLLMProvider for Anthropic {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }
}
