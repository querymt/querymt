/// Setup tracing + log integration
///
/// This is a thin wrapper around querymt_utils::telemetry::setup_telemetry
/// that provides the CLI's package name and version.
pub fn setup_logging() {
    querymt_utils::telemetry::setup_telemetry(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}
