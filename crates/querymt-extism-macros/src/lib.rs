use extism_pdk::*;
use serde::{Deserialize, Serialize};

pub fn decode_base64_standard(s: &str) -> Result<Vec<u8>, Error> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    BASE64.decode(s).map_err(|e| Error::msg(e.to_string()))
}

// Import base serializable types from querymt
use querymt::plugin::extism_impl::{
    SerializableHttpRequest as BaseHttpRequest, SerializableHttpResponse as BaseHttpResponse,
};

// Create plugin-side wrappers that implement ToBytes/FromBytes
#[derive(Serialize, Deserialize, ToBytes, FromBytes, Clone)]
#[encoding(Json)]
pub struct HttpRequest(pub BaseHttpRequest);

#[derive(Serialize, Deserialize, ToBytes, FromBytes, Clone)]
#[encoding(Json)]
pub struct HttpResponse(pub BaseHttpResponse);

// Declare the custom host function
#[host_fn("extism:host/user")]
extern "ExtismHost" {
    fn qmt_http_request(req: HttpRequest) -> HttpResponse;
    fn qmt_http_stream_open(req: HttpRequest) -> Json<i64>;
    fn qmt_http_stream_next(stream_id: Json<i64>) -> Vec<u8>;
    fn qmt_http_stream_close(stream_id: Json<i64>);
    fn qmt_yield_chunk(chunk: Vec<u8>);
}

/// Call custom qmt_http_request host function using http-serde-ext
pub fn qmt_http_request_wrapper(
    req: &::http::Request<Vec<u8>>,
) -> Result<::http::Response<Vec<u8>>, Error> {
    // Wrap request in serializable types
    let ser_req = BaseHttpRequest { req: req.clone() };
    let wrapped_req = HttpRequest(ser_req);

    // Call host function
    let wrapped_resp = unsafe { qmt_http_request(wrapped_req)? };

    // Extract the response
    Ok(wrapped_resp.0.resp)
}

/// Open an HTTP stream using the host function
pub fn qmt_http_stream_open_wrapper(req: &::http::Request<Vec<u8>>) -> Result<Json<i64>, Error> {
    let ser_req = BaseHttpRequest { req: req.clone() };
    let wrapped_req = HttpRequest(ser_req);
    unsafe { qmt_http_stream_open(wrapped_req) }
    //    Ok(x?.0)
}

/// Get the next chunk from an HTTP stream
pub fn qmt_http_stream_next_wrapper(stream_id: i64) -> Result<Option<Vec<u8>>, Error> {
    let res = unsafe { qmt_http_stream_next(Json(stream_id))? };
    if res.is_empty() {
        Ok(None)
    } else {
        Ok(Some(res))
    }
}

/// Close an HTTP stream
pub fn qmt_http_stream_close_wrapper(stream_id: i64) -> Result<(), Error> {
    unsafe { qmt_http_stream_close(Json(stream_id)) }
}

/// Yield a chat chunk back to the host
pub fn qmt_yield_chunk_wrapper(
    chunk: &querymt::plugin::extism_impl::ExtismChatChunk,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(chunk).map_err(|e| Error::msg(e.to_string()))?;
    unsafe { qmt_yield_chunk(bytes) }
}

/// Macro to generate all the Extism exports for an HTTP‐based LLM plugin
#[macro_export]
macro_rules! impl_extism_http_plugin {
    (
        config = $Config:ty,
        factory = $Factory:path,
        name = $name:expr,
    ) => {
        use extism_pdk::{Error as PdkError, FnResult, FromBytes, Json, ToBytes, plugin_fn};
        use querymt::{
            HTTPLLMProvider,
            chat::http::HTTPChatProvider,
            completion::{CompletionResponse, http::HTTPCompletionProvider},
            embedding::http::HTTPEmbeddingProvider,
            plugin::{
                HTTPLLMProviderFactory,
                extism_impl::{
                    BinaryCodec, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest,
                    ExtismEmbedRequest, ExtismSttRequest, ExtismSttResponse,
                },
            },
            stt,
        };
        use serde_json::Value;
        use $crate::qmt_http_request_wrapper;

        // Export the factory name
        #[plugin_fn]
        pub fn name() -> FnResult<String> {
            Ok($name.to_string())
        }

        #[plugin_fn]
        pub fn supports_streaming(Json(cfg): Json<$Config>) -> FnResult<Json<bool>> {
            Ok(Json(cfg.supports_streaming()))
        }

        // Export the API key env var name
        #[plugin_fn]
        pub fn api_key_name() -> FnResult<Option<String>> {
            Ok(HTTPLLMProviderFactory::api_key_name(&$Factory))
        }

        // Export the JSON schema for the config type
        #[plugin_fn]
        pub fn config_schema() -> FnResult<String> {
            let schema = schemars::schema_for!($Config).schema;
            let s = serde_json::to_string(&schema).map_err(PdkError::new)?;
            Ok(s)
        }

        // Validate the config inside WASM
        #[plugin_fn]
        pub fn from_config(cfg: Json<$Config>) -> FnResult<Json<$Config>> {
            // Try to deserialize into the config type
            let native_cfg: $Config = cfg.0;

            Ok(Json(native_cfg))
        }

        // list models
        #[plugin_fn]
        pub fn list_models(cfg: Json<Value>) -> FnResult<Json<Vec<String>>> {
            let req = HTTPLLMProviderFactory::list_models_request(&$Factory, &cfg.0)
                .map_err(PdkError::new)?;
            let native_resp = qmt_http_request_wrapper(&req)?;

            let models = HTTPLLMProviderFactory::parse_list_models(&$Factory, native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            Ok(Json(models))
        }

        // chat_request wrapper
        #[plugin_fn]
        pub fn chat(
            Json(input): Json<ExtismChatRequest<$Config>>,
        ) -> FnResult<Json<ExtismChatResponse>> {
            let req = input
                .cfg
                .chat_request(&input.messages, input.tools.as_deref())
                .map_err(PdkError::new)?;
            let native_resp = qmt_http_request_wrapper(&req)?;

            let chat_response = input
                .cfg
                .parse_chat(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            let dto: ExtismChatResponse = chat_response.into();
            Ok(Json(dto))
        }

        // chat_stream wrapper (wireframe implementation)
        #[plugin_fn]
        pub fn chat_stream(Json(input): Json<ExtismChatRequest<$Config>>) -> FnResult<()> {
            use querymt::chat::StreamChunk;
            use querymt::plugin::extism_impl::ExtismChatChunk;
            use $crate::{
                qmt_http_stream_close_wrapper, qmt_http_stream_next_wrapper,
                qmt_http_stream_open_wrapper, qmt_yield_chunk_wrapper,
            };

            let req = input
                .cfg
                .chat_request(&input.messages, input.tools.as_deref())
                .map_err(PdkError::new)?;

            let stream_id = qmt_http_stream_open_wrapper(&req)?.0;

            let mut buffer = Vec::new();
            let mut done_received = false;

            while let Some(raw_chunk) = qmt_http_stream_next_wrapper(stream_id)? {
                buffer.extend_from_slice(&raw_chunk);

                // Process complete lines from the buffer
                if let Some(last_newline_pos) = buffer.iter().rposition(|&b| b == b'\n') {
                    let to_process = &buffer[..=last_newline_pos];
                    let chunks = input.cfg.parse_chat_stream_chunk(to_process).map_err(|e| {
                        PdkError::msg(format!("parse_chat_stream_chunk failed: {}", e))
                    })?;

                    for chunk in chunks.iter() {
                        // Extract usage if this is a Usage chunk
                        let usage_to_send = match &chunk {
                            StreamChunk::Usage(usage) => Some(usage.clone()),
                            _ => None,
                        };

                        qmt_yield_chunk_wrapper(&ExtismChatChunk {
                            chunk: chunk.clone(),
                            usage: usage_to_send,
                        })?;

                        // Check for Done AFTER yielding it
                        if matches!(chunk, StreamChunk::Done { .. }) {
                            done_received = true;
                            break; // Stop yielding more chunks after Done
                        }
                    }
                    buffer.drain(..=last_newline_pos);
                }

                if done_received {
                    break;
                }
            }

            // Process any remaining data in the buffer after the stream ends
            if !buffer.is_empty() && !done_received {
                let chunks = input.cfg.parse_chat_stream_chunk(&buffer).map_err(|e| {
                    PdkError::msg(format!(
                        "parse_chat_stream_chunk failed on remaining buffer: {}",
                        e
                    ))
                })?;

                for chunk in chunks {
                    // Extract usage if this is a Usage chunk
                    let usage_to_send = match &chunk {
                        StreamChunk::Usage(usage) => Some(usage.clone()),
                        _ => None,
                    };
                    qmt_yield_chunk_wrapper(&ExtismChatChunk {
                        chunk,
                        usage: usage_to_send,
                    })?;
                }
            }

            qmt_http_stream_close_wrapper(stream_id)?;
            Ok(())
        }

        // embed wrapper
        #[plugin_fn]
        pub fn embed(
            Json(input): Json<ExtismEmbedRequest<$Config>>,
        ) -> FnResult<Json<Vec<Vec<f32>>>> {
            let req = input
                .cfg
                .embed_request(&input.inputs)
                .map_err(PdkError::new)?;
            let native_resp = qmt_http_request_wrapper(&req)?;

            let embed_response = input
                .cfg
                .parse_embed(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            Ok(Json(embed_response))
        }

        #[plugin_fn]
        pub fn complete(
            Json(input): Json<ExtismCompleteRequest<$Config>>,
        ) -> FnResult<Json<CompletionResponse>> {
            let req = input
                .cfg
                .complete_request(&input.req)
                .map_err(PdkError::new)?;
            let native_resp = qmt_http_request_wrapper(&req)?;

            let complete_response = input
                .cfg
                .parse_complete(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            Ok(Json(complete_response))
        }

        #[plugin_fn]
        pub fn transcribe(
            Json(input): Json<ExtismSttRequest<$Config>>,
        ) -> FnResult<Json<ExtismSttResponse>> {
            let audio = $crate::decode_base64_standard(&input.audio_base64)?;
            let stt_req = stt::SttRequest {
                audio,
                filename: input.filename,
                mime_type: input.mime_type,
                model: input.model,
                language: input.language,
            };

            let req = input.cfg.stt_request(&stt_req).map_err(PdkError::new)?;
            let native_resp = qmt_http_request_wrapper(&req)?;
            let resp = input
                .cfg
                .parse_stt(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;

            Ok(Json(ExtismSttResponse { text: resp.text }))
        }

        #[plugin_fn]
        pub fn base_url() -> FnResult<String> {
            Ok(<$Config>::default_base_url().as_str().to_string())
        }
    };
}
