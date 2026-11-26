use crate::api::ProfileRuntimeHandle;
use crate::session::StorageBackend;
use axum::Router;
use axum::routing::get;
#[cfg(feature = "dashboard-server")]
use axum::{
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
#[cfg(feature = "dashboard-server")]
use rust_embed::RustEmbed;
use std::sync::Arc;

/// The web UI served by `ServerMode::Dashboard`: QueryMT Desktop web, prepared by
/// build.rs (`dashboard` feature).
#[cfg(feature = "dashboard")]
#[derive(RustEmbed)]
#[folder = "$QMT_UI_EMBED_DIR"]
struct DashboardAssets;

/// `dashboard-server` without `dashboard`: the dashboard routes and the same-origin
/// ACP endpoint work, but the page only says the UI is not embedded. Lets checks and
/// tests of the server code skip the wasm build.
#[cfg(all(feature = "dashboard-server", not(feature = "dashboard")))]
#[derive(RustEmbed)]
#[folder = "dashboard-placeholder"]
struct DashboardAssets;

pub struct AgentServer {
    agent: Arc<crate::agent::LocalAgentHandle>,
    storage: Arc<dyn StorageBackend>,
    profiles: Option<ProfileRuntimeHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMode {
    #[cfg(feature = "dashboard-server")]
    Dashboard,
    Api,
}

impl AgentServer {
    pub fn new(
        agent: Arc<crate::agent::LocalAgentHandle>,
        storage: Arc<dyn StorageBackend>,
    ) -> Self {
        Self {
            agent,
            storage,
            profiles: None,
        }
    }

    pub fn with_profiles(mut self, profiles: ProfileRuntimeHandle) -> Self {
        #[cfg(feature = "remote")]
        if let Some(mesh) = self.agent.mesh() {
            profiles.set_mesh_handle(mesh);
        }
        self.agent.set_profiles(profiles.clone());
        self.profiles = Some(profiles);
        self
    }

    #[cfg(test)]
    pub(crate) fn profiles(&self) -> Option<ProfileRuntimeHandle> {
        self.profiles.clone()
    }

    pub async fn run(self, addr: &str, mode: ServerMode) -> anyhow::Result<()> {
        let profiles = self.profiles.clone();
        let app = self.build_app(mode)?;
        let agent = self.agent.clone();

        let listener = tokio::net::TcpListener::bind(addr).await?;
        match mode {
            #[cfg(feature = "dashboard-server")]
            ServerMode::Dashboard => log::info!("UI dashboard listening on http://{}", addr),
            ServerMode::Api => log::info!("API server listening on http://{}", addr),
        }
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            crate::acp::shutdown::signal().await;
            log::info!("Received shutdown signal, stopping server...");
        })
        .await?;

        // Request shutdown without waiting for background drains.
        if let Some(profiles) = profiles {
            profiles.shutdown().await;
        }
        agent.shutdown().await;

        Ok(())
    }

    fn build_app(&self, mode: ServerMode) -> anyhow::Result<Router> {
        let acp_router = match mode {
            #[cfg(feature = "dashboard-server")]
            ServerMode::Dashboard => crate::acp::websocket::same_origin_router(self.agent.clone()),
            ServerMode::Api => crate::acp::websocket::router(self.agent.clone()),
        };
        let export_storage = self.storage.clone();
        let app = acp_router.route(
            "/api/export/sft",
            get(move |query| handle_sft_export(query, export_storage)),
        );

        Ok(match mode {
            #[cfg(feature = "dashboard-server")]
            ServerMode::Dashboard => app
                .route("/", get(dashboard_index_handler))
                .fallback(dashboard_static_handler),
            ServerMode::Api => app,
        })
    }
}

#[cfg(feature = "dashboard-server")]
async fn dashboard_index_handler(headers: HeaderMap) -> impl IntoResponse {
    serve_asset::<DashboardAssets>("index.html", &headers)
}

#[cfg(feature = "dashboard-server")]
async fn dashboard_static_handler(uri: axum::http::Uri, headers: HeaderMap) -> impl IntoResponse {
    serve_static_path::<DashboardAssets>(&uri, &headers)
}

#[cfg(feature = "dashboard-server")]
fn serve_static_path<A: RustEmbed>(uri: &axum::http::Uri, headers: &HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_asset::<A>("index.html", headers);
    }
    serve_asset::<A>(path, headers)
}

#[cfg(feature = "dashboard-server")]
fn serve_asset<A: RustEmbed>(path: &str, headers: &HeaderMap) -> Response {
    match find_asset::<A>(path) {
        Some(asset) => asset_response(path, asset, headers),
        None => {
            // Client-side routes: unknown paths get the app.
            if path != "index.html" {
                return serve_asset::<A>("index.html", headers);
            }
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// An embedded dashboard file as stored: build.rs replaces compressible files
/// with `<name>.gz` (crates/agent/build_support/gpui_web.rs).
#[cfg(feature = "dashboard-server")]
struct EmbeddedAsset {
    bytes: std::borrow::Cow<'static, [u8]>,
    gzipped: bool,
    /// SHA-256 of `bytes`, computed by rust-embed at build time.
    sha256: [u8; 32],
}

#[cfg(feature = "dashboard-server")]
fn find_asset<A: RustEmbed>(path: &str) -> Option<EmbeddedAsset> {
    let (file, gzipped) = match A::get(&format!("{path}.gz")) {
        Some(file) => (file, true),
        None => (A::get(path)?, false),
    };
    Some(EmbeddedAsset {
        sha256: file.metadata.sha256_hash(),
        bytes: file.data,
        gzipped,
    })
}

/// The response for an embedded file: gzip bytes as-is when the client accepts
/// gzip (every browser does), decompressed otherwise, and revalidation through
/// `ETag`/`If-None-Match` so a reload transfers nothing until the binary changes.
#[cfg(feature = "dashboard-server")]
fn asset_response(path: &str, asset: EmbeddedAsset, request: &HeaderMap) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let send_gzip = asset.gzipped && accepts_gzip(request);
    let hash = hex::encode(&asset.sha256[..16]);
    // The decompressed variant is a different representation, so a different tag.
    let etag = if asset.gzipped && !send_gzip {
        format!("\"{hash}-identity\"")
    } else {
        format!("\"{hash}\"")
    };

    let mut response = Response::builder()
        .header(header::ETAG, &etag)
        // Cache, but ask the server before every use: a new qmtcode build changes
        // the tags, an unchanged one answers 304.
        .header(header::CACHE_CONTROL, "no-cache");
    if asset.gzipped {
        response = response.header(header::VARY, "accept-encoding");
    }

    if if_none_match(request, &etag) {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let body = if asset.gzipped && !send_gzip {
        match gunzip(&asset.bytes) {
            Ok(bytes) => axum::body::Body::from(bytes),
            Err(err) => {
                log::error!("failed to decompress embedded dashboard asset {path}: {err}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        axum::body::Body::from(asset.bytes)
    };
    if send_gzip {
        response = response.header(header::CONTENT_ENCODING, "gzip");
    }
    response
        .status(StatusCode::OK)
        // From the requested name, so `.wasm` stays `application/wasm` (needed for
        // streaming compilation) even though the stored file is `.wasm.gz`.
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(feature = "dashboard-server")]
fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::with_capacity(bytes.len() * 3);
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
}

/// Whether `Accept-Encoding` allows gzip: an explicit `gzip`/`x-gzip` entry decides,
/// otherwise `*`; `q=0` refuses.
#[cfg(feature = "dashboard-server")]
fn accepts_gzip(request: &HeaderMap) -> bool {
    let mut wildcard = None;
    for value in request.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for entry in value.split(',') {
            let mut parts = entry.split(';');
            let coding = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let allowed = parts
                .filter_map(|param| {
                    let (name, value) = param.split_once('=')?;
                    name.trim()
                        .eq_ignore_ascii_case("q")
                        .then(|| value.trim().parse::<f32>().ok())
                        .flatten()
                })
                .next_back()
                .is_none_or(|q| q > 0.0);
            match coding.as_str() {
                "gzip" | "x-gzip" => return allowed,
                "*" => wildcard = Some(allowed),
                _ => {}
            }
        }
    }
    wildcard.unwrap_or(false)
}

/// Whether `If-None-Match` names `etag` (weak comparison, as for GET) or is `*`.
#[cfg(feature = "dashboard-server")]
fn if_none_match(request: &HeaderMap, etag: &str) -> bool {
    request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|tag| tag.trim())
        .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
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

#[cfg(all(test, feature = "dashboard-server"))]
mod dashboard_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    async fn response_text(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        String::from_utf8(body.to_vec()).expect("UTF-8 response")
    }

    fn websocket_request(uri: &str, host: &str, origin: Option<&str>) -> Request<Body> {
        let request = Request::builder()
            .uri(uri)
            .header(header::HOST, host)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==");
        let request = match origin {
            Some(origin) => request.header(header::ORIGIN, origin),
            None => request,
        };
        request.body(Body::empty()).expect("WebSocket request")
    }

    #[tokio::test]
    async fn dashboard_serves_embedded_assets_and_acp_only() {
        let removed_ui_ws_path = "/ui/ws";
        let fixture = crate::test_utils::TestAgent::new().await;
        let server = AgentServer::new(fixture.handle.clone(), fixture.storage.clone());
        let dashboard = server
            .build_app(ServerMode::Dashboard)
            .expect("dashboard router");

        let response = dashboard
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("dashboard response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response_text(response)
                .await
                .contains("<title>QueryMT</title>")
        );

        let manifest = dashboard
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/querymt-ui.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("embedded manifest response");
        assert_eq!(manifest.status(), StatusCode::OK);
        let manifest: serde_json::Value =
            serde_json::from_str(&response_text(manifest).await).expect("embedded manifest JSON");
        assert_eq!(manifest["target"], "embedded");
        assert_eq!(manifest["acpWebSocketPath"], "/acp/ws");

        let acp_ws = dashboard
            .clone()
            .oneshot(websocket_request("/acp/ws", "127.0.0.1:3000", None))
            .await
            .expect("ACP websocket route");
        // Direct router calls do not provide hyper's OnUpgrade extension, so a
        // matched WebSocket route rejects the otherwise valid handshake with 426.
        assert_eq!(acp_ws.status(), StatusCode::UPGRADE_REQUIRED);

        let cross_origin_acp_ws = dashboard
            .clone()
            .oneshot(websocket_request(
                "/acp/ws",
                "127.0.0.1:3000",
                Some("http://evil.example"),
            ))
            .await
            .expect("cross-origin dashboard ACP websocket route");
        assert_eq!(cross_origin_acp_ws.status(), StatusCode::FORBIDDEN);

        let legacy_ws = dashboard
            .oneshot(websocket_request(
                removed_ui_ws_path,
                "127.0.0.1:3000",
                None,
            ))
            .await
            .expect("dashboard SPA fallback");
        assert_eq!(legacy_ws.status(), StatusCode::OK);
        assert_eq!(
            legacy_ws.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html"
        );
        assert!(
            response_text(legacy_ws)
                .await
                .contains("<title>QueryMT</title>")
        );

        let api = AgentServer::new(fixture.handle.clone(), fixture.storage.clone())
            .build_app(ServerMode::Api)
            .expect("API router");
        let cross_origin_acp_ws = api
            .clone()
            .oneshot(websocket_request(
                "/acp/ws",
                "127.0.0.1:3000",
                Some("http://evil.example"),
            ))
            .await
            .expect("unrestricted API ACP websocket route");
        assert_eq!(cross_origin_acp_ws.status(), StatusCode::UPGRADE_REQUIRED);

        let legacy_ws = api
            .oneshot(websocket_request(
                removed_ui_ws_path,
                "127.0.0.1:3000",
                None,
            ))
            .await
            .expect("missing legacy websocket route");
        assert_eq!(legacy_ws.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_index_revalidates_with_etag() {
        let fixture = crate::test_utils::TestAgent::new().await;
        let dashboard = AgentServer::new(fixture.handle.clone(), fixture.storage.clone())
            .build_app(ServerMode::Dashboard)
            .expect("dashboard router");

        let first = dashboard
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("dashboard response");
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CACHE_CONTROL], "no-cache");
        let etag = first.headers()[header::ETAG].clone();

        let revalidated = dashboard
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("conditional dashboard response");
        assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(revalidated.headers()[header::ETAG], etag);
        assert!(response_text(revalidated).await.is_empty());
    }

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), value.parse().unwrap());
        }
        map
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    const WASM: &[u8] = b"\0asm\x01\0\0\0 pretend module";

    fn gzipped_wasm() -> EmbeddedAsset {
        EmbeddedAsset {
            bytes: gzip(WASM).into(),
            gzipped: true,
            sha256: [7; 32],
        }
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn gzipped_asset_is_sent_as_is_to_gzip_clients() {
        let stored = gzip(WASM);
        let response = asset_response(
            "app_bg.wasm",
            gzipped_wasm(),
            &headers(&[(header::ACCEPT_ENCODING, "gzip, deflate")]),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::VARY], "accept-encoding");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
        assert_eq!(body_bytes(response).await, stored);
    }

    #[tokio::test]
    async fn gzipped_asset_is_decompressed_for_other_clients() {
        let gzip_etag = asset_response(
            "app_bg.wasm",
            gzipped_wasm(),
            &headers(&[(header::ACCEPT_ENCODING, "gzip")]),
        )
        .headers()[header::ETAG]
            .clone();

        for request in [
            headers(&[]),
            headers(&[(header::ACCEPT_ENCODING, "br, zstd")]),
            headers(&[(header::ACCEPT_ENCODING, "gzip;q=0, *")]),
        ] {
            let response = asset_response("app_bg.wasm", gzipped_wasm(), &request);
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/wasm");
            assert_eq!(response.headers()[header::VARY], "accept-encoding");
            assert_ne!(response.headers()[header::ETAG], gzip_etag);
            assert_eq!(body_bytes(response).await, WASM);
        }
    }

    #[tokio::test]
    async fn matching_if_none_match_gets_not_modified() {
        let accept = (header::ACCEPT_ENCODING, "gzip");
        let etag = asset_response(
            "app_bg.wasm",
            gzipped_wasm(),
            &headers(std::slice::from_ref(&accept)),
        )
        .headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();

        for tags in [
            etag.clone(),
            format!("\"other\", {etag}"),
            format!("W/{etag}"),
            "*".to_owned(),
        ] {
            let response = asset_response(
                "app_bg.wasm",
                gzipped_wasm(),
                &headers(&[accept.clone(), (header::IF_NONE_MATCH, &tags)]),
            );
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{tags}");
            assert_eq!(response.headers()[header::ETAG], etag.as_str());
            assert!(body_bytes(response).await.is_empty());
        }

        let stale = asset_response(
            "app_bg.wasm",
            gzipped_wasm(),
            &headers(&[accept, (header::IF_NONE_MATCH, "\"older-build\"")]),
        );
        assert_eq!(stale.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn plain_asset_has_an_etag_but_no_encoding() {
        let asset = EmbeddedAsset {
            bytes: b"{}".as_slice().into(),
            gzipped: false,
            sha256: [1; 32],
        };
        let response = asset_response(
            "querymt-ui.json",
            asset,
            &headers(&[(header::ACCEPT_ENCODING, "gzip")]),
        );
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response.headers().get(header::VARY).is_none());
        assert!(response.headers().contains_key(header::ETAG));
        assert_eq!(body_bytes(response).await, b"{}");
    }

    #[test]
    fn accept_encoding_parsing() {
        let cases = [
            ("gzip, deflate", true),
            ("gzip, deflate, br, zstd", true),
            ("GZIP", true),
            ("x-gzip", true),
            ("br;q=1.0, gzip;q=0.8", true),
            ("*", true),
            ("br, *;q=0.1", true),
            ("gzip;q=0", false),
            ("gzip;q=0.0, *", false),
            ("*;q=0", false),
            ("br, zstd", false),
            ("identity", false),
            ("", false),
        ];
        for (value, expected) in cases {
            assert_eq!(
                accepts_gzip(&headers(&[(header::ACCEPT_ENCODING, value)])),
                expected,
                "{value:?}"
            );
        }
        assert!(!accepts_gzip(&HeaderMap::new()));
    }
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use super::*;
    use crate::api::{AgentInfra, ProfileRuntimeHandle};
    use crate::profiles::{LocalProfileCatalog, ProfileRuntimeManager};
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::empty_plugin_registry;

    #[tokio::test]
    async fn with_profiles_bridges_existing_root_mesh_to_profile_manager() {
        let (registry, _registry_dir) = empty_plugin_registry().expect("empty registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("in-memory storage"),
        );
        let root = crate::api::Agent::builder()
            .provider("test", "test-model")
            .infra(AgentInfra {
                plugin_registry: Arc::new(registry),
                storage: Some(storage.clone()),
                session_mcp_attachment_source: None,
                event_fanout: None,
            })
            .build()
            .await
            .expect("root agent builds");
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh()
            .await
            .clone();
        root.handle().set_mesh(mesh);

        let dir = tempfile::Builder::new()
            .prefix("querymt-server-profiles-")
            .tempdir()
            .expect("temp profile dir");
        std::fs::write(
            dir.path().join("alpha.toml"),
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        )
        .expect("write profile");
        let catalog = LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build();
        let profiles: ProfileRuntimeHandle = Arc::new(ProfileRuntimeManager::with_infra_boxed(
            Arc::new(catalog),
            "alpha",
            AgentInfra {
                plugin_registry: root.handle().config.provider.plugin_registry(),
                storage: Some(storage.clone()),
                session_mcp_attachment_source: None,
                event_fanout: None,
            },
        ));

        let server = root.server().with_profiles(profiles);
        let profiles = server.profiles().expect("profiles attached");
        let runtime = profiles
            .runtime_for_profile("alpha")
            .await
            .expect("runtime materializes");
        assert!(
            runtime.handle().mesh().is_some(),
            "profile manager should synchronously store root mesh for future runtimes"
        );
        profiles.shutdown().await;
    }
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
