//! Catalog and quota usage invalidations for peer access views.

use std::sync::Arc;

use kameo::actor::ActorRef;
use querymt_remote::{
    MeshHandle, NodeId,
    peer_access::{PeerGrant, PeerGrantKey, SharingProtocol, quota::QuotaStore},
};
use tokio::sync::broadcast;

use super::{Catalog, local_catalog, notify, notify_peer};
use crate::{
    acp::protocol::ExtNotification,
    agent::remote::{RemoteNodeManager, node_manager::PeerAccessInvalidated},
    model_inventory::ModelInventory,
};

pub(super) fn spawn(
    store: &Arc<dyn QuotaStore>,
    actor: &ActorRef<RemoteNodeManager>,
    inventory: &ModelInventory,
    notifications: &broadcast::Sender<ExtNotification>,
    mesh: &MeshHandle,
) {
    let mut catalog_updates = inventory.subscribe_updates();
    let mut previous_catalog = local_catalog(inventory);
    tokio::spawn({
        let actor = actor.downgrade();
        let notifications = notifications.clone();
        let store = Arc::clone(store);
        let mesh = mesh.clone();
        let inventory = inventory.clone();
        async move {
            let stopped = actor.wait_for_shutdown();
            tokio::pin!(stopped);
            let host = NodeId::from_peer_id(*mesh.peer_id());
            loop {
                tokio::select! {
                    () = &mut stopped => break,
                    update = catalog_updates.recv() => if matches!(update, Err(broadcast::error::RecvError::Closed)) { break; },
                }
                let current = local_catalog(&inventory);
                if current == previous_catalog {
                    continue;
                }
                let previous = std::mem::replace(&mut previous_catalog, current);
                let Ok(peers) = store.managed_peers(&host).await else {
                    continue;
                };
                for peer in peers {
                    notify(
                        &notifications,
                        &peer,
                        "outgoing",
                        PeerAccessInvalidated::Catalog,
                    );
                    let key = PeerGrantKey {
                        host: host.clone(),
                        peer: peer.clone(),
                    };
                    if let Ok(state) = store.grant(&key).await
                        && peer_catalog_changed(&state.grant, &previous, &previous_catalog)
                    {
                        notify_peer(mesh.clone(), peer, PeerAccessInvalidated::Catalog);
                    }
                }
            }
        }
    });
    if let Some(mut usage) = store.subscribe_usage_changes() {
        let mesh = mesh.clone();
        let store = Arc::clone(store);
        let notifications = notifications.clone();
        let actor = actor.downgrade();
        tokio::spawn(async move {
            let stopped = actor.wait_for_shutdown();
            tokio::pin!(stopped);
            loop {
                let key = tokio::select! {
                    () = &mut stopped => break,
                    key = usage.recv() => match key {
                        Ok(key) => key,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                };
                if key.host.as_peer_id() == mesh.peer_id() {
                    notify(
                        &notifications,
                        &key.peer,
                        "outgoing",
                        PeerAccessInvalidated::Quota,
                    );
                    if store
                        .grant(&key)
                        .await
                        .is_ok_and(|state| matches!(state.grant, PeerGrant::Custom { .. }))
                    {
                        notify_peer(mesh.clone(), key.peer, PeerAccessInvalidated::Quota);
                    }
                }
            }
        });
    }
}

fn peer_catalog_changed(grant: &PeerGrant, previous: &Catalog, current: &Catalog) -> bool {
    previous.iter().chain(current).any(|(provider, _)| {
        let before = previous.get(provider);
        let after = current.get(provider);
        before
            .into_iter()
            .flatten()
            .chain(after.into_iter().flatten())
            .any(|model| {
                grant.allows_model(SharingProtocol::V1, provider, model)
                    && before.is_some_and(|models| models.contains(model))
                        != after.is_some_and(|models| models.contains(model))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_hints_only_disclose_changes_visible_to_the_recipient() {
        use querymt_remote::peer_access::{ModelSelection, ProviderGrant};
        let grant = PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            shell: false,
            providers: vec![ProviderGrant {
                provider_id: "fixture".into(),
                models: ModelSelection::Selected {
                    model_ids: vec!["allowed".into()],
                },
            }],
        };
        let old = Catalog::from([("fixture".into(), ["allowed".into()].into())]);
        let hidden =
            Catalog::from([("fixture".into(), ["allowed".into(), "hidden".into()].into())]);
        assert!(!peer_catalog_changed(&grant, &old, &hidden));
        assert!(!peer_catalog_changed(
            &PeerGrant::None,
            &old,
            &Catalog::new()
        ));
        assert!(peer_catalog_changed(&grant, &old, &Catalog::new()));
        assert!(peer_catalog_changed(&PeerGrant::Full, &old, &hidden));
    }
}
