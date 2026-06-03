use crate::{MeshHandle, MeshScopeId, MeshTransportKind, enabled_transports_from_mode, mode_has_transport, scoped_actor_name};
use kameo::actor::{ActorRef, RemoteActorRef};
use kameo::remote::LookupStream;
use libp2p::PeerId;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::{InviteError, InviteStore, MeshStateStore, PeerEntry, SignedInviteGrant};
use crate::mesh_events::PeerEvent;

#[derive(Clone)]
pub struct MeshRuntimeHandle {
    inner: Arc<MeshHandle>,
}

impl Debug for MeshRuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshRuntimeHandle").finish_non_exhaustive()
    }
}

impl MeshRuntimeHandle {
    pub fn new(handle: MeshHandle) -> Self {
        Self {
            inner: Arc::new(handle),
        }
    }

    pub fn as_mesh_handle(&self) -> &MeshHandle {
        &self.inner
    }

    pub fn peer_id(&self) -> &PeerId {
        self.inner.peer_id()
    }

    pub fn local_hostname(&self) -> &str {
        self.inner.local_hostname()
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.inner.is_peer_alive(peer_id)
    }

    pub fn stream_reconnect_grace(&self) -> std::time::Duration {
        self.inner.stream_reconnect_grace()
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.inner.subscribe_peer_events()
    }

    pub fn known_peer_ids(&self) -> Vec<PeerId> {
        self.inner.known_peer_ids()
    }

    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        self.inner.active_scopes()
    }

    pub fn joined_iroh_scopes(&self) -> Vec<MeshScopeId> {
        self.inner.joined_iroh_scopes()
    }

    pub async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: impl Into<String>)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.register_actor(actor_ref, name).await
    }

    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor(name).await
    }

    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor_no_retry(name).await
    }

    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_all_actors(name)
    }

    pub fn deregister_actor(&self, name: &str) {
        self.inner.deregister_actor(name)
    }

    pub fn re_register_fns_count(&self) -> usize {
        self.inner.re_register_fns_count()
    }

    pub fn has_re_register_fn(&self, name: &str) -> bool {
        self.inner.has_re_register_fn(name)
    }

    pub fn scope(&self, scope: MeshScopeId) -> MeshScopeHandle {
        MeshScopeHandle {
            runtime: self.clone(),
            scope,
        }
    }

    pub async fn register_actor_scoped<A>(&self, actor_ref: ActorRef<A>, scope: &MeshScopeId, name: &str)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner
            .register_actor(actor_ref, scoped_actor_name(scope, name))
            .await
    }

    pub async fn lookup_actor_scoped<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor(scoped_actor_name(scope, name)).await
    }

    pub async fn lookup_actor_scoped_no_retry<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner
            .lookup_actor_no_retry(scoped_actor_name(scope, name))
            .await
    }

    pub fn lookup_all_actors_scoped<A>(&self, scope: &MeshScopeId, name: &str) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_all_actors(scoped_actor_name(scope, name))
    }

    pub fn deregister_actor_scoped(&self, scope: &MeshScopeId, name: &str) {
        self.inner.deregister_actor(&scoped_actor_name(scope, name))
    }

    pub fn enabled_transports(&self) -> Vec<MeshTransportKind> {
        enabled_transports_from_mode(self.inner.transport_mode())
    }

    pub fn has_transport(&self, kind: MeshTransportKind) -> bool {
        mode_has_transport(self.inner.transport_mode(), kind)
    }

    pub async fn resolve_peer_node_id(&self, peer_name: &str) -> Option<crate::NodeId> {
        self.inner.resolve_peer_node_id(peer_name).await
    }

    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.inner.dial_peer(peer_id)
    }

    pub fn create_invite(
        &self,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: Option<u32>,
        can_invite: bool,
    ) -> Result<SignedInviteGrant, InviteError> {
        self.inner.create_invite(mesh_name, ttl_secs, max_uses, can_invite)
    }

    pub fn keypair(&self) -> &libp2p::identity::Keypair {
        self.inner.keypair()
    }

    pub fn invite_store(&self) -> Option<&Arc<parking_lot::RwLock<InviteStore>>> {
        self.inner.invite_store()
    }

    pub fn mesh_state_store(&self) -> Option<&Arc<parking_lot::RwLock<MeshStateStore>>> {
        self.inner.mesh_state_store()
    }

    pub fn update_mesh_state_peers(&self, mesh_id: &str, peers: Vec<PeerEntry>) {
        self.inner.update_mesh_state_peers(mesh_id, peers)
    }

    pub fn leave_iroh_scope(&self, mesh_id: &str) -> Result<bool, InviteError> {
        self.inner.leave_iroh_scope(mesh_id)
    }
}

impl From<MeshHandle> for MeshRuntimeHandle {
    fn from(handle: MeshHandle) -> Self {
        Self::new(handle)
    }
}

impl From<&MeshRuntimeHandle> for MeshRuntimeHandle {
    fn from(handle: &MeshRuntimeHandle) -> Self {
        handle.clone()
    }
}

impl AsRef<MeshHandle> for MeshRuntimeHandle {
    fn as_ref(&self) -> &MeshHandle {
        &self.inner
    }
}

#[derive(Clone, Debug)]
pub struct MeshScopeHandle {
    runtime: MeshRuntimeHandle,
    scope: MeshScopeId,
}

impl MeshScopeHandle {
    pub fn scope_id(&self) -> &MeshScopeId {
        &self.scope
    }

    pub fn runtime(&self) -> &MeshRuntimeHandle {
        &self.runtime
    }

    pub fn peer_id(&self) -> &PeerId {
        self.runtime.peer_id()
    }

    pub fn local_hostname(&self) -> &str {
        self.runtime.local_hostname()
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.runtime.is_peer_alive(peer_id)
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.runtime.subscribe_peer_events()
    }

    pub async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: &str)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.register_actor_scoped(actor_ref, &self.scope, name).await
    }

    pub async fn lookup_actor<A>(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.lookup_actor_scoped(&self.scope, name).await
    }

    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.lookup_actor_scoped_no_retry(&self.scope, name).await
    }

    pub fn lookup_all_actors<A>(&self, name: &str) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.lookup_all_actors_scoped(&self.scope, name)
    }

    pub fn deregister_actor(&self, name: &str) {
        self.runtime.deregister_actor_scoped(&self.scope, name)
    }

    pub async fn resolve_peer_node_id(&self, peer_name: &str) -> Option<crate::NodeId> {
        self.runtime.resolve_peer_node_id(peer_name).await
    }

    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.runtime.dial_peer(peer_id)
    }
}
