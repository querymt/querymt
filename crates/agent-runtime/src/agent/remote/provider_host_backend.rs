use crate::agent::agent_config::AgentConfig;
use crate::session::provider::ProviderRequest;
use async_trait::async_trait;
use querymt::LLMProvider;
#[cfg(any(test, feature = "test-support"))]
use querymt_remote::ProviderHostActor;
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
            .build_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref())
                    .with_provider_node_id(Some("local")),
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
            .build_provider_snapshot(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref())
                    .with_provider_node_id(Some("local")),
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
            .build_provider(
                ProviderRequest::new(&request.provider_name, &request.model)
                    .with_params(request.params.as_ref())
                    .with_provider_node_id(Some("local")),
            )
            .await
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
    let schema: serde_json::Value = serde_json::from_str(schema).ok()?;
    if schema["x-querymt-chat-token-bound"] != "llama-cpp-text-stream-n-ctx-v2" {
        return None;
    }
    let config: serde_json::Value = serde_json::from_str(resolved_config).ok()?;
    let limit = i32::try_from(config["n_ctx"].as_u64()?).ok()?;
    (limit > 0 && config["speculative"].is_null()).then_some(i64::from(limit))
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
pub(crate) struct HostedProvider {
    inner: Arc<dyn LLMProvider>,
    peer: Arc<crate::agent::session_actor::PeerTurn>,
    store: Arc<dyn querymt_remote::peer_access::quota::QuotaStore>,
    provider: String,
    model: String,
    text_stream_limit: Option<i64>,
}

use futures_util::StreamExt;
use querymt::chat::{ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream};
use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
use querymt_remote::peer_access::quota::{
    AttemptUsage, DispatchedAttempt, QuotaAttempt, QuotaError, QuotaSettlement, QuotaStore,
    ReservedAttempt, StreamMeter,
};

impl HostedProvider {
    pub(crate) fn new(
        inner: Arc<dyn LLMProvider>,
        peer: Arc<crate::agent::session_actor::PeerTurn>,
        store: Arc<dyn QuotaStore>,
        provider: String,
        model: String,
        text_stream_limit: Option<i64>,
    ) -> Self {
        Self {
            inner,
            peer,
            store,
            provider,
            model,
            text_stream_limit,
        }
    }

    async fn check(&self) -> Result<(), LLMError> {
        let grant = self
            .peer
            .grant()
            .await
            .map_err(|_| quota_failure(QuotaError::Denied))?;
        if !grant.allows_model(self.peer.protocol, &self.provider, &self.model) {
            return Err(quota_failure(QuotaError::Denied));
        }
        Ok(())
    }

    async fn begin(
        &self,
        call: &ChatCall<'_>,
        streaming: bool,
    ) -> Result<DispatchedAttempt, LLMError> {
        self.check().await?;
        let attempt = ReservedAttempt::reserve(
            self.store.clone(),
            QuotaAttempt {
                peer: self.peer.key.clone(),
                grant_revision: self.peer.revision,
                protocol: self.peer.protocol,
                provider_id: self.provider.clone(),
                model_id: self.model.clone(),
                request_id: uuid::Uuid::new_v4(),
                bound_tokens: text_stream_bound(self.text_stream_limit, call, streaming),
                now: time::OffsetDateTime::now_utc().unix_timestamp(),
            },
        )
        .await
        .map_err(quota_failure)?;
        attempt.begin().await.map_err(quota_failure)
    }
}

fn quota_failure(error: QuotaError) -> LLMError {
    let (kind, code, message) = match &error {
        QuotaError::Denied | QuotaError::InvalidAttempt => (
            ProviderErrorKind::Authentication,
            "mesh_access_denied",
            error.to_string(),
        ),
        QuotaError::Exhausted(_) => (
            ProviderErrorKind::QuotaExceeded,
            "mesh_quota_exhausted",
            error.to_string(),
        ),
        QuotaError::Unsupported(_) => (
            ProviderErrorKind::UnsupportedOperation,
            "mesh_quota_unsupported",
            error.to_string(),
        ),
        _ => (
            ProviderErrorKind::UnknownPermanent,
            "mesh_access_unavailable",
            "mesh usage accounting unavailable".into(),
        ),
    };
    ProviderFailure::new(kind, message)
        .with_code(Some(code.into()))
        .into()
}

async fn settle_hosted(attempt: DispatchedAttempt, usage: AttemptUsage) -> Result<(), LLMError> {
    if attempt.settle(usage).await.map_err(quota_failure)? == QuotaSettlement::BoundExceeded {
        return Err(ProviderFailure::new(
            ProviderErrorKind::UnknownPermanent,
            "provider exceeded its enforced token bound",
        )
        .with_code(Some("mesh_quota_bound_exceeded".into()))
        .into());
    }
    Ok(())
}

#[async_trait]
impl ChatProvider for HostedProvider {
    fn capabilities(&self) -> ChatCapabilities {
        self.inner.capabilities()
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let attempt = self.begin(call, false).await?;
        let output = self.inner.chat(call).await;
        let usage = output
            .as_ref()
            .ok()
            .and_then(|output| output.usage.as_ref())
            .map_or(AttemptUsage::Unknown, AttemptUsage::from_usage);
        settle_hosted(attempt, usage).await?;
        self.check().await?;
        output
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        let attempt = self.begin(call, true).await?;
        let stream = match self.inner.chat_stream(call).await {
            Ok(stream) => stream,
            Err(error) => {
                settle_hosted(attempt, AttemptUsage::Unknown).await?;
                return Err(error);
            }
        };
        let peer = self.peer.clone();
        let revoked = Box::pin(async move { peer.revoked().await });
        Ok(Box::pin(futures_util::stream::try_unfold(
            (stream, Some(attempt), StreamMeter::default(), revoked),
            |(mut stream, mut attempt, mut meter, mut revoked)| async move {
                let item = tokio::select! {
                    biased;
                    _ = &mut revoked => return Err(quota_failure(QuotaError::Denied)),
                    item = stream.next() => item,
                };
                match item {
                    Some(Ok(chunk)) => {
                        meter.observe(&chunk);
                        if meter.has_complete_usage()
                            && let Some(attempt) = attempt.take()
                        {
                            settle_hosted(attempt, meter.complete_usage()).await?;
                        }
                        Ok(Some((chunk, (stream, attempt, meter, revoked))))
                    }
                    Some(Err(error)) => {
                        if let Some(attempt) = attempt.take() {
                            settle_hosted(attempt, meter.complete_usage()).await?;
                        }
                        Err(error)
                    }
                    None => {
                        if let Some(attempt) = attempt.take() {
                            settle_hosted(attempt, meter.complete_usage()).await?;
                        }
                        Ok(None)
                    }
                }
            },
        )))
    }
}

#[async_trait]
impl querymt::completion::CompletionProvider for HostedProvider {
    async fn complete(
        &self,
        _: &querymt::completion::CompletionRequest,
    ) -> Result<querymt::completion::CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "hosted completion is not metered".into(),
        ))
    }
}

#[async_trait]
impl querymt::embedding::EmbeddingProvider for HostedProvider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "hosted embeddings are not metered".into(),
        ))
    }
}

impl LLMProvider for HostedProvider {}

#[cfg(any(test, feature = "test-support"))]
pub fn provider_host_from_config(config: Arc<AgentConfig>) -> ProviderHostActor {
    ProviderHostActor::new(provider_backend_from_config(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::session_actor::PeerTurn, test_utils::MockLlmProvider};
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

    #[tokio::test]
    async fn session_inference_and_direct_inference_share_an_allowance() {
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
                    tokens: 16,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
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
        let provider = HostedProvider::new(
            Arc::new(MockLlmProvider::new()),
            peer,
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
        let after = store.quota(&key, &scope, now).await.unwrap();
        assert_eq!(after.known_tokens, 7);
        assert_eq!(after.reserved_tokens, 0);
    }
}
