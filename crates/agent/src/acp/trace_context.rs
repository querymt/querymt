// ─────────────────────────────────────────────────────────────────────────────
// QueryMT Agent — ACP Trace Context Extraction
//
// Extracts W3C traceparent/tracestate from ACP request `_meta`.
//
// The primary API is [`extract_acp_trace_context`] which returns an
// `Option<opentelemetry::Context>` for the caller to set as parent on a
// newly-created span *before* entering it.
//
// This enables cross-boundary trace parenting: mobile UI spans become
// parents of agent-side spans (acp.load_session, acp.prompt, etc.).
// ─────────────────────────────────────────────────────────────────────────────

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;

/// Extract a W3C trace context from ACP `_meta`.
///
/// Returns `Some(Context)` when a valid `traceparent` was found and
/// extracted successfully.  The caller should use
/// `tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(cx)` on a
/// newly-created span *before* entering it.
///
/// Returns `None` when `_meta` has no `traceparent` or extraction failed.
pub fn extract_acp_trace_context(meta: &serde_json::Value) -> Option<opentelemetry::Context> {
    extract_acp_trace_context_from_meta(meta.as_object()?)
}

/// Extract W3C trace context directly from a typed ACP `_meta` map.
pub fn extract_acp_trace_context_from_meta(
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Option<opentelemetry::Context> {
    if !meta.contains_key("traceparent") {
        return None;
    }

    let propagator = TraceContextPropagator::new();
    let carrier = JsonMetaCarrier(meta);
    let parent_cx = propagator.extract(&carrier);

    if parent_cx.has_active_span() {
        Some(parent_cx)
    } else {
        None
    }
}

// ─── Carrier adaptor for serde_json::Map ──────────────────────────────────────

/// Implements the `opentelemetry::propagation::Extractor` trait for a
/// `serde_json::Map<String, Value>` so `TraceContextPropagator::extract`
/// can read `traceparent` / `tracestate` from it.
struct JsonMetaCarrier<'a>(&'a serde_json::Map<String, serde_json::Value>);

impl opentelemetry::propagation::Extractor for JsonMetaCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceContextExt;

    #[test]
    fn typed_acp_meta_extracts_w3c_parent_and_tracestate() {
        let meta = serde_json::json!({
            "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "tracestate": "vendor=value",
            "other": "preserved"
        });
        let context = extract_acp_trace_context_from_meta(meta.as_object().unwrap())
            .expect("valid trace context");
        let span = context.span();
        assert_eq!(
            span.span_context().trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(
            span.span_context().span_id().to_string(),
            "b7ad6b7169203331"
        );
        assert_eq!(span.span_context().trace_state().header(), "vendor=value");
    }

    #[test]
    fn malformed_typed_acp_meta_is_ignored() {
        let meta = serde_json::json!({ "traceparent": "invalid" });
        assert!(extract_acp_trace_context_from_meta(meta.as_object().unwrap()).is_none());
    }
}
