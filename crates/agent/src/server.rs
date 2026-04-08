use crate::ui::UiServer;
use crate::{acp::AcpServer, session::StorageBackend};
use axum::Router;
use axum::routing::get;
#[cfg(feature = "dashboard")]
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
#[cfg(feature = "dashboard")]
use rust_embed::RustEmbed;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(feature = "dashboard")]
#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Assets;

pub struct AgentServer {
    agent: Arc<crate::agent::LocalAgentHandle>,
    storage: Arc<dyn StorageBackend>,
    default_cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMode {
    #[cfg(feature = "dashboard")]
    Dashboard,
    Api,
}

impl AgentServer {
    pub fn new(
        agent: Arc<crate::agent::LocalAgentHandle>,
        storage: Arc<dyn StorageBackend>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            agent,
            storage,
            default_cwd,
        }
    }

    pub async fn run(self, addr: &str, mode: ServerMode) -> anyhow::Result<()> {
        let app = self.build_app(mode)?;
        let agent = self.agent.clone();

        let listener = tokio::net::TcpListener::bind(addr).await?;
        match mode {
            #[cfg(feature = "dashboard")]
            ServerMode::Dashboard => log::info!("UI dashboard listening on http://{}", addr),
            ServerMode::Api => log::info!("API server listening on http://{}", addr),
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
                log::info!("Received shutdown signal, stopping UI server...");
            })
            .await?;

        // Run graceful agent shutdown (releases scheduler lease, stops background tasks)
        agent.shutdown().await;

        Ok(())
    }

    fn build_app(&self, mode: ServerMode) -> anyhow::Result<Router> {
        let acp_router = AcpServer::new(self.agent.clone()).router();
        let view_store = self.storage.view_store().ok_or_else(|| {
            anyhow::anyhow!("ViewStore is required to serve the UI websocket API")
        })?;
        let ui_router = UiServer::new(
            self.agent.clone(),
            view_store,
            self.storage.session_store().clone(),
            self.default_cwd.clone(),
        )
        .router();

        let export_storage = self.storage.clone();
        let app = Router::new()
            .nest("/acp", acp_router)
            .nest("/ui", ui_router)
            .route(
                "/api/export/sft",
                get(move |query| handle_sft_export(query, export_storage)),
            );

        Ok(match mode {
            #[cfg(feature = "dashboard")]
            ServerMode::Dashboard => app.route("/", get(index_handler)).fallback(static_handler),
            ServerMode::Api => app,
        })
    }
}

#[cfg(feature = "dashboard")]
async fn index_handler() -> impl IntoResponse {
    serve_asset("index.html")
}

#[cfg(feature = "dashboard")]
async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_asset("index.html");
    }
    serve_asset(path)
}

#[cfg(feature = "dashboard")]
fn serve_asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(axum::body::Body::from(content.data))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        None => {
            if path != "index.html" {
                return serve_asset("index.html");
            }
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// SFT export endpoint
// ---------------------------------------------------------------------------

/// Query parameters for `GET /api/export/sft`.
#[derive(serde::Deserialize)]
struct SftExportQuery {
    /// Output format: "openai" (default) or "sharegpt".
    #[serde(default = "default_format")]
    format: String,
    /// Minimum LLM turns per session (default: 1).
    #[serde(default)]
    min_turns: Option<usize>,
    /// Only include sessions using these models (comma-separated).
    #[serde(default)]
    models: Option<String>,
    /// Exclude sessions with errors (default: false).
    #[serde(default)]
    exclude_errored: Option<bool>,
    /// Maximum tool error rate 0.0-1.0 (default: 1.0 = no filter).
    #[serde(default)]
    max_tool_error_rate: Option<f32>,
    /// Replace home directory paths (default: false).
    #[serde(default)]
    scrub_paths: Option<bool>,
    /// Include thinking/reasoning content (default: false).
    #[serde(default)]
    include_thinking: Option<bool>,
    /// Include tool result content (default: true).
    #[serde(default)]
    include_tool_results: Option<bool>,
    /// Max context messages per example (default: 40).
    #[serde(default)]
    max_context: Option<usize>,
    /// Stats-only mode: return stats without data (default: false).
    #[serde(default)]
    stats_only: Option<bool>,
}

fn default_format() -> String {
    "openai".to_string()
}

async fn handle_sft_export(
    axum::extract::Query(query): axum::extract::Query<SftExportQuery>,
    storage: Arc<dyn StorageBackend>,
) -> axum::response::Response {
    use crate::export::sft::{self, SessionFilter, SftExportOptions, SftFormat};
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    // Parse format
    let format = match SftFormat::parse(&query.format) {
        Some(f) => f,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "Unknown format '{}'. Use 'openai' or 'sharegpt'.",
                    query.format
                ),
            )
                .into_response();
        }
    };

    // Parse model filter
    let source_models = query.models.map(|m| {
        m.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });

    let options = SftExportOptions {
        format,
        filter: SessionFilter {
            min_turns: query.min_turns.unwrap_or(1),
            max_tool_error_rate: query.max_tool_error_rate.unwrap_or(1.0),
            source_models,
            exclude_errored: query.exclude_errored.unwrap_or(false),
        },
        scrub_paths: query.scrub_paths.unwrap_or(false),
        path_replacement: "/workspace".to_string(),
        max_context_messages: Some(query.max_context.unwrap_or(40)),
        include_thinking: query.include_thinking.unwrap_or(false),
        include_tool_results: query.include_tool_results.unwrap_or(true),
    };

    // Stats-only mode
    if query.stats_only.unwrap_or(false) {
        match sft::preview_export(storage.as_ref(), &options).await {
            Ok(stats) => {
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::json!({
                        "sessions_total": stats.sessions_total,
                        "sessions_exported": stats.sessions_exported,
                        "sessions_skipped": stats.sessions_skipped,
                        "training_examples": stats.training_examples,
                    })
                    .to_string(),
                )
                    .into_response();
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
            }
        }
    }

    // Full export: stream JSONL via a channel.
    //
    // `export_all_sessions` is async (loads sessions/events) and writes to
    // a `&mut dyn Write` sink. To stream the response we pipe through an
    // mpsc channel. Because `&mut dyn Write` is not `Send`, we drive the
    // export on the current task and forward bytes through the channel.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut channel_writer = AsyncChannelWriter { tx: tx.clone() };
        let result =
            sft::export_all_sessions(storage.as_ref(), &options, &mut channel_writer).await;
        if let Err(e) = result {
            log::error!("SFT export failed: {}", e);
        }
        // tx is dropped here, closing the channel
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"training_data.jsonl\"",
        )
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// [`Write`](std::io::Write) adapter backed by a tokio mpsc channel.
///
/// This struct is `Send` so the future holding it can be spawned on the
/// tokio runtime. It uses [`tokio::sync::mpsc::Sender::try_send`] which is
/// not async, avoiding the need for `blocking_send` (which would panic
/// inside an async context).
struct AsyncChannelWriter {
    tx: tokio::sync::mpsc::Sender<Result<Vec<u8>, std::io::Error>>,
}

// Safety: the only field is the `Sender` which is `Send`.
unsafe impl Send for AsyncChannelWriter {}

impl std::io::Write for AsyncChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Use try_send in a loop with a brief yield if the channel is full.
        // The channel is bounded (32 items) to provide backpressure.
        let data = Ok(buf.to_vec());
        match self.tx.try_send(data) {
            Ok(()) => Ok(buf.len()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
                // Channel full — do a blocking send as fallback.
                // This is safe because export_all_sessions awaits between
                // write calls, giving the receiver time to drain.
                self.tx.blocking_send(data).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "receiver dropped")
                })?;
                Ok(buf.len())
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "receiver dropped",
            )),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
