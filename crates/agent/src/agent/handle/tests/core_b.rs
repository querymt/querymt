use super::*;

#[tokio::test]
async fn test_unknown_ext_method_returns_method_not_found() {
    let f = HandleFixture::new().await;
    let null_params =
        std::sync::Arc::from(serde_json::value::RawValue::from_string("null".to_string()).unwrap());
    let req = crate::acp::protocol::ExtRequest::new("my_method", null_params);
    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("unknown ext_method should fail");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
}

#[tokio::test]
async fn test_querymt_models_ext_method_returns_models() {
    let f = HandleFixture::new().await;
    let null_params =
        std::sync::Arc::from(serde_json::value::RawValue::from_string("null".to_string()).unwrap());
    let req = crate::acp::protocol::ExtRequest::new("querymt/models", null_params);
    let resp = f.handle.ext_method(req).await.expect("ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
    assert!(value.get("models").is_some());
}

#[tokio::test]
async fn test_querymt_profiles_ext_method_returns_empty_without_profiles() {
    let f = HandleFixture::new().await;
    let req = crate::acp::protocol::ExtRequest::new("querymt/profiles", raw_params("null"));

    let resp = f.handle.ext_method(req).await.expect("profiles ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");

    assert_eq!(value["profiles"], serde_json::json!([]));
    assert!(value["active_profile_id"].is_null());
}

#[tokio::test]
async fn test_querymt_profiles_ext_method_returns_rich_profile_metadata() {
    let beta = r#"
[profile]
name = "Beta"
description = "Beta profile"
tags = ["fast", "delegate"]

[agent]
provider = "test"
model = "test-model"
system = "beta"
"#;
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML), ("beta.toml", beta)])
            .await;
    let req = crate::acp::protocol::ExtRequest::new("querymt/profiles", raw_params("{}"));

    let resp = f.handle.ext_method(req).await.expect("profiles ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
    let profiles = value["profiles"].as_array().expect("profiles array");
    let beta = profiles
        .iter()
        .find(|profile| profile["id"] == "beta")
        .expect("beta profile");

    assert_eq!(value["active_profile_id"], "alpha");
    assert_eq!(beta["name"], "Beta");
    assert_eq!(beta["description"], "Beta profile");
    assert_eq!(beta["tags"], serde_json::json!(["fast", "delegate"]));
    assert_eq!(beta["config_kind"], "single");
    assert!(beta["source"].as_str().unwrap().starts_with("local:"));
    assert!(
        beta["fingerprint"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[tokio::test]
async fn test_querymt_profile_set_active_mutates_shared_default() {
    let (f, _profile_dir) = profile_fixture_with_files(&[
        ("alpha.toml", ALPHA_PROFILE_TOML),
        ("beta.toml", BETA_PROFILE_TOML),
    ])
    .await;
    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/profile/setActive",
        raw_params(r#"{"profile_id":"beta"}"#),
    );

    let resp = f
        .handle
        .ext_method(req)
        .await
        .expect("setActive ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");

    assert_eq!(value["active_profile_id"], "beta");
    assert_eq!(
        f.handle.profiles().unwrap().active_profile_id().await,
        "beta"
    );
}

#[tokio::test]
async fn test_querymt_profile_set_active_rejects_invalid_requests() {
    let f = HandleFixture::new().await;
    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/profile/setActive",
        raw_params(r#"{"profile_id":"beta"}"#),
    );
    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("profiles must be configured");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);

    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    for params in ["{}", r#"{"profile_id":" "}"#, r#"{"profile_id":"missing"}"#] {
        let req =
            crate::acp::protocol::ExtRequest::new("querymt/profile/setActive", raw_params(params));
        let err = f
            .handle
            .ext_method(req)
            .await
            .expect_err("invalid profile request should fail");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
}

#[tokio::test]
async fn test_querymt_profile_agents_uses_explicit_profile_and_sorts_delegates() {
    let (f, _profile_dir) = profile_fixture_with_files(&[
        ("alpha.toml", ALPHA_PROFILE_TOML),
        ("quorum.toml", QUORUM_PROFILE_TOML),
    ])
    .await;
    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/profile/agents",
        raw_params(r#"{"profile_id":"quorum"}"#),
    );

    let resp = f
        .handle
        .ext_method(req)
        .await
        .expect("profile agents ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
    let agents = value["agents"].as_array().expect("agents array");

    assert_eq!(value["profile_id"], "quorum");
    assert_eq!(agents[0]["id"], "primary");
    assert_eq!(agents[0]["name"], "Session");
    assert_eq!(agents[1]["id"], "coder");
    assert_eq!(agents[1]["capabilities"], serde_json::json!(["coding"]));
    assert_eq!(agents[2]["id"], "reviewer");
    assert_eq!(
        f.handle.profiles().unwrap().active_profile_id().await,
        "alpha"
    );
}

#[tokio::test]
async fn test_querymt_profile_agents_returns_primary_for_single_profile() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/profile/agents",
        raw_params(r#"{"profileId":"alpha"}"#),
    );

    let resp = f
        .handle
        .ext_method(req)
        .await
        .expect("profile agents ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");

    assert_eq!(value["agents"].as_array().unwrap().len(), 1);
    assert_eq!(value["agents"][0]["id"], "primary");
}

#[tokio::test]
async fn test_querymt_profile_agents_rejects_unknown_profile() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/profile/agents",
        raw_params(r#"{"profile_id":"missing"}"#),
    );

    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("unknown profile should fail");
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            method,
            raw_params(&params.to_string()),
        ))
        .await
        .unwrap();
    serde_json::from_str(response.0.get()).unwrap()
}

async fn persisted_delegate_parent(f: &HandleFixture) -> String {
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let session = runtime
        .agent()
        .handle()
        .config
        .provider
        .history_store()
        .create_session(None, None, None, None)
        .await
        .unwrap();
    bind_test_profile(f, &session.public_id, "quorum").await;
    session.public_id
}

#[tokio::test]
async fn delegate_profile_fixture_shares_storage_without_leaking_across_fixtures() {
    let files = [
        ("quorum.toml", QUORUM_PROFILE_TOML),
        ("alpha.toml", ALPHA_PROFILE_TOML),
    ];
    let (first, _first_dir) = profile_fixture_with_files(&files).await;
    let (second, _second_dir) = profile_fixture_with_files(&files).await;
    let session = persisted_delegate_parent(&first).await;
    let profiles = first.handle.profiles().unwrap();
    let alpha = profiles.runtime_for_profile("alpha").await.unwrap();
    let quorum = profiles.runtime_for_profile("quorum").await.unwrap();
    assert!(Arc::ptr_eq(
        &alpha.agent().storage_backend(),
        &quorum.agent().storage_backend()
    ));
    assert!(
        alpha
            .agent()
            .handle()
            .config
            .provider
            .history_store()
            .get_session(&session)
            .await
            .unwrap()
            .is_some()
    );
    let other = second
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    assert!(
        other
            .agent()
            .handle()
            .config
            .provider
            .history_store()
            .get_session(&session)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delegate_assignments_custom_storage_uses_legacy_memory_without_false_notifications() {
    let profile_dir = tempfile::tempdir().unwrap();
    write_profile(profile_dir.path(), "quorum.toml", QUORUM_PROFILE_TOML);
    let session = mock_session("legacy-parent");
    let mut store = MockSessionStore::new();
    store
        .expect_get_session()
        .returning(move |session_id| Ok((session_id == "legacy-parent").then(|| session.clone())))
        .times(0..);
    store
        .expect_set_session_runtime_binding()
        .returning(|_, _, _, _, _, _| Ok(()))
        .times(1);
    let event_storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );
    let backend = Arc::new(TestStorageBackend {
        session_store: Arc::new(store),
        event_storage,
    });
    let f = HandleFixture::new()
        .await
        .with_profile_storage("quorum", profile_dir.path(), backend)
        .await;
    bind_test_profile(&f, "legacy-parent", "quorum").await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let handle = runtime.agent().handle();
    let mut events = handle.subscribe_events();

    let before = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": "legacy-parent"}),
    )
    .await;
    assert_eq!(before["version"], 1);
    assert_eq!(before["durable"], false);
    assert!(before["revision"].is_null());

    let set = ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": "legacy-parent",
            "agent_id": "coder",
            "model_id": "test/test-model"
        }),
    )
    .await;
    assert_eq!(set["version"], 1);
    assert_eq!(set["durable"], false);
    assert!(set["revision"].is_null());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind(),
                AgentEventKind::DelegateModelsChanged { revision: None }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();

    let current = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": "legacy-parent"}),
    )
    .await;
    assert_eq!(current["assignments"][0]["source"], "override");
    let noop = ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": "legacy-parent",
            "agent_id": "coder",
            "model_id": "test/test-model"
        }),
    )
    .await;
    assert!(noop["revision"].is_null());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "legacy no-op must not emit an invalidation"
    );

    let error = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/setDelegateModel",
            raw_params(
                &serde_json::json!({
                    "session_id": "legacy-parent",
                    "agent_id": "coder",
                    "model_id": null,
                    "expected_revision": 0
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);

    handle
        .config
        .delegate_model_overrides
        .set(
            "legacy-parent",
            "removed-role",
            crate::delegation::DelegateModelOverride {
                model_id: "test/test-model".into(),
                node_id: None,
            },
        )
        .await;
    let orphaned = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": "legacy-parent"}),
    )
    .await;
    assert_eq!(
        orphaned["orphaned_overrides"][0]["agent_id"],
        "removed-role"
    );
    ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": "legacy-parent",
            "agent_id": "removed-role",
            "model_id": null
        }),
    )
    .await;
    assert!(
        handle
            .config
            .delegate_model_overrides
            .get("legacy-parent", "removed-role")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn delegate_assignments_restore_after_full_profile_restart_from_temporary_db() {
    let dir = tempfile::tempdir().unwrap();
    let profile_dir = dir.path().join("profiles");
    std::fs::create_dir(&profile_dir).unwrap();
    write_profile(&profile_dir, "quorum.toml", QUORUM_PROFILE_TOML);
    let db = dir.path().join("sessions.db");
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(db.clone())
            .await
            .unwrap(),
    );
    let first = HandleFixture::new()
        .await
        .with_profile_storage("quorum", &profile_dir, storage.clone())
        .await;
    let parent = persisted_delegate_parent(&first).await;
    let sibling = persisted_delegate_parent(&first).await;
    ext_method_json(&first.handle, "querymt/session/setDelegateModel", serde_json::json!({
        "session_id": parent, "agent_id": "coder", "model_id": "test/test-model", "expected_revision": 0
    })).await;
    // Persistence includes the profile binding, not just the model JSON.
    assert_eq!(
        storage
            .get_session_runtime_binding(&parent)
            .await
            .unwrap()
            .unwrap()
            .profile_id,
        "quorum"
    );
    first.handle.profiles().unwrap().shutdown().await;
    drop(first);
    drop(storage);
    let reopened = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(db)
            .await
            .unwrap(),
    );
    let second = HandleFixture::new()
        .await
        .with_profile_storage("quorum", &profile_dir, reopened)
        .await;
    let snapshot = ext_method_json(
        &second.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": parent}),
    )
    .await;
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(
        snapshot["assignments"][0]["model"]["model_id"],
        "test/test-model"
    );
    let other = ext_method_json(
        &second.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": sibling}),
    )
    .await;
    assert_eq!(other["revision"], 0);
    assert!(other["assignments"][0]["model"].is_null());
    let response = ext_method_json(
        &second.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": parent, "agent_id": "coder", "model_id": null, "expected_revision": 1
        }),
    )
    .await;
    assert_eq!(response["revision"], 2);
    second.handle.profiles().unwrap().shutdown().await;
}

#[tokio::test]
async fn delegate_assignment_user_fork_inherits_revision_zero_copy() {
    let (f, _dir) = profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let parent = persisted_delegate_parent(&f).await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let store = runtime.agent().handle().config.provider.history_store();
    ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": parent,
            "agent_id": "coder",
            "model_id": "test/test-model",
            "expected_revision": 0
        }),
    )
    .await;
    store
        .add_message(
            &parent,
            crate::model::AgentMessage {
                id: "fork-point".into(),
                session_id: parent.clone(),
                role: querymt::chat::ChatRole::User,
                parts: vec![crate::model::MessagePart::Prompt {
                    blocks: vec![crate::acp::protocol::ContentBlock::Text(
                        crate::acp::protocol::TextContent::new("task"),
                    )],
                }],
                created_at: 1,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        )
        .await
        .unwrap();
    let fork_id = store
        .fork_session(
            &parent,
            "fork-point",
            crate::session::domain::ForkOrigin::User,
        )
        .await
        .unwrap();
    bind_test_profile(&f, &fork_id, "quorum").await;
    let result = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": fork_id}),
    )
    .await;
    assert_eq!(result["editable"], true);
    assert_eq!(result["revision"], 0);
    assert_eq!(
        result["assignments"][0]["model"]["model_id"],
        "test/test-model"
    );
    ext_method_json(&f.handle, "querymt/session/setDelegateModel", serde_json::json!({
        "session_id": fork_id, "agent_id": "reviewer", "model_id": "test/test-model", "expected_revision": 0
    })).await;
    let parent_state = store
        .get_delegate_assignments(&parent)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent_state.revision, 1);
    assert_eq!(parent_state.overrides["coder"].model_id, "test/test-model");
    assert!(!parent_state.overrides.contains_key("reviewer"));
}

#[tokio::test]
async fn delegate_assignment_readback_survives_reconnect_and_preserves_unavailable_models() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let session_id = persisted_delegate_parent(&f).await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let handle = runtime.agent().handle();
    let model = crate::delegation::DelegateModelOverride {
        model_id: "provider/removed-model".into(),
        node_id: Some("offline-node".into()),
    };
    handle
        .config
        .provider
        .history_store()
        .set_delegate_assignment(&session_id, "coder", Some(model.clone()), None)
        .await
        .unwrap();
    handle
        .config
        .provider
        .history_store()
        .set_delegate_assignment(&session_id, "removed-role", Some(model), None)
        .await
        .unwrap();
    // A fresh handle has no loaded session actors or assignment cache.
    let reconnected = LocalAgentHandle::from_config(f.handle.config.clone());
    reconnected.set_profiles(f.handle.profiles().unwrap().clone());
    let response = ext_method_json(
        &reconnected,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert_eq!(response["revision"], 2);
    assert_eq!(
        response["assignments"][0]["model"]["model_id"],
        "provider/removed-model"
    );
    assert_eq!(
        response["assignments"][0]["model"]["node_id"],
        "offline-node"
    );
    assert_eq!(
        response["orphaned_overrides"][0]["agent_id"],
        "removed-role"
    );
    assert!(handle.registry.lock().await.get(&session_id).is_none());
    let clear = ext_method_json(&f.handle, "querymt/session/setDelegateModel", serde_json::json!({
        "session_id": session_id, "agent_id": "removed-role", "model_id": null, "expected_revision": 2
    })).await;
    assert_eq!(clear["revision"], 3);
}

#[tokio::test]
async fn delegate_assignment_child_cannot_edit_parent_routing() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let parent = persisted_delegate_parent(&f).await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let child = runtime
        .agent()
        .handle()
        .config
        .provider
        .history_store()
        .create_session(
            None,
            None,
            Some(parent),
            Some(crate::session::domain::ForkOrigin::Delegation),
        )
        .await
        .unwrap();
    bind_test_profile(&f, &child.public_id, "quorum").await;
    let response = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": child.public_id}),
    )
    .await;
    assert_eq!(response["editable"], false);
    assert!(response["assignments"].as_array().unwrap().is_empty());
    let error = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/setDelegateModel",
            raw_params(
                &serde_json::json!({
                    "session_id": child.public_id, "agent_id": "coder", "model_id": null
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn delegate_assignment_changes_notify_and_failed_writes_do_not() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let session_id = persisted_delegate_parent(&f).await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let mut events = runtime.agent().handle().subscribe_events();
    ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "session_id": session_id, "agent_id": "coder", "model_id": "test/test-model"
        }),
    )
    .await;
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind(), AgentEventKind::DelegateModelsChanged { .. }) {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(event.session_id(), session_id);
    let mut translator = crate::acp::shared::AcpLiveEventTranslator::new();
    let notification = translator.translate_notification(&event).unwrap();
    assert_eq!(
        notification["method"],
        "querymt/session/delegateModelsChanged"
    );
    assert_eq!(notification["params"]["version"], 1);
    assert_eq!(notification["params"]["session_id"], session_id);
    assert_eq!(notification["params"]["revision"], 1);
    let handle = runtime.agent().handle();
    let durable = handle
        .config
        .event_sink
        .journal()
        .load_session_stream(&session_id, None, None)
        .await
        .unwrap();
    assert!(
        durable
            .iter()
            .all(|event| { !matches!(event.kind, AgentEventKind::DelegateModelsChanged { .. }) }),
        "invalidation hints must not be journaled"
    );
    let stale = f.handle.ext_method(crate::acp::protocol::ExtRequest::new("querymt/session/setDelegateModel", raw_params(&serde_json::json!({
        "session_id": session_id, "agent_id": "coder", "model_id": null, "expected_revision": 0
    }).to_string()))).await;
    assert!(stale.is_err());
    let noop = ext_method_json(&f.handle, "querymt/session/setDelegateModel", serde_json::json!({
        "session_id": session_id, "agent_id": "coder", "model_id": "test/test-model", "expected_revision": 1
    })).await;
    assert_eq!(noop["revision"], 1);
    let no_change = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind(),
                AgentEventKind::DelegateModelsChanged { .. }
            ) {
                break;
            }
        }
    })
    .await;
    assert!(no_change.is_err());
}

#[tokio::test]
async fn test_querymt_session_set_delegate_model_sets_and_clears_override() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let session_id = persisted_delegate_parent(&f).await;
    let before = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"sessionId": session_id}),
    )
    .await;
    assert_eq!(before["version"], 1);
    assert_eq!(before["revision"], 0);
    assert_eq!(before["durable"], true);
    assert_eq!(before["editable"], true);
    assert_eq!(before["assignments"][0]["agent_id"], "coder");
    assert_eq!(before["assignments"][0]["source"], "profile_default");
    assert_eq!(
        before["assignments"][0]["configured_default_model_id"],
        "test/test-model"
    );
    let set = ext_method_json(&f.handle, "querymt/session/setDelegateModel", serde_json::json!({
        "session_id": session_id, "agent_id": "coder", "model_id": "test/test-model", "expected_revision": 0
    })).await;
    assert_eq!(set["version"], 1);
    assert_eq!(set["revision"], 1);
    assert_eq!(set["model"]["model_id"], "test/test-model");
    let current = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert_eq!(current["assignments"][0]["source"], "override");
    assert_eq!(current["assignments"][1]["source"], "profile_default");
    let stale = f.handle.ext_method(crate::acp::protocol::ExtRequest::new("querymt/session/setDelegateModel", raw_params(&serde_json::json!({
        "session_id": session_id, "agent_id": "coder", "model_id": null, "expected_revision": 0
    }).to_string()))).await.unwrap_err();
    assert_eq!(
        stale.code,
        agent_client_protocol::ErrorCode::Other(
            crate::control::delegate_models::DELEGATE_ASSIGNMENT_CONFLICT_ACP_CODE
        )
    );
    assert_eq!(stale.data.unwrap()["code"], "delegate_assignment_conflict");
    let cleared = ext_method_json(
        &f.handle,
        "querymt/session/setDelegateModel",
        serde_json::json!({
            "sessionId": session_id, "agentId": "coder", "modelId": null, "expectedRevision": 1
        }),
    )
    .await;
    assert_eq!(cleared["revision"], 2);
    assert!(cleared["model"].is_null());
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    assert!(
        runtime
            .agent()
            .handle()
            .registry
            .lock()
            .await
            .get(&session_id)
            .is_none(),
        "read/write must not materialize an actor"
    );
}

#[tokio::test]
async fn test_querymt_session_set_delegate_model_rejects_invalid_targets() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    let session_id = persisted_delegate_parent(&f).await;
    for params in [
        serde_json::json!({"session_id": "missing", "agent_id": "coder", "model_id": null}),
        serde_json::json!({"session_id": session_id, "agent_id": "missing", "model_id": null}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder"}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": "test/missing"}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": " "}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": null, "node_id": "node"}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": "test/test-model", "node_id": "  "}),
        serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": "test/test-model", "node_id": "unknown-node"}),
    ] {
        let req = crate::acp::protocol::ExtRequest::new(
            "querymt/session/setDelegateModel",
            raw_params(&params.to_string()),
        );
        let error = f
            .handle
            .ext_method(req)
            .await
            .expect_err("invalid target should fail");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
    let unchanged = ext_method_json(
        &f.handle,
        "querymt/session/delegateModels",
        serde_json::json!({"session_id": session_id}),
    )
    .await;
    assert_eq!(unchanged["revision"], 0);
}

#[tokio::test]
async fn test_delegate_model_override_cleanup_clears_bound_runtime() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    register_bound_test_session(&f, "parent-1", "quorum").await;
    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile("quorum")
        .await
        .unwrap();
    let store = &runtime.agent().handle().config.delegate_model_overrides;
    store
        .set(
            "parent-1",
            "coder",
            crate::delegation::DelegateModelOverride {
                model_id: "test/test-model".into(),
                node_id: None,
            },
        )
        .await;

    f.handle.clear_delegate_model_overrides("parent-1").await;

    assert!(store.get("parent-1", "coder").await.is_none());
}

#[tokio::test]
async fn test_querymt_refresh_models_ext_method_returns_immediately_with_trigger_meta() {
    let f = HandleFixture::new().await;
    let null_params =
        std::sync::Arc::from(serde_json::value::RawValue::from_string("null".to_string()).unwrap());
    let req = crate::acp::protocol::ExtRequest::new("querymt/refreshModels", null_params);
    let resp = tokio::time::timeout(
        tokio::time::Duration::from_millis(500),
        f.handle.ext_method(req),
    )
    .await
    .expect("refreshModels should not block the caller")
    .expect("ext_method");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");
    let meta = value
        .get("meta")
        .and_then(|meta| meta.as_object())
        .expect("response should include meta object");
    assert!(meta.contains_key("refresh_trigger"));
    assert!(meta.contains_key("started_new_refresh"));
    assert!(meta.contains_key("wait_for_completion"));
}

#[tokio::test]
async fn test_ext_notification_ok() {
    let f = HandleFixture::new().await;
    let null_params =
        std::sync::Arc::from(serde_json::value::RawValue::from_string("null".to_string()).unwrap());
    let notif = crate::acp::protocol::ExtNotification::new("my_event", null_params);
    f.handle
        .ext_notification(notif)
        .await
        .expect("ext_notification");
}

#[tokio::test]
async fn test_subscribe_and_emit_event() {
    let f = HandleFixture::new().await;
    let mut rx = f.handle.subscribe_events();

    f.handle
        .emit_event("test-session", crate::events::AgentEventKind::Cancelled);

    let event = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("should receive event in time")
        .expect("event channel should remain open");
    assert!(matches!(
        event.kind(),
        crate::events::AgentEventKind::Cancelled
    ));
    assert_eq!(event.session_id(), "test-session");
}

#[tokio::test]
async fn test_set_llm_config_unknown_provider_fails() {
    let f = HandleFixture::new().await;
    let config = LLMParams::new().provider("unknown-provider").model("gpt-4");
    let result = f.handle.set_llm_config("any-session", config).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    // Should be an UnknownProvider error mapped to ACP
    assert_eq!(
        err.code,
        agent_client_protocol::ErrorCode::InternalError,
        "expected internal error code"
    );
}

#[tokio::test]
async fn test_set_llm_config_no_provider_fails() {
    let f = HandleFixture::new().await;
    // LLMParams with no provider set
    let config = LLMParams::new().model("some-model");
    let result = f.handle.set_llm_config("any-session", config).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_session_limits_no_middleware_returns_none() {
    let f = HandleFixture::new().await;
    let limits = f.handle.get_session_limits();
    assert!(limits.is_none());
}

#[tokio::test]
async fn test_event_subscribe_works() {
    let f = HandleFixture::new().await;
    // Verify we can subscribe to events via the handle
    let _rx = f.handle.subscribe_events();
}

#[tokio::test]
async fn test_agent_registry_accessible() {
    let f = HandleFixture::new().await;
    let registry = f.handle.agent_registry();
    // DefaultAgentRegistry starts empty
    assert!(registry.list_agents().is_empty());
}
