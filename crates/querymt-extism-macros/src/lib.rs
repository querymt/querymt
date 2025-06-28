use extism_pdk::*;

pub fn to_pdk_request(req: &::http::Request<Vec<u8>>) -> HttpRequest {
    let mut p = HttpRequest::new(req.uri().to_string()).with_method(req.method().as_str());
    for (k, v) in req.headers().iter() {
        let v = v.to_str().unwrap_or_default();
        p = p.with_header(k.as_str(), v);
    }
    p
}

pub fn http_response_to_native(resp: HttpResponse) -> ::http::Response<Vec<u8>> {
    let status = resp.status_code();
    let body = resp.body(); // clones the bytes out of Wasm memory
    let mut builder = ::http::Response::builder().status(status);
    for (k, v) in resp.headers().iter() {
        builder = builder.header(k.as_str(), v.as_str());
    }
    builder.body(body).expect("failed to build http::Response")
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
            chat::http::HTTPChatProvider,
            completion::{CompletionResponse, http::HTTPCompletionProvider},
            embedding::http::HTTPEmbeddingProvider,
            plugin::{
                HTTPLLMProviderFactory,
                extism_impl::{
                    BinaryCodec, ExtismChatRequest, ExtismChatResponse, ExtismCompleteRequest,
                    ExtismEmbedRequest,
                },
            },
        };
        use serde_json::Value;
        use $crate::{http_response_to_native, to_pdk_request};

        // Export the factory name
        #[plugin_fn]
        pub fn name() -> FnResult<String> {
            Ok($name.to_string())
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
            let pdk_req = to_pdk_request(&req);
            let resp: extism_pdk::http::HttpResponse =
                extism_pdk::http::request(&pdk_req, Some(req.body()))?;

            let native_resp = http_response_to_native(resp);
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
            let pdk_req = to_pdk_request(&req);
            let resp: extism_pdk::http::HttpResponse =
                extism_pdk::http::request(&pdk_req, Some(req.body()))?;

            let native_resp = http_response_to_native(resp);
            let chat_response = input
                .cfg
                .parse_chat(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            let dto: ExtismChatResponse = chat_response.into();
            Ok(Json(dto))
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
            let pdk_req = to_pdk_request(&req);
            let resp: extism_pdk::http::HttpResponse =
                extism_pdk::http::request(&pdk_req, Some(req.body()))?;

            let native_resp = http_response_to_native(resp);
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
            let pdk_req = to_pdk_request(&req);
            let resp: extism_pdk::http::HttpResponse =
                extism_pdk::http::request(&pdk_req, Some(req.body()))?;

            let native_resp = http_response_to_native(resp);
            let complete_response = input
                .cfg
                .parse_complete(native_resp)
                .map_err(|e| PdkError::msg(format!("{:#}", e)))?;
            Ok(Json(complete_response))
        }

        #[plugin_fn]
        pub fn base_url() -> FnResult<String> {
            Ok(<$Config>::default_base_url().as_str().to_string())
        }
    };
}
