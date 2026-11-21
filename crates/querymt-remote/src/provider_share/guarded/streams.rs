//! Protected ingress adapters; the host actor owns the stream and its relay.

use super::GuardedProvider;
use crate::mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter};
use crate::provider_host_actor::{GuardedStreamControl, GuardedStreamStart};
use crate::{
    CancelProviderStreamRequest, GetProviderStreamStatus, MeshHandle, ProviderHostActor,
    ProviderStreamRequest, ProviderStreamRouterActor, RemoteProviderHostError,
    RenewProviderStreamLease,
};
use kameo::actor::{ActorId, ActorRef, RemoteActorRef};
use kameo::error::SendError;
use std::sync::Arc;

type StreamRequest = ProviderStreamRequest<RemoteActorRef<ProviderStreamRouterActor>>;

pub(in crate::provider_share) fn install(
    provider: Arc<GuardedProvider>,
    mesh: &MeshHandle,
    router: &InboundRouter,
    id: ActorId,
    host: &ActorRef<ProviderHostActor>,
) {
    let host = host.downgrade();
    let start = {
        let egress = mesh.sharing_egress();
        let host = host.clone();
        move |peer, msg| {
            let host = host.clone();
            let provider = provider.clone();
            let egress = egress.clone();
            async move {
                let host = host.upgrade().ok_or_else(stopped)?;
                let admission = tokio_util::sync::CancellationToken::new();
                // Detaching from the mailbox must not detach preparation from
                // its caller. The accepted stream instead follows its lease.
                let _cancel_pending = admission.clone().drop_guard();
                host.ask(GuardedStreamStart(provider, egress, peer, msg, admission))
                    .await
                    .map_err(actor_error)
            }
        }
    };
    router.register::<ProviderHostActor, StreamRequest, _, _, _, _>(
        id,
        InboundDelivery::LegacyTellV1Ask,
        InboundAccess::Providers,
        start,
    );
    macro_rules! control {
        ($message:ty) => {{
            let host = host.clone();
            let handler = move |peer, msg| {
                let host = host.clone();
                async move {
                    let host = host.upgrade().ok_or_else(stopped)?;
                    host.ask(GuardedStreamControl(peer, msg))
                        .await
                        .map_err(actor_error)
                }
            };
            router.register::<ProviderHostActor, $message, _, _, _, _>(
                id,
                InboundDelivery::Ask,
                InboundAccess::Providers,
                handler,
            );
        }};
    }
    control!(GetProviderStreamStatus);
    control!(RenewProviderStreamLease);
    control!(CancelProviderStreamRequest);
}

fn stopped() -> RemoteProviderHostError {
    RemoteProviderHostError::Internal("provider host stopped".into())
}

fn actor_error<M>(error: SendError<M, RemoteProviderHostError>) -> RemoteProviderHostError {
    match error {
        SendError::HandlerError(error) => error,
        _ => stopped(),
    }
}
