use kameo::remote;
use libp2p::PeerId;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use crate::mesh_events::MeshEvent;
use crate::mesh_routes::{MeshRoute, RouteTable};
use crate::mesh_runtime_support::{DialReason, SwarmCommand};
use crate::{
    InviteError, InviteGrant, InvitePermissions, InviteStore, MeshScopeId, MeshStateStore,
    MeshTransportMode, NodeId, PeerEntry, SignedInviteGrant, mesh_id_for,
};

pub type ReRegisterFn = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

type PeerNodeResolver = Arc<
    dyn Fn(&MeshHandle, &str) -> Pin<Box<dyn Future<Output = Option<NodeId>> + Send>> + Send + Sync,
>;

#[derive(Clone)]
pub struct MeshHandle {
    peer_id: PeerId,
    pub(crate) peer_events_tx: broadcast::Sender<MeshEvent>,
    routes: Arc<RouteTable>,
    local_hostname: Arc<String>,
    pub(crate) re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    keypair: Arc<libp2p::identity::Keypair>,
    invite_store: Option<Arc<RwLock<InviteStore>>>,
    mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
    transport_mode: MeshTransportMode,
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    stream_reconnect_grace: std::time::Duration,
    config_scopes: Arc<RwLock<Vec<MeshScopeId>>>,
    peer_node_resolver: Option<PeerNodeResolver>,
}

impl std::fmt::Debug for MeshHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshHandle")
            .field("peer_id", &self.peer_id)
            .field("local_hostname", &self.local_hostname)
            .field("re_register_fns_count", &self.re_register_fns.read().len())
            .field("has_invite_store", &self.invite_store.is_some())
            .field("has_mesh_state_store", &self.mesh_state_store.is_some())
            .field("transport_mode", &self.transport_mode)
            .finish_non_exhaustive()
    }
}

impl MeshHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        peer_id: PeerId,
        peer_events_tx: broadcast::Sender<MeshEvent>,
        routes: Arc<RouteTable>,
        local_hostname: String,
        re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
        keypair: libp2p::identity::Keypair,
        invite_store: Option<Arc<RwLock<InviteStore>>>,
        mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
        transport_mode: MeshTransportMode,
        swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
        stream_reconnect_grace: std::time::Duration,
    ) -> Self {
        Self {
            peer_id,
            peer_events_tx,
            routes,
            local_hostname: Arc::new(local_hostname),
            re_register_fns,
            keypair: Arc::new(keypair),
            invite_store,
            mesh_state_store,
            transport_mode,
            swarm_cmd_tx,
            stream_reconnect_grace,
            config_scopes: Arc::new(RwLock::new(Vec::new())),
            peer_node_resolver: None,
        }
    }

    pub fn with_peer_node_resolver(mut self, resolver: PeerNodeResolver) -> Self {
        self.peer_node_resolver = Some(resolver);
        self
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn local_hostname(&self) -> &str {
        &self.local_hostname
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.routes.is_peer_alive(peer_id)
    }

    pub fn stream_reconnect_grace(&self) -> std::time::Duration {
        self.stream_reconnect_grace
    }

    #[cfg(test)]
    pub fn inject_known_peer_for_test(&self, peer_id: PeerId) {
        self.routes.upsert_addrs(
            peer_id,
            crate::MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [libp2p::Multiaddr::empty()],
            100,
        );
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.peer_events_tx.subscribe()
    }

    pub async fn register_actor<A>(&self, actor_ref: kameo::actor::ActorRef<A>, name: impl Into<String>)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name = name.into();
        actor_ref.into_remote_ref().await;
        if let Err(e) = actor_ref.register(name.clone()).await {
            log::warn!("MeshHandle: failed to register '{}' in DHT: {}", name, e);
        }

        let name_clone = name.clone();
        let actor_ref_clone = actor_ref.clone();
        let re_register: ReRegisterFn = Arc::new(move || {
            let name = name_clone.clone();
            let actor_ref = actor_ref_clone.clone();
            Box::pin(async move {
                actor_ref.into_remote_ref().await;
                if let Err(e) = actor_ref.register(name.clone()).await {
                    log::warn!(
                        "MeshHandle: re-registration of '{}' after peer discovery failed: {}",
                        name,
                        e
                    );
                }
            })
        });
        self.re_register_fns.write().insert(name, re_register);
    }

    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        const BACKOFF_MS: &[u64] = &[250, 500, 1_000];
        let name: String = name.into();

        if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
            return Ok(Some(r));
        }

        for &delay_ms in BACKOFF_MS {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
                return Ok(Some(r));
            }
        }

        Ok(None)
    }

    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name: String = name.into();
        kameo::actor::RemoteActorRef::<A>::lookup(name).await
    }

    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        kameo::actor::RemoteActorRef::<A>::lookup_all(name.into())
    }

    pub async fn resolve_peer_node_id(&self, peer_name: &str) -> Option<NodeId> {
        let resolver = self.peer_node_resolver.as_ref()?;
        resolver(self, peer_name).await
    }

    pub fn deregister_actor(&self, name: &str) {
        self.re_register_fns.write().remove(name);
    }

    pub fn re_register_fns_count(&self) -> usize {
        self.re_register_fns.read().len()
    }

    pub fn has_re_register_fn(&self, name: &str) -> bool {
        self.re_register_fns.read().contains_key(name)
    }

    pub fn known_peer_ids(&self) -> Vec<PeerId> {
        self.routes.peer_ids()
    }

    pub fn set_config_scopes(&mut self, scopes: Vec<MeshScopeId>) {
        let mut guard = self.config_scopes.write();
        *guard = scopes;
        guard.sort_by_key(|s| s.to_string());
        guard.dedup();
    }

    pub fn ensure_scope(&self, scope: MeshScopeId) -> bool {
        let mut guard = self.config_scopes.write();
        if guard.contains(&scope) {
            return false;
        }
        guard.push(scope);
        guard.sort_by_key(|s| s.to_string());
        guard.dedup();
        true
    }

    pub fn join_iroh_scope(&self, mesh_id: &str, peers: Vec<PeerId>) {
        self.ensure_scope(MeshScopeId::Iroh {
            mesh_id: mesh_id.to_string(),
        });
        let _ = self.swarm_cmd_tx.send(SwarmCommand::JoinIrohScope {
            mesh_id: mesh_id.to_string(),
            peers,
        });
    }

    pub fn emit_scope_joined(&self, scope: MeshScopeId) {
        let _ = self.peer_events_tx.send(MeshEvent::ScopeJoined(scope));
    }

    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        let mut scopes = Vec::new();
        scopes.extend(self.config_scopes.read().iter().cloned());

        if let Some(store) = &self.mesh_state_store {
            for mesh_id in store.read().active_mesh_ids() {
                let scope = MeshScopeId::Iroh { mesh_id };
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }

        if self.transport_mode.has_lan() {
            let lan = MeshScopeId::lan_default();
            if !scopes.contains(&lan) {
                scopes.push(lan);
            }
        }

        scopes.sort_by_key(|s| s.to_string());
        scopes.dedup();
        scopes
    }

    pub fn joined_iroh_scopes(&self) -> Vec<MeshScopeId> {
        let Some(store) = &self.mesh_state_store else {
            return Vec::new();
        };
        store
            .read()
            .active_mesh_ids()
            .into_iter()
            .map(|mesh_id| MeshScopeId::Iroh { mesh_id })
            .collect()
    }

    pub fn leave_iroh_scope(&self, mesh_id: &str) -> Result<bool, InviteError> {
        let Some(store) = &self.mesh_state_store else {
            return Ok(false);
        };
        let removed = store.write().mark_left(mesh_id)?;
        if !removed {
            return Ok(false);
        }

        let scope = MeshScopeId::Iroh {
            mesh_id: mesh_id.to_string(),
        };
        let prefix = scope.dht_prefix();
        self.re_register_fns
            .write()
            .retain(|name, _| !name.starts_with(&prefix));
        self.config_scopes
            .write()
            .retain(|existing| existing != &scope);

        let _ = self.peer_events_tx.send(MeshEvent::ScopeLeft(scope.clone()));
        let _ = self.swarm_cmd_tx.send(SwarmCommand::LeaveIrohScope {
            mesh_id: mesh_id.to_string(),
        });
        Ok(true)
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes.best_route_for_peer(peer_id)
    }

    pub fn route_peer_ids(&self) -> Vec<PeerId> {
        self.routes.peer_ids()
    }

    pub fn create_invite(
        &self,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: Option<u32>,
        can_invite: bool,
    ) -> Result<SignedInviteGrant, InviteError> {
        let max_uses = max_uses.unwrap_or(1);
        let permissions = InvitePermissions {
            can_invite,
            role: "member".to_string(),
        };

        let mesh_name_for_scope = mesh_name.clone();
        let invite = if let Some(ref store) = self.invite_store {
            store.write().create_invite(
                &self.keypair,
                &self.peer_id.to_string(),
                mesh_name,
                ttl_secs,
                max_uses,
                permissions,
            )
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expires_at = ttl_secs.map(|ttl| now + ttl).unwrap_or(0);

            let grant = InviteGrant {
                version: 3,
                invite_id: uuid::Uuid::now_v7().to_string(),
                inviter_peer_id: self.peer_id.to_string(),
                mesh_name,
                expires_at,
                max_uses,
                permissions,
            };
            grant.sign(&self.keypair)
        }?;

        let mesh_id = mesh_id_for(&self.peer_id.to_string(), mesh_name_for_scope.as_deref());
        self.ensure_scope(MeshScopeId::Iroh {
            mesh_id: mesh_id.clone(),
        });

        if let Some(ref store) = self.mesh_state_store {
            let _ = store.write().upsert_hosted_mesh(
                mesh_id,
                mesh_name_for_scope,
                Some(invite.grant.invite_id.clone()),
            );
        }

        Ok(invite)
    }

    pub fn keypair(&self) -> &libp2p::identity::Keypair {
        &self.keypair
    }

    pub fn invite_store(&self) -> Option<&Arc<RwLock<InviteStore>>> {
        self.invite_store.as_ref()
    }

    pub fn mesh_state_store(&self) -> Option<&Arc<RwLock<MeshStateStore>>> {
        self.mesh_state_store.as_ref()
    }

    pub fn update_mesh_state_peers(&self, mesh_id: &str, peers: Vec<PeerEntry>) {
        if let Some(ref store) = self.mesh_state_store {
            let _ = store.write().update_known_peers(mesh_id, peers);
        }
    }

    pub fn transport_mode(&self) -> MeshTransportMode {
        self.transport_mode.clone()
    }

    pub fn is_iroh_transport_internal(&self) -> bool {
        matches!(self.transport_mode, MeshTransportMode::Iroh | MeshTransportMode::Composite)
    }

    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.dial_peer_with_scope(peer_id, None, DialReason::Manual);
    }

    pub fn dial_peer_for_admission(&self, peer_id: &PeerId, scope: MeshScopeId) {
        self.dial_peer_with_scope(peer_id, Some(scope), DialReason::Admission);
    }

    pub fn dial_existing_iroh_peer(&self, peer_id: &PeerId, scope: MeshScopeId) {
        self.dial_peer_with_scope(peer_id, Some(scope), DialReason::ExistingMeshPeer);
    }

    fn dial_peer_with_scope(&self, peer_id: &PeerId, scope: Option<MeshScopeId>, reason: DialReason) {
        if peer_id == &self.peer_id {
            return;
        }
        if matches!(self.transport_mode, MeshTransportMode::Lan) {
            return;
        }

        let _ = self.swarm_cmd_tx.send(SwarmCommand::DialPeer {
            peer_id: *peer_id,
            scope,
            reason,
        });
    }
}
