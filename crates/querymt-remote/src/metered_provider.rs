//! Quota enforcement for calls made on behalf of a mesh peer.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::StreamExt;
use querymt::LLMProvider;
use querymt::chat::{
    ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream, chunk_is_terminal,
};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
use uuid::Uuid;

use crate::ProviderCatalogBackend;
use crate::peer_access::quota::{
    AttemptUsage, DispatchedAttempt, QuotaAttempt, QuotaError, QuotaEstimate, QuotaStore,
    ReservedAttempt, StreamMeter,
};
use crate::peer_access::{PeerGrant, PeerGrantKey, SharingProtocol, wait_for_policy_loss};

type Estimate = dyn Fn(&ChatCall<'_>) -> QuotaEstimate + Send + Sync;
type Revocation = Pin<Box<dyn Future<Output = LLMError> + Send>>;

#[derive(Clone)]
pub(crate) struct MeteredAccess {
    store: Arc<dyn QuotaStore>,
    peer: PeerGrantKey,
    protocol: SharingProtocol,
    provider: String,
    model: String,
    // Direct calls require an advertised binding. Hosted turns instead require session access.
    catalog: Option<Arc<dyn ProviderCatalogBackend>>,
}

impl MeteredAccess {
    #[cfg(feature = "kameo-mesh")]
    pub(crate) fn direct(
        store: Arc<dyn QuotaStore>,
        peer: &crate::mesh_ingress::PeerRequest,
        provider: String,
        model: String,
        catalog: Arc<dyn ProviderCatalogBackend>,
    ) -> Result<Self, LLMError> {
        let grant = peer.grant().ok_or_else(denied)?;
        let access = Self {
            store,
            peer: peer.key().clone(),
            protocol: peer.protocol(),
            provider,
            model,
            catalog: Some(catalog),
        };
        access.check_grant(grant)?;
        Ok(access)
    }

    /// The peer whose grant and quotas apply to this access.
    #[cfg(feature = "kameo-mesh")]
    pub(crate) fn authority(&self) -> &PeerGrantKey {
        &self.peer
    }

    pub(crate) async fn check(&self) -> Result<(), LLMError> {
        let state = self.store.grant(&self.peer).await.map_err(|error| {
            if self.catalog.is_none() {
                return denied();
            }
            tracing::warn!(%error, "mesh provider policy read failed");
            failure(
                ProviderErrorKind::UnknownPermanent,
                "mesh_access_unavailable",
                "mesh policy unavailable",
            )
        })?;
        self.check_grant(&state.grant)
    }

    fn check_grant(&self, grant: &PeerGrant) -> Result<(), LLMError> {
        let allowed = grant.allows_model(self.protocol, &self.provider, &self.model);
        match &self.catalog {
            Some(catalog)
                if !allowed
                    || !catalog.snapshot().providers.iter().any(|entry| {
                        entry.provider == self.provider
                            && (entry.model.as_deref() == Some(self.model.as_str())
                                || (entry.model.is_none() && *grant == PeerGrant::Full))
                    }) =>
            {
                Err(failure(
                    ProviderErrorKind::Authentication,
                    "mesh_access_denied",
                    "provider or model is not shared",
                ))
            }
            None if !allowed || !grant.allows_sessions(self.protocol) => Err(denied()),
            _ => Ok(()),
        }
    }

    pub(crate) fn revocation(&self) -> Revocation {
        let access = self.clone();
        let changes = access.store.subscribe_peer_changes(&access.peer);
        Box::pin(async move {
            wait_for_policy_loss(
                access.store.as_ref(),
                &access.peer,
                changes,
                |grant| access.check_grant(grant),
                denied,
            )
            .await
        })
    }
}

/// A provider binding pinned to the initiating peer and resolved model. It is
/// request/turn-local and must not be cached as an unguarded local provider.
#[derive(Clone)]
pub struct MeteredProvider {
    inner: Arc<dyn LLMProvider>,
    access: MeteredAccess,
    estimate: Arc<Estimate>,
    #[cfg(feature = "kameo-mesh")]
    direct_request: Option<(Uuid, crate::mesh_ingress::PeerRequest)>,
}

impl MeteredProvider {
    /// Wrap a pinned provider for a hosted turn using its effective local configuration.
    pub fn hosted(
        inner: Arc<dyn LLMProvider>,
        store: Arc<dyn QuotaStore>,
        peer: PeerGrantKey,
        protocol: SharingProtocol,
        binding: (String, String),
        estimate: impl Fn(&ChatCall<'_>) -> QuotaEstimate + Send + Sync + 'static,
    ) -> Self {
        let (provider, model) = binding;
        Self {
            inner,
            access: MeteredAccess {
                store,
                peer,
                protocol,
                provider,
                model,
                catalog: None,
            },
            estimate: Arc::new(estimate),
            #[cfg(feature = "kameo-mesh")]
            direct_request: None,
        }
    }

    #[cfg(feature = "kameo-mesh")]
    pub(crate) fn direct(
        inner: Arc<dyn LLMProvider>,
        access: MeteredAccess,
        estimate: QuotaEstimate,
        request_id: Uuid,
        peer: crate::mesh_ingress::PeerRequest,
    ) -> Self {
        Self {
            inner,
            access,
            estimate: Arc::new(move |_| estimate),
            direct_request: Some((request_id, peer)),
        }
    }

    pub(crate) async fn reserve(&self, call: &ChatCall<'_>) -> Result<ReservedAttempt, LLMError> {
        let hosted = || {
            (
                Uuid::new_v4(),
                self.access.store.subscribe_peer_changes(&self.access.peer),
                true,
            )
        };
        #[cfg(feature = "kameo-mesh")]
        let (request_id, history, require_sessions) = match &self.direct_request {
            Some((id, peer)) => (*id, peer.take_policy_changes(), false),
            None => hosted(),
        };
        #[cfg(not(feature = "kameo-mesh"))]
        let (request_id, history, require_sessions) = hosted();
        let history = history.ok_or_else(denied)?;
        let revoked = self.access.revocation();
        self.access.check().await?;
        let estimate = (self.estimate)(call);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|time| i64::try_from(time.as_secs()).ok())
            .ok_or_else(|| QuotaError::InvalidTime.into_provider_error())?;
        tokio::select! {
            biased;
            error = revoked => Err(error),
            result = ReservedAttempt::reserve_with_policy(self.access.store.clone(), QuotaAttempt {
                peer: self.access.peer.clone(),
                protocol: self.access.protocol,
                provider_id: self.access.provider.clone(),
                model_id: self.access.model.clone(),
                request_id,
                estimate,
                now,
            }, history, require_sessions) => result.map_err(QuotaError::into_provider_error),
        }
    }

    pub(crate) async fn stream_reserved(
        &self,
        call: &ChatCall<'_>,
        reserved: ReservedAttempt,
    ) -> Result<ChatStream, LLMError> {
        let mut revoked = self.access.revocation();
        let attempt = tokio::select! {
            biased;
            error = &mut revoked => return Err(error),
            result = reserved.begin() => result.map_err(QuotaError::into_provider_error)?,
        };
        let opened = tokio::select! {
            biased;
            error = &mut revoked => Err(error),
            result = self.inner.chat_stream(call) => result,
        };
        let stream = match opened {
            Ok(stream) => stream,
            Err(error) => {
                self.finish(attempt, AttemptUsage::Unknown).await?;
                return Err(error);
            }
        };
        let state = (
            self.clone(),
            stream,
            attempt,
            StreamMeter::default(),
            revoked,
        );
        Ok(Box::pin(futures_util::stream::try_unfold(
            Some(state),
            |state| async move {
                let Some((provider, mut stream, attempt, mut meter, mut revoked)) = state else {
                    return Ok(None);
                };
                let item = tokio::select! {
                    biased;
                    error = &mut revoked => Some(Err(error)),
                    item = stream.next() => item,
                };
                match item {
                    Some(Ok(chunk)) => {
                        meter.observe(&chunk);
                        if chunk_is_terminal(&chunk) {
                            drop(stream);
                            provider.finish(attempt, meter.complete_usage()).await?;
                            Ok(Some((chunk, None)))
                        } else {
                            Ok(Some((
                                chunk,
                                Some((provider, stream, attempt, meter, revoked)),
                            )))
                        }
                    }
                    other => {
                        drop(stream);
                        provider.finish(attempt, meter.complete_usage()).await?;
                        other
                            .transpose()
                            .map(|item| item.map(|chunk| (chunk, None)))
                    }
                }
            },
        )))
    }

    async fn finish(
        &self,
        attempt: DispatchedAttempt,
        usage: AttemptUsage,
    ) -> Result<(), LLMError> {
        attempt
            .settle(usage)
            .await
            .map_err(QuotaError::into_provider_error)?;
        self.access.check().await
    }
}

fn denied() -> LLMError {
    QuotaError::Denied.into_provider_error()
}

fn failure(kind: ProviderErrorKind, code: &str, message: &str) -> LLMError {
    ProviderFailure::new(kind, message)
        .with_code(Some(code.into()))
        .into()
}

#[async_trait]
impl ChatProvider for MeteredProvider {
    fn capabilities(&self) -> ChatCapabilities {
        self.inner.capabilities()
    }

    fn output_provenance(&self) -> Option<querymt::chat::ChatOutputProvenance> {
        self.inner.output_provenance()
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let mut revoked = self.access.revocation();
        let attempt = tokio::select! {
            biased;
            error = &mut revoked => return Err(error),
            result = async {
                self.reserve(call).await?.begin().await.map_err(QuotaError::into_provider_error)
            } => result?,
        };
        let output = tokio::select! {
            biased;
            error = &mut revoked => Err(error),
            result = self.inner.chat(call) => result,
        };
        let usage = output
            .as_ref()
            .ok()
            .and_then(|output| output.usage.as_ref())
            .map_or(AttemptUsage::Unknown, AttemptUsage::from_usage);
        self.finish(attempt, usage).await?;
        output
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        let reserved = self.reserve(call).await?;
        self.stream_reserved(call, reserved).await
    }
}

#[async_trait]
impl CompletionProvider for MeteredProvider {
    async fn complete(&self, _: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "hosted completion is not metered".into(),
        ))
    }
}

#[async_trait]
impl EmbeddingProvider for MeteredProvider {
    async fn embed(&self, _: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "hosted embeddings are not metered".into(),
        ))
    }
}

impl LLMProvider for MeteredProvider {
    fn key_resolver(&self) -> Option<&Arc<dyn querymt::auth::ApiKeyResolver>> {
        self.inner.key_resolver()
    }
}
