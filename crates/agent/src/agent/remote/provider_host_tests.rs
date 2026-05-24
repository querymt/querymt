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
    use tokio::sync::mpsc;
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
            params: None,
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
        CancelProviderStreamRequest, GetProviderStreamStatus, ProviderChatRequest,
        ProviderChatResponse, ProviderStreamPhase, ProviderStreamStatus, RenewProviderStreamLease,
        StreamChunkRelay, StreamReceiverActor, StreamRelayMessage, keep_stream_message_buffered,
        relay_message_is_terminal,
    };
    use crate::agent::remote::test_helpers::fixtures::ProviderHostFixture;
    use kameo::actor::Spawn;
    use querymt::chat::{ChatResponse, FinishReason, StreamChunk};
    use querymt::{FunctionCall, ToolCall};

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
            message: StreamRelayMessage::Chunk(StreamChunk::Text("delta text".to_string())),
        };
        let json = serde_json::to_string(&relay).expect("serialize");
        let back: StreamChunkRelay = serde_json::from_str(&json).expect("deserialize");
        match back.message {
            StreamRelayMessage::Chunk(StreamChunk::Text(t)) => {
                assert_eq!(t, "delta text");
            }
            other => panic!("expected Text chunk, got {:?}", other),
        }
    }

    #[test]
    fn test_stream_chunk_relay_batch_roundtrip() {
        let relay = StreamChunkRelay {
            message: StreamRelayMessage::ChunkBatch(vec![
                StreamChunk::Text("delta text".to_string()),
                StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                },
            ]),
        };
        let json = serde_json::to_string(&relay).expect("serialize");
        let back: StreamChunkRelay = serde_json::from_str(&json).expect("deserialize");
        match back.message {
            StreamRelayMessage::ChunkBatch(chunks) => {
                assert_eq!(chunks.len(), 2);
                assert!(matches!(chunks[0], StreamChunk::Text(_)));
                assert!(matches!(chunks[1], StreamChunk::Done { .. }));
            }
            other => panic!("expected ChunkBatch, got {:?}", other),
        }
    }

    // ── A.7 ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_stream_chunk_relay_err_roundtrip() {
        let relay = StreamChunkRelay {
            message: StreamRelayMessage::ProviderError {
                error: querymt::error::LLMError::ProviderError("oops".to_string()).to_payload(),
            },
        };
        let json = serde_json::to_string(&relay).expect("serialize");
        let back: StreamChunkRelay = serde_json::from_str(&json).expect("deserialize");
        match back.message {
            StreamRelayMessage::ProviderError { error } => {
                assert_eq!(
                    querymt::error::LLMError::from_payload(error).to_string(),
                    "LLM Provider Error: oops"
                )
            }
            other => panic!("expected ProviderError, got {:?}", other),
        }
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
            params: None,
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
            params: None,
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
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);

        let done_relay = StreamChunkRelay {
            message: StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: querymt::chat::FinishReason::Stop,
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
        assert!(
            matches!(
                received,
                StreamRelayMessage::Chunk(StreamChunk::Done { .. })
            ),
            "expected Done chunk, got {:?}",
            received
        );

        // The actor should have killed itself — subsequent tells return an error.
        let extra = StreamChunkRelay {
            message: StreamRelayMessage::Chunk(StreamChunk::Text("should be dropped".to_string())),
        };
        let send_result = actor_ref.tell(extra).await;
        assert!(
            send_result.is_err(),
            "actor should be dead after Done chunk"
        );
    }

    #[tokio::test]
    async fn test_stream_receiver_actor_kill_on_done_batch() {
        let (tx, mut rx) = mpsc::channel(8);
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);

        let done_relay = StreamChunkRelay {
            message: StreamRelayMessage::ChunkBatch(vec![
                StreamChunk::Text("delta".to_string()),
                StreamChunk::Done {
                    finish_reason: querymt::chat::FinishReason::Stop,
                },
            ]),
        };

        actor_ref
            .tell(done_relay)
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let received = rx.try_recv().expect("should have received done batch");
        assert!(
            matches!(received, StreamRelayMessage::ChunkBatch(ref chunks) if chunks.len() == 2),
            "expected done batch, got {:?}",
            received
        );

        let extra = StreamChunkRelay {
            message: StreamRelayMessage::Chunk(StreamChunk::Text("should be dropped".to_string())),
        };
        let send_result = actor_ref.tell(extra).await;
        assert!(
            send_result.is_err(),
            "actor should be dead after Done batch"
        );
    }

    // ── A.11 ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_receiver_actor_kill_on_error_chunk() {
        let (tx, mut rx) = mpsc::channel(8);
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);

        let error_relay = StreamChunkRelay {
            message: StreamRelayMessage::ProviderError {
                error: querymt::error::LLMError::ProviderError("boom".to_string()).to_payload(),
            },
        };

        actor_ref
            .tell(error_relay)
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let received = rx.try_recv().expect("should have received error chunk");
        match received {
            StreamRelayMessage::ProviderError { error } => {
                assert_eq!(
                    querymt::error::LLMError::from_payload(error).to_string(),
                    "LLM Provider Error: boom"
                )
            }
            other => panic!("expected ProviderError, got {:?}", other),
        }

        // Actor should be dead.
        let extra = StreamChunkRelay {
            message: StreamRelayMessage::Chunk(StreamChunk::Text("should be dropped".to_string())),
        };
        assert!(actor_ref.tell(extra).await.is_err());
    }

    #[tokio::test]
    async fn test_stream_receiver_actor_keeps_running_on_heartbeat() {
        let (tx, mut rx) = mpsc::channel(8);
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);

        actor_ref
            .tell(StreamChunkRelay {
                message: StreamRelayMessage::Heartbeat {
                    phase: ProviderStreamPhase::WaitingFirstChunk,
                    elapsed_ms: 1500,
                    idle_ms: 1500,
                    chunk_count: 0,
                },
            })
            .await
            .expect("tell should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let received = rx.try_recv().expect("should receive heartbeat");
        assert!(matches!(
            received,
            StreamRelayMessage::Heartbeat {
                phase: ProviderStreamPhase::WaitingFirstChunk,
                elapsed_ms: 1500,
                idle_ms: 1500,
                chunk_count: 0,
            }
        ));

        actor_ref
            .tell(StreamChunkRelay {
                message: StreamRelayMessage::Chunk(StreamChunk::Text("still alive".to_string())),
            })
            .await
            .expect("actor should remain alive after heartbeat");
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

    // ── A.13 — Bug: system prompt dropped for remote delegates ─────────────
    //
    // `get_or_build_provider` serialised only `initial_params.custom`
    // (the `#[serde(flatten)]` HashMap), silently dropping the dedicated
    // `system: Vec<String>` field.  Remote/mesh delegates therefore never
    // received their configured system prompt.
    //
    // This test calls the extracted helper `params_for_remote_provider`
    // and asserts that `system` survives serialisation.

    #[test]
    fn test_params_for_remote_provider_includes_system_prompt() {
        use crate::agent::remote::provider_host::params_for_remote_provider;
        use querymt::LLMParams;

        let llm = LLMParams::new()
            .provider("llama_cpp")
            .model("test-model")
            .system("You are a code expert.")
            .parameter("n_ctx", 32768_u64);

        let params_json = params_for_remote_provider(&llm);

        let obj = params_json
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .expect("params should serialize to a JSON object");

        // System prompt must be forwarded.
        let system = obj
            .get("system")
            .and_then(serde_json::Value::as_array)
            .expect("system key must be present as an array");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].as_str(), Some("You are a code expert."));

        // Custom params must still be forwarded.
        assert_eq!(
            obj.get("n_ctx").and_then(serde_json::Value::as_i64),
            Some(32768)
        );

        // Sensitive / separately-handled fields must be excluded.
        assert!(!obj.contains_key("provider"), "provider must be excluded");
        assert!(!obj.contains_key("model"), "model must be excluded");
        assert!(!obj.contains_key("api_key"), "api_key must be excluded");
        assert!(!obj.contains_key("name"), "name must be excluded");
    }

    #[test]
    fn test_params_for_remote_provider_empty_when_no_extra_fields() {
        use crate::agent::remote::provider_host::params_for_remote_provider;
        use querymt::LLMParams;

        let llm = LLMParams::new().provider("openai").model("gpt-4");

        let params_json = params_for_remote_provider(&llm);
        assert!(
            params_json.is_none(),
            "params should be None when only provider/model are set"
        );
    }

    #[test]
    fn test_params_for_remote_provider_strips_api_key() {
        use crate::agent::remote::provider_host::params_for_remote_provider;
        use querymt::LLMParams;

        let llm = LLMParams::new()
            .provider("anthropic")
            .model("claude-sonnet-4-20250514")
            .api_key("sk-secret-123")
            .system("Be helpful.");

        let params_json = params_for_remote_provider(&llm);

        let obj = params_json
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .expect("params should serialize to a JSON object");

        assert!(
            !obj.contains_key("api_key"),
            "api_key must never be forwarded"
        );
        assert!(obj.contains_key("system"), "system should be present");
    }

    // ── A.14 — ProviderChatRequest params serde round-trip ────────────────────

    #[test]
    fn test_provider_chat_request_params_roundtrip() {
        let req = ProviderChatRequest {
            provider: "llama_cpp".to_string(),
            model: "test-model".to_string(),
            messages: vec![],
            tools: None,
            params: Some(serde_json::json!({
                "system": ["You are a code expert."],
                "temperature": 0.3,
                "n_ctx": 32768
            })),
        };

        let json = serde_json::to_string(&req).expect("serialize");
        let back: ProviderChatRequest = serde_json::from_str(&json).expect("deserialize");
        assert!(back.params.is_some());
        let params = back.params.unwrap();
        assert_eq!(
            params
                .get("system")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            params.get("temperature").and_then(|v| v.as_f64()),
            Some(0.3)
        );

        // RemoteActorRef doesn't roundtrip through JSON serialization
        // Skip the stream request test since RemoteActorRef cannot be easily constructed in tests
        // The actual direct handoff is tested in integration tests
    }

    #[test]
    fn test_provider_stream_control_messages_roundtrip() {
        let cancel = CancelProviderStreamRequest {
            session_id: "session-test".to_string(),
            request_id: Some("request-test".to_string()),
            reason: Some("manual stop".to_string()),
        };
        let cancel_json = serde_json::to_string(&cancel).expect("serialize cancel");
        let cancel_back: CancelProviderStreamRequest =
            serde_json::from_str(&cancel_json).expect("deserialize cancel");
        assert_eq!(cancel_back.request_id.as_deref(), Some("request-test"));

        let renew = RenewProviderStreamLease {
            session_id: "session-test".to_string(),
            request_id: "request-test".to_string(),
            lease_ttl_secs: 45,
        };
        let renew_json = serde_json::to_string(&renew).expect("serialize renew");
        let renew_back: RenewProviderStreamLease =
            serde_json::from_str(&renew_json).expect("deserialize renew");
        assert_eq!(renew_back.lease_ttl_secs, 45);

        let status_req = GetProviderStreamStatus {
            session_id: "session-test".to_string(),
            request_id: Some("request-test".to_string()),
        };
        let status_json = serde_json::to_string(&status_req).expect("serialize status req");
        let status_back: GetProviderStreamStatus =
            serde_json::from_str(&status_json).expect("deserialize status req");
        assert_eq!(status_back.request_id.as_deref(), Some("request-test"));
    }

    // ── A.15 — params: None backward compatibility ────────────────────────────

    #[test]
    fn test_provider_chat_request_params_none_backward_compat() {
        // JSON without "params" field should deserialize with params = None
        let json = r#"{"provider":"test","model":"m","messages":[],"tools":null}"#;
        let req: ProviderChatRequest = serde_json::from_str(json).expect("deserialize");
        assert!(
            req.params.is_none(),
            "missing params field should deserialize as None"
        );
    }

    // ── A.16 — params skipped when None in serialization ──────────────────────

    #[test]
    fn test_provider_chat_request_params_none_not_serialized() {
        let req = ProviderChatRequest {
            provider: "test".to_string(),
            model: "m".to_string(),
            messages: vec![],
            tools: None,
            params: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("\"params\""),
            "params: None should be skipped in serialization, got: {}",
            json
        );
    }

    // ── A.17 — merge_params tests ─────────────────────────────────────────────

    #[test]
    fn test_merge_params_request_overrides_host_defaults() {
        use crate::agent::remote::provider_host::merge_params;

        let host = serde_json::json!({
            "n_ctx": 32768,
            "temperature": 0.7,
            "model": "hf:owner/repo:model.gguf"
        });
        let request = serde_json::json!({
            "system": ["You are a delegate."],
            "temperature": 0.3
        });

        let merged = merge_params(Some(&request), Some(&host)).expect("should merge");
        let obj = merged.as_object().expect("should be object");

        // Request overrides host
        assert_eq!(
            obj.get("temperature").and_then(|v| v.as_f64()),
            Some(0.3),
            "request temperature should override host default"
        );
        // Host defaults preserved
        assert_eq!(
            obj.get("n_ctx").and_then(|v| v.as_i64()),
            Some(32768),
            "host n_ctx should be preserved"
        );
        // Request-only fields added
        assert!(
            obj.contains_key("system"),
            "system from request should be present"
        );
        // Host-only fields preserved
        assert!(
            obj.contains_key("model"),
            "model from host should be preserved"
        );
    }

    #[test]
    fn test_merge_params_strips_api_key_from_request() {
        use crate::agent::remote::provider_host::merge_params;

        let request = serde_json::json!({
            "api_key": "sk-secret-123",
            "system": ["prompt"]
        });

        let merged = merge_params(Some(&request), None).expect("should merge");
        let obj = merged.as_object().expect("should be object");

        assert!(
            !obj.contains_key("api_key"),
            "api_key must be stripped from request params"
        );
        assert!(
            obj.contains_key("system"),
            "non-sensitive fields should survive"
        );
    }

    #[test]
    fn test_merge_params_strips_remote_transport_metadata_from_request() {
        use crate::agent::remote::provider_host::merge_params;

        let request = serde_json::json!({
            "_remote_session_id": "session-123",
            "temperature": 0.2
        });

        let merged = merge_params(Some(&request), None).expect("should merge");
        let obj = merged.as_object().expect("should be object");

        assert!(
            !obj.contains_key("_remote_session_id"),
            "transport-only metadata must be stripped from request params"
        );
        assert_eq!(obj.get("temperature").and_then(|v| v.as_f64()), Some(0.2));
    }

    #[test]
    fn test_merge_params_both_none_returns_none() {
        use crate::agent::remote::provider_host::merge_params;
        assert!(merge_params(None, None).is_none());
    }

    #[test]
    fn test_merge_params_host_only() {
        use crate::agent::remote::provider_host::merge_params;

        let host = serde_json::json!({"n_ctx": 32768});
        let merged = merge_params(None, Some(&host)).expect("should return host defaults");
        assert_eq!(merged, host);
    }

    #[test]
    fn test_should_ack_relay_message_uses_window_for_chunk_batches() {
        use crate::agent::remote::provider_host::should_ack_relay_message;
        use std::time::Duration;

        let chunk = StreamRelayMessage::Chunk(StreamChunk::Text("hello".to_string()));
        assert!(
            !should_ack_relay_message(&chunk, 0, Duration::from_millis(5), 8, Duration::from_millis(40)),
            "fresh chunk batches inside the window should not force an ack"
        );
        assert!(
            should_ack_relay_message(&chunk, 8, Duration::from_millis(5), 8, Duration::from_millis(40)),
            "chunk batches should ack once the batch window is reached"
        );
        assert!(
            should_ack_relay_message(&chunk, 0, Duration::from_millis(40), 8, Duration::from_millis(40)),
            "chunk batches should ack once the time window is reached"
        );

        let done = StreamRelayMessage::Chunk(StreamChunk::Done {
            finish_reason: querymt::chat::FinishReason::Stop,
        });
        assert!(
            should_ack_relay_message(&done, 0, Duration::from_millis(0), 8, Duration::from_millis(40)),
            "terminal messages must always be acked"
        );

        let heartbeat = StreamRelayMessage::Heartbeat {
            phase: ProviderStreamPhase::Streaming,
            elapsed_ms: 100,
            idle_ms: 10,
            chunk_count: 2,
        };
        assert!(
            should_ack_relay_message(&heartbeat, 0, Duration::from_millis(0), 8, Duration::from_millis(40)),
            "control messages should stay acked for health signaling"
        );
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

    // ── Phase 2: Direct-ref reconnect — phase variants ─────────────────────

    #[test]
    fn test_stream_phase_grace_expired_serde_roundtrip() {
        let phase = ProviderStreamPhase::GraceExpired;
        let json = serde_json::to_string(&phase).expect("serialize");
        assert_eq!(json, "\"grace_expired\"");
        let back: ProviderStreamPhase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ProviderStreamPhase::GraceExpired);
    }

    #[test]
    fn test_stream_phase_lease_expired_serde_roundtrip() {
        let phase = ProviderStreamPhase::LeaseExpired;
        let json = serde_json::to_string(&phase).expect("serialize");
        assert_eq!(json, "\"lease_expired\"");
        let back: ProviderStreamPhase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ProviderStreamPhase::LeaseExpired);
    }

    #[test]
    fn test_stream_phase_all_distinct_variants_deserialize() {
        // Verify all phase variants produce distinct serialized values.
        let variants = vec![
            (ProviderStreamPhase::OpeningUpstream, "opening_upstream"),
            (
                ProviderStreamPhase::WaitingFirstChunk,
                "waiting_first_chunk",
            ),
            (ProviderStreamPhase::Streaming, "streaming"),
            (
                ProviderStreamPhase::ReceiverDisconnected,
                "receiver_disconnected",
            ),
            (ProviderStreamPhase::GraceExpired, "grace_expired"),
            (ProviderStreamPhase::LeaseExpired, "lease_expired"),
            (ProviderStreamPhase::Cancelling, "cancelling"),
            (ProviderStreamPhase::Completed, "completed"),
            (ProviderStreamPhase::Failed, "failed"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (variant, expected_name) in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            assert_eq!(json, format!("\"{}\"", expected_name));
            assert!(
                seen.insert(*expected_name),
                "duplicate phase name: {}",
                expected_name
            );
            let back: ProviderStreamPhase = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, *variant);
        }
    }

    // ── Phase 2: Stream status with reconnect phases ────────────────────────

    #[test]
    fn test_provider_stream_status_with_grace_expired_phase() {
        let status = ProviderStreamStatus {
            session_id: "s-1".to_string(),
            request_id: "r-1".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            phase: ProviderStreamPhase::GraceExpired,
            elapsed_ms: 5000,
            idle_ms: 3000,
            chunk_count: 10,
            receiver_connected: false,
            lease_expires_in_ms: 0,
            last_error: Some("reconnect grace expired".to_string()),
        };
        let json = serde_json::to_string(&status).expect("serialize status");
        assert!(json.contains("\"phase\":\"grace_expired\""));
        assert!(json.contains("\"receiver_connected\":false"));
        let back: ProviderStreamStatus = serde_json::from_str(&json).expect("deserialize status");
        assert_eq!(back.phase, ProviderStreamPhase::GraceExpired);
        assert_eq!(back.last_error.as_deref(), Some("reconnect grace expired"));
    }

    #[test]
    fn test_provider_stream_status_with_lease_expired_phase() {
        let status = ProviderStreamStatus {
            session_id: "s-2".to_string(),
            request_id: "r-2".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            phase: ProviderStreamPhase::LeaseExpired,
            elapsed_ms: 120000,
            idle_ms: 60000,
            chunk_count: 50,
            receiver_connected: true,
            lease_expires_in_ms: 0,
            last_error: Some("stream lease expired".to_string()),
        };
        let json = serde_json::to_string(&status).expect("serialize status");
        assert!(json.contains("\"phase\":\"lease_expired\""));
        let back: ProviderStreamStatus = serde_json::from_str(&json).expect("deserialize status");
        assert_eq!(back.phase, ProviderStreamPhase::LeaseExpired);
    }

    // ── Phase 2: relay helpers for reconnect messages ───────────────────────

    #[test]
    fn test_keep_stream_message_buffered_excludes_reconnect_signals() {
        // Heartbeats and transport control signals are not buffered for replay.
        assert!(!keep_stream_message_buffered(
            &StreamRelayMessage::Heartbeat {
                phase: ProviderStreamPhase::Streaming,
                elapsed_ms: 100,
                idle_ms: 50,
                chunk_count: 5,
            }
        ));
        assert!(!keep_stream_message_buffered(
            &StreamRelayMessage::TransportDisconnected {
                reason: "test".to_string(),
            }
        ));
        assert!(!keep_stream_message_buffered(
            &StreamRelayMessage::TransportReconnected { buffered_chunks: 3 }
        ));
        // Chunks and errors should be buffered.
        assert!(keep_stream_message_buffered(&StreamRelayMessage::Chunk(
            StreamChunk::Text("hi".to_string())
        )));
        assert!(keep_stream_message_buffered(
            &StreamRelayMessage::ProviderError {
                error: querymt::error::LLMError::ProviderError("fail".to_string()).to_payload(),
            }
        ));
        assert!(keep_stream_message_buffered(
            &StreamRelayMessage::TransportFailed {
                error: querymt::error::LLMError::Transport {
                    kind: querymt::error::TransportErrorKind::Timeout,
                    message: "grace expired".to_string(),
                }
                .to_payload(),
            }
        ));
    }

    #[test]
    fn test_relay_message_is_terminal_includes_transport_failed() {
        // TransportFailed from grace expiry is terminal.
        let msg = StreamRelayMessage::TransportFailed {
            error: querymt::error::LLMError::Transport {
                kind: querymt::error::TransportErrorKind::Timeout,
                message: "reconnect grace expired".to_string(),
            }
            .to_payload(),
        };
        assert!(relay_message_is_terminal(&msg));
    }

    // ── Phase 2: StreamReceiverActor on terminal does not need DHT unregistration ──

    #[tokio::test]
    async fn test_stream_receiver_actor_kills_on_transport_failed_without_dht() {
        // Verify the actor stops on TransportFailed without any DHT unregistration.
        let (tx, mut rx) = mpsc::channel(16);
        let actor = StreamReceiverActor::new(tx);
        let actor_ref = StreamReceiverActor::spawn(actor);

        // Send a TransportFailed message — this is terminal.
        let result = actor_ref
            .tell(StreamChunkRelay {
                message: StreamRelayMessage::TransportFailed {
                    error: querymt::error::LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::Timeout,
                        message: "reconnect grace expired".to_string(),
                    }
                    .to_payload(),
                },
            })
            .send()
            .await;
        // The tell should succeed — the message is forwarded and the actor stops.
        assert!(result.is_ok(), "tell should succeed: {:?}", result);

        // The channel should receive the TransportFailed message.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("should receive message before timeout");
        assert!(
            matches!(msg, Some(StreamRelayMessage::TransportFailed { .. })),
            "expected TransportFailed, got {:?}",
            msg
        );

        // The actor should have been killed (no further messages accepted).
        let tell_result = actor_ref
            .tell(StreamChunkRelay {
                message: StreamRelayMessage::Heartbeat {
                    phase: ProviderStreamPhase::Streaming,
                    elapsed_ms: 0,
                    idle_ms: 0,
                    chunk_count: 0,
                },
            })
            .send()
            .await;
        // The actor is dead, so subsequent tells should fail.
        assert!(
            tell_result.is_err(),
            "actor should be killed after terminal message"
        );
    }
}
