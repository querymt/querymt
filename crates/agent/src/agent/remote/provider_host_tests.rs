//! Module A — `ProviderHostActor` unit tests.
//!
//! Tests `ProviderChatResponse`, `StreamChunkRelay`, and `ProviderHostActor`
//! in isolation. No mesh required — all actors are local `ActorRef`s.
//!
//! Bug exposed: **#7** — `ProviderChatResponse::finish_reason` uses a
//! string-match that must align with `format!("{:?}", FinishReason::*)`.

#[cfg(all(test, feature = "remote"))]
#[allow(clippy::module_inception)]
mod provider_host_tests {
    // ── A.0 — SessionProvider::initial_params() ───────────────────────────────
    //
    // Regression test for the bug where `ProviderHostActor::get_or_build_provider`
    // passed `None` for the `params` argument, causing providers like `llama_cpp`
    // to receive only the bare friendly model name (e.g. `"qwen3-coder"`) without
    // the `[agent.parameters]` overrides (e.g. `model = "hf:owner/repo:file.gguf"`).
    //
    // We verify two things:
    //   1. `SessionProvider::initial_params()` exposes the custom params.
    //   2. When those params are present the `ProviderHostActor` forwards them:
    //      the error it produces changes from an "invalid model format" error (old
    //      behaviour, params == None) to an "unknown provider" error (new behaviour,
    //      params forwarded but plugin not registered in the test registry).

    #[tokio::test]
    async fn test_session_provider_initial_params_exposed() {
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::session::backend::StorageBackend as _;
        use crate::session::sqlite_storage::SqliteStorage;
        use querymt::LLMParams;
        use querymt::plugin::host::PluginRegistry;
        use std::sync::Arc;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry = Arc::new(PluginRegistry::from_path(&config_path).expect("registry"));

        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("sqlite"),
        );

        let llm = LLMParams::new()
            .provider("llama_cpp")
            .model("qwen3-coder")
            // Simulate the `[agent.parameters] model = "hf:..."` TOML entry.
            .parameter("model", "hf:owner/repo:model.gguf")
            .parameter("n_ctx", 32768_u64);

        let config = Arc::new(
            AgentConfigBuilder::new(
                registry,
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .build(),
        );

        let params = config.provider.initial_params();
        assert_eq!(params.provider.as_deref(), Some("llama_cpp"));
        assert_eq!(params.model.as_deref(), Some("qwen3-coder"));

        let custom = params
            .custom
            .as_ref()
            .expect("custom params must be present");
        assert_eq!(
            custom.get("model").and_then(|v| v.as_str()),
            Some("hf:owner/repo:model.gguf"),
            "model override must be in custom params"
        );
        assert!(
            custom.contains_key("n_ctx"),
            "n_ctx must be in custom params"
        );
    }

    #[tokio::test]
    async fn test_provider_host_actor_forwards_params_to_build() {
        // When the AgentConfig carries custom params (e.g. `model = "hf:..."`)
        // the ProviderHostActor must forward them.  Because `llama_cpp` is not
        // registered in the test plugin registry the call still fails, but the
        // error must be an "unknown provider" failure — not the old
        // "model must be a local .gguf path" parse error that occurred when
        // params were silently dropped.
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::agent::remote::provider_host::{ProviderChatRequest, ProviderHostActor};
        use crate::session::backend::StorageBackend as _;
        use crate::session::sqlite_storage::SqliteStorage;
        use kameo::actor::Spawn;
        use kameo::error::SendError;
        use querymt::LLMParams;
        use querymt::plugin::host::PluginRegistry;
        use std::sync::Arc;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().expect("create temp dir");
        let config_path = temp_dir.path().join("providers.toml");
        std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
        let registry = Arc::new(PluginRegistry::from_path(&config_path).expect("registry"));

        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("sqlite"),
        );

        let llm = LLMParams::new()
            .provider("llama_cpp")
            .model("qwen3-coder")
            .parameter("model", "hf:owner/repo:model.gguf")
            .parameter("n_ctx", 32768_u64);

        let config = Arc::new(
            AgentConfigBuilder::new(
                registry,
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .build(),
        );

        let actor = ProviderHostActor::new(config);
        let actor_ref = ProviderHostActor::spawn(actor);

        let req = ProviderChatRequest {
            provider: "llama_cpp".to_string(),
            model: "qwen3-coder".to_string(),
            messages: vec![],
            tools: None,
        };

        let result = actor_ref.ask(req).await;

        match result {
            Err(SendError::HandlerError(e)) => {
                let msg = e.to_string();
                // With the fix, params are forwarded so the provider factory is looked up.
                // Since llama_cpp is not registered in the test registry we get
                // "Unknown provider" — not the old "model must be a local .gguf path" error.
                assert!(
                    msg.contains("llama_cpp")
                        || msg.contains("provider")
                        || msg.contains("Unknown"),
                    "error should mention provider lookup failure, not model format: {}",
                    msg
                );
                assert!(
                    !msg.contains("model must be a local .gguf path")
                        && !msg.contains("<repo>:<selector>")
                        && !msg.contains("<owner>/<repo>"),
                    "params should have been forwarded — model parse error must not appear: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected an error — llama_cpp is not registered in test registry"),
            Err(e) => panic!("unexpected error variant: {:?}", e),
        }
    }
    use crate::agent::remote::provider_host::{
        ProviderChatRequest, ProviderChatResponse, StreamChunkRelay, StreamReceiverActor,
    };
    use crate::agent::remote::test_helpers::fixtures::ProviderHostFixture;
    use kameo::actor::Spawn;
    use querymt::chat::{ChatResponse, FinishReason, StreamChunk};
    use querymt::{FunctionCall, ToolCall};
    use tokio::sync::mpsc;

    // ── A.1 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_provider_chat_response_text_roundtrip() {
        let resp = ProviderChatResponse {
            text: Some("hello world".to_string()),
            thinking: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("Stop".to_string()),
        };
        assert_eq!(resp.text(), Some("hello world".to_string()));
    }

    // ── A.2 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_provider_chat_response_tool_calls_empty_is_none() {
        let resp = ProviderChatResponse {
            text: None,
            thinking: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: None,
        };
        assert!(
            resp.tool_calls().is_none(),
            "empty tool_calls vec should yield None"
        );
    }

    // ── A.3 — Bug #7 ─────────────────────────────────────────────────────────

    /// Verify that every `FinishReason` variant round-trips through the
    /// `format!("{:?}", r)` / string-match path used by `ProviderHostActor`
    /// and `ProviderChatResponse::finish_reason()`.
    ///
    /// The host serializes with `format!("{:?}", reason)`.
    /// The client deserializes with a `match` on the string.
    /// This test guarantees the two sides stay in sync.
    #[test]
    fn test_provider_chat_response_finish_reason_all_variants() {
        let cases: &[(FinishReason, &str)] = &[
            (FinishReason::Stop, "Stop"),
            (FinishReason::Length, "Length"),
            (FinishReason::ContentFilter, "ContentFilter"),
            (FinishReason::ToolCalls, "ToolCalls"),
            (FinishReason::Error, "Error"),
            (FinishReason::Other, "Other"),
        ];

        for (variant, expected_str) in cases {
            // Confirm the host-side serialization.
            let serialized = format!("{:?}", variant);
            assert_eq!(
                &serialized, expected_str,
                "Debug output for {:?} was '{}', expected '{}'",
                variant, serialized, expected_str
            );

            // Confirm the client-side deserialization round-trips.
            let resp = ProviderChatResponse {
                text: None,
                thinking: None,
                tool_calls: vec![],
                usage: None,
                finish_reason: Some(expected_str.to_string()),
            };
            let roundtripped = resp.finish_reason().expect("should be Some");
            // Compare via Debug string since FinishReason may not be PartialEq.
            assert_eq!(
                format!("{:?}", roundtripped),
                format!("{:?}", variant),
                "finish_reason() round-trip failed for '{}'",
                expected_str
            );
        }
    }

    // ── A.4 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_provider_chat_response_unknown_finish_reason() {
        let resp = ProviderChatResponse {
            text: None,
            thinking: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: Some("GibberishReason".to_string()),
        };
        let reason = resp.finish_reason().expect("should be Some");
        assert_eq!(
            format!("{:?}", reason),
            format!("{:?}", FinishReason::Unknown)
        );
    }

    // ── A.5 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_provider_chat_response_serde_roundtrip() {
        let original = ProviderChatResponse {
            text: Some("serde test".to_string()),
            thinking: Some("some thought".to_string()),
            tool_calls: vec![],
            usage: Some(querymt::Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            finish_reason: Some("Stop".to_string()),
        };

        let serialized = serde_json::to_string(&original).expect("serialize");
        let deserialized: ProviderChatResponse =
            serde_json::from_str(&serialized).expect("deserialize");

        assert_eq!(deserialized.text, original.text);
        assert_eq!(deserialized.thinking, original.thinking);
        assert_eq!(deserialized.finish_reason, original.finish_reason);
        assert_eq!(
            deserialized.usage.as_ref().map(|u| u.input_tokens),
            original.usage.as_ref().map(|u| u.input_tokens),
        );
    }

    // ── A.6 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_stream_chunk_relay_ok_roundtrip() {
        let relay = StreamChunkRelay {
            chunk: Ok(StreamChunk::Text("delta text".to_string())),
        };
        let json = serde_json::to_string(&relay).expect("serialize");
        let back: StreamChunkRelay = serde_json::from_str(&json).expect("deserialize");
        match back.chunk {
            Ok(StreamChunk::Text(t)) => {
                assert_eq!(t, "delta text");
            }
            other => panic!("expected Text chunk, got {:?}", other),
        }
    }

    // ── A.7 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_stream_chunk_relay_err_roundtrip() {
        let relay = StreamChunkRelay {
            chunk: Err("oops".to_string()),
        };
        let json = serde_json::to_string(&relay).expect("serialize");
        let back: StreamChunkRelay = serde_json::from_str(&json).expect("deserialize");
        assert!(back.chunk.is_err());
        assert_eq!(back.chunk.unwrap_err(), "oops");
    }

    // ── A.8 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_provider_host_actor_unknown_provider_returns_error() {
        let f = ProviderHostFixture::new().await;
        let req = ProviderChatRequest {
            provider: "nonexistent_provider_xyz".to_string(),
            model: "no-model".to_string(),
            messages: vec![],
            tools: None,
        };
        let result = f.actor_ref.ask(req).await;
        // The ask returns Result<Result<ProviderChatResponse, AgentError>, SendError>
        // The inner handler error comes back as SendError::HandlerError.
        use kameo::error::SendError;
        match result {
            Err(SendError::HandlerError(e)) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("nonexistent_provider_xyz") || msg.contains("provider"),
                    "error should mention the provider: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected an error for unknown provider"),
            Err(e) => panic!("unexpected error variant: {:?}", e),
        }
    }

    // ── A.9 ──────────────────────────────────────────────────────────────────
    //
    // Provider caching is an internal detail — verified indirectly: two asks
    // with the same (provider, model) both succeed (or both fail consistently)
    // without a panic on the second call.  A real cache hit test would require
    // a mock registry; this test documents the call pattern and ensures the
    // actor stays alive across repeated calls.

    #[tokio::test]
    async fn test_provider_host_actor_caches_provider() {
        let f = ProviderHostFixture::new().await;

        let req = ProviderChatRequest {
            provider: "nonexistent_provider_xyz".to_string(),
            model: "no-model".to_string(),
            messages: vec![],
            tools: None,
        };

        // Both calls fail for the same reason (provider not found).
        // The second call must not panic or produce a different error type —
        // confirming the actor processes both messages (no crash/restart).
        let r1 = f.actor_ref.ask(req.clone()).await;
        let r2 = f.actor_ref.ask(req.clone()).await;

        assert!(r1.is_err(), "first call should fail (provider not found)");
        assert!(r2.is_err(), "second call should fail (same provider)");
    }

    // ── A.10 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_receiver_actor_kill_on_done_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let actor = StreamReceiverActor::new(tx, "test-done".to_string());
        let actor_ref = StreamReceiverActor::spawn(actor);

        let done_relay = StreamChunkRelay {
            chunk: Ok(StreamChunk::Done {
                stop_reason: "end_turn".to_string(),
            }),
        };

        actor_ref
            .tell(done_relay)
            .await
            .expect("tell should succeed");

        // Give the actor time to process and stop.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The channel should have received the Done chunk.
        let received = rx.try_recv().expect("should have received Done chunk");
        assert!(received.is_ok(), "expected Ok chunk, got {:?}", received);

        // The actor should have killed itself — subsequent tells return an error.
        let extra = StreamChunkRelay {
            chunk: Ok(StreamChunk::Text("should be dropped".to_string())),
        };
        let send_result = actor_ref.tell(extra).await;
        assert!(
            send_result.is_err(),
            "actor should be dead after Done chunk"
        );
    }

    // ── A.11 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_receiver_actor_kill_on_error_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let actor = StreamReceiverActor::new(tx, "test-err".to_string());
        let actor_ref = StreamReceiverActor::spawn(actor);

        let error_relay = StreamChunkRelay {
            chunk: Err("boom".to_string()),
        };

        actor_ref
            .tell(error_relay)
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let received = rx.try_recv().expect("should have received error chunk");
        assert!(received.is_err());
        assert_eq!(received.unwrap_err(), "boom");

        // Actor should be dead.
        let extra = StreamChunkRelay {
            chunk: Ok(StreamChunk::Text("should be dropped".to_string())),
        };
        assert!(actor_ref.tell(extra).await.is_err());
    }

    // ── A.12 ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_provider_chat_response_display_impl() {
        let with_text = ProviderChatResponse {
            text: Some("my response".to_string()),
            thinking: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: None,
        };
        assert_eq!(with_text.to_string(), "my response");

        let no_text = ProviderChatResponse {
            text: None,
            thinking: None,
            tool_calls: vec![],
            usage: None,
            finish_reason: None,
        };
        assert_eq!(no_text.to_string(), "[no text]");
    }

    // ── A.3 supplemental — verify ToolCall round-trip ────────────────────────

    #[test]
    fn test_provider_chat_response_tool_calls_nonempty_is_some() {
        let tc = ToolCall {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "my_tool".to_string(),
                arguments: "{}".to_string(),
            },
        };
        let resp = ProviderChatResponse {
            text: None,
            thinking: None,
            tool_calls: vec![tc.clone()],
            usage: None,
            finish_reason: None,
        };
        let returned = resp.tool_calls().expect("should be Some");
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].function.name, "my_tool");
    }
}
