use extism_pdk::*;
use serde::{Deserialize, Serialize};

// Import base serializable types from querymt
use querymt::plugin::extism_impl::{SerializableHttpRequest as BaseHttpRequest, SerializableHttpResponse as BaseHttpResponse};

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
}

/// Call custom qmt_http_request host function using http-serde-ext
pub fn qmt_http_request_wrapper(req: &::http::Request<Vec<u8>>) -> Result<::http::Response<Vec<u8>>, Error> {
    // Wrap request in serializable types
    let ser_req = BaseHttpRequest { req: req.clone() };
    let wrapped_req = HttpRequest(ser_req);
    
    // Call host function
    let wrapped_resp = unsafe { qmt_http_request(wrapped_req)? };
    
    // Extract the response
    Ok(wrapped_resp.0.resp)
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
        use $crate::qmt_http_request_wrapper;

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
        pub fn base_url() -> FnResult<String> {
            Ok(<$Config>::default_base_url().as_str().to_string())
        }
    };
}
