use super::*;

#[test]
fn test_should_return_without_force_stop_only_for_idle() {
    assert!(LocalAgentHandle::should_return_without_force_stop(
        crate::agent::messages::SessionRuntimeStatus::Idle
    ));
    assert!(!LocalAgentHandle::should_return_without_force_stop(
        crate::agent::messages::SessionRuntimeStatus::Running
    ));
    assert!(!LocalAgentHandle::should_return_without_force_stop(
        crate::agent::messages::SessionRuntimeStatus::CancelRequested
    ));
}

#[tokio::test]
async fn test_from_config_creates_empty_registry() {
    let f = HandleFixture::new().await;
    let registry = f.handle.registry.lock().await;
    assert!(registry.is_empty());
}

#[tokio::test]
async fn test_from_config_does_not_trigger_background_model_refresh() {
    let f = HandleFixture::new().await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (_, meta) = f.handle.model_inventory.get_snapshot().await;
    assert!(
        meta.local_updated_at.is_none(),
        "constructing a handle should not eagerly refresh models"
    );
    assert!(
        !meta.refresh_in_progress,
        "constructing a handle should not start a background refresh"
    );
}

#[tokio::test]
async fn test_initialize_returns_latest_protocol() {
    let f = HandleFixture::new().await;
    let req = InitializeRequest::new(ProtocolVersion::LATEST);
    let resp = f.handle.initialize(req).await.expect("initialize");
    assert!(resp.protocol_version <= ProtocolVersion::LATEST);
}

#[tokio::test]
async fn test_initialize_downgrades_newer_client_protocol() {
    let f = HandleFixture::new().await;
    // Simulate a client claiming a future protocol version by using LATEST
    // (we can't construct a truly higher version, but LATEST is still valid)
    let req = InitializeRequest::new(ProtocolVersion::LATEST);
    let resp = f.handle.initialize(req).await.expect("initialize");
    // Server caps at LATEST
    assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
}

#[tokio::test]
async fn test_initialize_advertises_session_capabilities() {
    let f = HandleFixture::new().await;
    let req = InitializeRequest::new(ProtocolVersion::LATEST);
    let resp = f.handle.initialize(req).await.expect("initialize");

    assert!(resp.agent_capabilities.load_session);
    assert!(resp.agent_capabilities.session_capabilities.list.is_some());
    assert!(resp.agent_capabilities.session_capabilities.fork.is_some());
    assert!(
        resp.agent_capabilities
            .session_capabilities
            .resume
            .is_some()
    );
    assert!(resp.agent_capabilities.session_capabilities.close.is_some());
    assert!(
        resp.agent_capabilities
            .session_capabilities
            .delete
            .is_some()
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_cancel_known_remote_session_routes_cancel_to_session_ref() {
    use crate::agent::core::SessionRuntime;
    use crate::agent::remote::scope::{MeshScopeId, scoped_session};

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = HandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let session_id = "remote-cancel-known".to_string();
    let actor = SessionActor::new(
        f.handle.config.clone(),
        session_id.clone(),
        SessionRuntime::new(
            None,
            std::collections::HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    )
    .with_mesh(Some(mesh.clone()));
    let local_ref = SessionActor::spawn(actor);
    let dht_name = scoped_session(&MeshScopeId::lan_default(), &session_id);
    mesh.register_actor(local_ref, dht_name.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let remote_ref = mesh
        .lookup_actor::<SessionActor>(&dht_name)
        .await
        .expect("DHT lookup should succeed")
        .expect("remote actor should be available");

    f.handle
        .attach_remote_session(
            session_id.clone(),
            remote_ref,
            "remote-peer".to_string(),
            None,
            None,
        )
        .await;

    let mut rx = f.handle.subscribe_events();
    let notif = CancelNotification::new(SessionId::from(session_id.clone()));
    SendAgent::cancel(&f.handle, notif)
        .await
        .expect("cancel should succeed");

    let event = tokio::time::timeout(tokio::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("should receive event in time")
        .expect("event channel should remain open");

    assert_eq!(event.session_id(), session_id);
    assert!(matches!(
        event.kind(),
        crate::events::AgentEventKind::Cancelled
    ));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_list_remote_nodes_prefers_per_peer_lookup() {
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
    use kameo::actor::Spawn;

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = HandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote_cfg = HandleFixture::new().await;
    let peer_id = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let node_manager = RemoteNodeManager::new(
        remote_cfg.handle.config.clone(),
        remote_cfg.handle.registry.clone(),
        Some(mesh.clone()),
        remote_cfg.handle.scheduler_handle.clone(),
    )
    .with_node_name("peer-alpha".to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref, per_peer_name).await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let nodes = crate::control::remote::list_remote_nodes(&f.handle).await;
    let labels: HashSet<_> = nodes.into_iter().map(|node| node.label).collect();

    assert!(
        labels.contains("peer-alpha"),
        "per-peer DHT registrations should be visible in list_remote_nodes"
    );
}

#[tokio::test]
async fn test_list_sessions_empty() {
    let f = HandleFixture::new().await;
    let req = ListSessionsRequest::new();
    let resp = f.handle.list_sessions(req).await.expect("list_sessions");
    assert!(resp.sessions.is_empty());
}

#[tokio::test]
async fn test_list_sessions_filters_by_cwd() {
    let cwd_a = std::env::temp_dir().join("querymt-list-sessions-a");
    let cwd_b = std::env::temp_dir().join("querymt-list-sessions-b");

    let mut session_a = mock_session("session-a");
    session_a.cwd = Some(cwd_a.clone());
    let mut session_b = mock_session("session-b");
    session_b.cwd = Some(cwd_b.clone());

    let f = HandleFixture::with_list_sessions(vec![session_a, session_b]).await;

    let resp = f
        .handle
        .list_sessions(ListSessionsRequest::new().cwd(cwd_a.clone()))
        .await
        .expect("list_sessions filtered by cwd");

    assert_eq!(resp.sessions.len(), 1);
    assert_eq!(resp.sessions[0].cwd, cwd_a);
}

#[tokio::test]
async fn test_cancel_unknown_session_is_noop() {
    let f = HandleFixture::new().await;
    let notif = CancelNotification::new(SessionId::from("no-such-session".to_string()));
    // Should not return an error — stop for unknown sessions is a no-op
    let result = SendAgent::cancel(&f.handle, notif).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_close_unknown_session_is_noop() {
    let f = HandleFixture::new().await;
    let req = CloseSessionRequest::new(SessionId::from("no-such-session".to_string()));
    let result = SendAgent::close_session(&f.handle, req).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_delete_unknown_session_is_noop() {
    let f = HandleFixture::new().await;
    let req = DeleteSessionRequest::new(SessionId::from("no-such-session".to_string()));
    let result = SendAgent::delete_session(&f.handle, req).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_prompt_unknown_session_returns_error() {
    let f = HandleFixture::new().await;
    let req = agent_client_protocol::schema::PromptRequest::new(
        SessionId::from("no-such-session".to_string()),
        vec![],
    );
    let result = SendAgent::prompt(&f.handle, req).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_profile_config_option_absent_without_profiles() {
    let f = HandleFixture::new().await;

    let options = profile_config_options(&f, None).await;

    assert!(
        !options
            .iter()
            .any(|option| option.id.0.as_ref() == "profile")
    );
    assert_eq!(options[0].id.0.as_ref(), "mode");
}

#[tokio::test]
async fn test_profile_config_option_includes_configured_profiles() {
    let profile_dir = tempfile::tempdir().expect("profile dir");
    write_profile(
        profile_dir.path(),
        "alpha.toml",
        r#"
[agent]
provider = "test"
model = "test-model"
system = "alpha"
"#,
    );
    write_profile(
        profile_dir.path(),
        "beta.toml",
        r#"
[profile]
name = "Beta"
description = "Beta profile"

[agent]
provider = "test"
model = "test-model"
system = "beta"
"#,
    );
    let f = HandleFixture::new()
        .await
        .with_profiles("alpha", profile_dir.path())
        .await;
    register_test_session(&f, "session-1").await;
    bind_test_profile(&f, "session-1", "alpha").await;

    let options = profile_config_options(&f, Some("session-1")).await;

    assert_eq!(options[0].id.0.as_ref(), "profile");
    assert_eq!(select_option_values(&options[0]), vec!["alpha", "beta"]);
    assert_eq!(options[1].id.0.as_ref(), "mode");
}

#[tokio::test]
async fn test_profile_config_option_omits_unbound_sessions() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    register_test_session(&f, "session-1").await;

    let options = profile_config_options(&f, Some("session-1")).await;

    assert!(
        !options
            .iter()
            .any(|option| option.id.0.as_ref() == "profile")
    );
    assert_eq!(options[0].id.0.as_ref(), "mode");
}

#[test]
fn test_config_options_with_profiles_uses_profile_metadata() {
    let profiles = vec![
        test_profile_metadata("alpha", "Alpha", None),
        test_profile_metadata("beta", "Beta", Some("Beta profile")),
    ];

    let options = crate::agent::session_registry::config_options_with_profiles(
        AgentMode::Build,
        None,
        Some("beta"),
        &profiles,
    );

    assert_eq!(options[0].id.0.as_ref(), "profile");
    let select = match &options[0].kind {
        agent_client_protocol::schema::SessionConfigKind::Select(select) => select,
        _ => panic!("expected select config option"),
    };
    assert_eq!(select.current_value.0.as_ref(), "beta");
    let profile_options = select_options(&options[0]);
    assert_eq!(profile_options[1].name, "Beta");
    assert_eq!(
        profile_options[1].description.as_deref(),
        Some("Beta profile")
    );
}

#[tokio::test]
async fn test_set_profile_config_option_rejects_unknown_profile() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    register_test_session(&f, "session-1").await;
    bind_test_profile(&f, "session-1", "alpha").await;

    let req = agent_client_protocol::schema::SetSessionConfigOptionRequest::new(
        "session-1",
        "profile",
        "missing",
    );

    let err = f
        .handle
        .set_session_config_option(req)
        .await
        .expect_err("unknown profile should fail");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn test_set_profile_config_option_rejects_different_bound_profile() {
    let (f, _profile_dir) = profile_fixture_with_files(&[
        ("alpha.toml", ALPHA_PROFILE_TOML),
        ("beta.toml", BETA_PROFILE_TOML),
    ])
    .await;
    register_test_session(&f, "session-1").await;
    bind_test_profile(&f, "session-1", "alpha").await;

    let req = agent_client_protocol::schema::SetSessionConfigOptionRequest::new(
        "session-1",
        "profile",
        "beta",
    );

    let err = f
        .handle
        .set_session_config_option(req)
        .await
        .expect_err("different profile should fail");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn test_set_profile_config_option_accepts_same_bound_profile() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    register_test_session(&f, "session-1").await;
    bind_test_profile(&f, "session-1", "alpha").await;
    let req = agent_client_protocol::schema::SetSessionConfigOptionRequest::new(
        "session-1",
        "profile",
        "alpha",
    );

    let resp = f
        .handle
        .set_session_config_option(req)
        .await
        .expect("same profile should succeed");

    assert_eq!(resp.config_options[0].id.0.as_ref(), "profile");
    assert!(
        resp.config_options
            .iter()
            .any(|option| option.id.0.as_ref() == "mode")
    );
    assert!(
        resp.config_options
            .iter()
            .any(|option| option.id.0.as_ref() == "reasoning_effort")
    );
}
