use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent::LocalAgentHandle;
use querymt_agent::acp::protocol::ExtRequest;
use querymt_agent::agent::agent_config_builder::AgentConfigBuilder;
use querymt_agent::agent::remote::bootstrap_mesh_runtime;
use querymt_agent::send_agent::SendAgent;
use querymt_agent::session::sqlite_storage::SqliteStorage;
use querymt_remote::{LanDiscovery, LanMeshConfig, MeshRuntimeConfig};
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
    let mesh = bootstrap_mesh_runtime(&MeshRuntimeConfig {
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
    .expect("guarded LAN mesh");
    let mesh = mesh.as_mesh_handle().clone();
    assert!(mesh.inbound_router().is_some());
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
    assert!(result.get("mesh_access_version").is_none());
    let request = |method, params| {
        ExtRequest::new(
            method,
            serde_json::value::to_raw_value(&params).unwrap().into(),
        )
    };
    let response = handle
        .owner_ext_method(request("querymt/capabilities", serde_json::json!({})))
        .await
        .unwrap();
    let owner_caps: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    assert_eq!(owner_caps["mesh_access_version"], 1);
    let peer = libp2p::PeerId::random().to_string();
    let read_params = serde_json::json!({"version": 1, "peer_id": peer});
    let denied = handle
        .ext_method(request("querymt/mesh/getPeerAccess", read_params.clone()))
        .await
        .unwrap_err();
    assert_eq!(denied.data.unwrap()["code"], "mesh_access_owner_required");
    let response = handle
        .owner_ext_method(request("querymt/mesh/getPeerAccess", read_params.clone()))
        .await
        .unwrap();
    let initial: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    assert_eq!(initial["outgoing"]["grant"]["mode"], "none");
    let mut updates = handle.subscribe_ext_notifications();
    let mut params = serde_json::json!({"version": 2, "peer_id": peer,
        "expected_revision": initial["outgoing"]["revision"], "scope": {"provider_id":"fixture","model_id":"offline"},
        "operation":"reconfigure", "limit": {"kind":"finite", "limit_tokens":"9007199254740993", "window":"weekly"}});
    let invalid = handle
        .owner_ext_method(request("querymt/mesh/setPeerQuota", params.clone()))
        .await
        .unwrap_err();
    assert_eq!(invalid.data.unwrap()["code"], "mesh_access_invalid_params");
    assert!(updates.try_recv().is_err());
    params["version"] = serde_json::json!(1);
    let response = handle
        .owner_ext_method(request("querymt/mesh/setPeerQuota", params.clone()))
        .await
        .unwrap();
    let written: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    let quota = &written["outgoing"]["quotas"][0];
    assert_eq!(quota["limit"]["limit_tokens"], "9007199254740993");
    assert_eq!(quota["usage"]["available_tokens"], "9007199254740993");
    assert_eq!(quota["active"], false);
    let notification = updates
        .try_recv()
        .expect("committed quota change notification");
    assert_eq!(
        notification.method.as_ref(),
        "querymt/mesh/peerAccessChanged"
    );
    let stale = handle
        .owner_ext_method(request("querymt/mesh/setPeerQuota", params))
        .await
        .unwrap_err();
    assert_eq!(stale.data.unwrap()["code"], "mesh_access_conflict");
    assert!(updates.try_recv().is_err());

    // Use the same durable store as inference, so the ACP reset is tested against
    // an actual outstanding reservation, not a simulated ACP error response.
    use querymt_remote::peer_access::quota::{AttemptUsage, QuotaAttempt};
    use querymt_remote::peer_access::{PeerGrantKey, SharingProtocol};
    let grant = serde_json::json!({"mode":"custom", "sessions":true, "filesystem":false,
        "providers":[{"provider_id":"fixture", "models":{"mode":"selected", "model_ids":["offline"]}}]});
    let set_grant = |revision, grant| {
        request(
            "querymt/mesh/setPeerAccess",
            serde_json::json!({
                "version":1, "peer_id":peer, "expected_revision":revision, "grant":grant
            }),
        )
    };
    let revision = written["outgoing"]["revision"].clone();
    let reply = handle
        .owner_ext_method(set_grant(revision, grant.clone()))
        .await
        .unwrap();
    let enabled: serde_json::Value = serde_json::from_str(reply.0.get()).unwrap();
    assert_eq!(enabled["outgoing"]["quotas"][0]["active"], true);
    let change = updates.try_recv().unwrap();
    let change: serde_json::Value = serde_json::from_str(change.params.get()).unwrap();
    assert_eq!(
        change,
        serde_json::json!({"version":1,"peer_id":peer,"direction":"outgoing","reason":"policy"})
    );

    let revision = enabled["outgoing"]["revision"].clone();
    let reply = handle
        .owner_ext_method(set_grant(revision.clone(), grant))
        .await
        .unwrap();
    let unchanged: serde_json::Value = serde_json::from_str(reply.0.get()).unwrap();
    assert_eq!(unchanged["outgoing"]["revision"], revision);
    assert!(updates.try_recv().is_err());
    let reply = handle
        .owner_ext_method(request("querymt/mesh/nodes", serde_json::json!({})))
        .await
        .unwrap();
    let nodes: serde_json::Value = serde_json::from_str(reply.0.get()).unwrap();
    assert_eq!(nodes["managed_peer_ids"], serde_json::json!([peer]));

    let store = mesh.quota_store().unwrap();
    let reserved = store
        .reserve(QuotaAttempt {
            peer: PeerGrantKey {
                host: querymt_remote::NodeId::from_peer_id(*mesh.peer_id()),
                peer: querymt_remote::NodeId::parse(&peer).unwrap(),
            },
            grant_revision: serde_json::from_value(revision.clone()).unwrap(),
            protocol: SharingProtocol::V1,
            provider_id: "fixture".into(),
            model_id: "offline".into(),
            request_id: uuid::Uuid::now_v7(),
            bound_tokens: Some(20),
            now: time::OffsetDateTime::now_utc().unix_timestamp(),
        })
        .await
        .unwrap();
    store.mark_dispatched(reserved).await.unwrap();
    let scope = serde_json::json!({"provider_id":"fixture", "model_id":"offline"});
    let reset = |revision, scope| {
        request(
            "querymt/mesh/resetPeerQuota",
            serde_json::json!({
                "version":1, "peer_id":peer, "expected_revision":revision, "scope":scope
            }),
        )
    };
    let non_owner = handle
        .ext_method(reset(revision.clone(), scope.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        non_owner.data.unwrap()["code"],
        "mesh_access_owner_required"
    );
    let in_flight = handle
        .owner_ext_method(reset(revision.clone(), scope.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        in_flight.data.unwrap(),
        serde_json::json!({"code":"mesh_quota_in_flight", "scope":scope})
    );
    assert!(updates.try_recv().is_err());

    store
        .settle(reserved, AttemptUsage::Observed(12))
        .await
        .unwrap();
    let reply = handle
        .owner_ext_method(reset(revision, scope))
        .await
        .unwrap();
    let reset: serde_json::Value = serde_json::from_str(reply.0.get()).unwrap();
    let reset_revision = &reset["outgoing"]["revision"];
    let quota_request = |operation, limit| {
        request(
            "querymt/mesh/setPeerQuota",
            serde_json::json!({
                "version": 1, "peer_id": peer, "expected_revision": reset_revision,
                "scope": {"provider_id":"fixture", "model_id":"offline"},
                "operation": operation, "limit": limit
            }),
        )
    };
    // A rejected amount or window edit must not replace the accounting period.
    while updates.try_recv().is_ok() {}
    for (amount, window, code) in [
        ("+100", "weekly", "mesh_access_invalid_params"),
        (
            "9223372036854775808",
            "weekly",
            "mesh_access_invalid_params",
        ),
        ("0", "weekly", "mesh_access_invalid_params"),
        ("100", "monthly", "mesh_quota_reconfigure_required"),
    ] {
        let error = handle
            .owner_ext_method(quota_request(
                "update",
                serde_json::json!({
                    "kind":"finite", "limit_tokens":amount, "window":window
                }),
            ))
            .await
            .unwrap_err();
        assert_eq!(error.data.unwrap()["code"], code);
    }
    assert!(updates.try_recv().is_err());
    let response = handle
        .owner_ext_method(quota_request(
            "update",
            serde_json::json!({
                "kind":"finite", "limit_tokens":"9007199254740993", "window":"weekly"
            }),
        ))
        .await
        .unwrap();
    let unchanged: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
    assert_eq!(unchanged["outgoing"], reset["outgoing"]);
    assert!(updates.try_recv().is_err());
    let quota = reset["outgoing"]["quotas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["scope"]["model_id"] == "offline")
        .unwrap();
    assert_eq!(quota["usage"]["known_tokens"], "0");
    assert_eq!(quota["usage"]["reserved_tokens"], "0");
    assert_eq!(quota["usage"]["available_tokens"], "9007199254740993");
    let reply = handle
        .owner_ext_method(set_grant(
            reset["outgoing"]["revision"].clone(),
            serde_json::json!({"mode":"none"}),
        ))
        .await
        .unwrap();
    let revoked: serde_json::Value = serde_json::from_str(reply.0.get()).unwrap();
    assert_eq!(revoked["outgoing"]["grant"]["mode"], "none");
    assert!(
        revoked["outgoing"]["quotas"]
            .as_array()
            .unwrap()
            .iter()
            .all(|q| q["active"] == false)
    );

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
    assert_eq!(err.code, agent_client_protocol::ErrorCode::INVALID_REQUEST);
    assert!(
        err.data.expect("structured error data")["error"]
            .as_str()
            .expect("error string")
            .contains("iroh transport")
    );
    mesh.shutdown().await;
}
