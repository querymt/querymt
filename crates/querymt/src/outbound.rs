mod http_client {
    #[cfg(not(target_arch = "wasm32"))]
    pub mod imp {
        use crate::error::{LLMError, classify_http_status};
        use futures::StreamExt;
        use http::{Request, Response};
        use once_cell::sync::Lazy;
        use reqwest::Client;
        #[cfg(debug_assertions)]
        use serde_json::Value;

        /// A single, global client, built once
        pub static CLIENT: Lazy<Client> = Lazy::new(Client::new);

        #[cfg(debug_assertions)]
        fn header_token_hint(value: Option<&http::HeaderValue>) -> String {
            let Some(value) = value else {
                return "<missing>".to_string();
            };
            let Ok(value_str) = value.to_str() else {
                return "<non-utf8>".to_string();
            };
            let mut parts = value_str.splitn(2, ' ');
            let scheme = parts.next().unwrap_or("<unknown>");
            let token = parts.next().unwrap_or("");
            if token.is_empty() {
                return format!("{scheme} <empty>");
            }
            format!("{scheme} <redacted>")
        }

        #[cfg(debug_assertions)]
        fn request_json_summary(req: &Request<Vec<u8>>) -> String {
            let Ok(value) = serde_json::from_slice::<Value>(req.body()) else {
                return "<non-json>".to_string();
            };
            let Some(obj) = value.as_object() else {
                return "<json-non-object>".to_string();
            };

            let model = obj.get("model").and_then(Value::as_str).unwrap_or("<none>");
            let stream = obj
                .get("stream")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<missing>".to_string());
            let messages_len = obj
                .get("messages")
                .and_then(Value::as_array)
                .map(|v| v.len().to_string())
                .unwrap_or_else(|| "<missing>".to_string());

            format!("model={model} stream={stream} messages_len={messages_len}")
        }

        #[cfg(debug_assertions)]
        fn is_sensitive_key(key: &str) -> bool {
            let key = key.to_ascii_lowercase();
            matches!(
                key.as_str(),
                "api_key" | "apikey" | "authorization" | "bearer" | "token" | "access_token"
            ) || key.ends_with("_token")
                || key.ends_with("_key")
                || key.ends_with("-token")
                || key.ends_with("-key")
        }

        #[cfg(debug_assertions)]
        fn redact_json_value(value: &mut Value) {
            match value {
                Value::Object(obj) => {
                    for (key, value) in obj.iter_mut() {
                        if is_sensitive_key(key) {
                            *value = Value::String("[redacted]".to_string());
                        } else {
                            redact_json_value(value);
                        }
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        redact_json_value(value);
                    }
                }
                _ => {}
            }
        }

        #[cfg(debug_assertions)]
        fn truncate_preview(mut out: String, max_len: usize) -> String {
            if out.len() > max_len {
                out.truncate(max_len);
                out.push_str("...(truncated)");
            }
            out
        }

        #[cfg(debug_assertions)]
        fn redacted_error_body(bytes: &[u8], max_len: usize) -> String {
            let Ok(mut value) = serde_json::from_slice::<Value>(bytes) else {
                return format!("<non-json body omitted: {} bytes>", bytes.len());
            };
            redact_json_value(&mut value);
            truncate_preview(value.to_string(), max_len)
        }

        /// Send an HTTP request and preserve the response status, headers, and body.
        ///
        /// Provider adapters use this to apply provider-specific error classification.
        pub async fn call_outbound_raw(
            req: Request<Vec<u8>>,
        ) -> Result<Response<Vec<u8>>, LLMError> {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(|e| LLMError::HttpError(e.to_string()))?;

            #[cfg(debug_assertions)]
            {
                let auth_hint = header_token_hint(req.headers().get(http::header::AUTHORIZATION));
                log::debug!(
                    "outbound.call method={} uri={} content_type={} has_authorization={} auth_hint={} body_len={} body_summary={}",
                    req.method(),
                    req.uri(),
                    req.headers()
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    req.headers().contains_key(http::header::AUTHORIZATION),
                    auth_hint,
                    req.body().len(),
                    request_json_summary(&req)
                );
            }

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value
                    .to_str()
                    .map_err(|e| LLMError::HttpError(e.to_string()))?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?;
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await?.to_vec();

            if !status.is_success() {
                #[cfg(debug_assertions)]
                log::debug!(
                    "outbound.call error status={} content_type={} request_id={} body_preview={}",
                    status.as_u16(),
                    headers
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    headers
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    redacted_error_body(&bytes, 2048)
                );
                #[cfg(not(debug_assertions))]
                log::debug!(
                    "outbound.call error status={} content_type={} request_id={}",
                    status.as_u16(),
                    headers
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    headers
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>")
                );
            }

            let mut builder = Response::builder().status(status.as_u16());
            for (name, value) in headers.iter() {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            Ok(builder.body(bytes).unwrap())
        }

        /// Send a streaming HTTP request and preserve the response metadata.
        ///
        /// Non-success bodies are returned as a one-item stream so provider adapters
        /// can classify the complete response without changing the public stream API.
        pub async fn call_outbound_stream_raw(
            req: Request<Vec<u8>>,
        ) -> Result<
            (
                Response<()>,
                std::pin::Pin<
                    Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
                >,
            ),
            LLMError,
        > {
            let client = &*CLIENT;

            let method = req
                .method()
                .as_str()
                .parse::<reqwest::Method>()
                .map_err(|e| LLMError::HttpError(e.to_string()))?;

            #[cfg(debug_assertions)]
            {
                let auth_hint = header_token_hint(req.headers().get(http::header::AUTHORIZATION));
                log::debug!(
                    "outbound.call_stream method={} uri={} content_type={} has_authorization={} auth_hint={} body_len={} body_summary={}",
                    req.method(),
                    req.uri(),
                    req.headers()
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    req.headers().contains_key(http::header::AUTHORIZATION),
                    auth_hint,
                    req.body().len(),
                    request_json_summary(&req)
                );
            }

            let mut rb = client.request(method, req.uri().to_string());

            for (name, value) in req.headers().iter() {
                let val_str = value
                    .to_str()
                    .map_err(|e| LLMError::HttpError(e.to_string()))?;
                rb = rb.header(name.as_str(), val_str);
            }

            let resp = rb.body(req.into_body()).send().await?;
            let status = resp.status();
            if !status.is_success() {
                let headers = resp.headers().clone();
                let bytes = resp.bytes().await?.to_vec();
                #[cfg(debug_assertions)]
                log::debug!(
                    "outbound.call_stream error status={} content_type={} request_id={} body_preview={}",
                    status.as_u16(),
                    headers
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    headers
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    redacted_error_body(&bytes, 2048)
                );
                #[cfg(not(debug_assertions))]
                log::debug!(
                    "outbound.call_stream error status={} content_type={} request_id={}",
                    status.as_u16(),
                    headers
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>"),
                    headers
                        .get("x-request-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>")
                );
                let mut builder = Response::builder().status(status.as_u16());
                for (name, value) in headers.iter() {
                    builder = builder.header(name.as_str(), value.as_bytes());
                }
                let response = builder.body(()).unwrap();
                return Ok((
                    response,
                    Box::pin(futures::stream::once(async move {
                        Ok(bytes::Bytes::from(bytes))
                    })),
                ));
            }

            let headers = resp.headers().clone();
            let mut builder = Response::builder().status(status.as_u16());
            for (name, value) in headers.iter() {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            Ok((builder.body(()).unwrap(), Box::pin(resp.bytes_stream())))
        }

        /// Send an HTTP request using the generic status classifier.
        pub async fn call_outbound(req: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, LLMError> {
            let response = call_outbound_raw(req).await?;
            if response.status().is_success() {
                return Ok(response);
            }

            Err(classify_http_status(
                response.status().as_u16(),
                response.headers(),
                response.body(),
            ))
        }

        /// Send a streaming HTTP request using the generic status classifier.
        pub async fn call_outbound_stream(
            req: Request<Vec<u8>>,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
            LLMError,
        > {
            let (response, mut stream) = call_outbound_stream_raw(req).await?;
            if response.status().is_success() {
                return Ok(stream);
            }

            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                body.extend_from_slice(&chunk?);
            }
            Err(classify_http_status(
                response.status().as_u16(),
                response.headers(),
                &body,
            ))
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub mod imp {
        use crate::error::LLMError;
        use http::{Request, Response};

        pub async fn call_outbound_raw(
            _req: Request<Vec<u8>>,
        ) -> Result<Response<Vec<u8>>, LLMError> {
            Err(LLMError::InvalidRequest("".into()))
        }

        pub async fn call_outbound_stream_raw(
            _req: Request<Vec<u8>>,
        ) -> Result<
            (
                Response<()>,
                std::pin::Pin<
                    Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>,
                >,
            ),
            LLMError,
        > {
            Err(LLMError::InvalidRequest("".into()))
        }

        pub async fn call_outbound(_req: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, LLMError> {
            Err(LLMError::InvalidRequest("".into()))
        }

        pub async fn call_outbound_stream(
            _req: Request<Vec<u8>>,
        ) -> Result<futures::stream::Empty<reqwest::Result<bytes::Bytes>>, LLMError> {
            Err(LLMError::InvalidRequest("".into()))
        }
    }
}

use crate::error::LLMError;
use http::Response;
pub use http_client::imp::{
    call_outbound, call_outbound_raw, call_outbound_stream, call_outbound_stream_raw,
};

/// Apply the generic HTTP status policy to a raw response.
pub fn ensure_success(response: Response<Vec<u8>>) -> Result<Response<Vec<u8>>, LLMError> {
    if response.status().is_success() {
        return Ok(response);
    }

    Err(crate::error::classify_http_status(
        response.status().as_u16(),
        response.headers(),
        response.body(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_success_classifies_non_success_response() {
        let response = Response::builder()
            .status(429)
            .header(http::header::RETRY_AFTER, "7")
            .body(br#"{"error":{"message":"slow down"}}"#.to_vec())
            .unwrap();

        let error = ensure_success(response).unwrap_err();
        assert!(matches!(error, LLMError::RateLimited { .. }));
        assert_eq!(error.retry_after_secs(), Some(7));
    }

    #[test]
    fn ensure_success_preserves_success_response() {
        let response = Response::builder().status(200).body(vec![1, 2, 3]).unwrap();
        assert_eq!(ensure_success(response).unwrap().body(), &[1, 2, 3]);
    }
}
