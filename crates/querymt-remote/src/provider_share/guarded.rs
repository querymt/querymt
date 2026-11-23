//! Guarded provider catalog, contract, and inference routes.

use std::sync::Arc;

use kameo::actor::{ActorId, ActorRef, WeakActorRef};
use querymt::chat::{ChatCall, ChatProvider, ReasoningEffort};
use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
use querymt::params::SystemPrompt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest};
use crate::metered_provider::{MeteredAccess, MeteredProvider};
use crate::peer_access::quota::{QuotaError, QuotaStore};
use crate::peer_access::{PeerGrant, PeerGrantState, SharingProtocol};
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

pub(crate) struct GuardedProvider {
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
        router: &Arc<InboundRouter>,
        catalog: ActorId,
        host_id: ActorId,
        host: &ActorRef<ProviderHostActor>,
    ) {
        let service = self.clone();
        router.register_checked::<ProviderCatalogActor, GetProviderCatalog, _, _, _, _>(
            catalog,
            InboundDelivery::Ask,
            InboundAccess::Providers,
            move |peer, msg| {
                let service = service.clone();
                async move { service.catalog(&peer, msg).await }
            },
            Some(catalog_reply_allowed),
        );
        let host = host.downgrade();
        macro_rules! host_route {
            ($message:ty, $delivery:ident, $access:ident, |$service:ident, $peer:ident, $msg:ident| $call:expr) => {{
                let service = self.clone();
                let host = host.clone();
                router.register::<ProviderHostActor, $message, _, _, _, _>(
                    host_id,
                    InboundDelivery::$delivery,
                    InboundAccess::$access,
                    move |$peer, $msg| {
                        let $service = service.clone();
                        let host = host.clone();
                        async move { while_host_alive(&host, $call).await }
                    },
                );
            }};
        }
        host_route!(
            GetProviderContractInfo,
            Ask,
            Providers,
            |service, peer, msg| service.contract(&peer, msg)
        );
        host_route!(
            ProviderChatRequest,
            LegacyAsk,
            Full,
            |service, peer, msg| service.chat(&peer, msg, Uuid::now_v7())
        );
        host_route!(MeteredChat, V1Ask, Providers, |service, peer, msg| service
            .chat(&peer, msg.request, msg.request_id));

        let router = Arc::downgrade(router);
        tokio::spawn(async move {
            host.wait_for_shutdown().await;
            if let Some(router) = router.upgrade() {
                router.withdraw_actor(host_id);
            }
        });
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
        if peer.protocol() == crate::peer_access::SharingProtocol::Legacy
            && state.grant != PeerGrant::Full
        {
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
        snapshot.providers.retain(|entry| {
            catalog_entry_allowed(
                &grant,
                peer.protocol(),
                &entry.provider,
                entry.model.as_deref(),
            )
        });
        // Discovery of provider sharing is not evidence of session or shell access.
        snapshot.node.capabilities.clear();
        Ok(snapshot)
    }

    pub(crate) fn access(
        &self,
        peer: &PeerRequest,
        provider: &str,
        model: &str,
    ) -> Result<MeteredAccess, RemoteProviderHostError> {
        MeteredAccess::direct(
            self.store.clone(),
            peer,
            provider.to_owned(),
            model.to_owned(),
            self.catalog.clone(),
        )
        .map_err(policy_error)
    }

    pub(crate) fn build_request(
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
        let access = self.access(peer, &msg.provider, &msg.model)?;
        access.check().await.map_err(policy_error)?;
        let request = self.build_request(&msg.provider, &msg.model, msg.params)?;
        let contract = self.backend.contract_info(request).await?;
        access.check().await.map_err(policy_error)?;
        Ok(contract)
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
        self.prepare(peer, request, &call, request_id)
            .await?
            .chat(&call)
            .await
            .map(ProviderChatResponse::from)
            .map_err(|error| RemoteProviderHostError::provider_chat("chat", &error))
    }

    pub(crate) async fn prepare(
        &self,
        peer: &PeerRequest,
        request: ProviderBuildRequest,
        call: &ChatCall<'_>,
        request_id: Uuid,
    ) -> Result<MeteredProvider, RemoteProviderHostError> {
        let access = self.access(peer, &request.provider_name, &request.model)?;
        access.check().await.map_err(policy_error)?;
        let (provider, estimate) = tokio::select! {
            biased;
            error = access.revocation() => return Err(policy_error(error)),
            result = self.backend.prepare_chat(request, call) => result?,
        };
        Ok(MeteredProvider::direct(
            provider,
            access,
            estimate,
            request_id,
            peer.clone(),
        ))
    }
}

async fn while_host_alive<R>(
    host: &WeakActorRef<ProviderHostActor>,
    operation: impl std::future::Future<Output = Result<R, RemoteProviderHostError>>,
) -> Result<R, RemoteProviderHostError> {
    let stopped = || RemoteProviderHostError::Internal("provider host stopped".into());
    let alive = || host.upgrade().is_some_and(|host| host.is_alive());
    if !alive() {
        return Err(stopped());
    }
    tokio::select! {
        biased;
        _ = host.wait_for_shutdown() => Err(stopped()),
        result = operation => if alive() { result } else { Err(stopped()) },
    }
}

fn catalog_entry_allowed(
    grant: &PeerGrant,
    protocol: SharingProtocol,
    provider: &str,
    model: Option<&str>,
) -> bool {
    match model {
        Some(model) => grant.allows_model(protocol, provider, model),
        None => *grant == PeerGrant::Full,
    }
}

fn catalog_reply_allowed(reply: &[u8], grant: &PeerGrant, protocol: SharingProtocol) -> bool {
    rmp_serde::from_slice::<ProviderCatalogSnapshot>(reply).is_ok_and(|snapshot| {
        snapshot.providers.iter().all(|entry| {
            catalog_entry_allowed(grant, protocol, &entry.provider, entry.model.as_deref())
        })
    })
}

pub(crate) fn policy_error(error: LLMError) -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat("mesh_policy", &error)
}

pub(crate) fn quota_error(error: QuotaError) -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat("mesh_policy", &error.into_provider_error())
}

pub(crate) fn failure(
    kind: ProviderErrorKind,
    code: &str,
    message: impl Into<String>,
) -> RemoteProviderHostError {
    let failure = ProviderFailure::new(kind, message).with_code(Some(code.into()));
    RemoteProviderHostError::provider_chat("mesh_policy", &LLMError::from(failure))
}
