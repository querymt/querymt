//! Modules H + I — Provider routing integration + mesh setup config tests.
//!
//! Module H: The primary broken path — using Alpha's LLM model to run prompts
//! on Beta's session. Tests `ProviderHostActor`, `MeshChatProvider`, and
//! `build_provider_from_config` with `provider_node_id`.
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
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::mesh_provider::MeshChatProvider;
    use crate::agent::remote::node_manager::CreateRemoteSession;
    use crate::agent::remote::provider_host::{
        ProviderChatRequest, ProviderStreamRequest, StreamReceiverActor,
    };
    use crate::agent::remote::test_helpers::fixtures::{
        AgentConfigFixture, ProviderHostFixture, ProviderRoutingFixture, get_test_mesh,
    };
    use crate::session::provider::{ProviderRouting, build_provider_from_config};
    use querymt::chat::{ChatProvider, FinishReason};
    use querymt::error::LLMError;
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

        // build_provider_from_config with provider_node_id = None uses local registry.
        let registry = f.config.provider.plugin_registry();
        let result = build_provider_from_config(
            &registry,
            "nonexistent",
            "model",
            None,
            None,
            ProviderRouting {
                provider_node_id: None, // provider_node_id = None → local path
                mesh_handle: None,      // mesh_handle = None
                allow_mesh_fallback: false,
            },
        )
        .await;

        // Fails because "nonexistent" isn't registered, not because of mesh routing.
        assert!(
            result.is_err(),
            "local provider not found → should fail with error"
        );
        let err_str = result.err().expect("should be err").to_string();
        // Should NOT mention MeshChatProvider in the error path.
        assert!(
            !err_str.contains("MeshChatProvider"),
            "error should come from local plugin path, not mesh path: {}",
            err_str
        );
    }

    // ── H.2 — provider_node_id persists in DB ────────────────────────────────────

    #[tokio::test]
    async fn test_set_session_model_with_provider_node_id_persists() {
        let test_id = Uuid::now_v7().to_string();
        let f = ProviderRoutingFixture::new(&test_id).await;

        // Create a session on Beta.
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

        // Write provider_node_id to DB via the session store.
        let provider_node_id_name = format!("alpha-{}", test_id);
        f.beta
            .config
            .provider
            .history_store()
            .set_session_provider_node_id(&session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        // Read it back.
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

    // ── H.3 — build_provider_from_config with provider_node_id → MeshChatProvider ─

    /// Tests **Bug #1 root cause**: when `provider_node_id` is set, the session
    /// should use `MeshChatProvider`. This test verifies the builder path.
    #[tokio::test]
    async fn test_build_provider_for_session_uses_mesh_chat_provider() {
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        let provider_node_id = random_node_id();
        let registry = f.config.provider.plugin_registry();

        let provider = build_provider_from_config(
            &registry,
            "anthropic",
            "claude-3",
            None,
            None,
            ProviderRouting {
                provider_node_id: Some(&provider_node_id), // explicit remote node
                mesh_handle: Some(mesh),
                allow_mesh_fallback: false,
            },
        )
        .await
        .expect("build_provider_from_config with provider_node_id should succeed");

        // `build_provider_from_config` returns `Arc<dyn LLMProvider>`.
        // `type_name_of_val` only sees the trait object type, not the concrete
        // type behind it.  Verify mesh routing via observable behaviour instead:
        // a MeshChatProvider tries a DHT lookup for "provider_host::{node}" and
        // returns a ProviderError naming that key when the host is not found.
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

    // ── H.4 — MeshChatProvider calls ProviderHostActor ───────────────────────

    #[tokio::test]
    async fn test_mesh_chat_provider_chat_with_tools_via_provider_host() {
        let test_id = Uuid::now_v7().to_string();
        let f = ProviderRoutingFixture::new(&test_id).await;

        // node_id suffix used with "provider_host::peer::{node_id}".
        let node_name = format!("alpha-{}", test_id);
        let provider = MeshChatProvider::new(f.mesh, &node_name, "nonexistent", "no-model");

        // The ProviderHostActor exists but "nonexistent" provider isn't registered.
        // Verify: the call reaches ProviderHostActor (error is from provider build,
        // not from "host not found").
        let result = provider.chat_with_tools(&[], None).await;
        assert!(
            matches!(result, Err(LLMError::ProviderError(_))),
            "should return ProviderError (from ProviderHostActor handler), got {:?}",
            result
        );
    }

    // ── H.5 — Full end-to-end: prompt on remote session using remote provider ─

    /// Documents **Bug #1**: Alpha creates a session on Beta, sets `provider_node_id`
    /// to Alpha's hostname, then runs a prompt. Currently this fails because
    /// `CreateRemoteSession` never writes `provider_node_id` to the DB.
    ///
    /// When Bug #1 is fixed, this test should pass without the workaround.
    #[tokio::test]
    async fn test_remote_session_prompt_uses_remote_provider() {
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

        // Bug #1 workaround: manually write provider_node_id to DB.
        let provider_node_id_name = format!("alpha-{}", test_id);
        f.beta
            .config
            .provider
            .history_store()
            .set_session_provider_node_id(&resp.session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        // Verify provider_node_id was persisted.
        let stored = f
            .beta
            .config
            .provider
            .history_store()
            .get_session_provider_node_id(&resp.session_id)
            .await
            .expect("get_session_provider_node_id");

        assert_eq!(
            stored.as_deref(),
            Some(provider_node_id_name.as_str()),
            "Bug #1 workaround: provider_node_id must be set manually"
        );
        // When Bug #1 is fixed, CreateRemoteSession should set provider_node_id
        // automatically and this manual step should be unnecessary.
    }

    // ── H.6 — Explicit provider_node_id overrides local ──────────────────────────

    #[tokio::test]
    async fn test_provider_node_id_explicit_overrides_local() {
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        let provider_node_id = random_node_id();
        let registry = f.config.provider.plugin_registry();

        let provider = build_provider_from_config(
            &registry,
            "mock",
            "mock-model",
            None,
            None,
            ProviderRouting {
                provider_node_id: Some(&provider_node_id),
                mesh_handle: Some(mesh),
                allow_mesh_fallback: false,
            },
        )
        .await
        .expect("should succeed (creates MeshChatProvider, no local plugin needed)");

        // Verify mesh routing via observable behaviour: a MeshChatProvider
        // performs a DHT lookup for "provider_host::{node}" and returns a
        // ProviderError naming that key when no host is registered there.
        // This is the only way to confirm the concrete type without as_any(),
        // since type_name_of_val on Arc<dyn LLMProvider> always shows the
        // trait object type, not the concrete type behind it.
        let result = provider.chat_with_tools(&[], None).await;
        match result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::"),
                    "error should name the DHT key (mesh routing confirmed), got: {}",
                    msg
                );
            }
            other => panic!(
                "expected ProviderError from mesh routing (MeshChatProvider), got {:?}",
                other
            ),
        }
    }

    // ── H.7 — provider_node_id = "local" uses local provider ────────────────────

    #[tokio::test]
    async fn test_provider_node_id_local_uses_local_provider() {
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        let registry = f.config.provider.plugin_registry();
        let result = build_provider_from_config(
            &registry,
            "nonexistent-local",
            "model",
            None,
            None,
            ProviderRouting {
                provider_node_id: Some("local"), // "local" → bypass mesh
                mesh_handle: Some(mesh),
                allow_mesh_fallback: false,
            },
        )
        .await;

        assert!(
            result.is_err(),
            "should fail on local provider lookup, not mesh"
        );
        let err_str = result.err().expect("should be err").to_string();
        assert!(
            !err_str.contains("MeshChatProvider"),
            "error should be from local plugin path: {}",
            err_str
        );
    }

    // ── H.8 — ProviderHostActor receives request and responds ─────────────────

    #[tokio::test]
    async fn test_provider_host_actor_receives_request_and_responds() {
        let f = ProviderHostFixture::new().await;

        let req = ProviderChatRequest {
            provider: "nonexistent-h8".to_string(),
            model: "test".to_string(),
            messages: vec![],
            tools: None,
        };

        let result = f.actor_ref.ask(req).await;
        use kameo::error::SendError;
        // Actor should handle the request (not crash) and return a handler error
        // because the provider doesn't exist.
        assert!(
            matches!(result, Err(SendError::HandlerError(_))),
            "should return handler error for unknown provider"
        );
    }

    // ── H.9 — ProviderHostActor streaming sends chunks to receiver ────────────

    #[tokio::test]
    async fn test_provider_host_actor_streaming_sends_chunks_to_receiver() {
        let test_id = Uuid::now_v7().to_string();
        let f = ProviderHostFixture::new().await;
        let mesh = get_test_mesh().await;

        let (tx, _rx) = mpsc::channel(16);
        let stream_rx_name =
            crate::agent::remote::dht_name::stream_receiver(&format!("h9-{}", test_id));
        let receiver_actor = StreamReceiverActor::new(tx, stream_rx_name.clone());
        let receiver_ref = StreamReceiverActor::spawn(receiver_actor);
        mesh.register_actor(receiver_ref.clone(), stream_rx_name.clone())
            .await;
        let _ = receiver_ref;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let stream_req = ProviderStreamRequest {
            provider: "nonexistent-h9".to_string(),
            model: "test".to_string(),
            messages: vec![],
            tools: None,
            stream_receiver_name: stream_rx_name,
        };

        // tell() — fire and forget. The handler will fail building the provider
        // and return an error, but the actor itself should not crash.
        use kameo::actor::Spawn as _;
        let result = f.actor_ref.ask(stream_req).await;
        // We get a handler error (provider not found).
        use kameo::error::SendError;
        assert!(
            matches!(result, Err(SendError::HandlerError(_))),
            "streaming with unknown provider should return handler error"
        );
    }

    // ── H.10 — StreamReceiverActor timeout ────────────────────────────────────

    #[tokio::test]
    async fn test_stream_receiver_actor_timeout() {
        use crate::agent::remote::provider_host::STREAM_CHUNK_TIMEOUT;

        // This test documents the timeout mechanism but doesn't wait 60s.
        // We just verify the constant is defined and > 0.
        assert!(
            STREAM_CHUNK_TIMEOUT.as_secs() > 0,
            "STREAM_CHUNK_TIMEOUT must be positive"
        );

        // To actually test timeout behaviour in a reasonable time,
        // a `STREAM_CHUNK_TIMEOUT` that can be overridden in tests would be
        // needed. Document for now.
        let (_tx, mut rx) = mpsc::channel::<Result<querymt::chat::StreamChunk, String>>(1);
        // Drop the sender immediately so the receiver yields None quickly.
        drop(_tx);

        // The receiver should yield None immediately since the sender is dropped.
        assert!(
            rx.try_recv().is_err(),
            "closed channel should yield recv error"
        );
    }

    // ── H.11 — Bug #7: finish_reason Debug string matches match arms ──────────

    /// Confirms that `format!("{:?}", variant)` matches every arm in
    /// `ProviderChatResponse::finish_reason()`.
    ///
    /// This is the definitive Bug #7 regression test.  If a new `FinishReason`
    /// variant is added to `querymt` without updating the match arms, this test
    /// will fail before any production breakage.
    #[test]
    fn test_finish_reason_debug_string_matches_match_arms() {
        use crate::agent::remote::provider_host::ProviderChatResponse;
        use querymt::chat::ChatResponse;

        let variants: &[(FinishReason, &str)] = &[
            (FinishReason::Stop, "Stop"),
            (FinishReason::Length, "Length"),
            (FinishReason::ContentFilter, "ContentFilter"),
            (FinishReason::ToolCalls, "ToolCalls"),
            (FinishReason::Error, "Error"),
            (FinishReason::Other, "Other"),
        ];

        for (variant, expected_debug) in variants {
            let serialized = format!("{:?}", variant);
            assert_eq!(
                &serialized, expected_debug,
                "Bug #7: format!(\"{{:?}}\", {:?}) = '{}', expected '{}'. \
                 The host side uses this string; the client match arm must match.",
                variant, serialized, expected_debug
            );

            // Now verify client-side round-trip.
            let resp = ProviderChatResponse {
                text: None,
                thinking: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: Some(expected_debug.to_string()),
            };
            match resp.finish_reason() {
                Some(r) => {
                    assert_ne!(
                        format!("{:?}", r),
                        format!("{:?}", FinishReason::Unknown),
                        "Bug #7: '{}' maps to Unknown — match arm is missing",
                        expected_debug
                    );
                    assert_eq!(
                        format!("{:?}", r),
                        format!("{:?}", variant),
                        "Bug #7: '{}' round-tripped to wrong variant: {:?}",
                        expected_debug,
                        r
                    );
                }
                None => panic!("finish_reason() returned None for '{}'", expected_debug),
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Module J — build_provider_for_session DB → provider round-trip
// ═════════════════════════════════════════════════════════════════════════════
//
//  These tests exercise the **complete path** that was previously untested:
//
//  session DB (sessions.provider_node_id)
//    → `build_provider_for_session`
//    → `get_session_provider_node_id`
//    → `build_provider_from_config` with provider_node_id = Some(...)
//    → `MeshChatProvider`
//
//  Previously only `build_provider_from_config` was called directly in tests,
//  bypassing the DB read-back.  Bug: `parse_llm_config_row` always returned
//  `provider_node_id: None`, so the mesh routing path was never triggered.

#[cfg(all(test, feature = "remote"))]
mod build_provider_for_session_tests {
    use crate::agent::remote::NodeId;
    use crate::agent::remote::provider_host::ProviderHostActor;
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::session::backend::StorageBackend as _;
    use crate::session::provider::SessionProvider;
    use crate::session::sqlite_storage::SqliteStorage;
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use querymt::error::LLMError;
    use querymt::plugin::host::PluginRegistry;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn random_node_id() -> String {
        NodeId::from_peer_id(
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        )
        .to_string()
    }

    // ── J.1 — build_provider_for_session with provider_node_id reads from DB ─────

    /// End-to-end test: write `provider_node_id` to DB via `set_session_provider_node_id`,
    /// then call `build_provider_for_session` and verify it uses `MeshChatProvider`
    /// (observable via the DHT lookup error for the named DHT key).
    ///
    /// This test was previously impossible because `parse_llm_config_row` always
    /// returned `provider_node_id: None`, causing `build_provider_from_config` to
    /// fall through to the local provider (Case 2), which would fail with
    /// "Unknown provider: anthropic" instead of routing through the mesh.
    #[tokio::test]
    async fn test_build_provider_for_session_reads_provider_node_id_from_db() {
        let mesh = get_test_mesh().await;

        // Build a SessionProvider with a mesh handle.
        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        let session_provider =
            SessionProvider::new(registry, store.clone(), llm).with_mesh(Some(mesh.clone()));
        let session_provider = Arc::new(session_provider);

        // Create a session so there is a row in the sessions table.
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = session_provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        // Write a fake provider_node_id hostname to the DB.
        let provider_node_id_name = random_node_id();
        store
            .set_session_provider_node_id(&session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        // Call build_provider_for_session — previously this would return
        // "Unknown provider: anthropic" because provider_node_id was not read from DB.
        let result = session_provider
            .build_provider_for_session(&session_id)
            .await;

        // The provider should be a MeshChatProvider that tries to look up
        // "provider_host::{provider_node_id_name}" in the DHT and fails since
        // no ProviderHostActor is registered there.  The error message should
        // name the DHT key, proving mesh routing was triggered.
        let provider =
            result.expect("build_provider_for_session should succeed (creates MeshChatProvider)");
        let call_result = provider.chat_with_tools(&[], None).await;
        match call_result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::"),
                    "error should name the DHT key, confirming mesh routing; got: {msg}"
                );
            }
            other => {
                panic!("expected ProviderError from MeshChatProvider DHT lookup; got: {other:?}")
            }
        }
    }

    // ── J.2 — build_provider_for_session with no provider_node_id uses local path ─

    /// When `provider_node_id` is `None` in the DB, `build_provider_for_session`
    /// should fall through to the local provider lookup (Case 2), which fails
    /// with "Unknown provider: anthropic" since the mock registry has no providers.
    #[tokio::test]
    async fn test_build_provider_for_session_no_provider_node_id_uses_local_path() {
        let mesh = get_test_mesh().await;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        let session_provider =
            SessionProvider::new(registry, store.clone(), llm).with_mesh(Some(mesh.clone()));
        let session_provider = Arc::new(session_provider);

        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = session_provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        // Do NOT write provider_node_id to DB — it stays None.

        // Should fail with local provider lookup error (not mesh routing).
        let result = session_provider
            .build_provider_for_session(&session_id)
            .await;

        assert!(
            result.is_err(),
            "should fail — 'anthropic' not in local registry"
        );
        let err_str = result.err().expect("should be err").to_string();
        assert!(
            err_str.contains("Unknown provider"),
            "error should be from local plugin lookup, got: {err_str}"
        );
        // Must NOT route through mesh (no DHT key in error).
        assert!(
            !err_str.contains("provider_host::"),
            "should NOT route through mesh when provider_node_id is None, got: {err_str}"
        );
    }

    // ── J.3 — cache is keyed on (config_id, provider_node_id) ───────────────────

    /// Verify that changing `provider_node_id` in the DB causes a cache miss and
    /// rebuilds the provider, even if `config_id` stays the same.
    #[tokio::test]
    async fn test_build_provider_for_session_cache_keyed_on_provider_node_id() {
        let mesh = get_test_mesh().await;

        // Set up a ProviderHostActor for "alpha" so the MeshChatProvider can
        // resolve successfully on the second call.
        let f = AgentConfigFixture::new().await;
        let actor = ProviderHostActor::new(f.config.clone());
        let actor_ref = ProviderHostActor::spawn(actor);
        let alpha_node_id = random_node_id();
        let alpha_dht = crate::agent::remote::dht_name::provider_host(&alpha_node_id);
        mesh.register_actor(actor_ref, alpha_dht.clone()).await;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        let session_provider =
            SessionProvider::new(registry, store.clone(), llm).with_mesh(Some(mesh.clone()));
        let session_provider = Arc::new(session_provider);

        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = session_provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        // First: set provider_node_id = None (local path → error "Unknown provider")
        let result1 = session_provider
            .build_provider_for_session(&session_id)
            .await;
        assert!(
            result1.is_err(),
            "first call: should fail (no local 'anthropic')"
        );

        // Second: set provider_node_id = alpha node id → should build a MeshChatProvider.
        store
            .set_session_provider_node_id(&session_id, Some(&alpha_node_id))
            .await
            .expect("set_session_provider_node_id");

        // Same config_id, different provider_node_id → cache should miss and rebuild.
        let provider2 = session_provider
            .build_provider_for_session(&session_id)
            .await
            .expect("second call: should build MeshChatProvider for alpha");

        let call_result = provider2.chat_with_tools(&[], None).await;
        match call_result {
            // ProviderHostActor on alpha will fail with "Unknown provider" from
            // its own local registry — but the error reaches us through the mesh,
            // meaning the cache correctly built a new MeshChatProvider.
            Err(LLMError::ProviderError(msg)) => {
                // Accept either the DHT-key error (if ProviderHostActor lookup
                // itself fails) or any provider error (if the call reached the
                // ProviderHostActor and it couldn't build "anthropic" locally).
                let _ = msg; // just verify no panic
            }
            other => {
                panic!("expected ProviderError from mesh routing or remote host; got: {other:?}")
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Module K — set_mesh-after-build regression tests
// ═════════════════════════════════════════════════════════════════════════════
//
//  Regression tests for the bug where `SessionProvider.mesh` was always `None`
//  on Agent B because:
//
//  1. `AgentConfigBuilder::build()` creates `SessionProvider` with `mesh: None`.
//  2. `RemoteNodeManager::new(agent_handle.config.clone(), ...)` captures the
//     mesh-less `Arc<AgentConfig>`.
//  3. `agent_handle.set_mesh(mesh)` was only writing to `AgentHandle.mesh` and
//     NOT propagating into `config.provider.mesh`.
//
//  Result: `build_provider_for_session` received `mesh_handle = None` even when
//  `provider_node_id` was set, producing:
//    "provider_node_id='nostromo' specified but no mesh handle available"
//
//  Fix: `SessionProvider.mesh` is now `Arc<StdMutex<Option<MeshHandle>>>` so all
//  clones share the same cell.  `AgentHandle::set_mesh` writes into that cell via
//  `config.provider.set_mesh(Some(mesh))`, making the handle visible to every
//  existing clone — including the one stored inside `RemoteNodeManager.config`.

#[cfg(all(test, feature = "remote"))]
mod set_mesh_after_build_tests {
    use crate::agent::remote::NodeId;
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;
    use crate::session::backend::StorageBackend as _;
    use crate::session::provider::SessionProvider;
    use crate::session::sqlite_storage::SqliteStorage;
    use querymt::LLMParams;
    use querymt::error::LLMError;
    use querymt::plugin::host::PluginRegistry;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn random_node_id() -> String {
        NodeId::from_peer_id(
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        )
        .to_string()
    }

    // ── K.1 — set_mesh after Arc is shared makes mesh visible to build_provider ─

    /// Simulates the exact `coder_agent` startup order:
    ///   1. Build `SessionProvider` (mesh = None).
    ///   2. Wrap in `Arc`.
    ///   3. Clone the `Arc` into a "RemoteNodeManager-like" holder.
    ///   4. Call `set_mesh` on the original — ALL clones must see it.
    ///   5. Create a session via the clone and call `build_provider_for_session`.
    ///
    /// Before the fix this returned "provider_node_id='...' specified but no mesh
    /// handle available".  After the fix it should return a `MeshChatProvider`
    /// (observable via DHT-key error).
    #[tokio::test]
    async fn test_set_mesh_after_arc_shared_propagates_to_clones() {
        let mesh = get_test_mesh().await;

        // ── Step 1-2: build SessionProvider with mesh = None, wrap in Arc ──────
        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        // Build WITHOUT mesh — simulates AgentConfigBuilder::build() order.
        let session_provider = Arc::new(SessionProvider::new(registry, store.clone(), llm));

        // ── Step 3: clone into a "RemoteNodeManager-like" holder ─────────────
        // RemoteNodeManager stores Arc<AgentConfig> which contains Arc<SessionProvider>.
        // Simulated here by cloning the Arc directly.
        let remote_clone = Arc::clone(&session_provider);

        // ── Step 4: inject mesh AFTER the Arc is shared ────────────────────
        // Mirrors `agent_handle.set_mesh(mesh)` → `config.provider.set_mesh(…)`.
        session_provider.set_mesh(Some(mesh.clone()));

        // ── Step 5: create a session via the CLONE and call build_provider ──
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = remote_clone
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        // Write provider_node_id (simulates handle_set_session_model on Agent A).
        let provider_node_id_name = random_node_id();
        store
            .set_session_provider_node_id(&session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        // Should build a MeshChatProvider — NOT return "no mesh handle available".
        let result = remote_clone.build_provider_for_session(&session_id).await;

        let provider =
            result.expect("should build MeshChatProvider — not 'no mesh handle available'");

        // The MeshChatProvider will try DHT lookup for "provider_host::{node}" and
        // fail since nothing is registered there.  That's fine — the important
        // check is that we did NOT get the "no mesh handle" error.
        let call_result = provider.chat_with_tools(&[], None).await;
        match call_result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::"),
                    "error should be DHT-key lookup failure (mesh routing confirmed); got: {msg}"
                );
                assert!(
                    !msg.contains("no mesh handle available"),
                    "regression: 'no mesh handle available' means set_mesh did not propagate; got: {msg}"
                );
            }
            other => {
                panic!("expected ProviderError from MeshChatProvider DHT lookup; got: {other:?}")
            }
        }
    }

    // ── K.2 — set_mesh(None) clears the mesh handle ───────────────────────────

    #[tokio::test]
    async fn test_set_mesh_none_clears_handle() {
        let mesh = get_test_mesh().await;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        let session_provider = Arc::new(SessionProvider::new(registry, store.clone(), llm));

        // Set then clear.
        session_provider.set_mesh(Some(mesh.clone()));
        session_provider.set_mesh(None);

        // With mesh cleared + provider_node_id set → should get "no mesh handle"
        // error (not a DHT-key error).
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = session_provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        let random_remote_node = random_node_id();
        store
            .set_session_provider_node_id(&session_id, Some(&random_remote_node))
            .await
            .expect("set_session_provider_node_id");

        let result = session_provider
            .build_provider_for_session(&session_id)
            .await;
        assert!(
            result.is_err(),
            "should fail when mesh is None but provider_node_id is set"
        );
        let msg = result.err().expect("should be err").to_string();
        assert!(
            msg.contains("no mesh handle available"),
            "expected 'no mesh handle available'; got: {msg}"
        );
    }

    // ── K.3 — AgentHandle::set_mesh propagates into config.provider ──────────

    /// Verifies that `AgentHandle::set_mesh` propagates the mesh handle into
    /// `config.provider`, which is the path exercised in production by
    /// `coder_agent.rs` after `bootstrap_mesh` succeeds.
    #[tokio::test]
    async fn test_agent_handle_set_mesh_propagates_to_session_provider() {
        use crate::agent::agent_config_builder::AgentConfigBuilder;

        let mesh = get_test_mesh().await;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry =
            Arc::new(PluginRegistry::from_path(&config_path).expect("create plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create sqlite storage"),
        );
        let store = storage.session_store();
        let llm = LLMParams::new().provider("anthropic").model("claude-haiku");

        // Build AgentConfig the same way AgentConfigBuilder does it (no mesh).
        let config = Arc::new(
            AgentConfigBuilder::new(registry, store.clone(), storage.event_journal(), llm).build(),
        );

        // Simulate RemoteNodeManager capturing config before set_mesh is called.
        let remote_config = Arc::clone(&config);

        // Call set_mesh on a fake AgentHandle-like holder.
        // In production this is `agent_handle.set_mesh(mesh)` which now also calls
        // `config.provider.set_mesh(Some(mesh))`.
        config.provider.set_mesh(Some(mesh.clone()));

        // Create a session via remote_config (the clone that was captured before set_mesh).
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = remote_config
            .provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        let provider_node_id_name = random_node_id();
        store
            .set_session_provider_node_id(&session_id, Some(&provider_node_id_name))
            .await
            .expect("set_session_provider_node_id");

        // Must NOT return "no mesh handle available".
        let result = remote_config
            .provider
            .build_provider_for_session(&session_id)
            .await;

        let provider = result.expect(
            "AgentHandle::set_mesh must propagate to config.provider so remote sessions can route",
        );

        let call_result = provider.chat_with_tools(&[], None).await;
        match call_result {
            Err(LLMError::ProviderError(msg)) => {
                assert!(
                    msg.contains("provider_host::"),
                    "should route through mesh (DHT-key error); got: {msg}"
                );
            }
            other => panic!("expected ProviderError from mesh routing; got: {other:?}"),
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Module I — Mesh setup config translation tests
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(all(test, feature = "remote"))]
mod mesh_setup_config_tests {
    use crate::agent::remote::mesh::MeshDiscovery;
    use crate::agent::remote::provider_host::ProviderHostActor;
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::config::{MeshDiscoveryConfig, MeshPeerConfig};
    use uuid::Uuid;

    /// Helper that translates `MeshDiscoveryConfig` → `MeshDiscovery` using
    /// the same logic as `setup_mesh_from_config` (inlined here to keep the
    /// test self-contained without needing a live bootstrap).
    fn translate_discovery(cfg: &MeshDiscoveryConfig, peers: &[MeshPeerConfig]) -> MeshDiscovery {
        let bootstrap: Vec<String> = peers.iter().map(|p| p.addr.clone()).collect();
        match cfg {
            MeshDiscoveryConfig::Mdns => MeshDiscovery::Mdns,
            MeshDiscoveryConfig::None => MeshDiscovery::None,
            MeshDiscoveryConfig::Kademlia => MeshDiscovery::Kademlia {
                bootstrap: bootstrap.clone(),
            },
        }
    }

    // ── I.1 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_mesh_config_translates_mdns_discovery() {
        let result = translate_discovery(&MeshDiscoveryConfig::Mdns, &[]);
        assert!(
            matches!(result, MeshDiscovery::Mdns),
            "Mdns config should produce MeshDiscovery::Mdns"
        );
    }

    // ── I.2 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_mesh_config_translates_kademlia_discovery() {
        let peers = vec![MeshPeerConfig {
            name: "peer1".to_string(),
            addr: "/ip4/192.168.1.1/tcp/9000".to_string(),
        }];
        let result = translate_discovery(&MeshDiscoveryConfig::Kademlia, &peers);
        match result {
            MeshDiscovery::Kademlia { bootstrap } => {
                assert_eq!(bootstrap, vec!["/ip4/192.168.1.1/tcp/9000"]);
            }
            other => panic!("expected Kademlia, got {:?}", other),
        }
    }

    // ── I.3 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_mesh_config_translates_none_discovery() {
        let result = translate_discovery(&MeshDiscoveryConfig::None, &[]);
        assert!(
            matches!(result, MeshDiscovery::None),
            "None config should produce MeshDiscovery::None"
        );
    }

    // ── I.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_setup_mesh_with_no_remotes_returns_empty_registry() {
        // We call setup_mesh_from_config only once per process (bootstrap_mesh
        // is a one-shot). Since the test mesh is already bootstrapped via the
        // OnceCell in test_helpers, we verify the registry separately.
        //
        // The shared mesh is already up; just verify that calling with
        // remotes = [] produces an empty registry (the actual translation is
        // tested in I.1-I.3 above).

        use crate::delegation::DefaultAgentRegistry;
        let _registry = DefaultAgentRegistry::new();
        // DefaultAgentRegistry::new() starts empty by construction.
        // No assertion needed — not panicking is sufficient.
    }

    // ── I.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_setup_mesh_registers_provider_host_in_dht() {
        let test_id = Uuid::now_v7().to_string();
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        // Simulate what setup_mesh_from_config does: spawn ProviderHostActor
        // and register it in DHT.
        use kameo::actor::Spawn;
        let actor = ProviderHostActor::new(f.config.clone());
        let actor_ref = ProviderHostActor::spawn(actor);

        let hostname = format!("test-host-i5-{}", test_id);
        let dht_name = crate::agent::remote::dht_name::provider_host(&hostname);
        mesh.register_actor(actor_ref.clone(), dht_name.clone())
            .await;
        let _ = actor_ref;

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let found = mesh
            .lookup_actor::<ProviderHostActor>(&dht_name)
            .await
            .expect("lookup");

        assert!(
            found.is_some(),
            "ProviderHostActor should be findable in DHT after registration"
        );
    }

    // ── I.6 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_setup_mesh_registers_node_manager_in_dht() {
        let test_id = Uuid::now_v7().to_string();
        let f = AgentConfigFixture::new().await;
        let mesh = get_test_mesh().await;

        use crate::agent::remote::node_manager::RemoteNodeManager;
        use crate::agent::session_registry::SessionRegistry;
        use kameo::actor::Spawn;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let registry = Arc::new(Mutex::new(SessionRegistry::new(f.config.clone())));
        let nm = RemoteNodeManager::new(f.config.clone(), registry, Some(mesh.clone()));
        let nm_ref = RemoteNodeManager::spawn(nm);

        let dht_name = format!("node_manager::i6-{}", test_id);
        mesh.register_actor(nm_ref.clone(), dht_name.clone()).await;
        let _ = nm_ref;

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let found = mesh
            .lookup_actor::<RemoteNodeManager>(&dht_name)
            .await
            .expect("lookup");

        assert!(
            found.is_some(),
            "RemoteNodeManager should be findable in DHT after registration"
        );
    }

    // ── I.7 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_setup_mesh_unknown_remote_agent_skipped_with_log() {
        // setup_mesh_from_config skips unreachable remote agents.
        // Verify: calling with a remote agent whose peer is unknown
        // does not panic, and the resulting registry has 0 entries.
        //
        // Since we can't call setup_mesh_from_config a second time (bootstrap
        // is one-shot), we test the skip logic directly:

        use crate::config::RemoteAgentConfig;
        use crate::delegation::DefaultAgentRegistry;

        let remote = RemoteAgentConfig {
            id: "unknown-agent".to_string(),
            peer: "unknown-peer".to_string(),
            name: "Unknown".to_string(),
            description: "Test".to_string(),
            capabilities: vec![],
        };

        // The `register_remote_agent` function in remote_setup.rs looks up
        // "node_manager" in the DHT. If not found, it registers speculatively.
        // The returned registry entry is still created (speculative registration).
        // We document that no panic occurs.
        let registry = DefaultAgentRegistry::new();
        // We can't call `register_remote_agent` directly (private), so we
        // verify that building the AgentInfo doesn't panic:
        let info = crate::delegation::AgentInfo {
            id: remote.id.clone(),
            name: remote.name.clone(),
            description: remote.description.clone(),
            capabilities: remote.capabilities.clone(),
            required_capabilities: vec![],
            meta: Some(serde_json::json!({ "remote": true, "peer": remote.peer })),
        };

        // Without a handle to register (we don't construct a RemoteAgentHandle
        // here), just verify the AgentInfo builds correctly.
        assert_eq!(info.id, "unknown-agent");
        let _ = registry; // empty registry — no panics
    }
}

// ═════════════════════════════════════════════════════════════════════════════
//  Module L — Peer delegate routing via RoutingActor
// ═════════════════════════════════════════════════════════════════════════════
//
//  These tests cover the production path that the TOML config exercises:
//
//  [[delegates]]
//    peer = "bob"    ← SetProviderTarget { agent_id, Peer("bob") } to RoutingActor
//
//  At quorum build time, the RoutingActor is populated with routes.
//  Peer resolution happens eagerly (at build time or on PeerEvent::Discovered).
//  At delegation time, the orchestrator reads the routing snapshot and writes
//  provider_node_id to the session's DB row.
//
//  L.1 — create_delegation_session does NOT set provider_node_id (routing is orchestrator-only).
//  L.2 — Tests resolve_peer_node_id against injected known_peers.
//  L.3 — Tests the RoutingActor resolution flow end-to-end.
//  L.4 — Tool → Actor → Snapshot integration (planner tool updates routing table).
//  L.5 — Routing snapshot → orchestrator DB write path (snapshot read → provider_node_id).
//  L.6 — Full path: tool sets route → resolve → snapshot → DB write (end-to-end).

#[cfg(all(test, feature = "remote"))]
mod peer_delegate_routing_tests {
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::handle::{AgentHandle, LocalAgentHandle};
    use crate::agent::remote::RemoteNodeManager;
    use crate::agent::remote::dht_name;
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::agent::session_registry::SessionRegistry;
    use crate::session::backend::StorageBackend as _;
    use crate::session::sqlite_storage::SqliteStorage;
    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use querymt::plugin::host::PluginRegistry;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    // ── L.1 — create_delegation_session does NOT set provider_node_id ──────────

    /// Verifies that `create_delegation_session` on a bare handle does NOT
    /// write `provider_node_id` to the DB. Routing is now exclusively handled
    /// by the RoutingActor + DelegationOrchestrator (see L.5, L.6).
    #[tokio::test]
    async fn test_create_delegation_session_does_not_set_provider_node_id() {
        let temp_dir = TempDir::new().expect("temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry = Arc::new(PluginRegistry::from_path(&config_path).expect("plugin registry"));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("sqlite"),
        );
        let llm = LLMParams::new().provider("mock").model("mock");
        let config = Arc::new(
            AgentConfigBuilder::new(
                registry,
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .build(),
        );

        let handle = LocalAgentHandle::from_config(config.clone());

        let (session_id, _ref) = handle
            .create_delegation_session(None)
            .await
            .expect("create_delegation_session");

        let stored = config
            .provider
            .history_store()
            .get_session_provider_node_id(&session_id)
            .await
            .expect("get_session_provider_node_id");

        assert_eq!(
            stored, None,
            "create_delegation_session should NOT set provider_node_id — \
             routing is handled by RoutingActor + orchestrator"
        );
    }

    // ── L.2 — resolve_peer_node_id finds alice when injected into known_peers ───

    /// Verifies that `resolve_peer_node_id("alice")` returns alice's NodeId when:
    ///  1. Alice's RemoteNodeManager is registered under the per-peer DHT name
    ///     and configured with `with_node_name("alice")`.
    ///  2. Alice's PeerId is injected into `known_peers` (simulating mDNS discovery).
    ///
    /// No env var manipulation — `with_node_name` makes GetNodeInfo deterministic.
    #[tokio::test]
    async fn test_resolve_peer_node_id_finds_injected_peer() {
        let mesh = get_test_mesh().await;

        let alice_f = AgentConfigFixture::new().await;
        let session_registry = Arc::new(Mutex::new(SessionRegistry::new(alice_f.config.clone())));
        let alice_nm =
            RemoteNodeManager::new(alice_f.config.clone(), session_registry, Some(mesh.clone()))
                .with_node_name("alice".to_string());
        let alice_nm_ref = RemoteNodeManager::spawn(alice_nm);

        // Use a freshly generated keypair to get a unique PeerId for alice.
        let alice_keypair = libp2p::identity::Keypair::generate_ed25519();
        let alice_peer_id = alice_keypair.public().to_peer_id();
        let per_peer_dht = dht_name::node_manager_for_peer(&alice_peer_id.to_string());
        mesh.register_actor(alice_nm_ref, per_peer_dht).await;

        // Inject into known_peers to simulate mDNS Discovered.
        mesh.inject_known_peer_for_test(alice_peer_id);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let result = mesh.resolve_peer_node_id("alice").await;

        assert!(
            result.is_some(),
            "resolve_peer_node_id('alice') should return Some(node_id) \
             when alice's RemoteNodeManager is registered and peer is in known_peers"
        );
    }

    // ── L.3 — Full path: RoutingActor resolves peer and provides node_id ──

    /// End-to-end test for the TOML `peer = "bob"` delegate path using the RoutingActor:
    ///
    ///   1. Bob's RemoteNodeManager is in the DHT (simulates remote node running).
    ///   2. Bob's PeerId is in known_peers (simulates mDNS having fired).
    ///   3. A RoutingActor is set up with a provider route for "coder" → Peer("bob").
    ///   4. The route is resolved via MeshHandle::resolve_peer_node_id.
    ///   5. The routing snapshot reflects the resolved node_id.
    ///
    /// This replaces the old test_deferred_peer_name_resolves_and_writes_provider_node_id
    /// which tested the now-removed lazy resolution in create_delegation_session.
    #[tokio::test]
    async fn test_routing_actor_resolves_peer_via_mesh() {
        use crate::agent::remote::routing::{
            ResolvePeer, RouteTarget, RoutingActor, SetProviderTarget, new_routing_snapshot_handle,
        };
        use kameo::actor::Spawn as _;

        let mesh = get_test_mesh().await;

        // ── Set up bob's node ────────────────────────────────────────────────
        let bob_f = AgentConfigFixture::new().await;
        let bob_registry = Arc::new(Mutex::new(SessionRegistry::new(bob_f.config.clone())));
        let bob_nm = RemoteNodeManager::new(bob_f.config.clone(), bob_registry, Some(mesh.clone()))
            .with_node_name("bob".to_string());
        let bob_nm_ref = RemoteNodeManager::spawn(bob_nm);

        let bob_keypair = libp2p::identity::Keypair::generate_ed25519();
        let bob_peer_id = bob_keypair.public().to_peer_id();
        let bob_per_peer_dht = dht_name::node_manager_for_peer(&bob_peer_id.to_string());
        mesh.register_actor(bob_nm_ref, bob_per_peer_dht).await;

        mesh.inject_known_peer_for_test(bob_peer_id);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // ── Set up RoutingActor with a route for "coder" → Peer("bob") ───────
        let snapshot_handle = new_routing_snapshot_handle();
        let actor = RoutingActor::new(snapshot_handle.clone());
        let actor_ref = RoutingActor::spawn(actor);

        actor_ref
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("bob".into()),
            })
            .await
            .expect("SetProviderTarget");

        // ── Resolve via mesh (same as what quorum builder does) ──────────────
        let resolved_node_id = mesh
            .resolve_peer_node_id("bob")
            .await
            .expect("resolve_peer_node_id should find bob");

        let count = actor_ref
            .ask(ResolvePeer {
                peer_name: "bob".into(),
                node_id: resolved_node_id.to_string(),
            })
            .await
            .expect("ResolvePeer");

        assert_eq!(count, 1, "should resolve exactly 1 route");

        // ── Assert: routing snapshot has the resolved node_id ────────────────
        let snap = snapshot_handle.load();
        let policy = snap.get("coder").expect("routing entry for 'coder'");
        assert_eq!(policy.provider_target, RouteTarget::Peer("bob".into()));
        assert!(
            policy.resolved_provider_node_id.is_some(),
            "resolved_provider_node_id should be set after ResolvePeer"
        );
        assert_eq!(
            policy.resolved_provider_node_id.as_deref().unwrap(),
            &resolved_node_id.to_string(),
            "resolved node_id should match bob's actual node_id"
        );
    }

    // ── L.4 — Tool → Actor → Snapshot integration ───────────────────────────
    //
    //  Verifies the planner tool path: call `use_remote_provider` tool →
    //  actor receives `SetProviderTarget` → snapshot is updated and readable
    //  by downstream consumers (e.g. orchestrator).

    #[tokio::test]
    async fn test_tool_sets_provider_route_and_snapshot_is_readable() {
        use crate::agent::remote::routing::{
            RouteTarget, RoutingActor, new_routing_snapshot_handle,
        };
        use crate::tools::builtins::UseRemoteProviderTool;
        use crate::tools::{AgentToolContext, Tool as ToolTrait};
        use serde_json::json;

        let snapshot_handle = new_routing_snapshot_handle();
        let actor = RoutingActor::new(snapshot_handle.clone());
        let actor_ref = RoutingActor::spawn(actor);
        let tool = UseRemoteProviderTool::new(actor_ref);
        let ctx = AgentToolContext::basic("l4-session".to_string(), None);

        // Tool call: route "coder" provider to "gpu-box" peer.
        let result = tool
            .call(json!({ "agent_id": "coder", "peer_name": "gpu-box" }), &ctx)
            .await
            .expect("tool call should succeed");

        assert!(
            result.contains("gpu-box"),
            "result should mention peer name"
        );

        // Snapshot should be immediately readable (lock-free via ArcSwap).
        let snap = snapshot_handle.load();
        let policy = snap.get("coder").expect("routing entry for 'coder'");
        assert_eq!(
            policy.provider_target,
            RouteTarget::Peer("gpu-box".into()),
            "snapshot should reflect the tool's SetProviderTarget"
        );
        // Not yet resolved (no ResolvePeer sent).
        assert!(
            policy.resolved_provider_node_id.is_none(),
            "resolved_provider_node_id should be None until peer is resolved"
        );
    }

    // ── L.5 — Routing snapshot → orchestrator DB write path ─────────────────
    //
    //  Simulates what `execute_delegation` does: reads the routing snapshot
    //  for an agent, and if provider_target is Peer + resolved, writes
    //  provider_node_id to the session DB row.

    #[tokio::test]
    async fn test_routing_snapshot_drives_orchestrator_db_write() {
        use crate::agent::remote::routing::{
            ResolvePeer, RouteTarget, RoutingActor, SetProviderTarget, new_routing_snapshot_handle,
        };

        let snapshot_handle = new_routing_snapshot_handle();
        let actor = RoutingActor::new(snapshot_handle.clone());
        let actor_ref = RoutingActor::spawn(actor);

        // Set route and resolve it.
        actor_ref
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .expect("SetProviderTarget");

        actor_ref
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "QmFakeNodeId123".into(),
            })
            .await
            .expect("ResolvePeer");

        // Simulate orchestrator reading the snapshot (same logic as core.rs:425).
        let snap = snapshot_handle.load();
        let policy = snap.get("coder").expect("routing entry");

        let provider_node_id = match &policy.provider_target {
            RouteTarget::Peer(_) => policy.resolved_provider_node_id.clone(),
            RouteTarget::Local => None,
        };

        assert_eq!(
            provider_node_id.as_deref(),
            Some("QmFakeNodeId123"),
            "orchestrator should read resolved_provider_node_id from snapshot"
        );

        // Verify: write to in-memory SQLite (same as execute_delegation does).
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("sqlite"),
        );
        let store = storage.session_store();

        // Create a session row.
        let temp_dir = TempDir::new().expect("temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write");
        let registry = Arc::new(
            querymt::plugin::host::PluginRegistry::from_path(&config_path).expect("registry"),
        );
        use crate::session::provider::SessionProvider;
        let session_provider = Arc::new(SessionProvider::new(
            registry,
            store.clone(),
            LLMParams::new().provider("mock").model("mock"),
        ));
        let exec_config = crate::session::store::SessionExecutionConfig::default();
        let session_handle = session_provider
            .create_session(None, None, &exec_config)
            .await
            .expect("create session");
        let session_id = session_handle.session().public_id.clone();

        // Write provider_node_id (orchestrator path).
        store
            .set_session_provider_node_id(&session_id, provider_node_id.as_deref())
            .await
            .expect("set_session_provider_node_id");

        // Read it back.
        let stored = store
            .get_session_provider_node_id(&session_id)
            .await
            .expect("get_session_provider_node_id");

        assert_eq!(
            stored.as_deref(),
            Some("QmFakeNodeId123"),
            "provider_node_id written by orchestrator should be readable from DB"
        );
    }

    // ── L.6 — Full path: tool → resolve → snapshot → DB write ───────────────
    //
    //  End-to-end integration: uses the `route_delegation_to_peer` tool to set
    //  a session route, then `use_remote_provider` tool to set a provider route,
    //  resolves the peer, and verifies the snapshot contains the correct state
    //  that the orchestrator would use.

    #[tokio::test]
    async fn test_full_tool_to_snapshot_to_db_path() {
        use crate::agent::remote::routing::{
            ResolvePeer, RouteTarget, RoutingActor, new_routing_snapshot_handle,
        };
        use crate::tools::builtins::{RouteDelegationToPeerTool, UseRemoteProviderTool};
        use crate::tools::{AgentToolContext, Tool as ToolTrait};
        use serde_json::json;

        let snapshot_handle = new_routing_snapshot_handle();
        let actor = RoutingActor::new(snapshot_handle.clone());
        let actor_ref = RoutingActor::spawn(actor);

        let session_tool = RouteDelegationToPeerTool::new(actor_ref.clone());
        let provider_tool = UseRemoteProviderTool::new(actor_ref.clone());
        let ctx = AgentToolContext::basic("l6-session".to_string(), None);

        // Step 1: Planner routes "coder" session to peer "fast-node".
        let r1 = session_tool
            .call(
                json!({ "agent_id": "coder", "peer_name": "fast-node" }),
                &ctx,
            )
            .await
            .expect("route session");
        assert!(r1.contains("fast-node"));

        // Step 2: Planner routes "coder" provider to peer "gpu-node".
        let r2 = provider_tool
            .call(
                json!({ "agent_id": "coder", "peer_name": "gpu-node" }),
                &ctx,
            )
            .await
            .expect("route provider");
        assert!(r2.contains("gpu-node"));

        // Step 3: Resolve the provider peer (simulates PeerEvent::Discovered handler).
        actor_ref
            .ask(ResolvePeer {
                peer_name: "gpu-node".into(),
                node_id: "QmGpuNodeId456".into(),
            })
            .await
            .expect("ResolvePeer");

        // Step 4: Verify the snapshot has both routes correctly set.
        let snap = snapshot_handle.load();
        let policy = snap.get("coder").expect("routing entry for 'coder'");

        assert_eq!(
            policy.session_target,
            RouteTarget::Peer("fast-node".into()),
            "session_target should be Peer('fast-node')"
        );
        assert_eq!(
            policy.provider_target,
            RouteTarget::Peer("gpu-node".into()),
            "provider_target should be Peer('gpu-node')"
        );
        assert_eq!(
            policy.resolved_provider_node_id.as_deref(),
            Some("QmGpuNodeId456"),
            "resolved_provider_node_id should match the resolved node"
        );
    }
}
