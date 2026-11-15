//! Task-local handle to the semantic tool span.
//!
//! Only the built-in invocation future carries the span; tools reach it
//! through [`with_tool_span`] to attach attributes or rename it.

use opentelemetry::trace::TraceContextExt;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

tokio::task_local! {
    // Only the built-in invocation future carries the existing semantic tool span.
    static TOOL_SPAN: Span;
}

/// Run `future` with `span` as the current tool span.
pub async fn tool_scope<T>(span: Span, future: impl std::future::Future<Output = T>) -> T {
    TOOL_SPAN.scope(span, future).await
}

/// Call `f` with the current tool span, if the caller runs inside [`tool_scope`].
pub fn with_tool_span(f: impl FnOnce(&Span)) {
    let _ = TOOL_SPAN.try_with(f);
}

/// Rename the tool span, including the already started OpenTelemetry span.
pub fn rename_tool(span: &Span, name: String) {
    span.record("otel.name", &name);
    // Updating the tracing field alone cannot rename the started OTel span in 0.33.
    span.context().span().update_name(name);
}
