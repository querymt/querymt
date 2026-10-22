#![cfg(feature = "remote")]

use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::LocalAgentHandle;
use querymt_agent::acp::protocol::ExtRequest;
use querymt_agent::agent::agent_config_builder::AgentConfigBuilder;
use querymt_agent::send_agent::SendAgent;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_remote::{LanDiscovery, LanMeshConfig, MeshRuntimeConfig, bootstrap_mesh_handle};
use std::sync::Arc;
use std::time::Duration;

// A separate test binary keeps LAN-only bootstrap independent of the unit tests'
// shared LAN+Iroh fixture: Kameo permits only one global swarm per process.
#[tokio::test]
async fn lan_mesh_reports_lan_transport_and_rejects_invites() {
    let dir = tempfile::tempdir().expect("test directory");
    let providers = dir.path().join("providers.toml");
    std::fs::write(&providers, "providers = []\n").expect("empty provider config");
    let registry = Arc::new(PluginRegistry::from_path(&providers).expect("registry"));
    let storage = Arc::new(
        SqliteStorage::connect(":memory:".into())
            .await
            .expect("test storage"),
    );
    let config = Arc::new(
        AgentConfigBuilder::new(
            registry,
            storage,
            LLMParams::new().provider("mock").model("mock"),
        )
        .build(),
    );
    let handle = LocalAgentHandle::from_config(config);
    let mesh = bootstrap_mesh_handle(&MeshRuntimeConfig {
        enabled: true,
        lan: Some(LanMeshConfig {
            listen: Some("/ip4/127.0.0.1/tcp/0".into()),
            discovery: LanDiscovery::None,
            directory: Default::default(),
        }),
        iroh_enabled: false,
        iroh_gso: true,
        iroh_dns: None,
        iroh_scopes: Vec::new(),
        identity_file: Some(dir.path().join("identity.key")),
        request_timeout: Duration::from_secs(30),
        stream_reconnect_grace: Duration::from_secs(30),
        node_name: None,
        peers: Vec::new(),
        auto_fallback: false,
    })
    .await
    .expect("LAN mesh");
    handle.set_mesh(mesh.clone());

    let response = handle
        .ext_method(ExtRequest::new(
            "querymt/capabilities",
            serde_json::value::to_raw_value(&serde_json::json!({}))
                .unwrap()
                .into(),
        ))
        .await
        .expect("capabilities response");
    let result: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    assert_eq!(result["transport"]["mesh_transport"], "lan");
    assert_eq!(result["features"]["mesh"], true);
    assert_eq!(result["features"]["mesh_invites"], false);

    let response = handle
        .ext_method(ExtRequest::new(
            "querymt/mesh/status",
            serde_json::value::to_raw_value(&serde_json::json!({}))
                .unwrap()
                .into(),
        ))
        .await
        .expect("mesh status");
    let status: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["peer_id"], mesh.peer_id().to_string());
    assert_eq!(status["has_invite_store"], false);
    assert_eq!(status["has_mesh_state_store"], false);
    assert!(
        status["iroh"].is_null(),
        "LAN-only runtime has no iroh endpoint"
    );

    let err = handle
        .ext_method(ExtRequest::new(
            "querymt/mesh/createInvite",
            serde_json::value::to_raw_value(&serde_json::json!({
                "mesh_name": "test-mesh", "max_uses": 2, "ttl": "1h"
            }))
            .unwrap()
            .into(),
        ))
        .await
        .expect_err("LAN-only mesh must reject invite creation");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidRequest);
    assert!(
        err.data.expect("structured error data")["error"]
            .as_str()
            .expect("error string")
            .contains("iroh transport")
    );
    mesh.shutdown().await;
}
