#[cfg(feature = "kameo-mesh")]
use crate::{MeshRuntimeHandle, MeshScopeId, scoped_provider_catalog, scoped_provider_host};
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
    #[cfg(feature = "kameo-mesh")]
    guarded_registration: Arc<std::sync::OnceLock<Arc<guarded::GuardedProvider>>>,
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
            #[cfg(feature = "kameo-mesh")]
            guarded_registration: Default::default(),
        }
    }

    pub fn provider_host(&self) -> &ActorRef<ProviderHostActor> {
        &self.provider_host
    }

    pub fn provider_catalog(&self) -> &ActorRef<ProviderCatalogActor> {
        &self.provider_catalog
    }

    /// Publish this provider share in every active scope of the mesh runtime.
    #[cfg(feature = "kameo-mesh")]
    pub async fn register_on_mesh(&self, runtime: &MeshRuntimeHandle) {
        for scope in runtime.active_scopes() {
            self.register_scope(runtime, &scope).await;
        }
    }

    /// Publish the provider share in one scope. Adding a scope does not replace
    /// the existing guarded handlers or their active stream state.
    #[cfg(feature = "kameo-mesh")]
    pub async fn register_scope(&self, runtime: &MeshRuntimeHandle, scope: &MeshScopeId) {
        let mesh = runtime.as_mesh_handle();
        if let (Some(router), Some(store)) = (mesh.inbound_router(), mesh.quota_store()) {
            // Kameo has one mesh runtime per process, so the same share is
            // registered once and then published under multiple names.
            self.guarded_registration.get_or_init(|| {
                let provider = Arc::new(guarded::GuardedProvider {
                    backend: self.backend.clone(),
                    catalog: self.catalog.clone(),
                    store: store.clone(),
                });
                provider.clone().install(
                    router,
                    self.provider_catalog.id(),
                    self.provider_host.id(),
                );
                guarded::streams::install(
                    provider.clone(),
                    mesh,
                    router,
                    self.provider_host.id(),
                    &self.provider_host,
                );
                provider
            });
        }
        runtime
            .register_actor(
                self.provider_host.clone(),
                scoped_provider_host(scope, runtime.peer_id()),
            )
            .await;
        runtime
            .register_actor(
                self.provider_catalog.clone(),
                scoped_provider_catalog(scope, runtime.peer_id()),
            )
            .await;
    }
}
