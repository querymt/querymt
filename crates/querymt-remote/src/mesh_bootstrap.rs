use crate::mesh_events::MeshEvent;
use crate::mesh_handle::ReRegisterFn;
use crate::mesh_routes::RouteTable;
use crate::mesh_runtime_support::SwarmCommand;
use crate::{
    InviteStore, MeshError, MeshHandle, MeshStateStore, MeshTransportMode, default_identity_path,
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
    pub(crate) invite_store: Option<Arc<RwLock<InviteStore>>>,
    pub(crate) mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
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

    MeshHandle::new(
        local_peer_id,
        ctx.peer_events_tx,
        ctx.routes,
        ctx.local_hostname,
        ctx.re_register_fns,
        ctx.keypair,
        ctx.invite_store,
        ctx.mesh_state_store,
        transport_mode,
        swarm_cmd_tx,
        stream_reconnect_grace,
    )
}

fn same_identity_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// Preserve the existing default identity's files; custom identities get private sidecars.
fn state_path(
    identity_file: Option<&std::path::Path>,
    filename: &str,
) -> Result<std::path::PathBuf, MeshError> {
    if let Some(identity) = identity_file
        && !default_identity_path().is_ok_and(|default| same_identity_path(&default, identity))
    {
        let mut path = identity.as_os_str().to_os_string();
        path.push(format!(".{filename}"));
        return Ok(path.into());
    }
    let path = match filename {
        "invites.json" => default_invite_store_path(),
        _ => default_mesh_state_path(),
    };
    path.map_err(|e| MeshError::SwarmError(e.to_string()))
}

#[derive(Default)]
struct MeshStores {
    invite_store: Option<Arc<RwLock<InviteStore>>>,
    mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
}

fn load_stores(identity_file: Option<&std::path::Path>) -> MeshStores {
    let invite_store = match state_path(identity_file, "invites.json") {
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

    let mesh_state_store = match state_path(identity_file, "mesh_state.json") {
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

    MeshStores {
        invite_store,
        mesh_state_store,
    }
}

pub(crate) fn prepare_runtime_bootstrap(
    identity_file: Option<&std::path::Path>,
    peers: &[String],
    local_hostname: String,
    has_iroh: bool,
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

    let MeshStores {
        invite_store,
        mesh_state_store,
    } = if has_iroh {
        load_stores(identity_file)
    } else {
        MeshStores::default()
    };

    Ok(MeshBootstrapContext {
        invite_store,
        mesh_state_store,
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_identity_keeps_legacy_state_paths() {
        assert_eq!(
            state_path(None, "mesh_state.json").unwrap(),
            default_mesh_state_path().unwrap()
        );
        let identity = default_identity_path().unwrap();
        assert_eq!(
            state_path(Some(&identity), "invites.json").unwrap(),
            default_invite_store_path().unwrap()
        );
    }

    #[test]
    fn identity_path_comparison_recognizes_aliases_without_conflating_custom_keys() {
        let dir = tempfile::tempdir_in(".").unwrap();
        let identity = dir.path().join("default.key");
        let custom = dir.path().join("custom.key");
        std::fs::write(&identity, "default").unwrap();
        std::fs::write(&custom, "custom").unwrap();
        let absolute = std::fs::canonicalize(&identity).unwrap();
        assert!(same_identity_path(&identity, &absolute));
        assert!(!same_identity_path(&custom, &absolute));
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias.key");
            std::os::unix::fs::symlink(&absolute, &alias).unwrap();
            assert!(same_identity_path(&alias, &absolute));
        }
        let missing = dir.path().join("not-created.key");
        assert!(same_identity_path(&missing, &missing));
        assert!(!same_identity_path(&missing, &absolute));
    }

    #[test]
    fn custom_identities_have_distinct_state_even_in_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.key");
        let b = dir.path().join("b.key");
        let a_store = load_stores(Some(&a)).mesh_state_store;
        a_store
            .unwrap()
            .write()
            .upsert_hosted_mesh("a".into(), None, None)
            .unwrap();
        let b_store = load_stores(Some(&b)).mesh_state_store;
        assert!(b_store.unwrap().read().active_mesh_ids().is_empty());
        assert_ne!(
            state_path(Some(&a), "mesh_state.json").unwrap(),
            state_path(Some(&b), "mesh_state.json").unwrap()
        );
    }

    #[test]
    fn lan_only_bootstrap_does_not_load_iroh_membership() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("identity.key");
        let ctx = prepare_runtime_bootstrap(Some(&identity), &[], "test".into(), false).unwrap();
        assert!(ctx.mesh_state_store.is_none());
        assert!(ctx.invite_store.is_none());
    }

    #[test]
    fn handle_shares_the_runtime_membership_store() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("identity.key");
        let ctx = prepare_runtime_bootstrap(Some(&identity), &[], "test".into(), true).unwrap();
        let store = ctx.mesh_state_store.clone().unwrap();
        let peer = ctx.keypair.public().to_peer_id();
        let (tx, _rx) = mpsc::unbounded_channel();
        let handle = finalize_bootstrap(
            peer,
            ctx,
            "test",
            MeshTransportMode::Iroh,
            tx,
            Duration::from_secs(1),
        );
        assert!(Arc::ptr_eq(handle.mesh_state_store().unwrap(), &store));
        handle
            .create_invite(Some("test".into()), None, None, false)
            .unwrap();
        assert_eq!(store.read().active_mesh_ids().len(), 1);
    }
}
