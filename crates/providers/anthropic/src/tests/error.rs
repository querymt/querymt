//! Unit tests: error.

use crate::error::classify_anthropic_http_error;
use crate::tests::support::test_anthropic;
use http::Response;
use querymt::chat::http::HTTPChatProvider;
use querymt::error::{LLMError, ProviderErrorKind};

#[test]
fn classify_http_top_level_gateway_error_is_classified() {
    let response = Response::builder()
        .status(503)
        .body(br#"{"type":"api_error","message":"gateway busy"}"#.to_vec())
        .unwrap();
    let error = classify_anthropic_http_error(&response);
    assert!(matches!(
        error,
        LLMError::ProviderResponseError(ref failure)
            if failure.kind() == ProviderErrorKind::UnknownTransient
                && failure.message() == "gateway busy"
                && failure.error_type() == Some("api_error")
                && failure.code().is_none()
    ));
}

#[test]
fn classify_http_missing_message_uses_safe_fallback() {
    let response = Response::builder()
        .status(500)
        .body(br#"{"type":"error","error":{"type":"api_error","private":"secret"}}"#.to_vec())
        .unwrap();
    let error = classify_anthropic_http_error(&response);
    assert!(matches!(
        error,
        LLMError::ProviderResponseError(ref failure)
            if failure.message() == "anthropic request failed"
                && !failure.message().contains("private")
    ));
}

#[test]
fn classify_http_overloaded_is_retryable() {
    let response = Response::builder()
        .status(529)
        .body(
            br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                .to_vec(),
        )
        .unwrap();
    let error = classify_anthropic_http_error(&response);
    assert!(error.is_retryable());
    match &error {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
            assert_eq!(failure.error_type(), Some("overloaded_error"));
            assert_eq!(failure.code(), None);
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}

#[test]
fn classify_http_invalid_request_is_permanent() {
    let response = Response::builder()
        .status(400)
        .body(
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: text content blocks must be non-empty"}}"#
                .to_vec(),
        )
        .unwrap();
    let error = classify_anthropic_http_error(&response);
    assert!(!error.is_retryable());
    match &error {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::InvalidRequest);
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}

#[test]
fn classify_http_rate_limit_is_retryable() {
    let response = Response::builder()
        .status(429)
        .header(http::header::RETRY_AFTER, "12")
        .header("request-id", "req-ant-http")
        .body(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#
                .to_vec(),
        )
        .unwrap();
    let error = classify_anthropic_http_error(&response);
    assert!(error.is_retryable());
    assert_eq!(error.retry_after_secs(), Some(12));
    match &error {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::RateLimited);
            assert_eq!(failure.request_id(), Some("req-ant-http"));
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}

#[test]
fn streaming_error_event_overloaded_is_retryable() {
    let anthropic = test_anthropic("sk-ant-api03-test");
    let mut parser = anthropic
        .chat_stream_parser(serde_json::Value::Null)
        .unwrap();
    let line =
        br#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}
"#;
    let err = parser
        .parse_chunk(line)
        .expect_err("error event must fail the stream");
    assert!(err.is_retryable());
    match err {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}

#[test]
fn streaming_api_error_is_transient() {
    let anthropic = test_anthropic("sk-ant-api03-test");
    let mut parser = anthropic
        .chat_stream_parser(serde_json::Value::Null)
        .unwrap();
    let line =
        br#"data: {"type":"error","error":{"type":"api_error","message":"Internal server error"}}
"#;
    let error = parser
        .parse_chunk(line)
        .expect_err("api error event must fail the stream");

    assert!(error.is_retryable());
    match error {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::UnknownTransient);
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}

#[test]
fn streaming_error_event_invalid_request_is_permanent() {
    let anthropic = test_anthropic("sk-ant-api03-test");
    let mut parser = anthropic
        .chat_stream_parser(serde_json::Value::Null)
        .unwrap();
    let line =
        br#"data: {"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}
"#;
    let err = parser
        .parse_chunk(line)
        .expect_err("error event must fail the stream");
    assert!(!err.is_retryable());
    match err {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.kind(), ProviderErrorKind::InvalidRequest);
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
}
