//! Guarded provider catalog, contract, and inference routes.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kameo::actor::ActorId;
use querymt::chat::{ChatCall, ReasoningEffort};
use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
use querymt::params::SystemPrompt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::mesh_ingress::{InboundAccess, InboundRouter, PeerRequest};
use crate::peer_access::quota::{
    AttemptUsage, QuotaAttempt, QuotaError, QuotaSettlement, QuotaStore, ReservedAttempt,
};
use crate::peer_access::{PeerGrant, PeerGrantState};
use crate::provider_host_actor::validate_item_aware_request;
use crate::{
    GetProviderCatalog, GetProviderContractInfo, ProviderBuildRequest, ProviderCatalogActor,
    ProviderCatalogBackend, ProviderCatalogSnapshot, ProviderChatRequest, ProviderChatResponse,
    ProviderContractInfo, ProviderHostActor, RemoteProviderBackend, RemoteProviderHostError,
    merge_remote_provider_params,
};

pub(super) mod streams;
#[cfg(test)]
mod tests;

#[derive(Serialize, Deserialize)]
pub(crate) struct MeteredChat {
    pub request_id: Uuid,
    pub request: ProviderChatRequest,
}

impl kameo::remote::RemoteMessage<MeteredChat> for ProviderHostActor {
    const REMOTE_ID: &'static str = "querymt::MeteredChat.v1";
}

pub(super) struct GuardedProvider {
    pub(super) backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
    pub(super) catalog: Arc<dyn ProviderCatalogBackend>,
    pub(super) store: Arc<dyn QuotaStore>,
}

// Only these fields may cross from a peer into provider construction. In
// particular, arbitrary provider extras are not an extension of this list.
#[derive(Deserialize, Serialize)]
struct InferenceParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_budget_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system: Option<SystemInput>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum SystemInput {
    One(SystemPrompt),
    Many(Vec<SystemPrompt>),
}

impl GuardedProvider {
    pub(super) fn install(
        self: Arc<Self>,
        router: &InboundRouter,
        catalog: ActorId,
        host: ActorId,
    ) {
        macro_rules! register {
            ($actor:ty, $message:ty, $id:expr, $handle:ident) => {{
                let service = self.clone();
                router.register_ask::<$actor, $message, _, _, _, _>(
                    $id,
                    InboundAccess::LegacyFull,
                    move |peer, msg| {
                        let service = service.clone();
                        async move { service.$handle(&peer, msg).await }
                    },
                );
                let service = self.clone();
                router.register_v1_ask::<$actor, $message, _, _, _, _>(
                    $id,
                    InboundAccess::Providers,
                    move |peer, msg| {
                        let service = service.clone();
                        async move { service.$handle(&peer, msg).await }
                    },
                );
            }};
        }
        register!(ProviderCatalogActor, GetProviderCatalog, catalog, catalog);
        register!(ProviderHostActor, GetProviderContractInfo, host, contract);
        let service = self.clone();
        router.register_ask::<ProviderHostActor, ProviderChatRequest, _, _, _, _>(
            host,
            InboundAccess::LegacyFull,
            move |peer, msg| {
                let service = service.clone();
                async move { service.chat(&peer, msg, Uuid::now_v7()).await }
            },
        );
        router.register_v1_ask::<ProviderHostActor, MeteredChat, _, _, _, _>(
            host,
            InboundAccess::Providers,
            move |peer, msg| {
                let service = self.clone();
                async move { service.chat(&peer, msg.request, msg.request_id).await }
            },
        );
    }

    async fn grant(&self, peer: &PeerRequest) -> Result<PeerGrantState, RemoteProviderHostError> {
        let state = self.store.grant(peer.key()).await.map_err(|error| {
            tracing::warn!(%error, "mesh provider policy read failed");
            failure(
                ProviderErrorKind::UnknownPermanent,
                "mesh_access_unavailable",
                "mesh policy unavailable",
            )
        })?;
        if peer.revision() != Some(state.revision) {
            return Err(quota_error(QuotaError::Denied));
        }
        Ok(state)
    }

    async fn catalog(
        &self,
        peer: &PeerRequest,
        _: GetProviderCatalog,
    ) -> Result<ProviderCatalogSnapshot, String> {
        let grant = self
            .grant(peer)
            .await
            .map_err(|error| error.to_string())?
            .grant;
        let mut snapshot = self.catalog.snapshot();
        snapshot
            .providers
            .retain(|entry| match entry.model.as_deref() {
                Some(model) => grant.allows_model(peer.protocol(), &entry.provider, model),
                None => grant == PeerGrant::Full,
            });
        // Discovery of provider sharing is not evidence of session or shell access.
        snapshot.node.capabilities.clear();
        Ok(snapshot)
    }

    async fn binding(
        &self,
        peer: &PeerRequest,
        provider: &str,
        model: &str,
    ) -> Result<PeerGrantState, RemoteProviderHostError> {
        let state = self.grant(peer).await?;
        if !state.grant.allows_model(peer.protocol(), provider, model)
            || !self.catalog.snapshot().providers.iter().any(|entry| {
                entry.provider == provider
                    && (entry.model.as_deref() == Some(model)
                        || (entry.model.is_none() && state.grant == PeerGrant::Full))
            })
        {
            return Err(failure(
                ProviderErrorKind::Authentication,
                "mesh_access_denied",
                "provider or model is not shared",
            ));
        }
        Ok(state)
    }

    fn build_request(
        &self,
        provider: &str,
        model: &str,
        params: Option<Value>,
    ) -> Result<ProviderBuildRequest, RemoteProviderHostError> {
        let overlay = params
            .map(|value| {
                let parsed: InferenceParams = serde_json::from_value(value)?;
                serde_json::to_value(parsed)
            })
            .transpose()
            .map_err(|_| {
                failure(
                    ProviderErrorKind::InvalidRequest,
                    "mesh_access_invalid_params",
                    "invalid provider inference options",
                )
            })?;
        let defaults = self.backend.host_default_params(provider, model);
        let params = merge_remote_provider_params(overlay.as_ref(), defaults.as_ref());
        Ok(ProviderBuildRequest::new(provider, model).with_params(params))
    }

    async fn contract(
        &self,
        peer: &PeerRequest,
        msg: GetProviderContractInfo,
    ) -> Result<ProviderContractInfo, RemoteProviderHostError> {
        self.binding(peer, &msg.provider, &msg.model).await?;
        let request = self.build_request(&msg.provider, &msg.model, msg.params)?;
        self.backend.contract_info(request).await
    }

    async fn chat(
        &self,
        peer: &PeerRequest,
        msg: ProviderChatRequest,
        request_id: Uuid,
    ) -> Result<ProviderChatResponse, RemoteProviderHostError> {
        validate_item_aware_request(&msg.messages, msg.item_aware_contract_version)?;
        let request = self.build_request(&msg.provider, &msg.model, msg.params)?;
        let call = ChatCall::new(&msg.messages)
            .maybe_tools(msg.tools.as_deref())
            .options(msg.options.unwrap_or_default());
        let (provider, id) = self
            .prepare_attempt(peer, request, &call, request_id)
            .await?;
        let attempt = id.begin().await.map_err(quota_error)?;
        let watch = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(error) = self.binding(peer, &msg.provider, &msg.model).await {
                    return error;
                }
            }
        };
        let response = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(600), provider.chat(&call)) => result
                .map_err(|_| failure(ProviderErrorKind::UnknownPermanent, "mesh_provider_timeout", "mesh provider attempt timed out"))
                .and_then(|result| result.map_err(|error| RemoteProviderHostError::provider_chat("chat", &error))),
            error = watch => Err(error),
        };
        let usage = response
            .as_ref()
            .ok()
            .and_then(|reply| reply.usage.as_ref())
            .map_or(AttemptUsage::Unknown, AttemptUsage::from_usage);
        enforce_settlement(attempt.settle(usage).await.map_err(quota_error)?)?;
        self.binding(peer, &msg.provider, &msg.model).await?;
        response.map(ProviderChatResponse::from)
    }

    async fn prepare_attempt(
        &self,
        peer: &PeerRequest,
        request: ProviderBuildRequest,
        call: &ChatCall<'_>,
        request_id: Uuid,
    ) -> Result<(Arc<dyn querymt::LLMProvider>, ReservedAttempt), RemoteProviderHostError> {
        let grant = self
            .binding(peer, &request.provider_name, &request.model)
            .await?;
        let bound_tokens = self.backend.quota_bound(&request, call);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|time| i64::try_from(time.as_secs()).ok())
            .ok_or_else(|| quota_error(QuotaError::InvalidTime))?;
        let attempt = ReservedAttempt::reserve(
            self.store.clone(),
            QuotaAttempt {
                peer: peer.key().clone(),
                grant_revision: grant.revision,
                protocol: peer.protocol(),
                provider_id: request.provider_name.clone(),
                model_id: request.model.clone(),
                request_id,
                bound_tokens,
                now,
            },
        )
        .await
        .map_err(quota_error)?;
        let prepared = tokio::time::timeout(
            Duration::from_secs(30),
            self.backend.build_provider(request.clone()),
        )
        .await
        .unwrap_or_else(|_| {
            Err(failure(
                ProviderErrorKind::UnknownPermanent,
                "mesh_provider_timeout",
                "mesh provider preparation timed out",
            ))
        });
        let provider = match prepared {
            Ok(provider) => provider,
            Err(error) => {
                attempt.release().await.map_err(quota_error)?;
                return Err(error);
            }
        };
        // Release without charging if preparation lost its authorization.
        if let Err(error) = self
            .binding(peer, &request.provider_name, &request.model)
            .await
        {
            attempt.release().await.map_err(quota_error)?;
            return Err(error);
        }
        Ok((provider, attempt))
    }
}

fn enforce_settlement(outcome: QuotaSettlement) -> Result<(), RemoteProviderHostError> {
    if outcome == QuotaSettlement::BoundExceeded {
        return Err(failure(
            ProviderErrorKind::UnknownPermanent,
            "mesh_quota_bound_exceeded",
            "provider exceeded its enforced token bound",
        ));
    }
    Ok(())
}

fn quota_error(error: QuotaError) -> RemoteProviderHostError {
    let (kind, code) = match &error {
        QuotaError::Denied | QuotaError::InvalidAttempt => {
            (ProviderErrorKind::Authentication, "mesh_access_denied")
        }
        QuotaError::Exhausted(_) => (ProviderErrorKind::QuotaExceeded, "mesh_quota_exhausted"),
        QuotaError::Unsupported(_) => (
            ProviderErrorKind::UnsupportedOperation,
            "mesh_quota_unsupported",
        ),
        QuotaError::DuplicateAttempt => (
            ProviderErrorKind::InvalidRequest,
            "mesh_quota_duplicate_attempt",
        ),
        _ => {
            tracing::warn!(%error, "mesh quota operation failed");
            (
                ProviderErrorKind::UnknownPermanent,
                "mesh_access_unavailable",
            )
        }
    };
    failure(
        kind,
        code,
        if code == "mesh_access_unavailable" {
            "mesh policy unavailable".to_owned()
        } else {
            error.to_string()
        },
    )
}

fn failure(
    kind: ProviderErrorKind,
    code: &str,
    message: impl Into<String>,
) -> RemoteProviderHostError {
    let failure = ProviderFailure::new(kind, message).with_code(Some(code.into()));
    RemoteProviderHostError::provider_chat("mesh_policy", &LLMError::from(failure))
}
