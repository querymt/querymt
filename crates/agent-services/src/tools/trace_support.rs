//! Capture-based trace test helpers (services layer, test-only).

/// Isolated, capture-based trace tests; never installs global telemetry or uses OTLP.
pub(crate) mod genai_trace {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default, Debug)]
    struct Exporter(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Exporter {
        async fn export(&self, mut batch: Vec<SpanData>) -> OTelSdkResult {
            self.0.lock().unwrap().append(&mut batch);
            Ok(())
        }
    }

    pub(crate) async fn capture<T>(
        future: impl std::future::Future<Output = T>,
    ) -> (T, Vec<SpanData>) {
        capture_inner(
            future,
            false,
            tracing_subscriber::filter::LevelFilter::TRACE,
        )
        .await
    }

    /// Match normal INFO export while leaving existing diagnostic DEBUG logging unchanged.
    pub(crate) async fn capture_info<T>(
        future: impl std::future::Future<Output = T>,
    ) -> (T, Vec<SpanData>) {
        capture_inner(future, false, tracing_subscriber::filter::LevelFilter::INFO).await
    }

    async fn capture_inner<T>(
        future: impl std::future::Future<Output = T>,
        local_tasks: bool,
        level: tracing_subscriber::filter::LevelFilter,
    ) -> (T, Vec<SpanData>) {
        // Exercise the production log-to-tracing bridge as well as explicit span fields.
        let _ = tracing_log::LogTracer::init();
        let exporter = Exporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("genai-test"))
                .with_filter(level),
        );
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = local_tasks.then(|| tracing::dispatcher::set_default(&dispatch));
        let result = future.with_subscriber(dispatch.clone()).await;
        drop(guard);
        provider.force_flush().unwrap();
        let spans = exporter.0.lock().unwrap().clone();
        (result, spans)
    }

    /// Capture spawned actors as well; call only from a current-thread Tokio test.
    pub(crate) async fn capture_local_tasks<T>(
        future: impl std::future::Future<Output = T>,
    ) -> (T, Vec<SpanData>) {
        capture_inner(future, true, tracing_subscriber::filter::LevelFilter::TRACE).await
    }

    pub(crate) fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
        // SDK 0.32 retains updates in order; attribute map semantics are last-value-wins.
        span.attributes
            .iter()
            .rev()
            .find(|attr| attr.key.as_str() == key)
            .map(|attr| &attr.value)
    }

    pub(crate) fn assert_private(spans: &[SpanData]) {
        let capture = format!("{spans:?}");
        for secret in [
            "SECRET_PROMPT",
            "SECRET_RESPONSE",
            "SECRET_ARGUMENT",
            "SECRET_ERROR",
            "SECRET_ENDPOINT",
        ] {
            assert!(
                !capture.contains(secret),
                "sensitive value exported: {secret}"
            );
        }
    }
}
