#![cfg(all(test, feature = "remote"))]

use super::remote_setup::{
    register_remote_agents_from_config, spawn_and_register_local_mesh_actors_with_name,
};
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::remote::test_helpers::fixtures::{NodeManagerFixture, get_test_mesh};
use crate::agent::remote::{MeshRuntimeHandle, MeshScopeId, NodeId};
use crate::config::{MeshPeerConfig, RemoteAgentConfig};
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
    let owner_peer_id = random_peer_id();
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

    let registry = register_remote_agents_from_config(mesh, &remotes, &peers)
        .await
        .expect("register configured remote agents");
    let owner_info = registry.get_agent("owner-agent").expect("owner metadata");
    assert_eq!(
        owner_info.meta.as_ref().expect("owner metadata fields")["node_id"],
        serde_json::json!(owner_node_id),
        "the full configured Multiaddr's terminal /p2p id identifies the owner"
    );
    let decoy_info = registry.get_agent("decoy-agent").expect("decoy metadata");
    assert_eq!(
        decoy_info.meta.as_ref().expect("decoy metadata fields")["node_id"],
        serde_json::json!(decoy_node_id),
        "the remote agent name must select its own configured peer address"
    );
    let unresolved_info = registry
        .get_agent("unresolved-agent")
        .expect("unresolved metadata");
    assert!(
        unresolved_info
            .meta
            .as_ref()
            .expect("unresolved metadata fields")["node_id"]
            .is_null(),
        "an address without terminal /p2p must not invent an owner id"
    );

    // Publish after registration to exercise speculative startup: the fallback
    // owner id must survive until its exact peer-scoped node manager appears.
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

    let remote_handle = registry
        .get_handle("owner-agent")
        .expect("registered owner handle");
    let (session_id, _session_ref) = remote_handle
        .create_delegation_session(None, "parent-session".into())
        .await
        .expect("delegation session should be created on configured owner");

    let owner_sessions = owner
        .actor_ref
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: None,
            limit: None,
        })
        .await
        .expect("list owner sessions");
    assert!(
        owner_sessions
            .sessions
            .iter()
            .any(|session| session.session_id == session_id),
        "the exact configured owner should create the delegation session"
    );
    let decoy_sessions = decoy
        .actor_ref
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: None,
            limit: None,
        })
        .await
        .expect("list decoy sessions");
    assert!(
        decoy_sessions.sessions.is_empty(),
        "decoy peer was not selected"
    );
    let wrong_scope_sessions = wrong_scope
        .actor_ref
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: None,
            limit: None,
        })
        .await
        .expect("list unrelated-scope sessions");
    assert!(
        wrong_scope_sessions.sessions.is_empty(),
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

    let registry = register_remote_agents_from_config(&mesh, &remotes, &peers)
        .await
        .expect("register remote agent");
    let info = registry
        .get_agent("resolved-agent")
        .expect("resolved agent metadata");
    assert_eq!(
        info.meta.as_ref().expect("agent metadata fields")["node_id"],
        serde_json::json!(resolved_node_id.to_string()),
        "configured address fallback must not override a successful resolver"
    );
}
