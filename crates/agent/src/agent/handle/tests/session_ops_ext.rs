use super::*;

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
