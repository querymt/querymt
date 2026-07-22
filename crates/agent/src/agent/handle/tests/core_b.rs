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

#[tokio::test]
async fn test_querymt_session_set_delegate_model_sets_and_clears_override() {
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
    let profile_handle = runtime.agent().handle();

    let req = crate::acp::protocol::ExtRequest::new(
        "querymt/session/setDelegateModel",
        raw_params(
            r#"{"session_id":"parent-1","agent_id":"coder","model_id":"test/test-model","node_id":null}"#,
        ),
    );
    let resp = f.handle.ext_method(req).await.expect("set delegate model");
    let value: serde_json::Value = serde_json::from_str(resp.0.get()).expect("valid JSON");

    assert_eq!(value["model"]["model_id"], "test/test-model");
    assert_eq!(
        profile_handle
            .config
            .delegate_model_overrides
            .get("parent-1", "coder")
            .await
            .unwrap()
            .model_id,
        "test/test-model"
    );

    let clear_req = crate::acp::protocol::ExtRequest::new(
        "querymt/session/setDelegateModel",
        raw_params(
            r#"{"session_id":"parent-1","agent_id":"coder","model_id":null,"node_id":null}"#,
        ),
    );
    let clear_resp = f
        .handle
        .ext_method(clear_req)
        .await
        .expect("clear delegate model");
    let clear_value: serde_json::Value =
        serde_json::from_str(clear_resp.0.get()).expect("valid JSON");

    assert!(clear_value["model"].is_null());
    assert!(
        profile_handle
            .config
            .delegate_model_overrides
            .get("parent-1", "coder")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn test_querymt_session_set_delegate_model_rejects_invalid_targets() {
    let (f, _profile_dir) =
        profile_fixture_with_files(&[("quorum.toml", QUORUM_PROFILE_TOML)]).await;
    register_bound_test_session(&f, "parent-1", "quorum").await;

    for params in [
        r#"{"session_id":"missing","agent_id":"coder","model_id":null}"#,
        r#"{"session_id":"parent-1","agent_id":"missing","model_id":null}"#,
        r#"{"session_id":"parent-1","agent_id":"coder","model_id":"test/missing"}"#,
    ] {
        let req = crate::acp::protocol::ExtRequest::new(
            "querymt/session/setDelegateModel",
            raw_params(params),
        );
        let err = f
            .handle
            .ext_method(req)
            .await
            .expect_err("invalid target should fail");
        assert_eq!(err.code, agent_client_protocol::ErrorCode::InvalidParams);
    }
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
