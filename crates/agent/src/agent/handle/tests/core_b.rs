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
async fn test_querymt_profile_ext_methods_return_method_not_found() {
    let f = HandleFixture::new().await;
    for method in ["querymt/profiles", "querymt/profile/setActive"] {
        let req = crate::acp::protocol::ExtRequest::new(method, raw_params("null"));
        let err = f
            .handle
            .ext_method(req)
            .await
            .expect_err("profile ext_method should be removed");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::MethodNotFound);
    }
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
