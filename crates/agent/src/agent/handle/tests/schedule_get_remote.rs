use super::*;

#[cfg(feature = "remote")]
use crate::agent::remote::RemoteNodeManager;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
#[cfg(feature = "remote")]
use kameo::actor::Spawn;

#[cfg(feature = "remote")]
async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = agent_client_protocol::schema::ExtRequest::new(
        method,
        std::sync::Arc::from(serde_json::value::RawValue::from_string(params.to_string()).unwrap()),
    );
    let resp = handle.ext_method(req).await.expect("ext_method");
    serde_json::from_str(resp.0.get()).expect("valid JSON")
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_schedule_get_remote_returns_schedule() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
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
    .with_node_name("peer-alpha".to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let session = node_manager_ref
        .ask(crate::agent::remote::CreateRemoteSession {
            cwd: Some("/tmp/remote-schedule".to_string()),
        })
        .await
        .expect("create remote session");

    let created = ext_method_json(
        &f.handle,
        "querymt/schedules/create",
        serde_json::json!({
            "node_id": node_id,
            "session_id": session.session_id,
            "prompt": "daily summary",
            "trigger": { "type": "interval", "seconds": 300 },
            "max_runs": 2,
        }),
    )
    .await;
    let schedule_id = created["schedule"]["public_id"]
        .as_str()
        .expect("schedule id")
        .to_string();

    let fetched = ext_method_json(
        &f.handle,
        "querymt/schedules/get",
        serde_json::json!({
            "node_id": node_id,
            "schedule_public_id": schedule_id,
        }),
    )
    .await;

    assert_eq!(fetched["schedule"]["node_id"], node_id);
    assert_eq!(fetched["schedule"]["session_public_id"], session.session_id);
    assert!(fetched["schedule"]["public_id"].is_string());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_schedule_get_remote_missing_returns_not_found() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
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
    .with_node_name("peer-alpha".to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let req = agent_client_protocol::schema::ExtRequest::new(
        "querymt/schedules/get",
        std::sync::Arc::from(
            serde_json::value::RawValue::from_string(
                serde_json::json!({
                    "node_id": node_id,
                    "schedule_public_id": "missing-remote-schedule"
                })
                .to_string(),
            )
            .unwrap(),
        ),
    );

    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("missing remote schedule should reject get");

    assert_eq!(err.code, agent_client_protocol::ErrorCode::ResourceNotFound);
    assert!(err.message.contains("missing-remote-schedule"));
}
