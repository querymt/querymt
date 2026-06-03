use crate::scope::{MeshScopeId, scoped_provider_host};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCatalogEntry {
    pub provider: String,
    pub model: Option<String>,
    pub label: Option<String>,
    pub family: Option<String>,
    pub quant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCatalogNodeInfo {
    pub node_id: String,
    pub node_label: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderCatalogSnapshot {
    pub node: ProviderCatalogNodeInfo,
    pub providers: Vec<ProviderCatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetProviderCatalog;

pub trait ProviderCatalogBackend: Send + Sync {
    fn snapshot(&self) -> ProviderCatalogSnapshot;
}

#[derive(Actor)]
pub struct ProviderCatalogActor {
    backend: Arc<dyn ProviderCatalogBackend>,
}

impl ProviderCatalogActor {
    pub fn new(backend: Arc<dyn ProviderCatalogBackend>) -> Self {
        Self { backend }
    }
}

impl Message<GetProviderCatalog> for ProviderCatalogActor {
    type Reply = Result<ProviderCatalogSnapshot, String>;

    async fn handle(
        &mut self,
        _msg: GetProviderCatalog,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.backend.snapshot())
    }
}

impl kameo::remote::RemoteActor for ProviderCatalogActor {
    const REMOTE_ID: &'static str = "querymt::ProviderCatalogActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static PROVIDER_CATALOG_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <ProviderCatalogActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<ProviderCatalogActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<ProviderCatalogActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<ProviderCatalogActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

impl kameo::remote::RemoteMessage<GetProviderCatalog> for ProviderCatalogActor {
    const REMOTE_ID: &'static str = "querymt::GetProviderCatalog";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
#[linkme(crate = _internal::linkme)]
static REG_GET_PROVIDER_CATALOG: (
    _internal::RemoteMessageRegistrationID<'static>,
    _internal::RemoteMessageFns,
) = (
    _internal::RemoteMessageRegistrationID {
        actor_remote_id: <ProviderCatalogActor as kameo::remote::RemoteActor>::REMOTE_ID,
        message_remote_id: <ProviderCatalogActor as kameo::remote::RemoteMessage<GetProviderCatalog>>::REMOTE_ID,
    },
    _internal::RemoteMessageFns {
        ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
            Box::pin(_internal::ask::<ProviderCatalogActor, GetProviderCatalog>(
                actor_id,
                msg,
                mailbox_timeout,
                reply_timeout,
            ))
        }) as _internal::RemoteAskFn,
        try_ask: (|actor_id, msg, reply_timeout| {
            Box::pin(_internal::try_ask::<ProviderCatalogActor, GetProviderCatalog>(
                actor_id,
                msg,
                reply_timeout,
            ))
        }) as _internal::RemoteTryAskFn,
        tell: (|actor_id, msg, mailbox_timeout| {
            Box::pin(_internal::tell::<ProviderCatalogActor, GetProviderCatalog>(
                actor_id,
                msg,
                mailbox_timeout,
            ))
        }) as _internal::RemoteTellFn,
        try_tell: (|actor_id, msg| {
            Box::pin(_internal::try_tell::<ProviderCatalogActor, GetProviderCatalog>(actor_id, msg))
        }) as _internal::RemoteTryTellFn,
    },
);

pub fn fallback_provider_host_catalog(scope: &MeshScopeId, peer_id: &(impl std::fmt::Display + ?Sized)) -> String {
    scoped_provider_host(scope, peer_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::actor::Spawn;

    struct StaticCatalogBackend {
        snapshot: ProviderCatalogSnapshot,
    }

    impl ProviderCatalogBackend for StaticCatalogBackend {
        fn snapshot(&self) -> ProviderCatalogSnapshot {
            self.snapshot.clone()
        }
    }

    fn sample_snapshot() -> ProviderCatalogSnapshot {
        ProviderCatalogSnapshot {
            node: ProviderCatalogNodeInfo {
                node_id: "12D3KooWcatalog".to_string(),
                node_label: Some("catalog-node".to_string()),
                capabilities: vec!["shell".to_string(), "filesystem".to_string()],
            },
            providers: vec![ProviderCatalogEntry {
                provider: "mock".to_string(),
                model: Some("mock-model".to_string()),
                label: Some("Mock Model".to_string()),
                family: Some("mock-family".to_string()),
                quant: Some("Q4_K_M".to_string()),
            }],
        }
    }

    #[tokio::test]
    async fn actor_returns_backend_snapshot() {
        let actor = ProviderCatalogActor::new(Arc::new(StaticCatalogBackend {
            snapshot: sample_snapshot(),
        }));
        let actor_ref = ProviderCatalogActor::spawn(actor);

        let snapshot = actor_ref
            .ask(GetProviderCatalog)
            .await
            .expect("actor message should succeed");

        assert_eq!(snapshot.node.node_id, "12D3KooWcatalog");
        assert_eq!(snapshot.providers.len(), 1);
        assert_eq!(snapshot.providers[0].model.as_deref(), Some("mock-model"));
        assert_eq!(snapshot.providers[0].label.as_deref(), Some("Mock Model"));
        assert_eq!(snapshot.providers[0].family.as_deref(), Some("mock-family"));
        assert_eq!(snapshot.providers[0].quant.as_deref(), Some("Q4_K_M"));
    }

    #[test]
    fn fallback_provider_host_catalog_matches_provider_host_scope_name() {
        let scope = MeshScopeId::lan_default();
        let name = fallback_provider_host_catalog(&scope, "peer-123");
        assert_eq!(name, "scope::lan::default::provider_host::peer::peer-123");
    }

    #[test]
    fn snapshot_round_trips_rich_metadata() {
        let snapshot = sample_snapshot();
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        let decoded: ProviderCatalogSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");

        assert_eq!(decoded, snapshot);
    }
}
