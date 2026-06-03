use crate::mesh_events::MeshEvent;
use crate::mesh_handle::ReRegisterFn;
use crate::mesh_routes::RouteTable;
use crate::mesh_runtime_support::SwarmCommand;
use crate::{
    InviteStore, MeshError, MeshHandle, MeshStateStore, MeshTransportMode,
    default_invite_store_path, default_mesh_state_path,
};
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub(crate) struct MeshBootstrapContext {
    pub(crate) keypair: libp2p::identity::Keypair,
    pub(crate) peer_events_tx: broadcast::Sender<MeshEvent>,
    pub(crate) routes: Arc<RouteTable>,
    pub(crate) known_peers: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
    pub(crate) re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    pub(crate) local_hostname: String,
}

pub(crate) fn finalize_bootstrap(
    local_peer_id: PeerId,
    ctx: MeshBootstrapContext,
    listen_label: &str,
    transport_mode: MeshTransportMode,
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    stream_reconnect_grace: std::time::Duration,
) -> MeshHandle {
    log::info!(
        "Kameo mesh bootstrapped: peer_id={}, listen={}",
        local_peer_id,
        listen_label,
    );

    let invite_store = match default_invite_store_path() {
        Ok(path) => match InviteStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!("Failed to load invite store: {e}; invites will not be persisted");
                None
            }
        },
        Err(e) => {
            log::warn!("Cannot determine invite store path: {e}; invites will not be persisted");
            None
        }
    };

    let mesh_state_store = match default_mesh_state_path() {
        Ok(path) => match MeshStateStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!(
                    "Failed to load mesh state store: {e}; iroh state will not be persisted"
                );
                None
            }
        },
        Err(e) => {
            log::warn!("Cannot determine mesh state path: {e}; iroh state will not be persisted");
            None
        }
    };

    MeshHandle::new(
        local_peer_id,
        ctx.peer_events_tx,
        ctx.routes,
        ctx.local_hostname,
        ctx.re_register_fns,
        ctx.keypair,
        invite_store,
        mesh_state_store,
        transport_mode,
        swarm_cmd_tx,
        stream_reconnect_grace,
    )
}

pub(crate) fn prepare_runtime_bootstrap(
    identity_file: Option<&std::path::Path>,
    peers: &[String],
    local_hostname: String,
) -> Result<MeshBootstrapContext, MeshError> {
    let keypair = crate::load_or_generate_keypair(identity_file)
        .map_err(|e| MeshError::SwarmError(format!("failed to load mesh identity: {e}")))?;

    for peer_addr in peers {
        peer_addr
            .parse::<libp2p::Multiaddr>()
            .map_err(|e| MeshError::InvalidBootstrapAddr {
                addr: peer_addr.clone(),
                reason: e.to_string(),
            })?;
    }

    let (peer_events_tx, _) = broadcast::channel::<MeshEvent>(32);
    let known_peers = Arc::new(RwLock::new(HashMap::<PeerId, HashSet<Multiaddr>>::new()));
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));

    Ok(MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    })
}
