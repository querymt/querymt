use crate::acp::AcpServer;
use crate::session::projection::ViewStore;
use crate::ui::UiServer;
use axum::{
    Router,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct Assets;

pub struct AgentServer {
    agent: Arc<crate::agent::AgentHandle>,
    view_store: Arc<dyn ViewStore>,
    default_cwd: Option<PathBuf>,
}

impl AgentServer {
    pub fn new(
        agent: Arc<crate::agent::AgentHandle>,
        view_store: Arc<dyn ViewStore>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            agent,
            view_store,
            default_cwd,
        }
    }

    pub async fn run(self, addr: &str) -> anyhow::Result<()> {
        let agent = self.agent;
        let acp_router = AcpServer::new(agent.clone()).router();
        let ui_router = UiServer::new(agent, self.view_store, self.default_cwd).router();

        let app = Router::new()
            .nest("/acp", acp_router)
            .nest("/ui", ui_router)
            .route("/", get(index_handler))
            .fallback(static_handler);

        let listener = tokio::net::TcpListener::bind(addr).await?;
        log::info!("UI dashboard listening on http://{}", addr);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn index_handler() -> impl IntoResponse {
    serve_asset("index.html")
}

async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_asset("index.html");
    }
    serve_asset(path)
}

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
