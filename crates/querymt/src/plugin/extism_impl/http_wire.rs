//! HTTP wire format types for Extism host-plugin communication
//!
//! These types enable HTTP requests and responses to cross the WASM boundary
//! between the Extism host and WASM plugins.
//!
//! # Features
//!
//! This module is available when either of these features are enabled:
//! - `http-client` - For the host side that uses reqwest for HTTP
//! - `extism_plugin` - For the plugin side that runs in WASM
//!
//! Both contexts use the same wire format types to ensure compatibility.

use serde::{Deserialize, Serialize};

/// Serializable wrapper for http::Request that can cross WASM boundary
///
/// Uses `http-serde-ext` to serialize/deserialize `http::Request<Vec<u8>>`.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerializableHttpRequest {
    #[serde(with = "http_serde_ext::request")]
    pub req: http::Request<Vec<u8>>,
}

/// Serializable wrapper for http::Response that can cross WASM boundary
///
/// Uses `http-serde-ext` to serialize/deserialize `http::Response<Vec<u8>>`.
#[derive(Serialize, Deserialize, Clone)]
pub struct SerializableHttpResponse {
    #[serde(with = "http_serde_ext::response")]
    pub resp: http::Response<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::SerializableHttpRequest;

    #[test]
    fn request_roundtrip_preserves_authorization_header() {
        let req = http::Request::builder()
            .method("POST")
            .uri("https://example.invalid/v1/chat/completions")
            .header(http::header::AUTHORIZATION, "Bearer sk-test-1234567890")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(br#"{"model":"mimo-v2.5-pro"}"#.to_vec())
            .expect("request should build");

        let wrapped = SerializableHttpRequest { req };
        let encoded = serde_json::to_vec(&wrapped).expect("request should serialize");
        let decoded: SerializableHttpRequest =
            serde_json::from_slice(&encoded).expect("request should deserialize");

        assert_eq!(
            decoded
                .req
                .headers()
                .get(http::header::AUTHORIZATION)
                .expect("auth header should be present"),
            "Bearer sk-test-1234567890"
        );
        assert_eq!(
            decoded
                .req
                .headers()
                .get(http::header::CONTENT_TYPE)
                .expect("content-type should be present"),
            "application/json"
        );
    }
}
