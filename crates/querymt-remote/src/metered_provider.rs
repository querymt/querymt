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
    AttemptUsage, DispatchedAttempt, QuotaAttempt, QuotaError, QuotaSettlement, QuotaStore,
    ReservedAttempt, StreamMeter,
};
use crate::peer_access::{
    PeerGrant, PeerGrantKey, PeerGrantState, SharingProtocol, wait_for_revision_loss,
};

type Bound = dyn Fn(&ChatCall<'_>, bool) -> Option<i64> + Send + Sync;
type Revocation = Pin<Box<dyn Future<Output = LLMError> + Send>>;

#[derive(Clone)]
pub(crate) struct MeteredAccess {
    store: Arc<dyn QuotaStore>,
    peer: PeerGrantKey,
    revision: Uuid,
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
        Ok(Self {
            store,
            peer: peer.key().clone(),
            revision: peer.revision().ok_or_else(denied)?,
            protocol: peer.protocol(),
            provider,
            model,
            catalog: Some(catalog),
        })
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
        if state.revision != self.revision {
            return Err(denied());
        }
        let PeerGrantState { grant, .. } = state;
        let allowed = grant.allows_model(self.protocol, &self.provider, &self.model);
        match &self.catalog {
            Some(catalog)
                if !allowed
                    || !catalog.snapshot().providers.iter().any(|entry| {
                        entry.provider == self.provider
                            && (entry.model.as_deref() == Some(self.model.as_str())
                                || (entry.model.is_none() && grant == PeerGrant::Full))
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
        Box::pin(async move {
            wait_for_revision_loss(access.store.as_ref(), &access.peer, access.revision).await;
            denied()
        })
    }
}

/// A provider binding pinned to the initiating peer and grant revision. It is
/// request/turn-local and must not be cached as an unguarded local provider.
#[derive(Clone)]
pub struct MeteredProvider {
    inner: Arc<dyn LLMProvider>,
    access: MeteredAccess,
    bound: Arc<Bound>,
    request_id: Option<Uuid>,
}

impl MeteredProvider {
    /// Wrap a pinned provider for a hosted turn. `bound` uses its effective local
    /// configuration to determine whether a call qualifies for finite quota.
    pub fn hosted(
        inner: Arc<dyn LLMProvider>,
        store: Arc<dyn QuotaStore>,
        peer: PeerGrantKey,
        revision: Uuid,
        protocol: SharingProtocol,
        binding: (String, String),
        bound: impl Fn(&ChatCall<'_>, bool) -> Option<i64> + Send + Sync + 'static,
    ) -> Self {
        let (provider, model) = binding;
        Self {
            inner,
            access: MeteredAccess {
                store,
                peer,
                revision,
                protocol,
                provider,
                model,
                catalog: None,
            },
            bound: Arc::new(bound),
            request_id: None,
        }
    }

    #[cfg(feature = "kameo-mesh")]
    pub(crate) fn direct(
        inner: Arc<dyn LLMProvider>,
        access: MeteredAccess,
        bound: Option<i64>,
        request_id: Uuid,
    ) -> Self {
        Self {
            inner,
            access,
            bound: Arc::new(move |_, _| bound),
            request_id: Some(request_id),
        }
    }

    pub(crate) async fn reserve(
        &self,
        call: &ChatCall<'_>,
        streaming: bool,
    ) -> Result<ReservedAttempt, LLMError> {
        self.access.check().await?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|time| i64::try_from(time.as_secs()).ok())
            .ok_or_else(|| QuotaError::InvalidTime.into_provider_error())?;
        let reserved = ReservedAttempt::reserve(
            self.access.store.clone(),
            QuotaAttempt {
                peer: self.access.peer.clone(),
                grant_revision: self.access.revision,
                protocol: self.access.protocol,
                provider_id: self.access.provider.clone(),
                model_id: self.access.model.clone(),
                request_id: self.request_id.unwrap_or_else(Uuid::new_v4),
                bound_tokens: (self.bound)(call, streaming),
                now,
            },
        )
        .await
        .map_err(QuotaError::into_provider_error)?;
        self.check_reserved(reserved).await
    }

    async fn check_reserved(&self, reserved: ReservedAttempt) -> Result<ReservedAttempt, LLMError> {
        if let Err(error) = self.access.check().await {
            reserved
                .release()
                .await
                .map_err(QuotaError::into_provider_error)?;
            return Err(error);
        }
        Ok(reserved)
    }

    async fn begin(&self, reserved: ReservedAttempt) -> Result<DispatchedAttempt, LLMError> {
        let attempt = self
            .check_reserved(reserved)
            .await?
            .begin()
            .await
            .map_err(QuotaError::into_provider_error)?;
        if let Err(error) = self.access.check().await {
            self.finish(attempt, AttemptUsage::Unknown).await?;
            return Err(error);
        }
        Ok(attempt)
    }

    pub(crate) async fn stream_reserved(
        &self,
        call: &ChatCall<'_>,
        reserved: ReservedAttempt,
    ) -> Result<ChatStream, LLMError> {
        let attempt = self.begin(reserved).await?;
        let mut revoked = self.access.revocation();
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
        if attempt
            .settle(usage)
            .await
            .map_err(QuotaError::into_provider_error)?
            == QuotaSettlement::BoundExceeded
        {
            return Err(failure(
                ProviderErrorKind::UnknownPermanent,
                "mesh_quota_bound_exceeded",
                "provider exceeded its enforced token bound",
            ));
        }
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

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let attempt = self.begin(self.reserve(call, false).await?).await?;
        let output = tokio::select! {
            biased;
            error = self.access.revocation() => Err(error),
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
        let reserved = self.reserve(call, true).await?;
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

impl LLMProvider for MeteredProvider {}
