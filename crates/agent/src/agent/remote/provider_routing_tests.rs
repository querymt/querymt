//! Modules H + I — Provider routing integration + mesh setup config tests.
//!
//! Module H: The primary broken path — using Alpha's LLM model to run prompts
//! on Beta's session. Tests `ProviderHostActor`, `MeshChatProvider`, and
//! `SessionProvider::build_provider` with `provider_node_id`.
//!
//! Module I: `setup_mesh_from_config` TOML-config translation layer.
//!
//! Bugs documented:
//! - **#1** — `provider_node_id` never written to DB during `CreateRemoteSession`
//! - **#7** — `finish_reason` Debug string mismatch (H.11)

// ═════════════════════════════════════════════════════════════════════════════
//  Module H — Provider routing integration tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(all(test, feature = "remote"))]
mod provider_routing_integration_tests {
    use crate::agent::remote::NodeId;
    use crate::agent::remote::ProviderChatRequest;
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::node_manager::CreateRemoteSession;
    use crate::agent::remote::test_helpers::fixtures::{
        AgentConfigFixture, ProviderHostFixture, ProviderRoutingFixture, get_test_mesh,
    };
    use crate::session::provider::ProviderRequest;
    use querymt::chat::{ChatProvider, FinishReason};
    use querymt::error::LLMError;
    use querymt_remote::MeshChatProvider;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn random_node_id() -> String {
        NodeId::from_peer_id(
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        )
        .to_string()
    }

    // ── H.1 — Session without provider_node_id uses local provider ───────────────

    #[tokio::test]
    async fn test_session_with_no_provider_node_id_uses_local_provider() {
        let f = AgentConfigFixture::new().await;

        let result = f
            .config
            .provider
            .build_provider(ProviderRequest::new("nonexistent", "model"))
            .await;

        assert!(
            result.is_err(),
            "local provider not found → should fail with error"
        );
        let err_str = result.err().expect("should be err").to_string();
        assert!(
            !err_str.contains("MeshChatProvider"),
            "error should come from local plugin path, not mesh path: {}",
            err_str
        );
    }

    #[tokio::test]
    async fn test_set_session_model_with_provider_node_id_persists() {
        let test_id = Uuid::now_v7().to_string();
        let f = ProviderRoutingFixture::new(&test_id).await;

        let beta_mesh_ref = f
            .mesh
            .lookup_actor::<RemoteNodeManager>(&f.beta.dht_name)
            .await
            .expect("lookup beta")
            .expect("beta not found");

        let resp = beta_mesh_ref
            .ask(&CreateRemoteSession { cwd: None })
            .await
            .expect("create session on beta");

        let session_id = resp.session_id.clone();
        let provider_node_id_name = format!("alpha-{}", test_id);
        f.beta
            .config
            .provider
            .history_store()
            .set_session_provider_node_id(&session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        let stored = f
            .beta
            .config
            .provider
            .history_store()
            .get_session_provider_node_id(&session_id)
            .await
            .expect("get_session_provider_node_id");

        assert_eq!(
            stored.as_deref(),
            Some(provider_node_id_name.as_str()),
            "provider_node_id should persist in DB"
        );
    }

    #[tokio::test]
    async fn test_build_provider_for_session_uses_mesh_chat_provider() {
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        let provider_node_id = random_node_id();
        f.config.provider.set_mesh(Some(mesh.clone()));

        let provider = f
            .config
            .provider
            .build_provider(
                ProviderRequest::new("anthropic", "claude-3")
                    .with_provider_node_id(Some(&provider_node_id)),
            )
            .await
            .expect("build_provider with provider_node_id should succeed");

        let result = provider.chat_with_tools(&[], None).await;
        match result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::"),
                    "error should name the DHT key (mesh routing confirmed), got: {}",
                    msg
                );
            }
            other => panic!("expected ProviderError from mesh routing, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_mesh_chat_provider_chat_with_tools_via_provider_host() {
        let test_id = Uuid::now_v7().to_string();
        let f = ProviderRoutingFixture::new(&test_id).await;

        let node_name = format!("alpha-{}", test_id);
        let provider = MeshChatProvider::new(f.mesh, &node_name, "nonexistent", "no-model");

        let result = provider.chat_with_tools(&[], None).await;
        assert!(
            matches!(result, Err(LLMError::ProviderError(_))),
            "should return ProviderError (from ProviderHostActor handler), got {:?}",
            result
        );
    }
}
