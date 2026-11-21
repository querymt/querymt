use crate::agent::agent_config::AgentConfig;
use crate::session::provider::ProviderRequest;
use async_trait::async_trait;
use querymt::LLMProvider;
use querymt::chat::ChatCall;
#[cfg(any(test, feature = "test-support"))]
use querymt_remote::ProviderHostActor;
use querymt_remote::peer_access::quota::QuotaStore;
use querymt_remote::{
    ProviderBuildRequest, RemoteProviderBackend, RemoteProviderHostError,
    params_for_remote_provider,
};
use std::sync::Arc;

/// Map a session error to the host error sent to the peer.
///
/// A failed credential refresh keeps its classification: a rejected login
/// arrives as a non-retryable `AuthError`, a temporary failure as a retryable
/// `Transport` error. Everything else is an opaque internal error.
fn host_error(
    operation: &str,
    context: Option<String>,
    error: crate::session::error::SessionError,
) -> RemoteProviderHostError {
    use crate::session::error::SessionError;
    match error {
        error @ SessionError::CredentialRefresh { .. } => RemoteProviderHostError::provider_chat(
            operation,
            &querymt::error::LLMError::from(error),
        ),
        error => RemoteProviderHostError::Internal(match context {
            Some(context) => format!("{context}: {error}"),
            None => error.to_string(),
        }),
    }
}

struct AgentConfigProviderBackend {
    config: Arc<AgentConfig>,
}

#[async_trait]
impl RemoteProviderBackend for AgentConfigProviderBackend {
    type Error = RemoteProviderHostError;

    fn host_default_params(&self, _provider_name: &str, _model: &str) -> Option<serde_json::Value> {
        params_for_remote_provider(self.config.provider.initial_params())
    }

    async fn prepare_chat(
        &self,
        request: ProviderBuildRequest,
        call: &ChatCall<'_>,
        streaming: bool,
    ) -> Result<(Arc<dyn LLMProvider>, Option<i64>), Self::Error> {
        let snapshot = self
            .config
            .provider
            .build_local_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref()),
            )
            .await
            .map_err(|error| host_error("prepare_chat", None, error))?;
        let bound = text_stream_bound(snapshot.mesh_text_stream_limit, call, streaming);
        Ok((snapshot.provider, bound))
    }

    async fn contract_info(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<querymt_remote::ProviderContractInfo, Self::Error> {
        self.config
            .provider
            .build_local_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref()),
            )
            .await
            .map(|snapshot| snapshot.contract_info())
            .map_err(|error| host_error("contract_info", None, error))
    }

    async fn build_provider(
        &self,
        request: ProviderBuildRequest,
    ) -> Result<Arc<dyn LLMProvider>, Self::Error> {
        self.config
            .provider
            .build_local_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref()),
            )
            .await
            .map(|built| built.provider)
            .map_err(|e| {
                host_error(
                    "build_provider",
                    Some(format!(
                        "ProviderHostActor: failed to build provider '{}' model '{}'",
                        request.provider_name, request.model
                    )),
                    e,
                )
            })
    }
}

pub fn provider_backend_from_config(
    config: Arc<AgentConfig>,
) -> Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>> {
    Arc::new(AgentConfigProviderBackend { config })
}

pub(crate) fn qualified_text_stream_limit(schema: &str, resolved_config: &str) -> Option<i64> {
    if !has_text_stream_bound(schema) {
        return None;
    }
    let config: serde_json::Value = serde_json::from_str(resolved_config).ok()?;
    let limit = i32::try_from(config["n_ctx"].as_u64()?).ok()?;
    (limit > 0 && config["speculative"].is_null()).then_some(i64::from(limit))
}

pub(crate) fn has_text_stream_bound(schema: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(schema).is_ok_and(|schema| {
        schema["x-querymt-chat-token-bound"] == "llama-cpp-text-stream-n-ctx-v2"
    })
}

fn text_stream_bound(limit: Option<i64>, call: &ChatCall<'_>, streaming: bool) -> Option<i64> {
    use querymt::chat::{ChatInputPart, ToolResultPart};
    // The native checker runs after template rendering, so tool definitions and
    // text tool results are included in the enforced prompt length.
    if !streaming
        || call.messages.iter().any(|message| {
            message
                .portable_input_parts()
                .iter()
                .any(|part| match part {
                    ChatInputPart::Text { .. } => false,
                    ChatInputPart::Attachment(_) => true,
                    ChatInputPart::ToolResult(result) => result
                        .parts
                        .iter()
                        .any(|part| matches!(part, ToolResultPart::Attachment(_))),
                })
        })
    {
        return None;
    }
    limit
}

/// Wraps a single pinned host binding, never the shared provider cache.
pub(crate) fn hosted_provider(
    inner: Arc<dyn LLMProvider>,
    peer: &crate::agent::session_actor::PeerTurn,
    store: Arc<dyn QuotaStore>,
    provider: String,
    model: String,
    text_stream_limit: Option<i64>,
) -> querymt_remote::MeteredProvider {
    querymt_remote::MeteredProvider::hosted(
        inner,
        store,
        peer.key.clone(),
        peer.protocol,
        (provider, model),
        move |call, streaming| text_stream_bound(text_stream_limit, call, streaming),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub fn provider_host_from_config(config: Arc<AgentConfig>) -> ProviderHostActor {
    ProviderHostActor::new(provider_backend_from_config(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::session_actor::PeerTurn, test_utils::MockLlmProvider};
    use futures_util::StreamExt;
    use querymt::chat::{ChatOutput, ChatProvider, ChatStream};
    use querymt::error::LLMError;
    use querymt_remote::peer_access::quota::{AttemptUsage, QuotaAttempt, ReservedAttempt};
    use querymt_remote::{
        NodeId,
        peer_access::{
            GrantStore, ModelSelection, PeerGrant, PeerGrantKey, ProviderGrant, SharingProtocol,
            quota::{QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow},
        },
    };
    use uuid::Uuid;

    #[test]
    fn finite_support_requires_a_qualified_binding_and_streaming_text() {
        let schema = r#"{"x-querymt-chat-token-bound":"llama-cpp-text-stream-n-ctx-v2"}"#;
        assert_eq!(qualified_text_stream_limit("{}", r#"{"n_ctx":4096}"#), None);
        assert_eq!(
            qualified_text_stream_limit(schema, r#"{"n_ctx":4096,"speculative":{}}"#),
            None
        );
        assert_eq!(
            qualified_text_stream_limit(schema, r#"{"n_ctx":2147483648}"#),
            None
        );
        let limit = qualified_text_stream_limit(schema, r#"{"n_ctx":4096}"#);
        assert_eq!(limit, Some(4096));
        assert_eq!(
            text_stream_bound(limit, &ChatCall::new(&[]), true),
            Some(4096)
        );
        assert_eq!(text_stream_bound(limit, &ChatCall::new(&[]), false), None);
        let tools = [querymt::chat::Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: "fixture".into(),
                description: "Inert fixture".into(),
                parameters: serde_json::json!({"type": "object"}),
                strict: None,
            },
        }];
        assert_eq!(
            text_stream_bound(limit, &ChatCall::new(&[]).tools(&tools), true),
            Some(4096)
        );
    }

    async fn scope_status(
        store: &dyn QuotaStore,
        key: &PeerGrantKey,
        scope: &QuotaScope,
        now: i64,
    ) -> querymt_remote::peer_access::quota::QuotaStatus {
        store
            .access_snapshot(key, now)
            .await
            .unwrap()
            .1
            .into_iter()
            .find(|status| &status.scope == scope)
            .unwrap()
    }

    async fn finite_quota_fixture(
        tokens: i64,
    ) -> (
        tempfile::TempDir,
        Arc<querymt_agent_sqlite::mesh_access::SqliteMeshAccess>,
        PeerGrantKey,
        QuotaScope,
        Uuid,
        i64,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            querymt_agent_sqlite::mesh_access::SqliteMeshAccess::open_exclusive(
                dir.path().join("mesh.sqlite3"),
            )
            .await
            .unwrap(),
        );
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(libp2p::PeerId::random()),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let grant = store
            .replace_grant(
                &key,
                Uuid::nil(),
                PeerGrant::Custom {
                    sessions: true,
                    filesystem: false,
                    providers: vec![ProviderGrant {
                        provider_id: "fixture".into(),
                        models: ModelSelection::Selected {
                            model_ids: vec!["model".into()],
                        },
                    }],
                },
            )
            .await
            .unwrap();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let scope = QuotaScope {
            provider_id: "fixture".into(),
            model_id: None,
        };
        let revision = store
            .edit_quota(
                &key,
                grant.revision,
                &scope,
                QuotaEdit::Reconfigure(QuotaLimit::Finite {
                    tokens,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
        (dir, store, key, scope, revision, now)
    }

    #[tokio::test]
    async fn session_inference_and_direct_inference_share_an_allowance() {
        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let direct = ReservedAttempt::reserve(
            store.clone(),
            QuotaAttempt {
                peer: key.clone(),
                grant_revision: revision,
                protocol: SharingProtocol::V1,
                provider_id: "fixture".into(),
                model_id: "model".into(),
                request_id: Uuid::new_v4(),
                bound_tokens: Some(10),
                now,
            },
        )
        .await
        .unwrap()
        .begin()
        .await
        .unwrap();
        direct.settle(AttemptUsage::Observed(7)).await.unwrap();

        let peer = PeerTurn::test_peer(key.clone(), revision, store.clone());
        // No mock methods are configured: touching the provider would fail the test.
        let provider = hosted_provider(
            Arc::new(MockLlmProvider::new()),
            &peer,
            store.clone(),
            "fixture".into(),
            "model".into(),
            Some(10),
        );
        let error = provider
            .chat_stream(&ChatCall::new(&[]))
            .await
            .err()
            .expect("direct spend leaves fewer than ten tokens");
        assert!(
            matches!(error, LLMError::ProviderResponseError(failure) if failure.code() == Some("mesh_quota_exhausted"))
        );
        let after = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(after.known_tokens, 7);
        assert_eq!(after.reserved_tokens, 0);
    }

    struct ScriptedStream(std::sync::Mutex<Option<Result<ChatStream, LLMError>>>);

    #[async_trait]
    impl ChatProvider for ScriptedStream {
        async fn chat(&self, _: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            panic!("only streaming calls are expected")
        }

        async fn chat_stream(&self, _: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
            self.0.lock().unwrap().take().expect("one upstream call")
        }
    }

    #[async_trait]
    impl querymt::completion::CompletionProvider for ScriptedStream {
        async fn complete(
            &self,
            _: &querymt::completion::CompletionRequest,
        ) -> Result<querymt::completion::CompletionResponse, LLMError> {
            panic!("only streaming calls are expected")
        }
    }

    #[async_trait]
    impl querymt::embedding::EmbeddingProvider for ScriptedStream {
        async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            panic!("only streaming calls are expected")
        }
    }

    impl LLMProvider for ScriptedStream {}

    fn hosted_stream(
        stream: Result<ChatStream, LLMError>,
        key: &PeerGrantKey,
        revision: Uuid,
        store: Arc<dyn QuotaStore>,
    ) -> querymt_remote::MeteredProvider {
        hosted_provider(
            Arc::new(ScriptedStream(std::sync::Mutex::new(Some(stream)))),
            &PeerTurn::test_peer(key.clone(), revision, store.clone()),
            store,
            "fixture".into(),
            "model".into(),
            Some(10),
        )
    }

    #[tokio::test]
    async fn hosted_stream_settles_usage_only_after_terminal_chunk() {
        use querymt::chat::{FinishReason, StreamChunk};

        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let usage = querymt::Usage {
            input_tokens: 4,
            output_tokens: 3,
            ..Default::default()
        };
        let upstream = futures_util::stream::iter([
            Ok(StreamChunk::Usage(usage.clone())),
            Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            }),
        ]);
        let provider = hosted_stream(Ok(Box::pin(upstream)), &key, revision, store.clone());
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();

        assert!(
            matches!(stream.next().await, Some(Ok(StreamChunk::Usage(actual))) if actual == usage)
        );
        let ongoing = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((ongoing.known_tokens, ongoing.reserved_tokens), (0, 10));
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop
            }))
        ));
        let settled = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(
            (
                settled.known_tokens,
                settled.estimated_tokens,
                settled.reserved_tokens
            ),
            (7, 0, 0)
        );
        assert!(stream.next().await.is_none());
        assert_eq!(
            scope_status(store.as_ref(), &key, &scope, now).await,
            settled,
            "EOF must not settle a second time"
        );
    }

    #[tokio::test]
    async fn hosted_stream_charges_bound_when_upstream_does_not_report_terminal_usage() {
        use querymt::chat::StreamChunk;

        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(32).await;
        let provider_error = || LLMError::NotImplemented("fixture upstream error".into());
        let partial_usage = || {
            Ok(StreamChunk::Usage(querymt::Usage {
                input_tokens: 2,
                ..Default::default()
            }))
        };
        let eof = hosted_stream(
            Ok(Box::pin(futures_util::stream::iter([partial_usage()]))),
            &key,
            revision,
            store.clone(),
        );
        let mut stream = eof.chat_stream(&ChatCall::new(&[])).await.unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Usage(_)))
        ));
        assert!(stream.next().await.is_none());
        let after_eof = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(
            (
                after_eof.known_tokens,
                after_eof.estimated_tokens,
                after_eof.reserved_tokens
            ),
            (0, 10, 0)
        );

        let midstream = hosted_stream(
            Ok(Box::pin(futures_util::stream::iter([
                partial_usage(),
                Err(provider_error()),
            ]))),
            &key,
            revision,
            store.clone(),
        );
        let mut stream = midstream.chat_stream(&ChatCall::new(&[])).await.unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Usage(_)))
        ));
        assert!(
            matches!(stream.next().await, Some(Err(LLMError::NotImplemented(message))) if message == "fixture upstream error")
        );
        let after_error = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(
            (
                after_error.known_tokens,
                after_error.estimated_tokens,
                after_error.reserved_tokens
            ),
            (0, 20, 0)
        );

        let startup = hosted_stream(Err(provider_error()), &key, revision, store.clone());
        assert!(
            matches!(startup.chat_stream(&ChatCall::new(&[])).await, Err(LLMError::NotImplemented(message)) if message == "fixture upstream error")
        );
        let after_startup = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(
            (
                after_startup.known_tokens,
                after_startup.estimated_tokens,
                after_startup.reserved_tokens
            ),
            (0, 30, 0)
        );
    }

    #[tokio::test]
    async fn hosted_stream_rejects_completion_and_suspends_a_faulty_binding() {
        use querymt::chat::{FinishReason, StreamChunk};

        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(40).await;
        let upstream = futures_util::stream::iter([
            Ok(StreamChunk::Usage(querymt::Usage {
                input_tokens: 12,
                output_tokens: 3,
                ..Default::default()
            })),
            Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            }),
        ]);
        let provider = hosted_stream(Ok(Box::pin(upstream)), &key, revision, store.clone());
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamChunk::Usage(_)))
        ));
        assert!(
            matches!(stream.next().await, Some(Err(LLMError::ProviderResponseError(error)))
            if error.code() == Some("mesh_quota_bound_exceeded"))
        );
        let charged = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!(charged.known_tokens, 15);
        assert_eq!(charged.reserved_tokens, 0);
        assert_eq!(charged.available_tokens, Some(25));
        // The remaining balance can fit the advertised bound, but it is no
        // longer trustworthy. A second upstream invocation would panic.
        assert!(matches!(provider.chat_stream(&ChatCall::new(&[])).await,
            Err(LLMError::ProviderResponseError(error)) if error.code() == Some("mesh_quota_unsupported")));
        assert_eq!(
            scope_status(store.as_ref(), &key, &scope, now).await,
            charged
        );
    }

    #[tokio::test]
    async fn hosted_stream_drop_cancels_upstream_and_charges_bound() {
        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let (upstream, dropped) = tokio::sync::oneshot::channel::<()>();
        let stream = futures_util::stream::once(async move {
            let _upstream = upstream;
            std::future::pending().await
        });
        let provider = hosted_stream(Ok(Box::pin(stream)), &key, revision, store.clone());
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
        assert!(futures_util::poll!(stream.next()).is_pending());
        assert_eq!(
            scope_status(store.as_ref(), &key, &scope, now)
                .await
                .reserved_tokens,
            10
        );
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(5), dropped)
            .await
            .unwrap()
            .unwrap_err();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let status = scope_status(store.as_ref(), &key, &scope, now).await;
                if status.reserved_tokens == 0 {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!((status.known_tokens, status.estimated_tokens), (0, 10));
    }

    #[tokio::test]
    async fn held_stream_survives_cap_edits_with_capacity_and_stops_on_exhaustion() {
        use std::time::Duration;

        let (_dir, store, key, scope, mut revision, now) = finite_quota_fixture(16).await;
        let provider = hosted_stream(
            Ok(Box::pin(futures_util::stream::pending())),
            &key,
            revision,
            store.clone(),
        );
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
        let mut pending = stream.next();
        assert!(futures_util::poll!(&mut pending).is_pending());
        for tokens in [25, 11, 10] {
            revision = store
                .edit_quota(
                    &key,
                    revision,
                    &scope,
                    QuotaEdit::Update(QuotaLimit::Finite {
                        tokens,
                        window: QuotaWindow::Manual,
                    }),
                    now,
                )
                .await
                .unwrap();
            if tokens > 10 {
                assert!(
                    tokio::time::timeout(Duration::from_millis(75), &mut pending)
                        .await
                        .is_err()
                );
            }
        }
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(3), pending).await.unwrap(),
            Some(Err(LLMError::ProviderResponseError(failure))) if failure.code() == Some("mesh_quota_exhausted"))
        );
        let status = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((status.reserved_tokens, status.estimated_tokens), (0, 10));
    }

    #[tokio::test]
    async fn hosted_stream_revocation_wakes_a_pending_read_and_charges_bound() {
        use querymt::chat::StreamChunk;
        use std::time::Duration;

        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let upstream = futures_util::stream::iter([Ok(StreamChunk::Text("fixture".into()))])
            .chain(futures_util::stream::pending());
        let provider = hosted_stream(Ok(Box::pin(upstream)), &key, revision, store.clone());
        let mut stream = provider.chat_stream(&ChatCall::new(&[])).await.unwrap();
        assert!(
            matches!(stream.next().await, Some(Ok(StreamChunk::Text(text))) if text == "fixture")
        );
        assert_eq!(
            scope_status(store.as_ref(), &key, &scope, now)
                .await
                .reserved_tokens,
            10
        );
        let mut pending = stream.next();
        assert!(futures_util::poll!(&mut pending).is_pending());

        store
            .replace_grant(&key, revision, PeerGrant::None)
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap();
        assert!(
            matches!(result, Some(Err(LLMError::ProviderResponseError(failure))) if failure.code() == Some("mesh_access_denied"))
        );
        assert!(
            matches!(provider.chat_stream(&ChatCall::new(&[])).await, Err(LLMError::ProviderResponseError(failure)) if failure.code() == Some("mesh_access_denied"))
        );

        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = scope_status(store.as_ref(), &key, &scope, now).await;
                if status.reserved_tokens == 0 {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("revocation must release the reservation");
        assert_eq!((settled.known_tokens, settled.estimated_tokens), (0, 10));
    }
}
