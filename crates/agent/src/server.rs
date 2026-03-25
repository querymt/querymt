use crate::ui::UiServer;
use crate::{acp::AcpServer, session::StorageBackend};
use axum::Router;
#[cfg(feature = "dashboard")]
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
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
    ApiOnly,
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
            ServerMode::ApiOnly => log::info!("API-only server listening on http://{}", addr),
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

        let app = Router::new()
            .nest("/acp", acp_router)
            .nest("/ui", ui_router);

        Ok(match mode {
            #[cfg(feature = "dashboard")]
            ServerMode::Dashboard => app.route("/", get(index_handler)).fallback(static_handler),
            ServerMode::ApiOnly => app,
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
