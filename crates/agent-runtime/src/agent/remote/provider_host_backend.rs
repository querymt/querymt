use crate::agent::agent_config::AgentConfig;
use crate::session::provider::{ProviderQuotaInputs, ProviderRequest};
use async_trait::async_trait;
use querymt::LLMProvider;
use querymt::chat::ChatCall;
#[cfg(any(test, feature = "test-support"))]
use querymt_remote::ProviderHostActor;
use querymt_remote::peer_access::quota::{QuotaEstimate, QuotaStore};
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
    ) -> Result<(Arc<dyn LLMProvider>, QuotaEstimate), Self::Error> {
        let snapshot = self
            .config
            .provider
            .build_local_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref()),
            )
            .await
            .map_err(|error| host_error("prepare_chat", None, error))?;
        let estimate = snapshot
            .quota_inputs(self.config.fallback_context_tokens())
            .estimate(&request.provider_name, &request.model, call);
        Ok((snapshot.provider, estimate))
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

/// Wraps a single pinned host binding, never the shared provider cache.
pub(crate) fn hosted_provider(
    inner: Arc<dyn LLMProvider>,
    peer: &crate::agent::session_actor::PeerTurn,
    store: Arc<dyn QuotaStore>,
    binding: (String, String),
    quota_inputs: ProviderQuotaInputs,
) -> querymt_remote::MeteredProvider {
    let (provider, model) = binding.clone();
    querymt_remote::MeteredProvider::hosted(
        inner,
        store,
        peer.key.clone(),
        peer.protocol,
        binding,
        move |call| quota_inputs.estimate(&provider, &model, call),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub fn provider_host_from_config(config: Arc<AgentConfig>) -> ProviderHostActor {
    ProviderHostActor::new(provider_backend_from_config(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_actor::PeerTurn;
    use futures_util::StreamExt;
    use querymt::chat::{ChatOutput, ChatProvider, ChatStream, FinishReason, StreamChunk};
    use querymt::error::LLMError;
    use querymt_remote::{
        NodeId,
        peer_access::{
            GrantStore, ModelSelection, PeerGrant, PeerGrantKey, ProviderGrant, SharingProtocol,
            quota::{QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow},
        },
    };
    use uuid::Uuid;

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
                        models: ModelSelection::All,
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

    struct HttpFixture {
        url: String,
    }

    impl querymt::plugin::LLMProviderFactory for HttpFixture {
        fn name(&self) -> &str {
            "fixture"
        }

        fn config_schema(&self) -> String {
            r#"{"type":"object","properties":{"model":{},"max_tokens":{}}}"#.into()
        }

        fn from_config(&self, _: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
            Ok(Box::new(Self {
                url: self.url.clone(),
            }))
        }

        fn list_models<'a>(
            &'a self,
            _: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
            Box::pin(async { Ok(vec!["known".into(), "missing".into()]) })
        }
    }

    #[async_trait]
    impl ChatProvider for HttpFixture {
        async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            let body = serde_json::to_vec(&call.options)?;
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()?
                .post(&self.url)
                .header("content-type", "application/json")
                .timeout(std::time::Duration::from_secs(5))
                .body(body)
                .send()
                .await?
                .error_for_status()?;
            Ok(serde_json::from_slice(&response.bytes().await?)?)
        }
    }

    #[async_trait]
    impl querymt::completion::CompletionProvider for HttpFixture {
        async fn complete(
            &self,
            _: &querymt::completion::CompletionRequest,
        ) -> Result<querymt::completion::CompletionResponse, LLMError> {
            unreachable!("chat-only fixture")
        }
    }

    #[async_trait]
    impl querymt::embedding::EmbeddingProvider for HttpFixture {
        async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            unreachable!("chat-only fixture")
        }
    }

    impl LLMProvider for HttpFixture {}

    #[tokio::test]
    async fn finite_http_chat_reserves_with_known_and_missing_model_metadata() {
        use crate::agent::agent_config_builder::AgentConfigBuilder;
        use crate::session::sqlite_storage::SqliteStorage;
        use axum::{Json, Router, routing::post};
        use querymt::chat::{ChatMessage, GenerationOptions};
        use querymt::plugin::host::PluginRegistry;

        // Configure the on-disk models.dev lookup in a dedicated process; changing
        // the environment in this test process would race unrelated tests.
        const CHILD: &str = "QMT_RUNTIME_QUOTA_HTTP_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("models.dev.json"),
                r#"{"providers":{"fixture":{"models":{"known":{"limit":{"context":64}}}}}}"#,
            )
            .unwrap();
            let (_, module) = module_path!().split_once("::").unwrap();
            let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg(format!(
                    "{module}::finite_http_chat_reserves_with_known_and_missing_model_metadata"
                ))
                .arg("--nocapture")
                .env(CHILD, "1")
                .env("QMT_PROVIDER_CACHE_DIR", dir.path())
                .kill_on_drop(true);
            let result = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
                .await
                .unwrap()
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout);
            assert!(
                result.status.success() && stdout.contains("1 passed"),
                "{stdout}\n{}",
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat", listener.local_addr().unwrap());
        let (reservations, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (_dir, store, key, scope, mut revision, now) = finite_quota_fixture(5).await;
        let server = tokio::spawn({
            let (store, key, scope) = (store.clone(), key.clone(), scope.clone());
            async move {
                let router = Router::new().route(
                    "/chat",
                    post(move |Json(options): Json<GenerationOptions>| {
                        let (store, key, scope, reservations) = (
                            store.clone(),
                            key.clone(),
                            scope.clone(),
                            reservations.clone(),
                        );
                        async move {
                            let status = scope_status(store.as_ref(), &key, &scope, now).await;
                            reservations
                                .send((options.max_tokens, status.reserved_tokens))
                                .unwrap();
                            Json(ChatOutput::from_projections(
                                None,
                                Some("hello".into()),
                                None,
                                Some(querymt::Usage {
                                    input_tokens: 125,
                                    output_tokens: 37,
                                    ..Default::default()
                                }),
                                Some(FinishReason::Stop),
                            ))
                        }
                    }),
                );
                axum::serve(listener, router).await.unwrap();
            }
        });

        let registry = Arc::new(PluginRegistry::empty());
        registry.register_static(Arc::new(HttpFixture { url }));
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let peer = PeerTurn::test_peer(
            key.clone(),
            store.grant(&key).await.unwrap().grant,
            store.clone(),
        );
        let messages = [ChatMessage::user().text("Count this prompt").build()];
        let call = ChatCall::new(&messages).options(GenerationOptions::NONE.with_max_tokens(100));

        for (model, fallback, expected_ceiling) in [
            ("known", Some(17), Some(64)),
            ("missing", Some(17), Some(17)),
            ("missing", None, None),
        ] {
            let builder = AgentConfigBuilder::new(
                registry.clone(),
                storage.clone(),
                querymt::LLMParams::new(),
            );
            let builder = if let Some(fallback) = fallback {
                builder.with_context_management(
                    crate::middleware::ContextConfig::with_manual_limit(3)
                        .fallback_max_tokens(fallback),
                )
            } else {
                builder
            };
            let config = Arc::new(builder.build());
            assert_eq!(config.fallback_context_tokens(), fallback);
            let backend = provider_backend_from_config(config.clone());
            let params = serde_json::json!({"max_tokens": 8});
            let request = ProviderBuildRequest {
                provider_name: "fixture".into(),
                model: model.into(),
                params: Some(params.clone()),
            };
            let (_, direct) = backend.prepare_chat(request.clone(), &call).await.unwrap();
            assert!(direct.prompt_tokens > 0);
            assert_eq!(
                direct.reserved_tokens,
                expected_ceiling.unwrap_or(direct.prompt_tokens + 100)
            );
            if fallback.is_none() {
                let (_, default_cap) = backend
                    .prepare_chat(request, &ChatCall::new(&messages))
                    .await
                    .unwrap();
                assert_eq!(default_cap.reserved_tokens, default_cap.prompt_tokens + 8);
            }

            let snapshot = config
                .provider
                .build_local_provider_snapshot(
                    ProviderRequest::new("fixture", model).with_params(Some(&params)),
                )
                .await
                .unwrap();
            let inputs = snapshot.quota_inputs(config.fallback_context_tokens());
            let hosted = hosted_provider(
                snapshot.provider,
                &peer,
                store.clone(),
                ("fixture".into(), model.into()),
                inputs,
            );
            let output = hosted.chat(&call).await.unwrap();
            assert_eq!(output.text().as_deref(), Some("hello"));
            assert_eq!(output.finish_reason, Some(FinishReason::Stop));
            assert_eq!(
                seen.try_recv().unwrap(),
                (Some(100), direct.reserved_tokens)
            );
            let status = scope_status(store.as_ref(), &key, &scope, now).await;
            assert_eq!(
                (
                    status.known_tokens,
                    status.estimated_tokens,
                    status.reserved_tokens
                ),
                (162, 0, 0)
            );
            assert!(matches!(hosted.chat(&call).await,
                Err(LLMError::ProviderResponseError(error)) if error.code() == Some("mesh_quota_exhausted")));
            assert!(seen.try_recv().is_err());
            revision = store
                .edit_quota(&key, revision, &scope, QuotaEdit::Reset, now)
                .await
                .unwrap();
        }
        server.abort();
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
        store: Arc<dyn QuotaStore>,
    ) -> querymt_remote::MeteredProvider {
        querymt_remote::MeteredProvider::hosted(
            Arc::new(ScriptedStream(std::sync::Mutex::new(Some(stream)))),
            store,
            key.clone(),
            SharingProtocol::V1,
            ("fixture".into(), "model".into()),
            |_| QuotaEstimate {
                reserved_tokens: 10,
                prompt_tokens: 2,
            },
        )
    }

    fn terminal_stream(input_tokens: u32, output_tokens: u32) -> ChatStream {
        Box::pin(futures_util::stream::iter([
            Ok(StreamChunk::Usage(querymt::Usage {
                input_tokens,
                output_tokens,
                ..Default::default()
            })),
            Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            }),
        ]))
    }

    async fn assert_done(stream: &mut ChatStream) {
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("terminal chunk");
        assert!(matches!(
            terminal,
            Some(Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop
            }))
        ));
        assert!(stream.next().await.is_none());
    }

    async fn assert_exhausted(provider: &querymt_remote::MeteredProvider) {
        assert!(matches!(provider.chat_stream(&ChatCall::new(&[])).await,
            Err(LLMError::ProviderResponseError(failure)) if failure.code() == Some("mesh_quota_exhausted")));
    }

    #[tokio::test]
    async fn hosted_stream_settles_usage_only_after_terminal_chunk() {
        let (_dir, store, key, scope, _, now) = finite_quota_fixture(16).await;
        let usage = querymt::Usage {
            input_tokens: 4,
            output_tokens: 3,
            ..Default::default()
        };
        let provider = hosted_stream(Ok(terminal_stream(4, 3)), &key, store.clone());
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
    async fn hosted_stream_charges_estimate_when_upstream_does_not_report_terminal_usage() {
        let (_dir, store, key, scope, _, now) = finite_quota_fixture(32).await;
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

        let startup = hosted_stream(Err(provider_error()), &key, store.clone());
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
    async fn hosted_overshoot_delivers_terminal_and_only_blocks_new_attempts_until_raised() {
        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let call = ChatCall::new(&[]);
        let first = hosted_stream(Ok(terminal_stream(12, 6)), &key, store.clone());
        let held = hosted_stream(Ok(terminal_stream(4, 3)), &key, store.clone());
        let next = hosted_stream(Ok(terminal_stream(1, 1)), &key, store.clone());
        let mut first = first.chat_stream(&call).await.unwrap();
        // Six tokens remain, so the second estimate of ten can still be reserved.
        let mut held = held.chat_stream(&call).await.unwrap();
        assert!(matches!(
            first.next().await,
            Some(Ok(StreamChunk::Usage(_)))
        ));
        assert_done(&mut first).await;
        let charged = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((charged.known_tokens, charged.reserved_tokens), (18, 10));
        assert_exhausted(&next).await;

        assert!(matches!(held.next().await, Some(Ok(StreamChunk::Usage(_)))));
        assert_done(&mut held).await;
        let settled = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((settled.known_tokens, settled.reserved_tokens), (25, 0));
        assert_exhausted(&next).await;

        store
            .edit_quota(
                &key,
                revision,
                &scope,
                QuotaEdit::Update(QuotaLimit::Finite {
                    tokens: 26,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
        let mut next = next.chat_stream(&call).await.unwrap();
        assert!(matches!(next.next().await, Some(Ok(StreamChunk::Usage(_)))));
        assert_done(&mut next).await;
        assert_eq!(
            scope_status(store.as_ref(), &key, &scope, now)
                .await
                .known_tokens,
            27
        );
    }

    #[tokio::test]
    async fn hosted_stream_drop_cancels_upstream_and_charges_estimate() {
        let (_dir, store, key, scope, _, now) = finite_quota_fixture(16).await;
        let (upstream, dropped) = tokio::sync::oneshot::channel::<()>();
        let stream = futures_util::stream::once(async move {
            let _upstream = upstream;
            std::future::pending().await
        });
        let provider = hosted_stream(Ok(Box::pin(stream)), &key, store.clone());
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
    async fn held_stream_survives_lowered_cap_and_reset_readmits_new_work() {
        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let call = ChatCall::new(&[]);
        let (finish, wait) = tokio::sync::oneshot::channel();
        let upstream = futures_util::stream::once(async {
            wait.await.unwrap();
            Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            })
        });
        let provider = hosted_stream(Ok(Box::pin(upstream)), &key, store.clone());
        let next = hosted_stream(Ok(terminal_stream(1, 1)), &key, store.clone());
        let mut stream = provider.chat_stream(&call).await.unwrap();
        let mut pending = stream.next();
        assert!(futures_util::poll!(&mut pending).is_pending());
        let revision = store
            .edit_quota(
                &key,
                revision,
                &scope,
                QuotaEdit::Update(QuotaLimit::Finite {
                    tokens: 5,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
        assert_exhausted(&next).await;

        finish.send(()).unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), pending)
                .await
                .unwrap(),
            Some(Ok(StreamChunk::Done {
                finish_reason: FinishReason::Stop
            }))
        ));
        let status = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((status.reserved_tokens, status.estimated_tokens), (0, 10));
        assert_exhausted(&next).await;

        store
            .edit_quota(&key, revision, &scope, QuotaEdit::Reset, now)
            .await
            .unwrap();
        let mut next = next.chat_stream(&call).await.unwrap();
        assert!(matches!(next.next().await, Some(Ok(StreamChunk::Usage(_)))));
        assert_done(&mut next).await;
        let status = scope_status(store.as_ref(), &key, &scope, now).await;
        assert_eq!((status.known_tokens, status.estimated_tokens), (2, 0));
    }

    #[tokio::test]
    async fn hosted_stream_revocation_wakes_a_pending_read_and_charges_estimate() {
        use std::time::Duration;

        let (_dir, store, key, scope, revision, now) = finite_quota_fixture(16).await;
        let upstream = futures_util::stream::iter([Ok(StreamChunk::Text("fixture".into()))])
            .chain(futures_util::stream::pending());
        let provider = hosted_stream(Ok(Box::pin(upstream)), &key, store.clone());
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
