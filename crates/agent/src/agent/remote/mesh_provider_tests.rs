//! Module B — `MeshChatProvider` tests.
//!
//! B.1–B.4: No mesh required (pure construction + stub-path tests).
//! B.5–B.8: Mesh required (DHT lookup paths).
//!
//! Bug documented: **#2** — `find_provider_on_mesh` returns `provider_name`
//! as the hostname placeholder (B.6, marked `#[ignore]`).

#[cfg(all(test, feature = "remote"))]
#[allow(clippy::module_inception)]
mod mesh_provider_tests {
    use crate::agent::remote::mesh_provider::{MeshChatProvider, find_provider_on_mesh};
    use crate::agent::remote::test_helpers::fixtures::{ProviderHostFixture, get_test_mesh};
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
                    msg.contains("provider_host::peer::gpu-server"),
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

    // ── B.3b — MeshChatProvider with_params ─────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_with_params() {
        let mesh = get_test_mesh().await;
        let params = serde_json::json!({
            "system": ["You are a delegate."],
            "temperature": 0.3
        });
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3")
            .with_params(Some(params.clone()));

        // We can't directly inspect private fields, but we can verify it
        // compiles and the builder pattern works. The params are forwarded
        // in the actual chat_with_tools call (tested via integration tests).
        assert!(provider.supports_streaming());
    }

    // ── B.3c — MeshChatProvider with_params None ─────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_with_params_none() {
        let mesh = get_test_mesh().await;
        let provider =
            MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3").with_params(None);

        assert!(provider.supports_streaming());
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

    // ── B.6 — Bug #2 (FIXED) ─────────────────────────────────────────────────

    /// **Bug #2 — FIXED.** The root cause was `coder_agent.rs` registering
    /// `ProviderHostActor` under `"provider_host::{hostname}"` instead of
    /// `dht_name::provider_host(peer_id)` (`"provider_host::peer::{peer_id}"`).
    ///
    /// The fix was two-fold:
    /// 1. Introduced `agent::remote::dht_name` module to centralise all DHT
    ///    naming conventions, eliminating ad-hoc `format!` strings.
    /// 2. Changed `coder_agent.rs` (and all other call sites) to use
    ///    `dht_name::provider_host(mesh.peer_id())`.
    ///
    /// `find_provider_on_mesh` itself was correct — it returns `node_info.node_id`
    /// (a PeerId), not a hostname. The bug was only on the registration side.
    ///
    /// This test verifies the `dht_name` API produces the correct format.
    #[tokio::test]
    async fn test_dht_name_provider_host_uses_peer_id_not_hostname() {
        use crate::agent::remote::dht_name;

        let peer_id = "12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH";
        let dht_name = dht_name::provider_host(&peer_id);

        // Must use the "provider_host::peer::" prefix with peer_id.
        assert_eq!(
            dht_name, "provider_host::peer::12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH",
            "dht_name::provider_host must produce 'provider_host::peer::{{peer_id}}'"
        );

        // Must NOT contain just a hostname.
        assert!(
            dht_name.starts_with("provider_host::peer::"),
            "dht_name must start with 'provider_host::peer::'"
        );
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

        // Register a ProviderHostActor under a unique peer-id keyed DHT name.
        let f = ProviderHostFixture::new().await;
        let node_id = format!("test-node-b8-{}", test_id);
        let dht_name = crate::agent::remote::dht_name::provider_host(&node_id);
        mesh.register_actor(f.actor_ref, dht_name.clone()).await;

        let provider = MeshChatProvider::new(mesh, &node_id, "nonexistent", "no-model");

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
