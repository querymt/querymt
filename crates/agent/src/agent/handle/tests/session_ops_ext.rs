use super::*;

#[tokio::test]
async fn querymt_session_discard_queued_input_is_idempotent_when_not_pending() {
    let f = HandleFixture::new().await;
    let session = f
        .handle
        .config
        .provider
        .history_store()
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    register_test_session(&f, &session.public_id).await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/discardQueuedInput",
        serde_json::json!({
            "session_id": session.public_id,
            "input_id": "missing-input"
        }),
    )
    .await;

    assert_eq!(result["status"], "not_pending");
    assert_eq!(result["data"]["input_id"], "missing-input");
}

#[tokio::test]
async fn querymt_session_undo_stack_returns_empty_stack_for_new_session() {
    let f = HandleFixture::new().await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/undoStack",
        serde_json::json!({ "session_id": "test-session" }),
    )
    .await;

    assert_eq!(result["undo_stack"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn querymt_session_undo_stack_rejects_missing_session_id() {
    let f = HandleFixture::new().await;
    let req = crate::acp::protocol::ExtRequest::new("querymt/session/undoStack", raw_params("{}"));

    let err = f
        .handle
        .ext_method(req)
        .await
        .expect_err("missing session_id should be invalid params");

    assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
}

#[tokio::test]
async fn querymt_session_undo_routes_profile_bound_session_to_profile_runtime() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    register_bound_test_session(&f, "session-1", "alpha").await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/undo",
        serde_json::json!({ "session_id": "session-1", "message_id": "msg-1" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        !result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Session not found"),
        "profile-bound session should route to profile runtime: {result:?}"
    );
}

#[tokio::test]
async fn querymt_session_redo_routes_profile_bound_session_to_profile_runtime() {
    let (f, _profile_dir) = profile_fixture_with_files(&[("alpha.toml", ALPHA_PROFILE_TOML)]).await;
    register_bound_test_session(&f, "session-1", "alpha").await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/redo",
        serde_json::json!({ "session_id": "session-1" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        !result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Session not found"),
        "profile-bound session should route to profile runtime: {result:?}"
    );
}

#[tokio::test]
async fn querymt_session_undo_unknown_session_returns_failure_payload() {
    let f = HandleFixture::new().await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/undo",
        serde_json::json!({ "session_id": "missing", "message_id": "msg-1" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Session not found")
    );
    assert_eq!(result["undo_stack"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn querymt_session_redo_unknown_session_returns_failure_payload() {
    let f = HandleFixture::new().await;

    let result = ext_method_json(
        &f.handle,
        "querymt/session/redo",
        serde_json::json!({ "session_id": "missing" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Session not found")
    );
    assert_eq!(result["undo_stack"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn querymt_session_lifecycle_ext_reads_and_patches_without_loading() {
    let f = HandleFixture::new().await;
    let store = f.handle.config.provider.history_store();
    let session = store
        .create_session(None, None, None, None)
        .await
        .expect("create stored session");
    let session_id = session.public_id;

    let initial = ext_method_json(
        &f.handle,
        "querymt/session/getLifecycle",
        serde_json::json!({ "session_id": session_id }),
    )
    .await;
    assert_eq!(initial["resolved"], false);
    assert_eq!(initial["archived"], false);
    assert!(initial["resolved_at"].is_null());
    assert!(f.handle.registry.lock().await.get(&session_id).is_none());

    let updated = ext_method_json(
        &f.handle,
        "querymt/session/setLifecycle",
        serde_json::json!({
            "session_id": session_id,
            "resolved": true,
            "archived": true
        }),
    )
    .await;
    assert_eq!(updated["resolved"], true);
    assert_eq!(updated["archived"], true);
    assert!(updated["resolved_at"].is_string());
    assert!(updated["archived_at"].is_string());

    let restored = ext_method_json(
        &f.handle,
        "querymt/session/setLifecycle",
        serde_json::json!({ "session_id": session_id, "archived": false }),
    )
    .await;
    assert_eq!(restored["resolved"], true);
    assert_eq!(restored["archived"], false);
    assert!(restored["archived_at"].is_null());
    assert!(f.handle.registry.lock().await.get(&session_id).is_none());
}

#[tokio::test]
async fn querymt_session_lifecycle_ext_rejects_empty_and_null_patches() {
    let f = HandleFixture::new().await;
    let missing = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/setLifecycle",
            raw_params(&serde_json::json!({ "session_id": "missing" }).to_string()),
        ))
        .await
        .expect_err("empty patch is invalid");
    assert_eq!(
        missing.code,
        agent_client_protocol::ErrorCode::InvalidParams
    );

    let null_value = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/setLifecycle",
            raw_params(
                &serde_json::json!({
                    "session_id": "missing",
                    "resolved": null
                })
                .to_string(),
            ),
        ))
        .await
        .expect_err("null desired state is invalid");
    assert_eq!(
        null_value.code,
        agent_client_protocol::ErrorCode::InvalidParams
    );
}

#[tokio::test]
async fn querymt_session_set_pinned_notifies_only_when_state_changes() {
    let f = RealStorageHandleFixture::new().await;
    let session = f
        .storage
        .create_session(None, None, None, None)
        .await
        .expect("create local session");
    let session_id = session.public_id;
    let mut ext_notifications = f.handle.subscribe_ext_notifications();
    let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel(8);
    f.handle
        .set_bridge(crate::acp::client_bridge::ClientBridgeSender::new(
            bridge_tx,
        ))
        .await;

    let pinned = ext_method_json(
        &f.handle,
        "querymt/session/setPinned",
        serde_json::json!({ "session_id": session_id, "pinned": true }),
    )
    .await;
    assert_eq!(pinned["session_id"], session_id);
    assert_eq!(pinned["pinned"], true);
    assert!(pinned["pinned_at"].as_str().is_some());
    let timestamp = pinned["pinned_at"].clone();

    let global = ext_notifications.recv().await.expect("global notification");
    assert_eq!(
        global.method.as_ref(),
        crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(global.params.get()).unwrap()["session_id"],
        session_id
    );
    match bridge_rx.recv().await.expect("stdio bridge notification") {
        crate::acp::client_bridge::ClientBridgeMessage::ExtNotification(notification) => {
            assert_eq!(
                notification.method.as_ref(),
                crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED
            );
        }
        other => panic!("expected extension notification, got {other:?}"),
    }

    let repeated = ext_method_json(
        &f.handle,
        "querymt/session/setPinned",
        serde_json::json!({ "session_id": session_id, "pinned": true }),
    )
    .await;
    assert_eq!(repeated["pinned_at"], timestamp);
    assert!(matches!(
        ext_notifications.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        bridge_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    for params in [
        serde_json::json!({ "session_id": session_id }),
        serde_json::json!({ "session_id": session_id, "pinned": null }),
        serde_json::json!({ "session_id": session_id, "pinned": "true" }),
    ] {
        let error = f
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/session/setPinned",
                raw_params(&params.to_string()),
            ))
            .await
            .expect_err("invalid pinned state must be rejected");
        assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
    let missing = f
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/setPinned",
            raw_params(&serde_json::json!({ "session_id": "missing", "pinned": true }).to_string()),
        ))
        .await
        .expect_err("missing local session must be rejected");
    assert_eq!(
        missing.code,
        agent_client_protocol::ErrorCode::InvalidParams
    );
    assert!(matches!(
        ext_notifications.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    let unpinned = ext_method_json(
        &f.handle,
        "querymt/session/setPinned",
        serde_json::json!({ "session_id": session_id, "pinned": false }),
    )
    .await;
    assert_eq!(unpinned["pinned"], false);
    assert!(unpinned["pinned_at"].is_null());
    let changed = ext_notifications.recv().await.expect("unpin invalidation");
    assert_eq!(
        changed.method.as_ref(),
        crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED
    );
    let _ = bridge_rx.recv().await.expect("stdio unpin notification");

    ext_method_json(
        &f.handle,
        "querymt/session/setPinned",
        serde_json::json!({ "session_id": session_id, "pinned": true }),
    )
    .await;
    let _ = ext_notifications.recv().await.expect("repin invalidation");
    let _ = bridge_rx.recv().await.expect("stdio repin notification");
    crate::send_agent::SendAgent::delete_session(
        &f.handle,
        crate::acp::protocol::DeleteSessionRequest::new(crate::acp::protocol::SessionId::from(
            session_id,
        )),
    )
    .await
    .expect("delete pinned local session");
    let deleted = ext_notifications.recv().await.expect("delete invalidation");
    assert_eq!(
        deleted.method.as_ref(),
        crate::acp::shared::QMT_NOTIFICATION_SESSION_PINS_CHANGED
    );
    let _ = bridge_rx.recv().await.expect("stdio delete notification");
}

#[tokio::test]
async fn acp_pinned_filter_composes_before_paging_and_hydrates_local_pin_metadata() {
    let f = RealStorageHandleFixture::new().await;
    let cwd = std::path::PathBuf::from("/tmp/querymt-pinned-filter");
    let store = &f.storage;
    let oldest = store
        .create_session(None, Some(cwd.clone()), None, None)
        .await
        .expect("create oldest root");
    for _ in 0..10 {
        store
            .create_session(None, Some(cwd.clone()), None, None)
            .await
            .expect("create unpinned root");
    }
    let newest_pin = store
        .create_session(None, Some(cwd.clone()), None, None)
        .await
        .expect("create matching root pin");
    let child_pin = store
        .create_session(
            None,
            Some(cwd.clone()),
            Some(oldest.public_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await
        .expect("create child pin");
    let other_cwd_pin = store
        .create_session(None, Some("/tmp/querymt-other".into()), None, None)
        .await
        .expect("create other workspace pin");
    let lifecycle = crate::session::lifecycle::SessionLifecyclePatch {
        resolved: Some(true),
        archived: Some(true),
    };
    for session_id in [
        oldest.public_id.as_str(),
        newest_pin.public_id.as_str(),
        child_pin.public_id.as_str(),
        other_cwd_pin.public_id.as_str(),
    ] {
        store
            .patch_session_lifecycle(session_id, &lifecycle)
            .await
            .expect("set lifecycle");
        store
            .set_session_pinned(session_id, true)
            .await
            .expect("pin session");
    }
    {
        let conn = f.storage.conn_for_test();
        conn.lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET updated_at = '2000-01-01T00:00:00Z' WHERE public_id = ?1",
                rusqlite::params![oldest.public_id],
            )
            .unwrap();
    }

    let make_request = |meta| {
        crate::acp::protocol::ListSessionsRequest::new()
            .cwd(cwd.clone())
            .meta(meta)
    };
    let mut root_meta = crate::acp::protocol::Meta::new();
    root_meta.insert("session_scope".into(), serde_json::json!("root"));
    let activity_page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        make_request(root_meta),
    )
    .await
    .expect("unfiltered activity page");
    assert_eq!(activity_page.total_count, 12);
    assert_eq!(activity_page.sessions.len(), 10);
    assert!(activity_page.next_cursor.is_some());
    assert!(
        !activity_page
            .sessions
            .iter()
            .any(|info| info.session_id.to_string() == oldest.public_id)
    );

    let mut pinned_meta = crate::acp::protocol::Meta::new();
    pinned_meta.insert("session_scope".into(), serde_json::json!("root"));
    pinned_meta.insert(
        "querymt".into(),
        serde_json::json!({
            "sessionFilters": {
                "pinned": true,
                "resolution": "resolved",
                "archive": "only"
            }
        }),
    );
    let pinned_page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        make_request(pinned_meta.clone()),
    )
    .await
    .expect("filtered pinned page");
    assert_eq!(pinned_page.total_count, 2);
    assert_eq!(pinned_page.sessions.len(), 2);
    assert!(pinned_page.next_cursor.is_none());
    assert_eq!(
        pinned_page.sessions[0].session_id.to_string(),
        newest_pin.public_id
    );
    assert_eq!(
        pinned_page.sessions[1].session_id.to_string(),
        oldest.public_id
    );
    for info in &pinned_page.sessions {
        let querymt = info.meta.as_ref().unwrap().get("querymt").unwrap();
        assert_eq!(querymt["pin"]["pinned"], true);
        assert_eq!(querymt["pin"].as_object().unwrap().len(), 2);
        assert!(querymt["pin"]["pinned_at"].as_str().is_some());
        assert_eq!(querymt["lifecycle"]["resolved"], true);
        assert_eq!(querymt["lifecycle"]["archived"], true);
    }

    let mut unpinned_meta = crate::acp::protocol::Meta::new();
    unpinned_meta.insert("session_scope".into(), serde_json::json!("root"));
    unpinned_meta.insert(
        "querymt".into(),
        serde_json::json!({ "sessionFilters": { "pinned": false } }),
    );
    let unpinned_page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        make_request(unpinned_meta),
    )
    .await
    .expect("filtered unpinned page");
    assert_eq!(unpinned_page.total_count, 10);
    assert!(unpinned_page.sessions.iter().all(|info| {
        let querymt = info.meta.as_ref().and_then(|meta| meta.get("querymt"));
        querymt.is_some_and(|value| {
            value["pin"]["pinned"] == false && value["pin"]["pinned_at"].is_null()
        })
    }));

    for pinned in [serde_json::Value::Null, serde_json::json!("true")] {
        let mut invalid_meta = crate::acp::protocol::Meta::new();
        invalid_meta.insert(
            "querymt".into(),
            serde_json::json!({ "sessionFilters": { "pinned": pinned } }),
        );
        assert!(
            crate::api::AgentSessions::list_for_acp_with_runtime(
                &f.handle,
                f.storage.view_store().expect("view store"),
                make_request(invalid_meta),
            )
            .await
            .is_err()
        );
    }

    #[cfg(feature = "remote")]
    {
        pinned_meta.insert("remoteNodeIds".into(), serde_json::json!(["remote-node"]));
        let error = crate::api::AgentSessions::list_for_acp_with_runtime(
            &f.handle,
            f.storage.view_store().expect("view store"),
            make_request(pinned_meta),
        )
        .await
        .expect_err("pin filtering with remote nodes must be rejected");
        assert!(
            error
                .to_string()
                .contains("cannot be combined with remoteNodeIds")
        );
    }
}
