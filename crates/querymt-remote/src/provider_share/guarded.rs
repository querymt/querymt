//! Guarded provider catalog, contract, and inference routes.

use std::sync::Arc;

use kameo::actor::ActorId;
use querymt::chat::{ChatCall, ChatProvider, ReasoningEffort};
use querymt::error::{LLMError, ProviderErrorKind, ProviderFailure};
use querymt::params::SystemPrompt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest};
use crate::metered_provider::{MeteredAccess, MeteredProvider};
use crate::peer_access::quota::{QuotaError, QuotaStore};
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
                router.register::<$actor, $message, _, _, _, _>(
                    $id,
                    InboundDelivery::Ask,
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
        router.register::<ProviderHostActor, ProviderChatRequest, _, _, _, _>(
            host,
            InboundDelivery::LegacyAsk,
            InboundAccess::Full,
            move |peer, msg| {
                let service = service.clone();
                async move { service.chat(&peer, msg, Uuid::now_v7()).await }
            },
        );
        router.register::<ProviderHostActor, MeteredChat, _, _, _, _>(
            host,
            InboundDelivery::V1Ask,
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
    ) -> Result<(), RemoteProviderHostError> {
        self.access(peer, provider, model)?
            .check()
            .await
            .map_err(policy_error)
    }

    fn access(
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
        self.prepare(peer, request, &call, request_id, false)
            .await?
            .chat(&call)
            .await
            .map(ProviderChatResponse::from)
            .map_err(|error| RemoteProviderHostError::provider_chat("chat", &error))
    }

    async fn prepare(
        &self,
        peer: &PeerRequest,
        request: ProviderBuildRequest,
        call: &ChatCall<'_>,
        request_id: Uuid,
        streaming: bool,
    ) -> Result<MeteredProvider, RemoteProviderHostError> {
        let access = self.access(peer, &request.provider_name, &request.model)?;
        access.check().await.map_err(policy_error)?;
        // Preparation may need the local tokenizer; it cannot dispatch inference.
        let (provider, bound) = tokio::select! {
            biased;
            error = access.revocation() => return Err(policy_error(error)),
            result = self.backend.prepare_chat(request, call, streaming) => result?,
        };
        Ok(MeteredProvider::direct(provider, access, bound, request_id))
    }
}

fn policy_error(error: LLMError) -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat("mesh_policy", &error)
}

fn quota_error(error: QuotaError) -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat("mesh_policy", &error.into_provider_error())
}

fn failure(
    kind: ProviderErrorKind,
    code: &str,
    message: impl Into<String>,
) -> RemoteProviderHostError {
    let failure = ProviderFailure::new(kind, message).with_code(Some(code.into()));
    RemoteProviderHostError::provider_chat("mesh_policy", &LLMError::from(failure))
}
