use crate::{
    MeshRuntimeHandle, ProviderCatalogActor, ProviderCatalogBackend, ProviderHostActor,
    RemoteProviderBackend, RemoteProviderHostError, scoped_provider_catalog, scoped_provider_host,
};
use kameo::actor::{ActorRef, Spawn};
use std::sync::Arc;

#[derive(Clone)]
pub struct ProviderShare {
    provider_host: ActorRef<ProviderHostActor>,
    provider_catalog: ActorRef<ProviderCatalogActor>,
}

impl ProviderShare {
    pub fn new(
        backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
        catalog: Arc<dyn ProviderCatalogBackend>,
    ) -> Self {
        let provider_host = ProviderHostActor::spawn(ProviderHostActor::new(backend));
        let provider_catalog = ProviderCatalogActor::spawn(ProviderCatalogActor::new(catalog));
        Self {
            provider_host,
            provider_catalog,
        }
    }

    pub fn provider_host(&self) -> &ActorRef<ProviderHostActor> {
        &self.provider_host
    }

    pub fn provider_catalog(&self) -> &ActorRef<ProviderCatalogActor> {
        &self.provider_catalog
    }

    pub async fn register_on_mesh(&self, runtime: &MeshRuntimeHandle) {
        for scope in runtime.active_scopes() {
            runtime
                .register_actor(
                    self.provider_host.clone(),
                    scoped_provider_host(&scope, runtime.peer_id()),
                )
                .await;
            runtime
                .register_actor(
                    self.provider_catalog.clone(),
                    scoped_provider_catalog(&scope, runtime.peer_id()),
                )
                .await;
        }
    }
}
