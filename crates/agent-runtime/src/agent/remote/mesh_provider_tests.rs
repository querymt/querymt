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
    use crate::agent::remote::test_helpers::fixtures::{ProviderHostFixture, get_test_mesh};
    use querymt::chat::ChatProvider;
    use querymt::completion::CompletionProvider;
    use querymt::completion::CompletionRequest;
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;
    use querymt_remote::should_retry_remote_send;
    use querymt_remote::{MeshChatProvider, find_provider_on_mesh};

    #[tokio::test]
    async fn mesh_chat_provider_reports_no_output_provenance() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3");
        assert_eq!(provider.output_provenance(), None);
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
            .with_params(Some(params));

        // We can't directly inspect private fields, but we can verify it
        // compiles and the builder pattern works. The params are forwarded
        // in the actual chat_with_tools call (tested via integration tests).
        assert!(provider.capabilities().streaming);
    }

    // ── B.3c — MeshChatProvider with_params None ─────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_with_params_none() {
        let mesh = get_test_mesh().await;
        let provider =
            MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3").with_params(None);

        assert!(provider.capabilities().streaming);
    }

    #[tokio::test]
    async fn test_mesh_chat_provider_with_stream_controls_and_status_lookup() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "missing-node", "anthropic", "claude-3")
            .with_stream_controls(7, 33);

        assert!(provider.capabilities().streaming);
        assert!(
            provider
                .get_remote_stream_status("missing-session", None)
                .await
                .is_none(),
            "status lookup against an unregistered provider host should return None"
        );
    }

    // ── B.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_supports_streaming() {
        let mesh = get_test_mesh().await;
        let provider = MeshChatProvider::new(mesh, "any-node", "anthropic", "claude-3");
        assert!(
            provider.capabilities().streaming,
            "MeshChatProvider must advertise streaming support"
        );
    }

    // ── B.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_find_provider_on_mesh_returns_none_when_no_peers() {
        let mesh = get_test_mesh().await;
        // "completely-nonexistent-provider-xyz" is never registered anywhere.
        let result =
            find_provider_on_mesh(mesh, "completely-nonexistent-provider-xyz-b5", "demo").await;
        assert!(
            result.is_none(),
            "should return None when no peer advertises the provider"
        );
    }

    // ── B.6 — Bug #2 (FIXED) ─────────────────────────────────────────────────

    /// Verifies that `scoped_provider_host` produces the correct scoped DHT
    /// name format.  `MeshChatProvider::new` uses this to construct the DHT
    /// lookup key, so the registration side must produce the same string.
    #[tokio::test]
    async fn test_scoped_provider_host_uses_peer_id_not_hostname() {
        use crate::agent::remote::scope::{MeshScopeId, scoped_provider_host};

        let peer_id = "12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH";
        let dht_name = scoped_provider_host(&MeshScopeId::lan_default(), peer_id);

        // Must use the scoped "scope::lan::default::provider_host::peer::" prefix.
        assert_eq!(
            dht_name,
            "scope::lan::default::provider_host::peer::12D3KooWPv7fUDC2WqR5c6v71fMsoxhoYYqcPEciyCfuqRz6f6qH",
            "scoped_provider_host must produce 'scope::lan::default::provider_host::peer::{{peer_id}}'"
        );

        // Must NOT contain just a hostname.
        assert!(
            dht_name.contains("provider_host::peer::"),
            "dht_name must contain 'provider_host::peer::'"
        );
    }

    #[tokio::test]
    async fn test_mesh_chat_provider_chat_with_tools_local_host() {
        let mesh = get_test_mesh().await;

        ProviderHostFixture::registered(mesh).await;
        let node_id = mesh.peer_id().to_string();

        let provider = MeshChatProvider::new(mesh, &node_id, "nonexistent", "no-model");

        // The ProviderHostActor exists in the mesh but the provider "nonexistent"
        // can't be built → should get a HandlerError wrapped as ProviderError.
        let result = provider.chat(&querymt::chat::ChatCall::new(&[])).await;
        assert!(
            matches!(result, Err(LLMError::ProviderError(_))),
            "should return ProviderError (provider not buildable), got {:?}",
            result
        );
    }

    #[test]
    fn test_should_retry_remote_send_for_connection_failures() {
        use kameo::error::RemoteSendError;

        assert!(should_retry_remote_send::<()>(
            &RemoteSendError::ActorNotRunning
        ));
        assert!(should_retry_remote_send::<()>(
            &RemoteSendError::ConnectionClosed
        ));
        assert!(should_retry_remote_send::<()>(
            &RemoteSendError::UnknownActor {
                actor_remote_id: "actor".into(),
            }
        ));
        // Timeouts are replayable under the resumable provider-stream retry
        // policy (streams recover through stream leases), so reply and network
        // timeouts count as retryable connectivity failures.
        assert!(should_retry_remote_send::<()>(
            &RemoteSendError::NetworkTimeout
        ));
        assert!(should_retry_remote_send::<()>(
            &RemoteSendError::ReplyTimeout
        ));
    }

    #[test]
    fn test_should_not_retry_remote_send_for_non_connection_failures() {
        use kameo::error::RemoteSendError;

        assert!(!should_retry_remote_send::<()>(
            &RemoteSendError::MailboxFull
        ));
        assert!(!should_retry_remote_send::<()>(
            &RemoteSendError::BadActorType
        ));
    }
}
