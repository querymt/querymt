//! Extension requests as the stdio server receives them: ACP keeps the leading
//! `_` of an extension method, the agent's handler matches the bare name.

use super::*;
use crate::acp::stdio::call_ext_method;

fn wire_request(method: &str, params: serde_json::Value) -> crate::acp::protocol::ExtRequest {
    crate::acp::protocol::ExtRequest::new(method, raw_params(&params.to_string()))
}

#[tokio::test]
async fn stdio_serves_underscore_prefixed_querymt_methods() {
    let f = HandleFixture::new().await;

    let capabilities = call_ext_method(
        &f.handle,
        wire_request("_querymt/capabilities", serde_json::json!({})),
    )
    .await
    .expect("_querymt/capabilities must reach the agent handler");
    assert_eq!(capabilities["transport"]["stdio"], true);

    let models = call_ext_method(
        &f.handle,
        wire_request("_querymt/models", serde_json::json!({})),
    )
    .await
    .expect("_querymt/models must reach the agent handler");
    assert!(models.is_object(), "unexpected models payload: {models}");
}

#[tokio::test]
async fn stdio_still_serves_bare_querymt_methods() {
    let f = HandleFixture::new().await;

    let capabilities = call_ext_method(
        &f.handle,
        wire_request("querymt/capabilities", serde_json::json!({})),
    )
    .await
    .expect("querymt/capabilities");

    assert_eq!(capabilities["transport"]["stdio"], true);
}

#[tokio::test]
async fn stdio_unknown_querymt_method_is_method_not_found() {
    let f = HandleFixture::new().await;

    let err = call_ext_method(
        &f.handle,
        wire_request("_querymt/does/not/exist", serde_json::json!({})),
    )
    .await
    .expect_err("unknown method");

    assert_eq!(err.code, agent_client_protocol::ErrorCode::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn stdio_denies_question_recovery_instead_of_reporting_method_not_found() {
    let f = HandleFixture::new().await;

    for method in [
        "_querymt/elicitation/listPendingSessions",
        "_querymt/elicitation/attachSession",
    ] {
        let err = call_ext_method(&f.handle, wire_request(method, serde_json::json!({})))
            .await
            .expect_err("recovery needs the WebSocket connection registry");

        assert_eq!(
            i32::from(err.code),
            crate::control::elicitation_recovery::ELICITATION_RECOVERY_DENIED_CODE,
            "{method}: {err:?}"
        );
    }
}
