#![cfg(all(test, feature = "remote"))]

use super::remote_setup::{
    remote_members_from_config, spawn_and_register_local_mesh_actors_with_name,
};
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::remote::test_helpers::fixtures::{NodeManagerFixture, get_test_mesh};
use crate::agent::remote::{MeshRuntimeHandle, MeshScopeId, NodeId};
use crate::config::{MeshPeerConfig, RemoteAgentConfig};
use crate::delegation::roster::MemberAgent;
use crate::test_utils::{SharedLlmProvider, TestProviderFactory, mock_plugin_registry};
use querymt::LLMParams;
use std::sync::Arc;
use tokio::sync::Mutex;

fn random_peer_id() -> libp2p::PeerId {
    libp2p::identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
}

#[tokio::test]
async fn mesh_publication_triggers_local_only_model_refresh() {
    let provider = Arc::new(Mutex::new(crate::test_utils::MockLlmProvider::new()));
    let shared = SharedLlmProvider {
        inner: provider,
        tools: vec![].into_boxed_slice(),
    };
    let factory = Arc::new(TestProviderFactory::new(shared));
    let (plugin_registry, _temp_dir) = mock_plugin_registry(factory).expect("plugin registry");
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("create storage"),
    );
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .build(),
    );
    let handle = LocalAgentHandle::from_config(config);
    let mesh = get_test_mesh().await;
    handle.set_mesh(mesh.clone());

    let refresh = handle.model_inventory.ensure_local_refresh().await;
    assert!(refresh.started_new_refresh());
    assert!(refresh.waits_for_completion());

    let _actors = spawn_and_register_local_mesh_actors_with_name(&handle, mesh, None).await;

    tokio::time::timeout(std::time::Duration::from_secs(5), refresh.wait())
        .await
        .expect("local refresh should complete");

    let (_, meta) = handle.model_inventory.get_snapshot().await;
    assert!(meta.local_updated_at.is_some());
    assert!(meta.remote_updated_at.is_none());
}

#[tokio::test]
async fn configured_peer_address_routes_to_owner_and_creates_delegation_session() {
    let mesh = get_test_mesh().await;
    // The in-process owner is hosted by the test mesh, so its node id is the
    // mesh's own peer id (attaching checks the actor's peer against it).
    let owner_peer_id = *mesh.peer_id();
    let decoy_peer_id = random_peer_id();
    let owner_node_id = NodeId::from_peer_id(owner_peer_id).to_string();
    let decoy_node_id = NodeId::from_peer_id(decoy_peer_id).to_string();
    let peers = vec![
        MeshPeerConfig {
            name: "decoy-peer".into(),
            addr: format!("/ip4/127.0.0.1/tcp/41002/p2p/{decoy_node_id}"),
        },
        MeshPeerConfig {
            name: "owner-peer".into(),
            addr: format!("/ip4/127.0.0.1/tcp/41001/p2p/{owner_node_id}"),
        },
        MeshPeerConfig {
            name: "unresolved-peer".into(),
            addr: "/ip4/127.0.0.1/tcp/41003".into(),
        },
    ];
    let remotes = vec![
        RemoteAgentConfig {
            id: "owner-agent".into(),
            name: "Owner Agent".into(),
            description: "Configured owner".into(),
            peer: "owner-peer".into(),
            capabilities: vec![],
        },
        RemoteAgentConfig {
            id: "decoy-agent".into(),
            name: "Decoy Agent".into(),
            description: "Different configured peer".into(),
            peer: "decoy-peer".into(),
            capabilities: vec![],
        },
        RemoteAgentConfig {
            id: "unresolved-agent".into(),
            name: "Unresolved Agent".into(),
            description: "Address has no /p2p owner".into(),
            peer: "unresolved-peer".into(),
            capabilities: vec![],
        },
    ];

    // Listing contacts no peer: nothing is published yet.
    let registry = remote_members_from_config(&remotes, &peers).expect("list remote members");
    assert_eq!(registry.len(), 3);
    let node_of = |id: &str| match member_agent(&registry, id) {
        MemberAgent::Remote(member) => member.node_id().map(str::to_owned),
        _ => panic!("remote member"),
    };
    assert_eq!(
        node_of("owner-agent"),
        Some(owner_node_id.clone()),
        "the full configured Multiaddr's terminal /p2p id identifies the owner"
    );
    assert_eq!(
        node_of("decoy-agent"),
        Some(decoy_node_id.clone()),
        "the remote agent name must select its own configured peer address"
    );
    assert_eq!(
        node_of("unresolved-agent"),
        None,
        "an address without terminal /p2p must not invent an owner id"
    );

    // Publish after listing: the delegation resolves the node when it runs.
    let owner = NodeManagerFixture::new_with_mesh().await;
    let decoy = NodeManagerFixture::new_with_mesh().await;
    let wrong_scope = NodeManagerFixture::new_with_mesh().await;
    let runtime = MeshRuntimeHandle::from(mesh.clone());
    let scope = MeshScopeId::lan_default();
    let unrelated_scope = MeshScopeId::Iroh {
        mesh_id: format!("unrelated-{}", uuid::Uuid::now_v7()),
    };
    assert!(!runtime.active_scopes().contains(&unrelated_scope));
    runtime
        .register_actor(
            owner.actor_ref.clone(),
            crate::agent::remote::scope::scoped_node_manager_for_peer(&scope, &owner_node_id),
        )
        .await;
    runtime
        .register_actor(
            decoy.actor_ref.clone(),
            crate::agent::remote::scope::scoped_node_manager_for_peer(&scope, &decoy_node_id),
        )
        .await;
    runtime
        .register_actor(
            wrong_scope.actor_ref.clone(),
            crate::agent::remote::scope::scoped_node_manager_for_peer(
                &unrelated_scope,
                &owner_node_id,
            ),
        )
        .await;

    let root = root_handle(mesh).await;
    let (session_id, _session_ref) = member_agent(&registry, "owner-agent")
        .create_delegation_session(&root, None, "parent-session".into())
        .await
        .expect("delegation session should be created on configured owner");

    let sessions_of = |fixture: &NodeManagerFixture| {
        let actor = fixture.actor_ref.clone();
        async move {
            actor
                .ask(crate::agent::remote::ListRemoteSessions {
                    offset: None,
                    limit: None,
                })
                .await
                .expect("list sessions")
        }
    };
    assert!(
        sessions_of(&owner)
            .await
            .sessions
            .iter()
            .any(|session| session.session_id == session_id),
        "the exact configured owner should create the delegation session"
    );
    assert!(
        sessions_of(&decoy).await.sessions.is_empty(),
        "decoy peer was not selected"
    );
    assert!(
        sessions_of(&wrong_scope).await.sessions.is_empty(),
        "owner lookup must remain within the mesh's active scope"
    );
}

#[tokio::test]
async fn peer_node_resolver_precedes_configured_address_fallback() {
    let base_mesh = get_test_mesh().await;
    let resolved_node_id = NodeId::from_peer_id(base_mesh.peer_id().to_owned());
    let resolved_for_callback = resolved_node_id.clone();
    let mesh = base_mesh
        .clone()
        .with_peer_node_resolver(Arc::new(move |_, _| {
            let resolved = resolved_for_callback.clone();
            Box::pin(async move { Some(resolved) })
        }));
    let owner = NodeManagerFixture::new_with_mesh().await;
    let scope = MeshScopeId::lan_default();
    MeshRuntimeHandle::from(mesh.clone())
        .register_actor(
            owner.actor_ref.clone(),
            crate::agent::remote::scope::scoped_node_manager_for_peer(
                &scope,
                &resolved_node_id.to_string(),
            ),
        )
        .await;

    let configured_peer_id = random_peer_id();
    let peers = [MeshPeerConfig {
        name: "owner-peer".into(),
        addr: format!("/p2p/{configured_peer_id}"),
    }];
    let remotes = [RemoteAgentConfig {
        id: "resolved-agent".into(),
        name: "Resolved Agent".into(),
        description: "Resolver takes precedence".into(),
        peer: "owner-peer".into(),
        capabilities: vec![],
    }];

    let registry = remote_members_from_config(&remotes, &peers).expect("list remote member");
    let root = root_handle(&mesh).await;
    let (session_id, _) = member_agent(&registry, "resolved-agent")
        .create_delegation_session(&root, None, "parent-session".into())
        .await
        .expect("the mesh resolver wins over the configured address");
    assert!(
        owner
            .actor_ref
            .ask(crate::agent::remote::ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .unwrap()
            .sessions
            .iter()
            .any(|session| session.session_id == session_id),
        "configured address fallback must not override a successful resolver"
    );
}

/// A root runtime on `mesh` that delegations are opened from.
async fn root_handle(mesh: &crate::agent::remote::MeshHandle) -> Arc<LocalAgentHandle> {
    let (plugins, _dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
    std::mem::forget(_dir);
    let storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap(),
    );
    let config = Arc::new(
        AgentConfigBuilder::new(
            Arc::new(plugins),
            storage,
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .build(),
    );
    let handle = Arc::new(LocalAgentHandle::from_config(config));
    handle.set_mesh(mesh.clone());
    handle
}

fn member_agent(members: &[crate::delegation::roster::Member], id: &str) -> MemberAgent {
    members
        .iter()
        .find(|member| member.id().as_str() == id)
        .and_then(|member| member.agent().cloned())
        .unwrap_or_else(|| panic!("member `{id}` is listed"))
}
