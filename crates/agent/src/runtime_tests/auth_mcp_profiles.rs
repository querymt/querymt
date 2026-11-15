#![cfg(feature = "oauth")]
use crate::auth::service::mcp::tests::{
    MemoryBackend, redirect_query, serve_auth_server, status_of,
};
use crate::auth::service::*;
use crate::model_inventory::ModelInventory;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Service whose agent configuration declares no MCP server; `profiles`
/// (file name, TOML) make up the profile catalog.
async fn profile_service(
    backend: &Arc<MemoryBackend>,
    profiles: &[(&str, String)],
) -> (OAuthService, tempfile::TempDir) {
    use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};
    let dir = tempfile::tempdir().unwrap();
    for (file, toml) in profiles {
        std::fs::write(dir.path().join(file), toml).unwrap();
    }
    let fixture = crate::test_utils::TestAgent::new().await;
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build(),
    );
    let manager = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog,
        "alpha",
        crate::api::AgentInfra {
            plugin_registry: fixture.config.provider.plugin_registry(),
            storage: None,
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    ));
    let slot: crate::agent::handle::ProfileRuntimeSlot =
        Arc::new(arc_swap::ArcSwap::from_pointee(Some(manager)));
    let svc = OAuthService::new(
        fixture.config.clone(),
        ModelInventory::new(fixture.config.clone()),
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(None)),
    )
    .with_mcp_secret_backend(backend.clone())
    .with_profile_slot(slot);
    (svc, dir)
}

fn profile_toml(mcp: &str) -> String {
    format!("[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"p\"\n{mcp}")
}

fn oauth_entry(name: &str, url: &str, scopes: &str) -> String {
    format!(
        "[[mcp]]\nname = \"{name}\"\ntransport = \"http\"\nurl = \"{url}\"\nauth = {{ type = \"oauth\", scopes = [{scopes}] }}\n"
    )
}

#[tokio::test]
async fn profile_only_oauth_server_is_listed_and_startable() {
    let auth = serve_auth_server().await;
    let backend = Arc::new(MemoryBackend::default());
    let url = format!("{}/mcp", auth.base);
    let (svc, _dir) = profile_service(
        &backend,
        &[(
            "alpha.toml",
            profile_toml(&oauth_entry("ibkr_read", &url, "\"mcp.read\"")),
        )],
    )
    .await;

    let statuses = svc.mcp_auth_status(None).await;
    assert_eq!(
        status_of(&statuses, "mcp:ibkr_read"),
        Some(OAuthStatus::NotAuthenticated)
    );
    let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
    assert_eq!(started.provider, "mcp:ibkr_read");
    assert!(started.authorization_url.contains("scope=mcp.read"));
}

#[tokio::test]
async fn identical_declarations_across_profiles_are_merged() {
    let backend = Arc::new(MemoryBackend::default());
    let entry = oauth_entry("shared", "https://mcp.example/mcp", "\"read\"");
    let (svc, _dir) = profile_service(
        &backend,
        &[
            ("alpha.toml", profile_toml(&entry)),
            ("beta.toml", profile_toml(&entry)),
        ],
    )
    .await;
    let servers = svc.oauth_servers().await;
    assert_eq!(servers.len(), 1);
    assert!(!servers[0].conflicting);
    assert_eq!(servers[0].sources, ["profile alpha", "profile beta"]);
    assert!(svc.mcp_server("shared").await.is_ok());
}

#[tokio::test]
async fn conflicting_declarations_are_rejected() {
    let backend = Arc::new(MemoryBackend::default());
    let (svc, _dir) = profile_service(
        &backend,
        &[
            (
                "alpha.toml",
                profile_toml(&oauth_entry(
                    "ibkr",
                    "https://api.example/mcp",
                    "\"mcp.read\"",
                )),
            ),
            (
                "beta.toml",
                profile_toml(&oauth_entry(
                    "ibkr",
                    "https://api.example/mcp",
                    "\"mcp.read\", \"mcp.orders.submit\"",
                )),
            ),
        ],
    )
    .await;
    let err = svc.start_flow("acp", "mcp:ibkr").await.unwrap_err();
    let AuthFlowError::ConflictingMcpServer { server, sources } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(server, "ibkr");
    assert_eq!(sources, &["profile alpha", "profile beta"]);
    let acp: agent_client_protocol::Error = err.into();
    assert_eq!(
        acp.code,
        agent_client_protocol::Error::invalid_params().code
    );
    let out = svc.logout("acp", "mcp:ibkr").await;
    assert!(!out.success && out.message.contains("defined differently"));
}

#[tokio::test]
async fn dotagents_oauth_server_of_a_profile_is_listed() {
    let backend = Arc::new(MemoryBackend::default());
    let workspace = tempfile::tempdir().unwrap();
    let agents = workspace.path().join(".agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("mcp.json"),
        r#"{"mcpServers":{"corp":{"url":"https://corp.example/mcp",
            "auth":{"type":"oauth","scopes":["read"]}}}}"#,
    )
    .unwrap();
    let toml = format!(
        "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"p\"\n\
         [dotagents]\nenabled = true\nglobal_enabled = false\nworkspace = \"{}\"\n",
        workspace.path().display()
    );
    let (svc, _dir) = profile_service(&backend, &[("alpha.toml", toml)]).await;
    let servers = svc.oauth_servers().await;
    let corp = servers
        .iter()
        .find(|d| d.server.name == "corp")
        .expect("dotagents server listed");
    assert_eq!(corp.sources, ["profile alpha (.agents)"]);
    assert_eq!(corp.server.scopes(), ["read"]);
}

#[tokio::test]
async fn strict_scopes_refuse_a_wider_grant() {
    let auth = serve_auth_server().await;
    *auth.token_scope.lock() = Some("mcp.read mcp.orders.submit".into());
    let backend = Arc::new(MemoryBackend::default());
    let url = format!("{}/mcp", auth.base);
    let entry = format!(
        "[[mcp]]\nname = \"ibkr_read\"\ntransport = \"http\"\nurl = \"{url}\"\n\
         auth = {{ type = \"oauth\", scopes = [\"mcp.read\"], strict_scopes = true }}\n"
    );
    let (svc, _dir) = profile_service(&backend, &[("alpha.toml", profile_toml(&entry))]).await;

    let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
    let redirect = format!(
        "http://127.0.0.1:1/callback?{}",
        redirect_query(&auth, &started.authorization_url)
    );
    let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
    assert!(!done.success, "{done:?}");
    assert!(done.message.contains("mcp.orders.submit"), "{done:?}");
    assert!(backend.0.lock().is_empty(), "nothing persisted");

    // A grant within the configured scopes is accepted.
    *auth.token_scope.lock() = Some("mcp.read".into());
    let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
    let redirect = format!(
        "http://127.0.0.1:1/callback?{}",
        redirect_query(&auth, &started.authorization_url)
    );
    let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
    assert!(done.success, "{done:?}");
}
