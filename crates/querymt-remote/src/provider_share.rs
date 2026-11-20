#[cfg(feature = "kameo-mesh")]
use crate::{MeshRuntimeHandle, scoped_provider_catalog, scoped_provider_host};
use crate::{
    ProviderCatalogActor, ProviderCatalogBackend, ProviderHostActor, RemoteProviderBackend,
    RemoteProviderHostError,
};
use kameo::actor::{ActorRef, Spawn};
use std::sync::Arc;

#[cfg(feature = "kameo-mesh")]
pub(crate) mod guarded;

#[derive(Clone)]
pub struct ProviderShare {
    provider_host: ActorRef<ProviderHostActor>,
    provider_catalog: ActorRef<ProviderCatalogActor>,
    #[cfg(feature = "kameo-mesh")]
    backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
    #[cfg(feature = "kameo-mesh")]
    catalog: Arc<dyn ProviderCatalogBackend>,
}

impl ProviderShare {
    pub fn new(
        backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
        catalog: Arc<dyn ProviderCatalogBackend>,
    ) -> Self {
        let provider_host = ProviderHostActor::spawn(ProviderHostActor::new(backend.clone()));
        let provider_catalog =
            ProviderCatalogActor::spawn(ProviderCatalogActor::new(catalog.clone()));
        Self {
            provider_host,
            provider_catalog,
            #[cfg(feature = "kameo-mesh")]
            backend,
            #[cfg(feature = "kameo-mesh")]
            catalog,
        }
    }

    pub fn provider_host(&self) -> &ActorRef<ProviderHostActor> {
        &self.provider_host
    }

    pub fn provider_catalog(&self) -> &ActorRef<ProviderCatalogActor> {
        &self.provider_catalog
    }

    #[cfg(feature = "kameo-mesh")]
    pub async fn register_on_mesh(&self, runtime: &MeshRuntimeHandle) {
        let mesh = runtime.as_mesh_handle();
        if let (Some(router), Some(store)) = (mesh.inbound_router(), mesh.quota_store()) {
            Arc::new(guarded::GuardedProvider {
                backend: self.backend.clone(),
                catalog: self.catalog.clone(),
                store: store.clone(),
            })
            .install(router, self.provider_catalog.id(), self.provider_host.id());
        }
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
