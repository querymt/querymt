//! Classification of Anthropic HTTP and stream errors.

use crate::wire::response::AnthropicApiErrorBody;
use http::Response;
use querymt::error::{
    LLMError, ProviderErrorKind, ProviderFailure, classify_status_only, parse_retry_after,
    parse_retry_after_from_message,
};
use serde_json::Value;

/// Normalize Anthropic `error.type` tokens for kind lookup.
pub(crate) fn normalize_anthropic_error_token(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// Map Anthropic `error.type` to a unified kind.
///
/// Anthropic documents: `invalid_request_error`, `authentication_error`,
/// `permission_error`, `not_found_error`, `request_too_large`, `rate_limit_error`,
/// `api_error`, `overloaded_error`.
pub(crate) fn anthropic_error_kind(error_type: &str) -> Option<ProviderErrorKind> {
    match error_type {
        "overloaded_error" | "overloaded" => Some(ProviderErrorKind::ServerOverloaded),
        "rate_limit_error" | "rate_limit_exceeded" | "rate_limited" => {
            Some(ProviderErrorKind::RateLimited)
        }
        "authentication_error" | "permission_error" => Some(ProviderErrorKind::Authentication),
        "invalid_request_error" | "invalid_request" | "not_found_error" | "request_too_large" => {
            Some(ProviderErrorKind::InvalidRequest)
        }
        "api_error" => Some(ProviderErrorKind::UnknownTransient),
        _ => None,
    }
}

pub(crate) fn map_anthropic_api_error(
    error: Option<&AnthropicApiErrorBody>,
    header_retry_after: Option<u64>,
    request_id: Option<&str>,
    unknown_transient: bool,
) -> ProviderFailure {
    let error_type = error.and_then(|e| e.error_type.clone());
    let message = error
        .and_then(|e| e.message.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "anthropic request failed".to_owned());

    let type_norm = error_type.as_deref().map(normalize_anthropic_error_token);
    let kind = type_norm
        .as_deref()
        .and_then(anthropic_error_kind)
        .unwrap_or(if unknown_transient {
            ProviderErrorKind::UnknownTransient
        } else {
            ProviderErrorKind::UnknownPermanent
        });

    let retry_after_secs = if kind == ProviderErrorKind::RateLimited {
        header_retry_after.or_else(|| parse_retry_after_from_message(&message))
    } else {
        header_retry_after
    };

    ProviderFailure::new(kind, message)
        .with_error_type(error_type)
        .with_request_id(request_id.map(str::to_owned))
        .with_retry_after_secs(retry_after_secs)
}

/// Classify an Anthropic HTTP error body.
pub(crate) fn classify_anthropic_http_error(response: &Response<Vec<u8>>) -> LLMError {
    let status = response.status().as_u16();
    let retry_after_secs = parse_retry_after(response.headers());
    let request_id = response
        .headers()
        .get("request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|value| value.to_str().ok());
    // Prefer structured `{"type":"error","error":{...}}` or top-level gateway errors.
    if let Ok(envelope) = serde_json::from_slice::<Value>(response.body()) {
        let error_value = envelope.get("error").cloned().or_else(|| {
            (envelope.get("message").is_some() && envelope.get("type").is_some())
                .then_some(envelope.clone())
        });

        if let Some(error_value) = error_value {
            let parsed: Option<AnthropicApiErrorBody> = serde_json::from_value(error_value).ok();
            if parsed
                .as_ref()
                .is_some_and(|e| e.error_type.is_some() || e.message.is_some())
            {
                let status_guess_transient = matches!(status, 429 | 500..=599);
                return map_anthropic_api_error(
                    parsed.as_ref(),
                    retry_after_secs,
                    request_id,
                    status_guess_transient,
                )
                .into();
            }
        }
    }

    classify_status_only(status, response.headers(), response.body())
}
