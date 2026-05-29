use extism_pdk::*;
use serde::{Deserialize, Serialize};

struct ExtismHostLogger;

impl log::Log for ExtismHostLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let payload = querymt::plugin::extism_impl::ExtismLogRecord {
            level: match record.level() {
                log::Level::Error => 1,
                log::Level::Warn => 2,
                log::Level::Info => 3,
                log::Level::Debug => 4,
                log::Level::Trace => 5,
            },
            target: record.target().to_string(),
            message: format!("{}", record.args()),
        };

        let _ = qmt_log_wrapper(&payload);
    }

    fn flush(&self) {}
}

static EXTISM_HOST_LOGGER: ExtismHostLogger = ExtismHostLogger;

fn level_filter_from_usize(max_level: usize) -> log::LevelFilter {
    match max_level {
        0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    }
}

pub fn init_plugin_logging(max_level: usize) {
    let _ = log::set_logger(&EXTISM_HOST_LOGGER);
    log::set_max_level(level_filter_from_usize(max_level));
}

pub fn decode_base64_standard(s: &str) -> Result<Vec<u8>, Error> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    BASE64.decode(s).map_err(|e| Error::msg(e.to_string()))
}

pub fn encode_base64_standard(bytes: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    BASE64.encode(bytes)
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
    fn qmt_http_stream_open(
        req: HttpRequest,
    ) -> Json<querymt::plugin::extism_impl::StreamOpenResult>;
    fn qmt_http_stream_next(stream_id: Json<i64>) -> Vec<u8>;
    fn qmt_http_stream_close(stream_id: Json<i64>);
    fn qmt_yield_chunk(chunk: Vec<u8>);
    fn qmt_log(record: Json<querymt::plugin::extism_impl::ExtismLogRecord>);
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
pub fn qmt_http_stream_open_wrapper(
    req: &::http::Request<Vec<u8>>,
) -> Result<querymt::plugin::extism_impl::StreamOpenResult, Error> {
    let ser_req = BaseHttpRequest { req: req.clone() };
    let wrapped_req = HttpRequest(ser_req);
    let result = unsafe { qmt_http_stream_open(wrapped_req)? };
    Ok(result.0)
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

pub fn qmt_log_wrapper(
    record: &querymt::plugin::extism_impl::ExtismLogRecord,
) -> Result<(), Error> {
    unsafe { qmt_log(Json(record.clone())) }
}

/// Macro to generate all the Extism exports for an HTTP‐based LLM plugin
#[macro_export]
macro_rules! impl_extism_http_plugin {
    (
        config = $Config:ty,
        factory = $Factory:path,
        name = $name:expr,
    ) => {
        use extism_pdk::{
            Error as PdkError, FnResult, FromBytes, Json, ToBytes, WithReturnCode, plugin_fn,
        };
        use querymt::{
            HTTPLLMProvider,
            chat::{StreamChunk, http::HTTPChatProvider},
            completion::{CompletionResponse, http::HTTPCompletionProvider},
            embedding::http::HTTPEmbeddingProvider,
            plugin::{
                HTTPLLMProviderFactory,
                extism_impl::{
                    BinaryCodec, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest,
                    ExtismEmbedRequest, ExtismSttRequest, ExtismSttResponse, ExtismTtsRequest,
                    ExtismTtsResponse, PluginError,
                },
            },
            stt, tts,
        };
        use serde_json::Value;
        use $crate::{init_plugin_logging, qmt_http_request_wrapper};

        /// Convert an LLMError into a WithReturnCode<PdkError> with the
        /// appropriate error code and JSON-serialized payload.
        fn llm_err_to_pdk(e: querymt::error::LLMError) -> WithReturnCode<PdkError> {
            let (json, code) = PluginError::encode(&e);
            WithReturnCode::new(PdkError::msg(json), code)
        }

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

        #[plugin_fn]
        pub fn init_logging(Json(max_level): Json<usize>) -> FnResult<()> {
            init_plugin_logging(max_level);
            Ok(())
        }

        // Export the JSON schema for the config type
        #[plugin_fn]
        pub fn config_schema() -> FnResult<String> {
            let schema = schemars::schema_for!($Config);
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

        // list models (new request/parse split)
        #[plugin_fn]
        pub fn list_models_static(
            Json(input): Json<querymt::plugin::extism_impl::ExtismListModelsRequest>,
        ) -> FnResult<Json<Option<Vec<String>>>> {
            let cfg_str = serde_json::to_string(&input.cfg).map_err(PdkError::new)?;
            match HTTPLLMProviderFactory::list_models_static(&$Factory, &cfg_str) {
                Some(Ok(models)) => Ok(Json(Some(models))),
                Some(Err(err)) => Err(llm_err_to_pdk(err)),
                None => Ok(Json(None)),
            }
        }

        #[plugin_fn]
        pub fn list_models_request(
            Json(input): Json<querymt::plugin::extism_impl::ExtismListModelsRequest>,
        ) -> FnResult<Json<querymt::plugin::extism_impl::SerializableHttpRequest>> {
            let cfg_str = serde_json::to_string(&input.cfg).map_err(PdkError::new)?;
            let req = HTTPLLMProviderFactory::list_models_request(&$Factory, &cfg_str)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(querymt::plugin::extism_impl::SerializableHttpRequest { req }))
        }

        #[plugin_fn]
        pub fn parse_list_models_response(
            Json(input): Json<querymt::plugin::extism_impl::ExtismListModelsParseRequest>,
        ) -> FnResult<Json<Vec<String>>> {
            let models = HTTPLLMProviderFactory::parse_list_models(&$Factory, input.resp.resp)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(models))
        }

        #[plugin_fn]
        pub fn chat_request(
            Json(input): Json<ExtismChatRequest<$Config>>,
        ) -> FnResult<Json<querymt::plugin::extism_impl::SerializableHttpRequest>> {
            let req = input
                .cfg
                .chat_request(&input.messages, input.tools.as_deref())
                .map_err(llm_err_to_pdk)?;
            Ok(Json(querymt::plugin::extism_impl::SerializableHttpRequest { req }))
        }

        #[plugin_fn]
        pub fn chat_stream_request(
            Json(input): Json<ExtismChatRequest<$Config>>,
        ) -> FnResult<Json<querymt::plugin::extism_impl::SerializableHttpRequest>> {
            let req = input
                .cfg
                .chat_stream_request(&input.messages, input.tools.as_deref())
                .map_err(llm_err_to_pdk)?;
            Ok(Json(querymt::plugin::extism_impl::SerializableHttpRequest { req }))
        }

        #[plugin_fn]
        pub fn parse_chat_response(
            Json(input): Json<querymt::plugin::extism_impl::ExtismChatParseRequest<$Config>>,
        ) -> FnResult<Json<ExtismChatResponse>> {
            let chat_response = input
                .cfg
                .parse_chat(input.resp.resp)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(chat_response.into()))
        }

        thread_local! {
            static STREAM_PARSERS: std::cell::RefCell<std::collections::HashMap<i64, Box<dyn querymt::chat::http::ChatStreamParser>>> =
                std::cell::RefCell::new(std::collections::HashMap::new());
            static NEXT_STREAM_PARSER_ID: std::cell::Cell<i64> = const { std::cell::Cell::new(1) };
        }

        #[plugin_fn]
        pub fn chat_stream_parser_start(Json(cfg): Json<$Config>) -> FnResult<Json<i64>> {
            let parser = cfg.chat_stream_parser().map_err(llm_err_to_pdk)?;
            let parser_id = NEXT_STREAM_PARSER_ID.with(|next| {
                let id = next.get();
                next.set(id + 1);
                id
            });
            STREAM_PARSERS.with(|parsers| {
                parsers.borrow_mut().insert(parser_id, parser);
            });
            Ok(Json(parser_id))
        }

        #[plugin_fn]
        pub fn chat_stream_parser_parse(
            Json(input): Json<querymt::plugin::extism_impl::ExtismChatChunkParseRequest>,
        ) -> FnResult<Json<Vec<querymt::plugin::extism_impl::ExtismChatChunk>>> {
            let chunks = STREAM_PARSERS.with(|parsers| {
                let mut parsers = parsers.borrow_mut();
                let parser = parsers.get_mut(&input.parser_id).ok_or_else(|| {
                    PdkError::msg(format!("Unknown parser id {}", input.parser_id))
                })?;
                parser.parse_chunk(&input.chunk).map_err(llm_err_to_pdk)
            })?;

            let out = chunks
                .into_iter()
                .map(|chunk| {
                    let usage = match &chunk {
                        StreamChunk::Usage(usage) => Some(usage.clone()),
                        _ => None,
                    };
                    querymt::plugin::extism_impl::ExtismChatChunk { chunk, usage }
                })
                .collect();
            Ok(Json(out))
        }

        #[plugin_fn]
        pub fn chat_stream_parser_finish(
            Json(parser_id): Json<i64>,
        ) -> FnResult<Json<Vec<querymt::plugin::extism_impl::ExtismChatChunk>>> {
            let chunks = STREAM_PARSERS.with(|parsers| {
                let mut parsers = parsers.borrow_mut();
                let mut parser = parsers.remove(&parser_id).ok_or_else(|| {
                    PdkError::msg(format!("Unknown parser id {}", parser_id))
                })?;
                parser.finish().map_err(llm_err_to_pdk)
            })?;

            let out = chunks
                .into_iter()
                .map(|chunk| {
                    let usage = match &chunk {
                        StreamChunk::Usage(usage) => Some(usage.clone()),
                        _ => None,
                    };
                    querymt::plugin::extism_impl::ExtismChatChunk { chunk, usage }
                })
                .collect();
            Ok(Json(out))
        }

        #[plugin_fn]
        pub fn chat_stream_parser_close(Json(parser_id): Json<i64>) -> FnResult<()> {
            STREAM_PARSERS.with(|parsers| {
                parsers.borrow_mut().remove(&parser_id);
            });
            Ok(())
        }

        #[plugin_fn]
        pub fn embed_request(
            Json(input): Json<ExtismEmbedRequest<$Config>>,
        ) -> FnResult<Json<querymt::plugin::extism_impl::SerializableHttpRequest>> {
            let req = input
                .cfg
                .embed_request(&input.inputs)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(querymt::plugin::extism_impl::SerializableHttpRequest { req }))
        }

        #[plugin_fn]
        pub fn parse_embed_response(
            Json(input): Json<querymt::plugin::extism_impl::ExtismEmbedParseRequest<$Config>>,
        ) -> FnResult<Json<Vec<Vec<f32>>>> {
            let out = input
                .cfg
                .parse_embed(input.resp.resp)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(out))
        }

        #[plugin_fn]
        pub fn complete_request(
            Json(input): Json<ExtismCompleteRequest<$Config>>,
        ) -> FnResult<Json<querymt::plugin::extism_impl::SerializableHttpRequest>> {
            let req = input
                .cfg
                .complete_request(&input.req)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(querymt::plugin::extism_impl::SerializableHttpRequest { req }))
        }

        #[plugin_fn]
        pub fn parse_complete_response(
            Json(input): Json<querymt::plugin::extism_impl::ExtismCompleteParseRequest<$Config>>,
        ) -> FnResult<Json<CompletionResponse>> {
            let out = input
                .cfg
                .parse_complete(input.resp.resp)
                .map_err(llm_err_to_pdk)?;
            Ok(Json(out))
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

            let req = input.cfg.stt_request(&stt_req).map_err(llm_err_to_pdk)?;
            let native_resp = qmt_http_request_wrapper(&req)?;
            let resp = input.cfg.parse_stt(native_resp).map_err(llm_err_to_pdk)?;

            Ok(Json(ExtismSttResponse { text: resp.text }))
        }

        #[plugin_fn]
        pub fn speech(
            Json(input): Json<ExtismTtsRequest<$Config>>,
        ) -> FnResult<Json<ExtismTtsResponse>> {
            let voice_config = input
                .voice_config
                .map(|vc| vc.into_voice_config())
                .transpose()
                .map_err(llm_err_to_pdk)?;
            let tts_req = tts::TtsRequest {
                text: input.text,
                model: input.model,
                voice_config,
                format: input.format,
                speed: input.speed,
                language: input.language,
            };

            let req = input.cfg.tts_request(&tts_req).map_err(llm_err_to_pdk)?;
            let native_resp = qmt_http_request_wrapper(&req)?;
            let resp = input.cfg.parse_tts(native_resp).map_err(llm_err_to_pdk)?;

            Ok(Json(ExtismTtsResponse {
                audio_base64: $crate::encode_base64_standard(&resp.audio),
                mime_type: resp.mime_type,
            }))
        }

        #[plugin_fn]
        pub fn base_url() -> FnResult<String> {
            Ok(<$Config>::default_base_url().as_str().to_string())
        }
    };
}
