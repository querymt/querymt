use crate::{
    MeshHandle, MeshScopeId, MeshTransportKind, RemoteLookupError, enabled_transports_from_mode,
    mode_has_transport, scoped_actor_name,
};
use kameo::actor::{ActorRef, RemoteActorRef};
use kameo::remote::LookupStream;
use libp2p::PeerId;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use crate::mesh_events::PeerEvent;
use crate::{InviteError, InviteStore, MeshStateStore, PeerEntry, SignedInviteGrant};

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

    pub fn request_shutdown(&self) {
        self.inner.request_shutdown();
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

    pub async fn lookup_actor_with_timeout<A>(
        &self,
        name: impl Into<String>,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor_with_timeout(name, timeout).await
    }

    pub async fn lookup_actor_no_retry_with_timeout<A>(
        &self,
        name: impl Into<String>,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner
            .lookup_actor_no_retry_with_timeout(name, timeout)
            .await
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

    pub async fn register_actor_scoped<A>(
        &self,
        actor_ref: ActorRef<A>,
        scope: &MeshScopeId,
        name: &str,
    ) where
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
        self.inner
            .lookup_actor(scoped_actor_name(scope, name))
            .await
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

    pub async fn lookup_actor_scoped_with_timeout<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner
            .lookup_actor_with_timeout(scoped_actor_name(scope, name), timeout)
            .await
    }

    pub async fn lookup_actor_scoped_no_retry_with_timeout<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner
            .lookup_actor_no_retry_with_timeout(scoped_actor_name(scope, name), timeout)
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
        self.inner
            .create_invite(mesh_name, ttl_secs, max_uses, can_invite)
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
        self.runtime
            .register_actor_scoped(actor_ref, &self.scope, name)
            .await
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
        self.runtime
            .lookup_actor_scoped_no_retry(&self.scope, name)
            .await
    }

    pub async fn lookup_actor_with_timeout<A>(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime
            .lookup_actor_scoped_with_timeout(&self.scope, name, timeout)
            .await
    }

    pub async fn lookup_actor_no_retry_with_timeout<A>(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<Option<RemoteActorRef<A>>, RemoteLookupError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime
            .lookup_actor_scoped_no_retry_with_timeout(&self.scope, name, timeout)
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh_runtime_support::{DialReason, SwarmCommand};
    use crate::{MeshEvent, MeshTransportMode, RouteTable};
    use parking_lot::RwLock;
    use std::collections::HashMap;

    fn test_runtime(
        mode: MeshTransportMode,
    ) -> (
        MeshRuntimeHandle,
        tokio::sync::mpsc::UnboundedReceiver<SwarmCommand>,
    ) {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let (peer_events_tx, _peer_events_rx) = broadcast::channel(8);
        let routes = Arc::new(RouteTable::new(Duration::from_secs(60)));
        let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
        let (swarm_cmd_tx, swarm_cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = MeshHandle::new(
            peer_id,
            peer_events_tx,
            routes,
            "test-host".to_string(),
            re_register_fns,
            keypair,
            None,
            None,
            mode,
            swarm_cmd_tx,
            Duration::from_secs(30),
        );
        (MeshRuntimeHandle::new(handle), swarm_cmd_rx)
    }

    #[test]
    fn runtime_handle_exposes_basic_identity_and_transport_state() {
        let (runtime, _swarm_cmd_rx) = test_runtime(MeshTransportMode::Composite);

        assert_eq!(runtime.local_hostname(), "test-host");
        assert_eq!(runtime.stream_reconnect_grace(), Duration::from_secs(30));
        assert_eq!(
            runtime.enabled_transports(),
            vec![MeshTransportKind::Lan, MeshTransportKind::Iroh]
        );
        assert!(runtime.has_transport(MeshTransportKind::Lan));
        assert!(runtime.has_transport(MeshTransportKind::Iroh));
        assert_eq!(
            MeshRuntimeHandle::from(&runtime).peer_id(),
            runtime.peer_id()
        );
        assert_eq!(runtime.as_ref().peer_id(), runtime.peer_id());
    }

    #[test]
    fn runtime_handle_scope_wraps_runtime_and_preserves_scope_metadata() {
        let (runtime, _swarm_cmd_rx) = test_runtime(MeshTransportMode::Iroh);
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };

        let scoped = runtime.scope(scope.clone());
        assert_eq!(scoped.scope_id(), &scope);
        assert_eq!(scoped.runtime().peer_id(), runtime.peer_id());
        assert_eq!(scoped.peer_id(), runtime.peer_id());
        assert_eq!(scoped.local_hostname(), runtime.local_hostname());
    }

    #[test]
    fn runtime_handle_reports_injected_known_peers() {
        let (runtime, _swarm_cmd_rx) = test_runtime(MeshTransportMode::Lan);
        let known_peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();

        runtime
            .as_mesh_handle()
            .inject_known_peer_for_test(known_peer);

        assert!(runtime.is_peer_alive(&known_peer));
        assert_eq!(runtime.known_peer_ids(), vec![known_peer]);
    }

    #[test]
    fn subscribe_peer_events_receives_handle_broadcasts() {
        let (runtime, _swarm_cmd_rx) = test_runtime(MeshTransportMode::Lan);
        let mut rx = runtime.subscribe_peer_events();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };

        runtime.as_mesh_handle().emit_scope_joined(scope.clone());

        assert!(matches!(rx.try_recv().unwrap(), MeshEvent::ScopeJoined(found) if found == scope));
    }

    #[test]
    fn request_shutdown_delegates_to_mesh_handle_command_channel() {
        let (runtime, mut swarm_cmd_rx) = test_runtime(MeshTransportMode::Composite);

        runtime.request_shutdown();

        match swarm_cmd_rx.try_recv().unwrap() {
            SwarmCommand::Shutdown => {}
            other => panic!("expected Shutdown command, got {other:?}"),
        }
    }

    #[test]
    fn dial_peer_delegates_to_mesh_handle_command_channel() {
        let (runtime, mut swarm_cmd_rx) = test_runtime(MeshTransportMode::Composite);
        let remote_peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();

        runtime.dial_peer(&remote_peer);

        match swarm_cmd_rx.try_recv().unwrap() {
            SwarmCommand::DialPeer {
                peer_id,
                scope,
                reason,
            } => {
                assert_eq!(peer_id, remote_peer);
                assert_eq!(scope, None);
                assert_eq!(reason, DialReason::Manual);
            }
            other => panic!("expected DialPeer command, got {other:?}"),
        }
    }

    #[test]
    fn dial_peer_is_noop_in_lan_only_mode() {
        let (runtime, mut swarm_cmd_rx) = test_runtime(MeshTransportMode::Lan);
        let remote_peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();

        runtime.dial_peer(&remote_peer);

        assert!(swarm_cmd_rx.try_recv().is_err());
    }
}
