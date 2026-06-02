use super::events::{
    connection_route_plan, peer_id_from_multiaddr, seed_scoped_dial_peer, should_dial_peer_command,
    should_track_iroh_reconnect,
};
use super::{
    DialReason, MeshEvent, MeshHandle, MeshScopeId, MeshTransportKind, MeshTransportMode,
    RouteTable, SwarmCommand,
};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn reconnect_peers_for_mesh_filters_to_specific_local_mesh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mesh_state.json");
    let mut store =
        crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&path).unwrap();

    let host_kp = libp2p::identity::Keypair::generate_ed25519();
    let host_peer_id = host_kp.public().to_peer_id().to_string();

    let mesh_a_id = crate::agent::remote::invite::mesh_id_for(&host_peer_id, Some("mesh-a"));
    store
        .upsert_hosted_mesh(
            mesh_a_id.clone(),
            Some("mesh-a".to_string()),
            Some("invite-a".to_string()),
        )
        .unwrap();
    let peer_a_kp = libp2p::identity::Keypair::generate_ed25519();
    let peer_a_id = peer_a_kp.public().to_peer_id().to_string();
    let token_a = crate::agent::remote::invite::MembershipToken::issue(
        mesh_a_id.clone(),
        &peer_a_id,
        &host_kp,
        "invite-a".to_string(),
        crate::agent::remote::invite::InvitePermissions::default(),
        u64::MAX,
    )
    .unwrap();
    store.add_admitted_peer(&mesh_a_id, token_a).unwrap();

    let mesh_b_id = crate::agent::remote::invite::mesh_id_for(&host_peer_id, Some("mesh-b"));
    store
        .upsert_hosted_mesh(
            mesh_b_id.clone(),
            Some("mesh-b".to_string()),
            Some("invite-b".to_string()),
        )
        .unwrap();
    let peer_b_kp = libp2p::identity::Keypair::generate_ed25519();
    let peer_b_id = peer_b_kp.public().to_peer_id().to_string();
    let token_b = crate::agent::remote::invite::MembershipToken::issue(
        mesh_b_id.clone(),
        &peer_b_id,
        &host_kp,
        "invite-b".to_string(),
        crate::agent::remote::invite::InvitePermissions::default(),
        u64::MAX,
    )
    .unwrap();
    store.add_admitted_peer(&mesh_b_id, token_b).unwrap();

    let local_mesh_id = mesh_a_id;
    let local = store.reconnect_peers_for_mesh(&local_mesh_id);

    assert_eq!(
        local.len(),
        1,
        "should include only local mesh admitted peers"
    );
    assert_eq!(local[0].peer_id, peer_a_id);
}

#[test]
fn peer_id_from_multiaddr_extracts_p2p_component() {
    let kp = libp2p::identity::Keypair::generate_ed25519();
    let peer_id = kp.public().to_peer_id();
    let addr: libp2p::Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{peer_id}")
        .parse()
        .unwrap();
    assert_eq!(peer_id_from_multiaddr(&addr), Some(peer_id));
}

fn test_mesh_with_memberships(
    mesh_ids: &[&str],
) -> (
    tempfile::TempDir,
    MeshHandle,
    tokio::sync::broadcast::Receiver<MeshEvent>,
    tokio::sync::mpsc::UnboundedReceiver<SwarmCommand>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mesh_state_path = dir.path().join("mesh_state.json");
    let mut mesh_state =
        crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&mesh_state_path).unwrap();

    let host_kp = libp2p::identity::Keypair::generate_ed25519();
    let host_peer_id = host_kp.public().to_peer_id().to_string();

    for mesh_id in mesh_ids {
        let token = crate::agent::remote::invite::MembershipToken::issue(
            (*mesh_id).to_string(),
            &host_peer_id,
            &host_kp,
            format!("invite-{mesh_id}"),
            crate::agent::remote::invite::InvitePermissions::default(),
            u64::MAX,
        )
        .unwrap();
        mesh_state.upsert_joined_mesh(token, Vec::new()).unwrap();
    }

    let (peer_events_tx, peer_events_rx) = tokio::sync::broadcast::channel::<MeshEvent>(16);
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let (swarm_cmd_tx, swarm_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    let mesh = MeshHandle::new(
        host_kp.public().to_peer_id(),
        peer_events_tx,
        routes,
        "test-host".to_string(),
        re_register_fns,
        host_kp,
        None,
        Some(Arc::new(RwLock::new(mesh_state))),
        MeshTransportMode::Composite,
        swarm_cmd_tx,
        Duration::from_secs(30),
    );

    (dir, mesh, peer_events_rx, swarm_cmd_rx)
}

#[test]
fn leave_iroh_scope_removes_membership_emits_event_and_notifies_swarm() {
    let mesh_id = "inviter:mesh-a";
    let (_dir, mesh, mut events_rx, mut swarm_cmd_rx) = test_mesh_with_memberships(&[mesh_id]);

    let removed = mesh.leave_iroh_scope(mesh_id).unwrap();
    assert!(removed, "existing scope should be removed");
    assert!(mesh.joined_iroh_scopes().is_empty());
    assert_eq!(mesh.active_scopes(), vec![MeshScopeId::lan_default()]);

    match events_rx.try_recv().unwrap() {
        MeshEvent::ScopeLeft(MeshScopeId::Iroh { mesh_id: left }) => {
            assert_eq!(left, mesh_id);
        }
        other => panic!("expected ScopeLeft event, got {other:?}"),
    }

    match swarm_cmd_rx.try_recv().unwrap() {
        SwarmCommand::LeaveIrohScope { mesh_id: left } => assert_eq!(left, mesh_id),
        other => panic!("expected LeaveIrohScope command, got {other:?}"),
    }
}

#[test]
fn leave_iroh_scope_missing_scope_returns_false_and_emits_nothing() {
    let (_dir, mesh, mut events_rx, mut swarm_cmd_rx) =
        test_mesh_with_memberships(&["inviter:mesh-a"]);

    let removed = mesh.leave_iroh_scope("inviter:mesh-b").unwrap();
    assert!(!removed, "missing scope should report false");
    assert_eq!(
        mesh.joined_iroh_scopes(),
        vec![MeshScopeId::Iroh {
            mesh_id: "inviter:mesh-a".to_string()
        }]
    );

    assert!(matches!(
        events_rx.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert!(swarm_cmd_rx.try_recv().is_err());
}

#[test]
fn create_invite_adds_iroh_scope_to_active_scopes() {
    let (_dir, mut mesh, _events_rx, _swarm_cmd_rx) = test_mesh_with_memberships(&[]);
    mesh.set_config_scopes(vec![MeshScopeId::lan_default()]);

    let invite = mesh
        .create_invite(Some("mesh-a".to_string()), None, Some(1), false)
        .unwrap();

    assert_eq!(
        invite.grant.mesh_name.as_deref(),
        Some("mesh-a"),
        "invite should preserve the requested mesh name"
    );
    assert!(mesh.active_scopes().contains(&MeshScopeId::Iroh {
        mesh_id: crate::agent::remote::invite::mesh_id_for(
            &mesh.peer_id().to_string(),
            Some("mesh-a"),
        )
    }));
}

#[test]
fn active_scopes_does_not_fallback_to_lan_for_iroh_only_mesh() {
    let host_kp = libp2p::identity::Keypair::generate_ed25519();
    let (peer_events_tx, _peer_events_rx) = tokio::sync::broadcast::channel::<MeshEvent>(16);
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let (swarm_cmd_tx, _swarm_cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    let mesh = MeshHandle::new(
        host_kp.public().to_peer_id(),
        peer_events_tx,
        routes,
        "test-host".to_string(),
        re_register_fns,
        host_kp,
        None,
        None,
        MeshTransportMode::Iroh,
        swarm_cmd_tx,
        Duration::from_secs(30),
    );

    assert_eq!(
        mesh.active_scopes(),
        Vec::<MeshScopeId>::new(),
        "iroh-only handles must not silently fall back to lan scope"
    );
}

#[test]
fn connection_route_plan_composite_with_iroh_peer_includes_lan_and_iroh() {
    let scope = MeshScopeId::Iroh {
        mesh_id: "mesh-a".to_string(),
    };
    let plan = connection_route_plan(true, true, Some(&scope));
    assert_eq!(plan.len(), 2);
    assert_eq!(
        plan[0],
        (MeshTransportKind::Lan, MeshScopeId::lan_default(), 100)
    );
    assert_eq!(plan[1], (MeshTransportKind::Iroh, scope, 70));
}

#[test]
fn connection_route_plan_iroh_only_without_scope_adds_no_lan() {
    let plan = connection_route_plan(false, true, None);
    assert!(
        plan.is_empty(),
        "iroh-only without known scope should not synthesize lan route"
    );
}

#[test]
fn should_track_iroh_reconnect_only_for_scoped_iroh_peers() {
    let iroh_peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let lan_only_peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();

    let mut scopes = HashMap::new();
    scopes.insert(
        iroh_peer,
        MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        },
    );

    assert!(should_track_iroh_reconnect(&iroh_peer, &scopes));
    assert!(!should_track_iroh_reconnect(&lan_only_peer, &scopes));
}

#[test]
fn admission_dial_command_is_allowed_before_scope_was_known() {
    let peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let scopes = HashMap::new();

    assert!(should_dial_peer_command(
        &peer,
        DialReason::Admission,
        &scopes,
        true
    ));
    assert!(!should_dial_peer_command(
        &peer,
        DialReason::Manual,
        &scopes,
        true
    ));
}

#[test]
fn reconnect_and_manual_dials_require_known_iroh_scope() {
    let peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let mut scopes = HashMap::new();

    assert!(!should_dial_peer_command(
        &peer,
        DialReason::Reconnect,
        &scopes,
        true
    ));
    assert!(!should_dial_peer_command(
        &peer,
        DialReason::Manual,
        &scopes,
        true
    ));

    scopes.insert(
        peer,
        MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        },
    );

    assert!(should_dial_peer_command(
        &peer,
        DialReason::Reconnect,
        &scopes,
        true
    ));
    assert!(should_dial_peer_command(
        &peer,
        DialReason::Manual,
        &scopes,
        true
    ));
}

#[test]
fn scoped_dial_seeds_iroh_scope_tracking() {
    let peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    let scope = MeshScopeId::Iroh {
        mesh_id: "mesh-a".to_string(),
    };
    let mut by_scope = HashMap::new();
    let mut peer_scopes = HashMap::new();

    seed_scoped_dial_peer(peer, Some(scope.clone()), &mut by_scope, &mut peer_scopes);

    assert_eq!(peer_scopes.get(&peer), Some(&scope));
    assert!(
        by_scope
            .get("mesh-a")
            .is_some_and(|peers| peers.contains(&peer))
    );
}

#[test]
fn route_table_prefers_lan_over_iroh_for_same_peer() {
    use libp2p::Multiaddr;

    let routes = RouteTable::new(Duration::from_secs(90));
    let peer = libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();

    let iroh_scope = MeshScopeId::Iroh {
        mesh_id: "mesh-a".to_string(),
    };

    let iroh_addr: Multiaddr = format!("/p2p/{peer}").parse().unwrap();
    let lan_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/12345/p2p/{peer}")
        .parse()
        .unwrap();

    routes.upsert_addrs(
        peer,
        MeshTransportKind::Iroh,
        iroh_scope.clone(),
        [iroh_addr],
        70,
    );
    routes.upsert_addrs(
        peer,
        MeshTransportKind::Lan,
        MeshScopeId::lan_default(),
        [lan_addr],
        100,
    );

    let best = routes
        .best_route_for_peer(&peer)
        .expect("best route exists");
    assert_eq!(best.transport, MeshTransportKind::Lan);
    assert_eq!(best.scope, MeshScopeId::lan_default());
    assert_eq!(best.priority, 100);
}

#[test]
fn scope_joined_and_left_events_roundtrip_through_broadcast_channel() {
    let (_dir, mesh, mut events_rx, _swarm_cmd_rx) =
        test_mesh_with_memberships(&["inviter:mesh-a"]);
    let joined = MeshScopeId::Iroh {
        mesh_id: "inviter:mesh-b".to_string(),
    };
    let left = MeshScopeId::Iroh {
        mesh_id: "inviter:mesh-a".to_string(),
    };

    let _ = mesh
        .peer_events_tx
        .send(MeshEvent::ScopeJoined(joined.clone()));
    let _ = mesh.peer_events_tx.send(MeshEvent::ScopeLeft(left.clone()));

    assert!(
        matches!(events_rx.try_recv().unwrap(), MeshEvent::ScopeJoined(scope) if scope == joined)
    );
    assert!(matches!(events_rx.try_recv().unwrap(), MeshEvent::ScopeLeft(scope) if scope == left));
}

/// `resolve_peer_node_id` with no known peers returns `None`.
///
/// This is a pure unit test: no DHT or network required. The `known_peers`
/// set is empty so the iteration body never executes and the method must
/// return `None` without panicking.
#[cfg(feature = "remote")]
#[tokio::test]
async fn resolve_peer_node_id_no_known_peers_returns_none() {
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

    let mesh = get_test_mesh().await.clone();
    // known_peers is empty right after bootstrap (no peers discovered yet)
    let result = mesh.resolve_peer_node_id("gpu-node").await;
    assert!(
        result.is_none(),
        "expected None when no peers are known, got {:?}",
        result
    );
}

/// `resolve_peer_node_id` returns `None` for an unknown peer name even
/// when the mesh has known peers. This test uses the test mesh (single node)
/// which has no remote peers with any hostname.
#[cfg(feature = "remote")]
#[tokio::test]
async fn resolve_peer_node_id_unknown_name_returns_none() {
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

    let mesh = get_test_mesh().await.clone();
    let result = mesh.resolve_peer_node_id("nonexistent-peer-xyz").await;
    assert!(result.is_none());
}

/// `re_register_fns_count` returns a valid count (test mesh is shared,
/// so the count may be non-zero if other tests have registered actors).
#[cfg(feature = "remote")]
#[tokio::test]
async fn re_register_fns_count_is_accessible() {
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

    let mesh = get_test_mesh().await.clone();
    // Just verify the method works and returns a sane value.
    let count = mesh.re_register_fns_count();
    assert!(
        count < 10_000,
        "re_register_fns_count should be finite, got {}",
        count
    );
}

/// `deregister_actor` removes the re-registration closure for a given name.
///
/// The test mesh is shared across all tests so we cannot rely on absolute
/// counts. Instead we verify the specific key is present after register
/// and absent after deregister.
#[cfg(feature = "remote")]
#[tokio::test]
async fn deregister_actor_removes_re_register_fn() {
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

    let mesh = get_test_mesh().await.clone();

    // Register a dummy actor under a unique name
    let test_name = format!("test_deregister_{}", uuid::Uuid::now_v7());
    use crate::agent::remote::provider_host::StreamReceiverActor;
    use kameo::actor::Spawn;
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let actor = StreamReceiverActor::new(tx);
    let actor_ref = StreamReceiverActor::spawn(actor);
    mesh.register_actor(actor_ref, test_name.clone()).await;

    // Verify the key is present
    assert!(
        mesh.has_re_register_fn(&test_name),
        "register_actor should insert a re-register fn under the given name"
    );

    // Deregister
    mesh.deregister_actor(&test_name);

    assert!(
        !mesh.has_re_register_fn(&test_name),
        "deregister_actor should remove the re-register fn"
    );
}

/// `deregister_actor` is a no-op for unknown names (no panic).
#[cfg(feature = "remote")]
#[tokio::test]
async fn deregister_actor_unknown_name_is_noop() {
    use crate::agent::remote::test_helpers::fixtures::get_test_mesh;

    let mesh = get_test_mesh().await.clone();
    let before = mesh.re_register_fns_count();
    mesh.deregister_actor("nonexistent_actor_name");
    assert_eq!(mesh.re_register_fns_count(), before);
}
