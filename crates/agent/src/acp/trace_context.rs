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
    // Malformed metadata must not inherit an unrelated consumer's attached context.
    let parent_cx = propagator.extract_with_context(&opentelemetry::Context::new(), &carrier);

    if parent_cx.has_active_span() {
        Some(parent_cx)
    } else {
        None
    }
}

/// Inject the current execution context into transient ACP request metadata only.
pub(crate) fn inject_current_acp_trace_context()
-> Option<serde_json::Map<String, serde_json::Value>> {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let context = tracing::Span::current().context();
    if !context.span().span_context().is_valid() {
        return None;
    }
    let mut meta = serde_json::Map::new();
    TraceContextPropagator::new().inject_context(&context, &mut JsonMetaInjector(&mut meta));
    Some(meta)
}

struct JsonMetaInjector<'a>(&'a mut serde_json::Map<String, serde_json::Value>);

impl opentelemetry::propagation::Injector for JsonMetaInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value.into());
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

    #[tokio::test]
    async fn genai_injection_round_trips_tracestate_and_is_noop_without_telemetry() {
        use crate::test_utils::helpers::genai_trace::capture;
        use tracing::Instrument;
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        assert!(inject_current_acp_trace_context().is_none());
        let (_, spans) = capture(async {
            let span = tracing::info_span!(parent: None, "emitter");
            let parent = serde_json::json!({
                "traceparent": "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
                "tracestate": "vendor=value"
            });
            span.set_parent(extract_acp_trace_context(&parent).unwrap())
                .unwrap();
            async {
                let meta = inject_current_acp_trace_context().unwrap();
                assert_eq!(meta["tracestate"], "vendor=value");
                let context = extract_acp_trace_context_from_meta(&meta).unwrap();
                assert_eq!(
                    context.span().span_context().trace_id().to_string(),
                    "0af7651916cd43dd8448eb211c80319c"
                );
                assert_eq!(
                    context.span().span_context().span_id(),
                    tracing::Span::current()
                        .context()
                        .span()
                        .span_context()
                        .span_id()
                );
            }
            .instrument(span)
            .await;
        })
        .await;
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn malformed_typed_acp_meta_is_ignored() {
        let meta = serde_json::json!({ "traceparent": "invalid" });
        assert!(extract_acp_trace_context_from_meta(meta.as_object().unwrap()).is_none());
    }
}
