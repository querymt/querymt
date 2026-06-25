use super::*;

#[cfg(feature = "remote")]
use crate::agent::remote::RemoteNodeManager;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
#[cfg(feature = "remote")]
use kameo::actor::Spawn;

async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = crate::acp::protocol::ExtRequest::new(
        method,
        std::sync::Arc::from(serde_json::value::RawValue::from_string(params.to_string()).unwrap()),
    );
    let resp = handle.ext_method(req).await.expect("ext_method");
    serde_json::from_str(resp.0.get()).expect("valid JSON")
}

#[cfg(feature = "remote")]
async fn register_remote_node(
    mesh: &crate::agent::remote::mesh::MeshHandle,
    remote: &RealStorageHandleFixture,
    node_name: &str,
) -> (String, kameo::actor::ActorRef<RemoteNodeManager>) {
    let peer_id = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let node_id = peer_id.to_string();
    let node_manager = RemoteNodeManager::new(
        remote.handle.config.clone(),
        remote.handle.registry.clone(),
        Some(mesh.clone()),
        remote.handle.scheduler_handle.clone(),
    )
    .with_node_name(node_name.to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (node_id, node_manager_ref)
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_sessions_returns_shared_shape() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let (node_id, node_manager_ref) = register_remote_node(mesh, &remote, "peer-remote").await;

    node_manager_ref
        .ask(crate::agent::remote::CreateRemoteSession {
            cwd: Some("/tmp/remote-a".to_string()),
        })
        .await
        .expect("create remote session");

    let listed = ext_method_json(
        &f.handle,
        "querymt/remote/sessions",
        serde_json::json!({ "node_id": node_id, "offset": 0, "limit": 20 }),
    )
    .await;

    assert_eq!(listed["node_id"], node_id);
    assert!(listed["sessions"].is_array());
    assert_eq!(listed["total_count"], 1);
    let first = &listed["sessions"][0];
    assert_eq!(first["node_id"], listed["node_id"]);
    assert!(first["id"].is_string());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_create_session_without_attach_returns_structured_result() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let (node_id, _node_manager_ref) = register_remote_node(mesh, &remote, "peer-create").await;

    let created = ext_method_json(
        &f.handle,
        "querymt/remote/createSession",
        serde_json::json!({ "node_id": node_id, "cwd": "/tmp/work", "attach": false }),
    )
    .await;

    assert_eq!(created["node_id"], node_id);
    assert_eq!(created["attached"], false);
    assert!(created["session_id"].is_string());
    assert_eq!(created["config_options"], serde_json::json!([]));
    assert_eq!(created.get("snapshot"), None);
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_dismiss_session_returns_structured_result() {
    let f = RealStorageHandleFixture::new().await;

    let dismissed = ext_method_json(
        &f.handle,
        "querymt/remote/dismissSession",
        serde_json::json!({ "session_id": "remote-session-1" }),
    )
    .await;

    assert_eq!(dismissed["success"], true);
    assert_eq!(dismissed["session_id"], "remote-session-1");
}
