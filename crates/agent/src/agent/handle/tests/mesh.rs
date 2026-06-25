use super::*;

#[cfg(feature = "remote")]
use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

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
#[tokio::test]
async fn test_querymt_mesh_status_without_mesh_is_disabled() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(&f.handle, "querymt/mesh/status", serde_json::json!({})).await;

    assert_eq!(result["enabled"], false);
    assert_eq!(result["known_peer_count"], 0);
    assert_eq!(result["has_invite_store"], false);
    assert_eq!(result["has_mesh_state_store"], false);
    assert_eq!(result["scopes"], serde_json::json!([]));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_nodes_returns_shared_shape() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(&f.handle, "querymt/mesh/nodes", serde_json::json!({})).await;

    assert!(result["nodes"].is_array());
    if let Some(first) = result["nodes"].as_array().and_then(|nodes| nodes.first()) {
        assert!(first["transport"].is_string());
        assert!(first.get("last_seen_at").is_some());
    }
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_status_with_test_mesh_reports_runtime_details() {
    let f = HandleFixture::new().await;
    let mesh = get_test_mesh().await.clone();
    f.handle.set_mesh(mesh.clone());

    let result = ext_method_json(&f.handle, "querymt/mesh/status", serde_json::json!({})).await;

    assert_eq!(result["enabled"], true);
    assert_eq!(result["peer_id"], mesh.peer_id().to_string());
    assert_eq!(result["has_invite_store"], true);
    assert_eq!(result["has_mesh_state_store"], true);
    assert!(result["scopes"].is_array());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_list_invites_without_mesh_returns_empty_list() {
    let f = HandleFixture::new().await;
    let result =
        ext_method_json(&f.handle, "querymt/mesh/listInvites", serde_json::json!({})).await;

    assert_eq!(result["invites"], serde_json::json!([]));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_list_invites_with_test_mesh_returns_shared_shape() {
    let f = HandleFixture::new().await;
    let mesh = get_test_mesh().await.clone();
    f.handle.set_mesh(mesh);

    let listed =
        ext_method_json(&f.handle, "querymt/mesh/listInvites", serde_json::json!({})).await;
    assert!(listed["invites"].is_array());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_create_invite_with_lan_mesh_returns_clear_error() {
    let f = HandleFixture::new().await;
    let mesh = get_test_mesh().await.clone();
    f.handle.set_mesh(mesh);

    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/mesh/createInvite",
        std::sync::Arc::from(
            serde_json::value::RawValue::from_string(
                serde_json::json!({ "mesh_name": "test-mesh", "max_uses": 2, "ttl": "1h" })
                    .to_string(),
            )
            .unwrap(),
        ),
    );
    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("lan-only mesh should reject invite creation");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidRequest);
    let data = err.data.expect("structured error data");
    assert!(
        data["error"]
            .as_str()
            .expect("error string")
            .contains("iroh transport")
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_revoke_invite_without_mesh_returns_structured_result() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(
        &f.handle,
        "querymt/mesh/revokeInvite",
        serde_json::json!({ "invite_id": "missing-invite" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert_eq!(result["invite_id"], "missing-invite");
    assert!(result["message"].is_string());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_mesh_revoke_invite_with_test_mesh_without_store_entry_is_structured() {
    let f = HandleFixture::new().await;
    let mesh = get_test_mesh().await.clone();
    f.handle.set_mesh(mesh);

    let revoked = ext_method_json(
        &f.handle,
        "querymt/mesh/revokeInvite",
        serde_json::json!({ "invite_id": "missing-live-invite" }),
    )
    .await;

    assert_eq!(revoked["success"], false);
    assert_eq!(revoked["invite_id"], "missing-live-invite");
    assert!(revoked["message"].is_string());
}
