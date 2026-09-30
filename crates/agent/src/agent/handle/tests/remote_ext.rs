use super::*;

#[cfg(feature = "remote")]
use crate::agent::remote::RemoteNodeManager;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
#[cfg(feature = "remote")]
use kameo::actor::Spawn;

#[cfg(feature = "remote")]
async fn register_remote_node(
    mesh: &crate::agent::remote::mesh::MeshHandle,
    remote: &RealStorageHandleFixture,
    node_name: &str,
    use_mesh_peer: bool,
) -> (String, kameo::actor::ActorRef<RemoteNodeManager>) {
    let peer_id = if use_mesh_peer {
        *mesh.peer_id()
    } else {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    };
    let node_id = peer_id.to_string();
    let node_manager = RemoteNodeManager::new(
        remote.handle.config.clone(),
        remote.handle.registry.clone(),
        Some(mesh.clone()),
        remote.handle.scheduler_handle.clone(),
    )
    .with_profiles_slot(remote.handle.profiles.clone())
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
    let (node_id, node_manager_ref) =
        register_remote_node(mesh, &remote, "peer-remote", false).await;

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
async fn test_remote_profile_from_owner_reaches_list_attach_and_bookmarked_load() {
    use crate::profiles::{
        LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager, SessionProfileBinding,
    };

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());
    let remote = RealStorageHandleFixture::new().await;
    let (node_id, node_manager) = register_remote_node(mesh, &remote, "owner", true).await;
    let created = node_manager
        .ask(crate::agent::remote::CreateRemoteSession { cwd: None })
        .await
        .expect("create owner session");
    let owner_ref = remote
        .handle
        .registry
        .lock()
        .await
        .get(&created.session_id)
        .cloned()
        .expect("owner session actor");
    owner_ref
        .set_mode(AgentMode::Plan)
        .await
        .expect("set owner mode");

    let catalog: Arc<dyn ProfileCatalog> = Arc::new(LocalProfileCatalog::builder().build());
    let profile = catalog
        .list_profiles()
        .await
        .expect("owner profiles")
        .remove(0);
    let (plugin_registry, _plugins_dir) = empty_plugin_registry().expect("plugins");
    let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog,
        profile.id.clone(),
        AgentInfra {
            plugin_registry: Arc::new(plugin_registry),
            storage: Some(remote.storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    ));
    // The node manager was spawned before the owner installed its profile runtime.
    remote.handle.set_profiles(profiles.clone());
    profiles
        .set_session_binding(
            &created.session_id,
            SessionProfileBinding {
                profile_id: profile.id.clone(),
                agent_id: None,
                profile_fingerprint: None,
                profile_source: None,
                profile_config_kind: None,
                provider_lock_digest: None,
                provider_locks_json: None,
            },
        )
        .await;

    let listed = ext_method_json(
        &f.handle,
        "querymt/remote/sessions",
        serde_json::json!({ "node_id": node_id }),
    )
    .await;
    assert_eq!(listed["sessions"][0]["profile_id"], profile.id);
    assert_eq!(listed["sessions"][0]["profile_label"], profile.name);

    let attached = ext_method_json(
        &f.handle,
        "querymt/remote/attachSession",
        serde_json::json!({
            "node_id": node_id, "session_id": created.session_id,
        }),
    )
    .await;
    assert_eq!(attached["profile_id"], profile.id);
    assert_eq!(attached["profile_label"], profile.name);
    assert_eq!(attached["config_options"].as_array().map(Vec::len), Some(1));
    assert_eq!(attached["config_options"][0]["id"], "mode");
    assert_eq!(attached["config_options"][0]["currentValue"], "plan");

    let response = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(created.session_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("bookmarked remote load");
    let json = serde_json::to_value(response).expect("serialize load response");
    assert_eq!(json["_meta"]["profileId"], profile.id);
    assert_eq!(json["_meta"]["profileLabel"], profile.name);
    assert_eq!(json["configOptions"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["configOptions"][0]["id"], "mode");
    assert_eq!(json["configOptions"][0]["currentValue"], "plan");

    let changed = f
        .handle
        .set_session_config_option(crate::acp::protocol::SetSessionConfigOptionRequest::new(
            created.session_id.clone(),
            "mode",
            "review",
        ))
        .await
        .expect("change remote mode through ACP config option");
    assert_eq!(
        config_option_value(&changed.config_options, "mode").as_deref(),
        Some("review")
    );
    assert_eq!(changed.config_options.len(), 1);
    assert_eq!(
        owner_ref.get_mode().await.expect("owner mode"),
        AgentMode::Review
    );

    let reloaded = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(created.session_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("reload remote session");
    assert_eq!(
        config_option_value(
            reloaded
                .config_options
                .as_deref()
                .expect("remote config options"),
            "mode"
        )
        .as_deref(),
        Some("review")
    );
    let reattached = ext_method_json(
        &f.handle,
        "querymt/remote/attachSession",
        serde_json::json!({ "node_id": node_id, "session_id": created.session_id }),
    )
    .await;
    assert_eq!(reattached["config_options"][0]["currentValue"], "review");
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_create_session_without_attach_returns_structured_result() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let (node_id, _node_manager_ref) =
        register_remote_node(mesh, &remote, "peer-create", false).await;

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
