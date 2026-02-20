//! Module B — `MeshChatProvider` tests.
//!
//! B.1–B.4: No mesh required (pure construction + stub-path tests).
//! B.5–B.8: Mesh required (DHT lookup paths).
//!
//! Bug documented: **#2** — `find_provider_on_mesh` returns `provider_name`
//! as the hostname placeholder (B.6, marked `#[ignore]`).

#[cfg(all(test, feature = "remote"))]
mod mesh_provider_tests {
    use crate::agent::remote::mesh_provider::{MeshChatProvider, find_provider_on_mesh};
    use crate::agent::remote::test_helpers::fixtures::{
        MeshNodeManagerFixture, ProviderHostFixture, get_test_mesh,
    };
    use querymt::chat::ChatProvider;
    use querymt::completion::CompletionProvider;
    use querymt::completion::CompletionRequest;
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;
    use uuid::Uuid;

    // ── B.1 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_dht_name_format() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "gpu-server", "anthropic", "claude-3");
        // We verify the internal DHT name via the error message on lookup failure,
        // which includes the name.  A cleaner approach would be an accessor; for
        // now the error path is the only observable.
        let result = provider.chat_with_tools(&[], None).await;
        match result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::gpu-server"),
                    "error should mention the formatted DHT name, got: {}",
                    msg
                );
            }
            other => panic!("expected ProviderError, got {:?}", other),
        }
    }

    // ── B.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_completion_not_supported() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3");
        let req = CompletionRequest {
            prompt: "test".to_string(),
            suffix: None,
            max_tokens: None,
            temperature: None,
        };
        let result = provider.complete(&req).await;
        assert!(
            matches!(result, Err(LLMError::NotImplemented(_))),
            "complete() should return NotImplemented, got {:?}",
            result
        );
    }

    // ── B.3 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_embedding_not_supported() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3");
        let result = provider.embed(vec!["hello".to_string()]).await;
        assert!(
            matches!(result, Err(LLMError::NotImplemented(_))),
            "embed() should return NotImplemented, got {:?}",
            result
        );
    }

    // ── B.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_supports_streaming() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3");
        assert!(
            provider.supports_streaming(),
            "MeshChatProvider must advertise streaming support"
        );
    }

    // ── B.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_find_provider_on_mesh_returns_none_when_no_peers() {
        let mesh = get_test_mesh().await;
        // "completely-nonexistent-provider-xyz" is never registered anywhere.
        let result = find_provider_on_mesh(mesh, "completely-nonexistent-provider-xyz-b5").await;
        assert!(
            result.is_none(),
            "should return None when no peer advertises the provider"
        );
    }

    // ── B.6 — Bug #2 (documented, test marked ignore) ────────────────────────

    /// Documents **Bug #2**: `find_provider_on_mesh` returns `provider_name` as
    /// the hostname string, so the caller constructs
    /// `"provider_host::{provider_name}"` instead of
    /// `"provider_host::{real-hostname}"`.
    ///
    /// When this is fixed, the `#[ignore]` tag should be removed and the
    /// assertion updated to check for the real hostname.
    #[tokio::test]
    #[ignore = "TODO: fix find_provider_on_mesh hostname resolution (Bug #2)"]
    async fn test_find_provider_on_mesh_hostname_placeholder_bug() {
        let test_id = Uuid::now_v7().to_string();
        let _nm = MeshNodeManagerFixture::new("b6", &test_id).await;
        let mesh = get_test_mesh().await;

        // Even if a node manager is in the mesh, find_provider_on_mesh
        // currently returns the provider_name string, not the real hostname.
        // A correct implementation would return the hostname under which
        // "provider_host::{hostname}" is registered.
        let result = find_provider_on_mesh(mesh, "mock").await;

        // Bug #2: result is Some("mock") — but "provider_host::mock" is not
        // a valid DHT registration.  When fixed, result should be Some of the
        // real hostname (e.g. "alpha-{test_id}").
        if let Some(ref hostname) = result {
            assert_ne!(
                hostname, "mock",
                "Bug #2: find_provider_on_mesh returned provider_name as hostname. \
                 The returned value '{}' would cause MeshChatProvider to look up \
                 'provider_host::mock' instead of the real registration.",
                hostname
            );
        }
        // If None, the test passes (no peers advertising the provider).
    }

    // ── B.7 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_chat_with_tools_no_host() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        // Use a DHT name that is guaranteed to be unregistered.
        let unregistered = format!("unregistered-node-b7-{}", test_id);
        let provider = MeshChatProvider::new(mesh, &unregistered, "anthropic", "claude-3");

        let result = provider.chat_with_tools(&[], None).await;
        assert!(
            matches!(result, Err(LLMError::ProviderError(_))),
            "should return ProviderError when DHT name is not registered, got {:?}",
            result
        );
    }

    // ── B.8 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_chat_with_tools_local_host() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        // Register a ProviderHostActor under a unique DHT name.
        let f = ProviderHostFixture::new().await;
        let dht_name = format!("provider_host::test-node-b8-{}", test_id);
        mesh.register_actor(f.actor_ref, dht_name.clone()).await;

        // Strip the "provider_host::" prefix for MeshChatProvider::new().
        let node_name = format!("test-node-b8-{}", test_id);
        let provider = MeshChatProvider::new(mesh, &node_name, "nonexistent", "no-model");

        // The ProviderHostActor exists in the mesh but the provider "nonexistent"
        // can't be built → should get a HandlerError wrapped as ProviderError.
        let result = provider.chat_with_tools(&[], None).await;
        assert!(
            matches!(result, Err(LLMError::ProviderError(_))),
            "should return ProviderError (provider not buildable), got {:?}",
            result
        );
    }
}
