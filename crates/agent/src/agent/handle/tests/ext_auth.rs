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
async fn set_api_token_rejects_empty_key() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(
        &f.handle,
        "querymt/auth/setApiToken",
        serde_json::json!({ "provider": "mock", "api_key": "   " }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot be empty")
    );
    // Never echo the submitted key (even when empty/whitespace).
    let serialized = result.to_string();
    assert!(!serialized.contains("sk-"));
}

#[tokio::test]
async fn set_api_token_rejects_unknown_provider() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(
        &f.handle,
        "querymt/auth/setApiToken",
        serde_json::json!({ "provider": "nonexistent", "api_key": "sk-test-secret" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert_eq!(result["provider"], "nonexistent");
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not configured")
    );
    assert!(!result.to_string().contains("sk-test-secret"));
}

#[tokio::test]
async fn set_api_token_rejects_provider_without_api_key_name() {
    // Default mock factory does not expose HTTPLLMProviderFactory / api_key_name.
    let f = HandleFixture::new().await;
    let result = ext_method_json(
        &f.handle,
        "querymt/auth/setApiToken",
        serde_json::json!({ "provider": "mock", "api_key": "sk-test-secret" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("does not have a known API key name"),
        "unexpected message: {}",
        result["message"]
    );
    assert!(!result.to_string().contains("sk-test-secret"));
}

#[tokio::test]
async fn clear_api_token_rejects_unknown_provider() {
    let f = HandleFixture::new().await;
    let result = ext_method_json(
        &f.handle,
        "querymt/auth/clearApiToken",
        serde_json::json!({ "provider": "nonexistent" }),
    )
    .await;

    assert_eq!(result["success"], false);
    assert!(
        result["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not configured")
    );
}

#[tokio::test]
async fn set_method_accepts_known_values_and_rejects_invalid() {
    let f = HandleFixture::new().await;

    // Invalid method is pure validation and must not depend on keyring.
    let invalid = ext_method_json(
        &f.handle,
        "querymt/auth/setMethod",
        serde_json::json!({ "provider": "mock", "method": "password" }),
    )
    .await;
    assert_eq!(invalid["success"], false);
    assert!(
        invalid["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown auth method"),
        "unexpected message: {}",
        invalid["message"]
    );

    let empty_provider = ext_method_json(
        &f.handle,
        "querymt/auth/setMethod",
        serde_json::json!({ "provider": "  ", "method": "oauth" }),
    )
    .await;
    assert_eq!(empty_provider["success"], false);

    // Persistence of valid methods requires keyring; skip if unavailable.
    let probe = ext_method_json(
        &f.handle,
        "querymt/auth/setMethod",
        serde_json::json!({ "provider": "mock", "method": "oauth" }),
    )
    .await;
    if probe["success"] == false {
        eprintln!(
            "skipping setMethod persistence assertion: {}",
            probe["message"]
        );
        return;
    }

    for method in ["oauth", "api_key", "env_var"] {
        let result = ext_method_json(
            &f.handle,
            "querymt/auth/setMethod",
            serde_json::json!({ "provider": "mock", "method": method }),
        )
        .await;
        assert_eq!(
            result["success"], true,
            "method {method} should succeed: {}",
            result["message"]
        );
        assert_eq!(result["provider"], "mock");
    }

    // Cleanup preferred method entry if keyring is available.
    if let Ok(mut store) = crate::SecretStore::new() {
        let _ = store.delete("auth_method_mock");
    }
}

#[tokio::test]
async fn set_and_clear_api_token_roundtrip() {
    let key_name = format!("QMT_TEST_API_KEY_{}", uuid::Uuid::new_v4().as_simple());
    let f = HandleFixture::with_api_key_name(&key_name).await;

    let set = ext_method_json(
        &f.handle,
        "querymt/auth/setApiToken",
        serde_json::json!({
            "provider": "mock",
            "api_key": "sk-test-roundtrip-value"
        }),
    )
    .await;

    if set["success"] == false {
        // Keyring may be unavailable in some CI environments; still assert
        // we never echo the secret and the method is reachable.
        assert!(!set.to_string().contains("sk-test-roundtrip-value"));
        eprintln!(
            "skipping roundtrip persistence assertion: {}",
            set["message"]
        );
        return;
    }

    assert_eq!(set["provider"], "mock");
    assert!(!set.to_string().contains("sk-test-roundtrip-value"));

    let store = crate::SecretStore::new().expect("secret store");
    assert_eq!(
        store.get(&key_name).as_deref(),
        Some("sk-test-roundtrip-value")
    );

    let clear = ext_method_json(
        &f.handle,
        "querymt/auth/clearApiToken",
        serde_json::json!({ "provider": "mock" }),
    )
    .await;
    assert_eq!(clear["success"], true);
    assert!(store.get(&key_name).is_none());

    // Idempotent clear
    let clear_again = ext_method_json(
        &f.handle,
        "querymt/auth/clearApiToken",
        serde_json::json!({ "provider": "mock" }),
    )
    .await;
    assert_eq!(clear_again["success"], true);
}
