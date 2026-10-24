use super::*;

#[cfg(feature = "remote")]
use crate::agent::remote::RemoteNodeManager;
#[cfg(feature = "remote")]
use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
#[cfg(feature = "remote")]
use kameo::actor::Spawn;

#[cfg(feature = "remote")]
async fn owner_profiles_fixture(active: &str) -> (RealStorageHandleFixture, tempfile::TempDir) {
    use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};
    let remote = RealStorageHandleFixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    write_profile(
        dir.path(),
        "alpha.toml",
        "[agent]\nprovider = 'mock'\nmodel = 'mock-model'\nsystem = 'owner default'\n",
    );
    write_profile(
        dir.path(),
        "cdr.toml",
        &QUORUM_PROFILE_TOML
            .replace("\"test\"", "\"mock\"")
            .replace("\"test-model\"", "\"mock-model\""),
    );
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(dir.path())
            .build(),
    );
    remote
        .handle
        .set_profiles(Arc::new(ProfileRuntimeManager::with_infra_boxed(
            catalog,
            active,
            AgentInfra {
                plugin_registry: remote.handle.config.provider.plugin_registry(),
                storage: Some(remote.storage.clone()),
                session_mcp_attachment_source: None,
                event_fanout: None,
            },
        )));
    (remote, dir)
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_create_keeps_explicit_owner_profile_and_rejects_invalid_selection() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let viewer = RealStorageHandleFixture::new().await;
    viewer.handle.set_mesh(mesh.clone());
    let (owner, _dir) = owner_profiles_fixture("alpha").await;
    let (node_id, _) = register_remote_node(mesh, &owner, "profile-probe", true).await;
    let created = ext_method_json(
        &viewer.handle,
        "querymt/remote/createSession",
        serde_json::json!({
            "node_id": node_id, "profile_id": "cdr", "attach": false,
        }),
    )
    .await;
    let binding = owner
        .storage
        .session_store()
        .get_session_runtime_binding(created["session_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        binding.profile_id, "cdr",
        "explicit selection must reach B, not select its active alpha profile"
    );
    assert_eq!(
        owner.handle.profiles().unwrap().active_profile_id().await,
        "alpha"
    );
    for key in ["node_id", "nodeId"] {
        let listed = ext_method_json(
            &viewer.handle,
            "querymt/profiles",
            serde_json::json!({ (key): node_id }),
        )
        .await;
        assert_eq!(
            listed["node_id"], node_id,
            "owner catalog requested via {key}"
        );
        assert!(
            listed["profiles"]
                .as_array()
                .unwrap()
                .iter()
                .any(|profile| profile["id"] == "cdr")
        );
    }
    assert!(
        viewer.handle.profiles().is_none(),
        "catalog is not borrowed from A"
    );
    let id = created["session_id"].as_str().unwrap();
    let loaded = owner
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            id.to_owned(),
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();
    assert_eq!(
        config_option_value(&loaded.config_options.unwrap(), "profile").as_deref(),
        Some("cdr")
    );
    let attached = ext_method_json(
        &viewer.handle,
        "querymt/remote/attachSession",
        serde_json::json!({"node_id": node_id, "session_id": id}),
    )
    .await;
    assert_eq!(attached["profile_id"], "cdr");
    for bad in ["missing", " "] {
        assert!(viewer.handle.ext_method(crate::acp::protocol::ExtRequest::new("querymt/remote/createSession",
            raw_params(&serde_json::json!({"node_id": node_id, "profile_id": bad, "attach": false}).to_string()))).await.is_err());
    }
    assert_eq!(
        owner
            .storage
            .session_store()
            .list_sessions()
            .await
            .unwrap()
            .len(),
        1,
        "invalid selection must not create a default session"
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_profile_creation_rejects_failed_binding_without_a_session_or_memory_binding() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let viewer = RealStorageHandleFixture::new().await;
    viewer.handle.set_mesh(mesh.clone());
    let (owner, _dir) = owner_profiles_fixture("alpha").await;
    let profiles = owner.handle.profiles().unwrap();
    let runtime = profiles.runtime_for_profile("cdr").await.unwrap();
    let (node_id, _) = register_remote_node(mesh, &owner, "binding-failure", true).await;
    owner
        .storage
        .conn_for_test()
        .lock()
        .unwrap()
        .execute_batch(
            "CREATE TABLE rejected_binding_ids (id TEXT NOT NULL);
         CREATE TRIGGER reject_profile_binding BEFORE INSERT ON profile_bindings
         BEGIN SELECT RAISE(ABORT, 'binding persistence rejected'); END;
         CREATE TRIGGER record_created_session AFTER INSERT ON sessions
         BEGIN INSERT INTO rejected_binding_ids VALUES (NEW.public_id); END;",
        )
        .unwrap();
    let error = viewer
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/remote/createSession",
            raw_params(
                &serde_json::json!({"node_id": node_id, "profile_id": "cdr", "attach": false})
                    .to_string(),
            ),
        ))
        .await
        .expect_err("remote create must fail when binding cannot be persisted");
    assert!(format!("{error:?}").contains("binding persistence rejected"));
    let id: String = owner
        .storage
        .conn_for_test()
        .lock()
        .unwrap()
        .query_row("SELECT id FROM rejected_binding_ids", [], |row| row.get(0))
        .unwrap();
    assert!(
        owner
            .storage
            .session_store()
            .list_sessions()
            .await
            .unwrap()
            .is_empty(),
        "failed creation must not leave an unbound session"
    );
    assert!(
        profiles.session_binding(&id).await.is_none(),
        "failed persistence must not install an in-memory binding"
    );
    assert!(
        runtime.agent().handle().registry_lock().await.is_empty(),
        "no session actor materialized"
    );
    let local_error = owner
        .handle
        .new_session(NewSessionRequest::new(std::path::PathBuf::new()))
        .await
        .expect_err("local profile create must fail too");
    assert!(format!("{local_error:?}").contains("binding persistence rejected"));
    assert!(
        owner
            .storage
            .session_store()
            .list_sessions()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        profiles
            .runtime_for_profile("alpha")
            .await
            .unwrap()
            .agent()
            .handle()
            .registry_lock()
            .await
            .is_empty()
    );
    owner
        .storage
        .conn_for_test()
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_profile_binding")
        .unwrap();
    let created = ext_method_json(
        &viewer.handle,
        "querymt/remote/createSession",
        serde_json::json!({"node_id": node_id, "profile_id": "cdr", "attach": false}),
    )
    .await;
    assert_eq!(
        owner
            .storage
            .session_store()
            .get_session_runtime_binding(created["session_id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .profile_id,
        "cdr"
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_delegate_child_reuses_session_listing_attach_and_bookmarked_load() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let viewer = RealStorageHandleFixture::new().await;
    viewer.handle.set_mesh(mesh.clone());
    let (owner, _dir) = owner_profiles_fixture("cdr").await;
    let (node_id, _) = register_remote_node(mesh, &owner, "delegate-child", true).await;
    let parent = owner
        .handle
        .new_session(NewSessionRequest::new(std::path::PathBuf::new()))
        .await
        .unwrap();
    let profiles = owner.handle.profiles().unwrap();
    let runtime = profiles.runtime_for_profile("cdr").await.unwrap();
    let delegate = runtime.agent().delegate("coder").unwrap();
    let (child_id, child_ref) = delegate
        .create_delegation_session(None, parent.session_id.to_string())
        .await
        .unwrap();
    let mut binding = runtime.session_binding();
    binding.agent_id = Some("coder".into());
    profiles
        .set_session_binding(&child_id, binding)
        .await
        .unwrap();
    profiles.set_active_profile("alpha").await.unwrap();
    child_ref.set_mode(AgentMode::Review).await.unwrap();
    delegate
        .as_any()
        .downcast_ref::<LocalAgentHandle>()
        .unwrap()
        .config
        .event_sink
        .emit_durable(
            &child_id,
            AgentEventKind::PromptReceived {
                content: "Delegate conversation".into(),
                message_id: None,
            },
        )
        .await
        .unwrap();

    for scope in ["root", "all"] {
        let listed = viewer
            .handle
            .list_sessions(
                serde_json::from_value(serde_json::json!({
                    "_meta": {"session_scope": scope, "remoteNodeIds": [node_id]}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let child = listed
            .sessions
            .iter()
            .find(|session| session.session_id.to_string() == child_id);
        if scope == "root" {
            assert!(child.is_none());
        } else {
            let meta = child.unwrap().meta.as_ref().unwrap();
            assert_eq!(meta["nodeId"], node_id);
            assert_eq!(meta["parentSessionId"], parent.session_id.to_string());
            assert_eq!(meta["forkOrigin"], "delegation");
        }
    }
    let attached = ext_method_json(
        &viewer.handle,
        "querymt/remote/attachSession",
        serde_json::json!({"node_id": node_id, "session_id": child_id}),
    )
    .await;
    assert_eq!(attached["profile_id"], "cdr");
    assert_eq!(attached["config_options"][0]["currentValue"], "review");
    assert!(
        attached["snapshot"]["audit"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| {
                event["kind"]["type"] == "prompt_received"
                    && event["kind"]["data"]["content"] == "Delegate conversation"
            })
    );
    let loaded = viewer
        .handle
        .load_session(LoadSessionRequest::new(
            child_id.clone(),
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();
    assert_eq!(loaded.meta.as_ref().unwrap()["nodeId"], node_id);
    let bookmark = viewer
        .storage
        .session_store()
        .get_remote_session_bookmark(&child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bookmark.node_id, node_id);
    assert!(
        viewer
            .storage
            .session_store()
            .get_session(&child_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_delegate_controls_reach_owner_and_preserve_revision_errors() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let viewer = RealStorageHandleFixture::new().await;
    viewer.handle.set_mesh(mesh.clone());
    assert!(
        crate::control::capabilities::get_capabilities(&viewer.handle)
            .methods
            .iter()
            .any(|method| method == "querymt/session/delegateModels")
    );
    let (owner, _dir) = owner_profiles_fixture("cdr").await;
    owner.handle.set_mesh(mesh.clone());
    let (node_id, _) = register_remote_node(mesh, &owner, "delegate-probe", true).await;
    let created = ext_method_json(
        &viewer.handle,
        "querymt/remote/createSession",
        serde_json::json!({
            "node_id": node_id, "attach": true,
        }),
    )
    .await;
    let session_id = created["session_id"].as_str().unwrap();
    let params = serde_json::json!({"session_id": session_id});
    let direct = ext_method_json(
        &owner.handle,
        "querymt/session/delegateModels",
        params.clone(),
    )
    .await;
    assert_eq!(
        direct["assignments"].as_array().unwrap().len(),
        2,
        "owner has coder and reviewer"
    );
    let result = viewer
        .handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            "querymt/session/delegateModels",
            raw_params(&params.to_string()),
        ))
        .await;
    assert!(
        result.is_ok(),
        "A must read B's delegate roles, got {result:?}"
    );
    let proxied: serde_json::Value = serde_json::from_str(result.unwrap().0.get()).unwrap();
    assert_eq!(proxied, direct);
    let mut events = viewer.handle.subscribe_events();
    let request = serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": "mock/mock-model", "node_id": node_id, "reasoning_effort": "high", "expected_revision": direct["revision"]});
    let set = ext_method_json(
        &viewer.handle,
        "querymt/session/setDelegateModel",
        request.clone(),
    )
    .await;
    assert_eq!(set["model"]["node_id"], node_id);
    assert_eq!(set["reasoning_effort"], "high");
    let notified = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind(), AgentEventKind::DelegateModelsChanged { .. }) {
                break event;
            }
        }
    })
    .await
    .expect("owner change must reach A through the existing relay");
    assert_eq!(notified.session_id(), session_id);
    let read = ext_method_json(
        &owner.handle,
        "querymt/session/delegateModels",
        params.clone(),
    )
    .await;
    let coder = read["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|role| role["agent_id"] == "coder")
        .unwrap();
    assert!(
        coder["model"]["node_id"].is_null(),
        "B-local model persists owner-local, not A-local"
    );
    async fn get_error(
        handle: &LocalAgentHandle,
        request: serde_json::Value,
    ) -> agent_client_protocol::Error {
        handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/session/setDelegateModel",
                raw_params(&request.to_string()),
            ))
            .await
            .unwrap_err()
    }
    let owner_error = get_error(&owner.handle, request.clone()).await;
    let proxy_error = get_error(&viewer.handle, request).await;
    assert_eq!(
        serde_json::to_value(owner_error).unwrap(),
        serde_json::to_value(proxy_error).unwrap(),
        "revision conflicts must survive proxying unchanged"
    );
    let clear = ext_method_json(&viewer.handle, "querymt/session/setDelegateModel", serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": null, "expected_revision": set["revision"]})).await;
    assert!(clear["model"].is_null());
    assert_eq!(
        clear["reasoning_effort"], "high",
        "omitted reasoning survives mesh serialization"
    );
    let reset = ext_method_json(&viewer.handle, "querymt/session/setDelegateModel", serde_json::json!({"session_id": session_id, "agent_id": "coder", "model_id": null, "reasoning_effort": null, "expected_revision": clear["revision"]})).await;
    assert!(reset["reasoning_effort"].is_null());
    assert!(
        viewer
            .storage
            .session_store()
            .get_session(session_id)
            .await
            .unwrap()
            .is_none(),
        "A cannot persist shadow routing"
    );
    viewer.handle.clear_mesh();
    assert!(
        viewer
            .handle
            .ext_method(crate::acp::protocol::ExtRequest::new(
                "querymt/session/delegateModels",
                raw_params(&params.to_string())
            ))
            .await
            .is_err(),
        "offline owner cannot fall back to A"
    );
}

#[cfg(feature = "remote")]
async fn register_remote_node(
    mesh: &crate::agent::remote::mesh::MeshHandle,
    remote: &RealStorageHandleFixture,
    node_name: &str,
    use_mesh_peer: bool,
) -> (String, kameo::actor::ActorRef<RemoteNodeManager>) {
    let peer_id = if use_mesh_peer {
        *mesh.peer_id()
    } else {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    };
    let node_id = peer_id.to_string();
    let node_manager = RemoteNodeManager::new(
        remote.handle.config.clone(),
        remote.handle.registry.clone(),
        Some(mesh.clone()),
        remote.handle.scheduler_handle.clone(),
    )
    .with_profiles_slot(remote.handle.profiles.clone())
    .with_node_name(node_name.to_string());
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let per_peer_name = scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &peer_id);
    mesh.register_actor(node_manager_ref.clone(), per_peer_name)
        .await;
    mesh.inject_known_peer_for_test(peer_id);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (node_id, node_manager_ref)
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_sessions_returns_shared_shape() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let (node_id, node_manager_ref) =
        register_remote_node(mesh, &remote, "peer-remote", false).await;

    node_manager_ref
        .ask(crate::agent::remote::CreateRemoteSession {
            cwd: Some("/tmp/remote-a".to_string()),
        })
        .await
        .expect("create remote session");

    let listed = ext_method_json(
        &f.handle,
        "querymt/remote/sessions",
        serde_json::json!({ "node_id": node_id, "offset": 0, "limit": 20 }),
    )
    .await;

    assert_eq!(listed["node_id"], node_id);
    assert!(listed["sessions"].is_array());
    assert_eq!(listed["total_count"], 1);
    let first = &listed["sessions"][0];
    assert_eq!(first["node_id"], listed["node_id"]);
    assert!(first["id"].is_string());
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn remote_owner_lifecycle_updates_and_filters_before_paging() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let remote = RealStorageHandleFixture::new().await;
    let (_node_id, manager) = register_remote_node(mesh, &remote, "lifecycle-owner", false).await;
    let store = remote.handle.config.provider.history_store();
    for index in 0..25 {
        store
            .create_session(
                Some(format!("Lifecycle {index}")),
                Some(std::path::PathBuf::from("/remote/work")),
                None,
                None,
            )
            .await
            .expect("persist owner session");
    }
    let unfiltered = manager
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: Some(0),
            limit: Some(100),
        })
        .await
        .expect("list owner sessions");
    for session in unfiltered.sessions.iter().take(10) {
        store
            .patch_session_lifecycle(
                &session.session_id,
                &crate::session::lifecycle::SessionLifecyclePatch {
                    resolved: Some(true),
                    archived: None,
                },
            )
            .await
            .expect("resolve first unfiltered page");
    }
    let remote_handle = crate::agent::remote::RemoteAgentHandle::new(
        "lifecycle-owner".to_string(),
        Some(_node_id.clone()),
        mesh.clone(),
    );
    let state = crate::agent::handle::AgentHandle::set_session_lifecycle(
        &remote_handle,
        &unfiltered.sessions[10].session_id,
        crate::session::lifecycle::SessionLifecyclePatch {
            resolved: None,
            archived: Some(true),
        },
    )
    .await
    .expect("patch lifecycle via remote agent handle");
    assert!(state.archived);
    let read = manager
        .ask(crate::agent::remote::GetRemoteSessionLifecycle {
            session_id: state.session_id.clone(),
        })
        .await
        .expect("read owner lifecycle");
    assert!(read.archived);

    let request = |offset| crate::agent::remote::ListRemoteSessionsWithLifecycle {
        offset: Some(offset),
        limit: Some(10),
        cwd: Some("/remote/work".to_string()),
        scope: crate::session::projection::SessionScope::All,
        lifecycle: crate::session::lifecycle::SessionLifecycleFilter {
            resolution: crate::session::lifecycle::ResolutionFilter::Open,
            archive: crate::session::lifecycle::ArchiveFilter::Excluded,
        },
    };
    let first_page = manager
        .ask(request(0))
        .await
        .expect("owner filters lifecycle before paging");
    assert_eq!(first_page.total_count, 14);
    assert_eq!(first_page.sessions.len(), 10);
    assert_eq!(first_page.next_offset, Some(10));
    assert!(
        first_page
            .sessions
            .iter()
            .all(|session| !session.lifecycle.resolved && !session.lifecycle.archived)
    );

    let second_page = manager.ask(request(10)).await.expect("second owner page");
    assert_eq!(second_page.total_count, 14);
    assert_eq!(second_page.sessions.len(), 4);
    assert_eq!(second_page.next_offset, None);
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn lifecycle_query_remote_owner_scopes_relationships_and_live_metadata() {
    use crate::agent::remote::{
        GetRemoteSessionLifecycle, ListRemoteSessionsWithLifecycle, SetRemoteSessionLifecycle,
    };
    use crate::session::lifecycle::{
        ArchiveFilter, ResolutionFilter, SessionLifecycleFilter, SessionLifecyclePatch,
    };
    use crate::session::projection::SessionScope;

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let local = RealStorageHandleFixture::new().await;
    local.handle.set_mesh(mesh.clone());
    let owner = RealStorageHandleFixture::new().await;
    let (node_id, manager) = register_remote_node(mesh, &owner, "scoped-owner", false).await;
    let root = manager
        .ask(crate::agent::remote::CreateRemoteSession {
            cwd: Some("/remote/work".into()),
        })
        .await
        .expect("create live owner session");
    let store = owner.handle.config.provider.history_store();
    let fork = store
        .create_session(
            Some("Archived fork".into()),
            Some("/remote/work".into()),
            Some(root.session_id.clone()),
            Some(crate::session::domain::ForkOrigin::User),
        )
        .await
        .expect("create persisted fork");
    let delegate = store
        .create_session(
            Some("Archived delegate".into()),
            Some("/remote/elsewhere".into()),
            Some(root.session_id.clone()),
            Some(crate::session::domain::ForkOrigin::Delegation),
        )
        .await
        .expect("create persisted delegate");
    for id in [&fork.public_id, &delegate.public_id] {
        manager
            .ask(SetRemoteSessionLifecycle {
                session_id: id.clone(),
                patch: SessionLifecyclePatch {
                    resolved: Some(true),
                    archived: Some(true),
                },
            })
            .await
            .expect("resolve and archive child at owner");
    }
    // Each lifecycle dimension must exclude a same-workspace user fork independently.
    for (resolved, archived) in [(false, true), (true, false)] {
        let excluded = store
            .create_session(
                None,
                Some("/remote/work".into()),
                Some(root.session_id.clone()),
                Some(crate::session::domain::ForkOrigin::User),
            )
            .await
            .expect("create nonmatching fork");
        store
            .patch_session_lifecycle(
                &excluded.public_id,
                &SessionLifecyclePatch {
                    resolved: Some(resolved),
                    archived: Some(archived),
                },
            )
            .await
            .expect("set nonmatching fork lifecycle");
    }
    let request = ListRemoteSessionsWithLifecycle {
        offset: None,
        limit: Some(0),
        cwd: Some("/remote/work/".into()),
        scope: SessionScope::Forks,
        lifecycle: SessionLifecycleFilter {
            resolution: ResolutionFilter::Resolved,
            archive: ArchiveFilter::Only,
        },
    };
    let forks = manager
        .ask(request.clone())
        .await
        .expect("filtered fork page");
    assert_eq!(forks.total_count, 1);
    assert_eq!(forks.sessions.len(), 1, "zero limit is clamped to one");
    assert_eq!(forks.next_offset, None);
    let row = &forks.sessions[0];
    assert_eq!(row.session.session_id, fork.public_id);
    assert_eq!(
        row.session.parent_session_id.as_deref(),
        Some(root.session_id.as_str())
    );
    assert_eq!(row.session.fork_origin.as_deref(), Some("user"));
    assert_eq!(row.session.actor_id, 0);
    assert_eq!(row.session.runtime_state.as_deref(), Some("persisted"));
    assert_eq!(row.session.title.as_deref(), Some("Archived fork"));
    assert!(row.lifecycle.resolved && row.lifecycle.archived);
    assert!(row.lifecycle.resolved_at.is_some() && row.lifecycle.archived_at.is_some());
    let delegates = manager
        .ask(ListRemoteSessionsWithLifecycle {
            scope: SessionScope::Delegates,
            cwd: None,
            limit: None,
            ..request.clone()
        })
        .await
        .expect("delegate scope crosses workspaces only when requested");
    assert_eq!(delegates.total_count, 1);
    assert_eq!(delegates.sessions[0].session.session_id, delegate.public_id);
    let children = manager
        .ask(ListRemoteSessionsWithLifecycle {
            scope: SessionScope::Children,
            cwd: None,
            limit: None,
            lifecycle: SessionLifecycleFilter::default(),
            ..request.clone()
        })
        .await
        .expect("children include forks and delegates");
    assert_eq!(children.total_count, 4);
    assert_eq!(children.sessions.len(), 4);
    assert!(
        children
            .sessions
            .iter()
            .all(|row| row.session.parent_session_id.as_deref() == Some(root.session_id.as_str()))
    );
    let roots = manager
        .ask(ListRemoteSessionsWithLifecycle {
            scope: SessionScope::Root,
            cwd: None,
            limit: None,
            lifecycle: SessionLifecycleFilter::default(),
            ..request.clone()
        })
        .await
        .expect("root scope includes live owner metadata");
    assert_eq!(roots.total_count, 1);
    assert_eq!(roots.sessions[0].session.session_id, root.session_id);
    assert_ne!(roots.sessions[0].session.actor_id, 0);
    assert_eq!(
        roots.sessions[0].session.runtime_state.as_deref(),
        Some("idle")
    );
    assert_eq!(roots.sessions[0].session.parent_session_id, None);
    assert!(!roots.sessions[0].lifecycle.resolved);
    let exhausted = manager
        .ask(ListRemoteSessionsWithLifecycle {
            offset: Some(1),
            ..request
        })
        .await
        .expect("offset after last matching fork");
    assert_eq!(exhausted.total_count, 1);
    assert!(exhausted.sessions.is_empty());
    assert_eq!(exhausted.next_offset, None);

    let local_store = local.handle.config.provider.history_store();
    local_store
        .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
            session_id: fork.public_id.clone(),
            node_id: node_id.clone(),
            peer_label: "scoped-owner".into(),
            cwd: Some("/remote/work".into()),
            created_at: 0,
            title: None,
        })
        .await
        .expect("bookmark owner fork");
    let sessions = crate::api::AgentSessions::new(
        Arc::new(local.handle),
        local.storage.view_store().expect("view store"),
        local_store,
        None,
    );
    let page = sessions
        .list_with_lifecycle(
            crate::api::ListSessionsOptions {
                remote: crate::api::RemoteSessionMode::Bookmarks,
                ..Default::default()
            },
            SessionLifecycleFilter::default(),
        )
        .await
        .expect("enrich bookmark from authoritative online owner");
    assert_eq!(
        page.groups[0].sessions[0].summary.node_id.as_deref(),
        Some(node_id.as_str())
    );
    assert_eq!(
        page.groups[0].sessions[0].lifecycle.as_ref(),
        Some(&row.lifecycle)
    );
    let error = manager
        .ask(GetRemoteSessionLifecycle {
            session_id: "missing-owner-session".into(),
        })
        .await
        .expect_err("unknown owner session must not become known-open");
    assert!(error.to_string().contains("not found"));
    let error = manager
        .ask(SetRemoteSessionLifecycle {
            session_id: "missing-owner-session".into(),
            patch: SessionLifecyclePatch {
                resolved: Some(true),
                archived: None,
            },
        })
        .await
        .expect_err("unknown owner session must not be created by a patch");
    assert!(error.to_string().contains("not found"));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn lifecycle_location_conflicts_reject_get_patch_and_acp_hydration() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let local = RealStorageHandleFixture::new().await;
    local.handle.set_mesh(mesh.clone());
    let owner = RealStorageHandleFixture::new().await;
    let (node_id, _manager) = register_remote_node(mesh, &owner, "lifecycle-owner", false).await;
    let owner_store = owner.handle.config.provider.history_store();

    let owner_session = owner_store
        .create_session(None, None, None, None)
        .await
        .expect("create remote owner session");
    owner_store
        .patch_session_lifecycle(
            &owner_session.public_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(true),
                archived: None,
            },
        )
        .await
        .expect("resolve remote owner session");

    let local_store = local.handle.config.provider.history_store();
    let collision = local_store
        .create_session(None, None, None, None)
        .await
        .expect("create colliding local session");
    local
        .storage
        .conn_for_test()
        .lock()
        .unwrap()
        .execute(
            "UPDATE sessions SET public_id = ?1 WHERE public_id = ?2",
            rusqlite::params![owner_session.public_id, collision.public_id],
        )
        .expect("assign colliding public id");
    local_store
        .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
            session_id: owner_session.public_id.clone(),
            node_id: node_id.clone(),
            peer_label: "lifecycle-owner".into(),
            cwd: None,
            created_at: 0,
            title: None,
        })
        .await
        .expect("bookmark colliding owner session");

    let get_error = local
        .handle
        .get_session_lifecycle_unloaded(&owner_session.public_id)
        .await
        .expect_err("conflicting location must not route to remote owner");
    assert!(get_error.to_string().contains("Session location conflict"));
    let patch_error = local
        .handle
        .patch_session_lifecycle_unloaded(
            &owner_session.public_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(false),
                archived: Some(true),
            },
        )
        .await
        .expect_err("conflicting location must not patch either owner");
    assert!(
        patch_error
            .to_string()
            .contains("Session location conflict")
    );

    let list_error = crate::api::AgentSessions::list_for_acp_with_runtime(
        &local.handle,
        local.storage.view_store().expect("view store"),
        crate::acp::protocol::ListSessionsRequest::new(),
    )
    .await
    .expect_err("local ACP hydration must reject an ambiguous owner");
    assert!(list_error.to_string().contains("Session location conflict"));

    let local_state = local_store
        .get_session_lifecycle(&owner_session.public_id)
        .await
        .expect("read local collision state");
    assert!(!local_state.resolved);
    assert!(!local_state.archived);
    let owner_state = owner_store
        .get_session_lifecycle(&owner_session.public_id)
        .await
        .expect("read remote owner state");
    assert!(owner_state.resolved);
    assert!(!owner_state.archived);

    let bookmark_only = owner_store
        .create_session(None, None, None, None)
        .await
        .expect("create bookmark-only owner session");
    owner_store
        .patch_session_lifecycle(
            &bookmark_only.public_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: Some(true),
                archived: None,
            },
        )
        .await
        .expect("resolve bookmark-only owner session");
    local_store
        .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
            session_id: bookmark_only.public_id.clone(),
            node_id,
            peer_label: "lifecycle-owner".into(),
            cwd: None,
            created_at: 0,
            title: None,
        })
        .await
        .expect("bookmark remote-only owner session");
    let owner_read = local
        .handle
        .get_session_lifecycle_unloaded(&bookmark_only.public_id)
        .await
        .expect("bookmark-only ID resolves to its remote owner");
    assert!(owner_read.resolved);
    local
        .handle
        .patch_session_lifecycle_unloaded(
            &bookmark_only.public_id,
            &crate::session::lifecycle::SessionLifecyclePatch {
                resolved: None,
                archived: Some(true),
            },
        )
        .await
        .expect("bookmark-only patch routes to the owner");
    let owner_after_patch = owner_store
        .get_session_lifecycle(&bookmark_only.public_id)
        .await
        .expect("read patched owner state");
    assert!(owner_after_patch.resolved);
    assert!(owner_after_patch.archived);

    let lifecycle_list = crate::api::AgentSessions::new(
        Arc::new(local.handle),
        local.storage.view_store().expect("view store"),
        local_store,
        None,
    );
    let list_error = lifecycle_list
        .list_with_lifecycle(
            crate::api::ListSessionsOptions {
                remote: crate::api::RemoteSessionMode::None,
                ..Default::default()
            },
            crate::session::lifecycle::SessionLifecycleFilter {
                resolution: crate::session::lifecycle::ResolutionFilter::Open,
                archive: crate::session::lifecycle::ArchiveFilter::Any,
            },
        )
        .await
        .expect_err("lifecycle list must reject a local/bookmark ownership conflict");
    assert!(list_error.to_string().contains("Session location conflict"));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn acp_remote_lifecycle_filter_uses_owner_before_page_and_includes_metadata() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());
    let remote = RealStorageHandleFixture::new().await;
    let (node_id, manager) = register_remote_node(mesh, &remote, "filtered-owner", false).await;
    let store = remote.handle.config.provider.history_store();
    for index in 0..30 {
        store
            .create_session(
                Some(format!("Remote session {index}")),
                Some(std::path::PathBuf::from("/remote/work")),
                None,
                None,
            )
            .await
            .expect("persist remote session");
    }
    let all_sessions = manager
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: Some(0),
            limit: Some(100),
        })
        .await
        .expect("list all owner sessions");
    for session in all_sessions.sessions.iter().take(10) {
        store
            .patch_session_lifecycle(
                &session.session_id,
                &crate::session::lifecycle::SessionLifecyclePatch {
                    resolved: Some(true),
                    archived: None,
                },
            )
            .await
            .expect("resolve first owner page");
    }

    let mut meta = crate::acp::protocol::Meta::new();
    meta.insert("remoteNodeIds".into(), serde_json::json!([node_id]));
    meta.insert(
        "querymt".into(),
        serde_json::json!({
            "sessionFilters": { "resolution": "open", "archive": "excluded" }
        }),
    );
    let request = |cursor| {
        crate::acp::protocol::ListSessionsRequest::new()
            .cwd("/remote/work")
            .meta(meta.clone())
            .cursor(cursor)
    };
    let first_page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        request(None),
    )
    .await
    .expect("owner-filtered first ACP page");
    assert_eq!(first_page.sessions.len(), 10);
    assert_eq!(first_page.next_cursor.as_deref(), Some("remote:0:10:live"));
    for session in &first_page.sessions {
        let lifecycle = session
            .meta
            .as_ref()
            .and_then(|meta| meta.get("querymt"))
            .and_then(|meta| meta.get("lifecycle"))
            .expect("authoritative owner lifecycle metadata");
        assert_eq!(lifecycle["resolved"], false);
        assert_eq!(lifecycle["archived"], false);
        assert!(lifecycle["resolved_at"].is_null());
        assert!(lifecycle["archived_at"].is_null());
    }

    let second_page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        request(first_page.next_cursor),
    )
    .await
    .expect("owner-filtered second ACP page");
    assert_eq!(second_page.sessions.len(), 10);
    assert_eq!(second_page.next_cursor, None);
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_remote_profile_from_owner_reaches_list_attach_and_bookmarked_load() {
    use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};

    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());
    let mut remote = RealStorageHandleFixture::new().await;
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("owner.sqlite");
    remote.storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(db_path.clone())
            .await
            .unwrap(),
    );
    remote.handle = LocalAgentHandle::from_config(Arc::new(
        AgentConfigBuilder::new(
            remote.handle.config.provider.plugin_registry(),
            remote.storage.clone(),
            LLMParams::new().provider("mock").model("raw-manager-model"),
        )
        .build(),
    ));
    let (node_id, node_manager) = register_remote_node(mesh, &remote, "owner", true).await;
    let profile_dir = tempfile::tempdir().unwrap();
    write_profile(
        profile_dir.path(),
        "alpha.toml",
        "[agent]\nprovider = 'mock'\nmodel = 'owner-model'\nsystem = 'owner alpha'\n",
    );
    write_profile(
        profile_dir.path(),
        "beta.toml",
        "[agent]\nprovider = 'mock'\nmodel = 'different-model'\nsystem = 'owner beta'\n",
    );
    let catalog: Arc<dyn ProfileCatalog> = Arc::new(
        LocalProfileCatalog::builder()
            .include_embedded_default(false)
            .local_dir(profile_dir.path())
            .build(),
    );
    let plugins = remote.handle.config.provider.plugin_registry();
    let mut mock = MockLlmProvider::new();
    mock.expect_chat().returning(|_| {
        Ok(crate::test_utils::MockChatResponse::text_only("completed on owner").into())
    });
    mock.expect_chat_with_tools().returning(|_, _| {
        Ok(crate::test_utils::MockChatResponse::text_only("completed on owner").into())
    });
    plugins.register_static(Arc::new(TestProviderFactory::new(SharedLlmProvider {
        inner: Arc::new(Mutex::new(mock)),
        tools: vec![].into_boxed_slice(),
    })));
    let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog.clone(),
        "alpha",
        AgentInfra {
            plugin_registry: plugins.clone(),
            storage: Some(remote.storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    ));
    // Installing profiles after mesh publication must still affect remote creation.
    remote.handle.set_profiles(profiles.clone());
    let created = node_manager
        .ask(crate::agent::remote::CreateRemoteSession { cwd: None })
        .await
        .expect("create owner session");
    remote
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            created.session_id.clone(),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("mesh-created session must load directly on its owner");
    let runtime = profiles.active_runtime().await.unwrap();
    let owner_handle = runtime.agent().handle();
    let owner_ref = owner_handle
        .registry
        .lock()
        .await
        .get(&created.session_id)
        .cloned()
        .unwrap();
    let profile = profiles
        .list_profiles()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == "alpha")
        .unwrap();
    assert_eq!(
        owner_ref
            .get_session_control()
            .await
            .unwrap()
            .effective_model
            .model,
        "owner-model"
    );
    let result = owner_ref
        .prompt_agent(crate::acp::protocol::PromptRequest::new(
            created.session_id.clone(),
            vec!["finish on B".into()],
        ))
        .await
        .expect("owner-only prompt completes");
    assert_eq!(
        result.stop_reason,
        crate::acp::protocol::StopReason::EndTurn
    );
    let history_before = remote
        .storage
        .session_store()
        .get_history(&created.session_id)
        .await
        .unwrap();
    assert!(
        history_before
            .iter()
            .any(|message| message.role == querymt::chat::ChatRole::Assistant)
    );
    profiles.set_active_profile("beta").await.unwrap();
    owner_ref.set_mode(AgentMode::Plan).await.unwrap();
    owner_ref
        .set_reasoning_effort(Some(ReasoningEffort::High))
        .await
        .unwrap();

    let listed = ext_method_json(
        &f.handle,
        "querymt/remote/sessions",
        serde_json::json!({ "node_id": node_id }),
    )
    .await;
    assert_eq!(listed["sessions"][0]["profile_id"], profile.id);
    assert_eq!(listed["sessions"][0]["profile_label"], profile.name);

    let attached = ext_method_json(
        &f.handle,
        "querymt/remote/attachSession",
        serde_json::json!({
            "node_id": node_id, "session_id": created.session_id,
        }),
    )
    .await;
    assert_eq!(attached["profile_id"], profile.id);
    assert_eq!(attached["profile_label"], profile.name);
    assert_eq!(attached["config_options"].as_array().map(Vec::len), Some(3));
    assert_eq!(attached["config_options"][1]["currentValue"], "high");
    assert_eq!(attached["config_options"][2]["id"], "model");
    assert_eq!(
        attached["config_options"][2]["_meta"]["querymt"]["modelEntry"]["provider"],
        "mock"
    );
    assert_eq!(attached["config_options"][0]["id"], "mode");
    assert_eq!(attached["config_options"][0]["currentValue"], "plan");
    let attachment_id = f
        .handle
        .registry
        .lock()
        .await
        .remote_attachment(&created.session_id)
        .expect("remote attachment")
        .attachment_id;

    let response = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(created.session_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("bookmarked remote load");
    let json = serde_json::to_value(response).expect("serialize load response");
    assert_eq!(json["_meta"]["profileId"], profile.id);
    assert_eq!(json["_meta"]["profileLabel"], profile.name);
    assert_eq!(json["configOptions"], attached["config_options"]);
    assert_eq!(json["configOptions"][0]["id"], "mode");
    assert_eq!(json["configOptions"][0]["currentValue"], "plan");
    assert_eq!(
        f.handle
            .registry
            .lock()
            .await
            .remote_attachment(&created.session_id)
            .expect("attachment after load")
            .attachment_id,
        attachment_id
    );

    let changed = f
        .handle
        .set_session_config_option(crate::acp::protocol::SetSessionConfigOptionRequest::new(
            created.session_id.clone(),
            "mode",
            "review",
        ))
        .await
        .expect("change remote mode through ACP config option");
    assert_eq!(
        config_option_value(&changed.config_options, "mode").as_deref(),
        Some("review")
    );
    assert_eq!(
        config_option_value(&changed.config_options, "reasoning_effort").as_deref(),
        Some("high")
    );
    assert!(config_option_value(&changed.config_options, "model").is_some());
    assert_eq!(
        owner_ref.get_mode().await.expect("owner mode"),
        AgentMode::Review
    );

    let reloaded = f
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            SessionId::from(created.session_id.clone()),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("reload remote session");
    assert_eq!(
        config_option_value(
            reloaded
                .config_options
                .as_deref()
                .expect("remote config options"),
            "mode"
        )
        .as_deref(),
        Some("review")
    );
    assert_eq!(
        f.handle
            .registry
            .lock()
            .await
            .remote_attachment(&created.session_id)
            .expect("attachment after reload")
            .attachment_id,
        attachment_id
    );
    let reattached = ext_method_json(
        &f.handle,
        "querymt/remote/attachSession",
        serde_json::json!({ "node_id": node_id, "session_id": created.session_id }),
    )
    .await;
    assert_eq!(reattached["config_options"][0]["currentValue"], "review");

    // A fork keeps its source profile even after the owner's active default changes.
    let forked = node_manager
        .ask(crate::agent::remote::ForkRemoteSession {
            source_session_id: created.session_id.clone(),
            message_id: history_before.last().unwrap().id.clone(),
        })
        .await
        .expect("fork in the source runtime");
    let fork_load = remote
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            forked.session_id.clone(),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("fork must load directly on its owner");
    assert_eq!(
        config_option_value(&fork_load.config_options.unwrap(), "profile").as_deref(),
        Some("alpha")
    );
    assert!(
        owner_handle
            .registry
            .lock()
            .await
            .get(&forked.session_id)
            .is_some()
    );
    node_manager
        .ask(crate::agent::remote::StopRemoteSessionRuntime {
            session_id: forked.session_id,
        })
        .await
        .unwrap();

    // Rebuild the owner's profile manager with a different active default. The
    // persisted binding must restore alpha, not silently switch this session to beta.
    f.handle.registry.lock().await.remove(&created.session_id);
    node_manager
        .ask(crate::agent::remote::StopRemoteSessionRuntime {
            session_id: created.session_id.clone(),
        })
        .await
        .unwrap();
    let mut restarted = RealStorageHandleFixture::new().await;
    restarted.storage = Arc::new(
        crate::session::sqlite_storage::SqliteStorage::connect(db_path)
            .await
            .unwrap(),
    );
    restarted.handle = LocalAgentHandle::from_config(Arc::new(
        AgentConfigBuilder::new(
            plugins.clone(),
            restarted.storage.clone(),
            LLMParams::new().provider("mock").model("raw-manager-model"),
        )
        .build(),
    ));
    let restarted_profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
        catalog,
        "beta",
        AgentInfra {
            plugin_registry: plugins,
            storage: Some(restarted.storage.clone()),
            session_mcp_attachment_source: None,
            event_fanout: None,
        },
    ));
    restarted.handle.set_profiles(restarted_profiles.clone());
    let (_, restarted_manager) = register_remote_node(mesh, &restarted, "owner", true).await;
    for lifecycle in [false, true] {
        if lifecycle {
            restarted_manager
                .ask(crate::agent::remote::ListRemoteSessionsWithLifecycle {
                    offset: None,
                    limit: None,
                    cwd: None,
                    scope: crate::session::projection::SessionScope::All,
                    lifecycle: Default::default(),
                })
                .await
                .expect("cold lifecycle listing");
        } else {
            restarted_manager
                .ask(crate::agent::remote::ListRemoteSessions {
                    offset: None,
                    limit: None,
                })
                .await
                .expect("cold listing");
        }
        assert!(
            restarted_profiles.materialized_runtimes().await.is_empty(),
            "listing persisted sessions must not build profile runtimes"
        );
    }
    let resumed = restarted_manager
        .ask(crate::agent::remote::ResumeRemoteSession {
            session_id: created.session_id.clone(),
        })
        .await
        .expect("mesh resumes the original owner profile after reopening storage");
    let runtime = restarted_profiles
        .runtime_for_profile("alpha")
        .await
        .unwrap();
    let restored_handle = runtime.agent().handle();
    let restored_actor = restored_handle
        .registry
        .lock()
        .await
        .local_actor_ref(&created.session_id)
        .cloned()
        .unwrap();
    let crate::agent::remote::node_manager::SessionHandoff::DirectRemote { session_ref } =
        resumed.handoff
    else {
        panic!("direct handoff");
    };
    assert_eq!(
        session_ref.id(),
        restored_actor.id(),
        "mesh and direct access must reuse one actor"
    );
    let loaded = restarted
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            created.session_id.clone(),
            std::path::PathBuf::new(),
        ))
        .await
        .expect("direct reload with empty runtime caches");
    let options = loaded.config_options.unwrap();
    assert_eq!(
        config_option_value(&options, "profile").as_deref(),
        Some("alpha")
    );
    assert_eq!(
        config_option_value(&options, "model").as_deref(),
        Some("mock/owner-model")
    );
    assert_eq!(
        config_option_value(&options, "reasoning_effort").as_deref(),
        Some("high")
    );
    assert_eq!(
        config_option_value(&options, "mode").as_deref(),
        Some("review")
    );
    assert_eq!(
        restarted
            .storage
            .session_store()
            .get_history(&created.session_id)
            .await
            .unwrap()
            .len(),
        history_before.len(),
        "loading does not rerun the prompt or discard history"
    );
    assert_eq!(
        restored_handle
            .registry
            .lock()
            .await
            .local_actor_ref(&created.session_id)
            .unwrap()
            .id(),
        restored_actor.id()
    );
    restored_actor.kill();
    restored_actor.wait_for_shutdown().await;
    restarted_manager
        .ask(crate::agent::remote::ResumeRemoteSession {
            session_id: created.session_id.clone(),
        })
        .await
        .expect("dead profile-bound actor rematerializes in its owner runtime");
    let replacement = restored_handle
        .registry
        .lock()
        .await
        .local_actor_ref(&created.session_id)
        .cloned()
        .unwrap();
    assert!(replacement.is_alive());
    assert_ne!(replacement.id(), restored_actor.id());
    assert!(
        restarted
            .handle
            .registry
            .lock()
            .await
            .get(&created.session_id)
            .is_none(),
        "profile recovery must not populate the root registry"
    );
    let loaded = restarted
        .handle
        .load_session(crate::acp::protocol::LoadSessionRequest::new(
            created.session_id.clone(),
            std::path::PathBuf::new(),
        ))
        .await
        .unwrap();
    assert_eq!(
        config_option_value(&loaded.config_options.unwrap(), "profile").as_deref(),
        Some("alpha")
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_acp_remote_listing_preserves_raw_offsets_across_filtered_pages() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());
    let remote = RealStorageHandleFixture::new().await;
    let (node_id, manager) = register_remote_node(mesh, &remote, "listing-owner", false).await;
    let store = remote.handle.config.provider.history_store();
    for index in 0..25 {
        store
            .create_session(
                Some(format!("Session {index}")),
                Some(std::path::PathBuf::from("/remote/work")),
                None,
                None,
            )
            .await
            .expect("persist remote session");
    }
    let expected = manager
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: Some(0),
            limit: Some(100),
        })
        .await
        .expect("owner list");
    let mut meta = crate::acp::protocol::Meta::new();
    meta.insert("remoteNodeIds".into(), serde_json::json!([node_id]));
    let mut cursor = None;
    let mut ids = Vec::new();
    for expected_size in [10, 10, 5] {
        let page = crate::api::AgentSessions::list_for_acp_with_runtime(
            &f.handle,
            f.storage.view_store().expect("view store"),
            crate::acp::protocol::ListSessionsRequest::new()
                .cwd("/remote/work")
                .meta(meta.clone())
                .cursor(cursor),
        )
        .await
        .expect("ACP remote page");
        assert_eq!(page.sessions.len(), expected_size);
        ids.extend(
            page.sessions
                .into_iter()
                .map(|session| session.session_id.to_string()),
        );
        cursor = page.next_cursor;
        if ids.len() < 25 {
            assert_eq!(
                cursor.as_deref(),
                Some(format!("remote:0:{}:live", ids.len()).as_str())
            );
        }
    }
    assert_eq!(cursor, None);
    assert_eq!(
        ids,
        expected
            .sessions
            .iter()
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>()
    );

    // No matches must still advance a bounded raw page, not scan all history.
    let page = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        crate::acp::protocol::ListSessionsRequest::new()
            .cwd("/missing")
            .meta(meta),
    )
    .await
    .expect("filtered remote page");
    assert!(page.sessions.is_empty());
    assert_eq!(page.next_cursor.as_deref(), Some("remote:0:10:live"));
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_acp_remote_cursor_restarts_when_peer_switches_between_bookmarks_and_live() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    let remote = RealStorageHandleFixture::new().await;
    let (node_id, manager) = register_remote_node(mesh, &remote, "transition-owner", false).await;
    let store = remote.handle.config.provider.history_store();
    for _ in 0..25 {
        store
            .create_session(
                None,
                Some(std::path::PathBuf::from("/remote/work")),
                None,
                None,
            )
            .await
            .expect("remote session");
    }
    let owner_page = manager
        .ask(crate::agent::remote::ListRemoteSessions {
            offset: Some(0),
            limit: Some(100),
        })
        .await
        .expect("owner list");
    for entry in owner_page.sessions.iter().skip(13) {
        f.handle
            .config
            .provider
            .history_store()
            .save_remote_session_bookmark(&crate::session::store::RemoteSessionBookmark {
                session_id: entry.session_id.clone(),
                node_id: node_id.clone(),
                peer_label: "owner".into(),
                cwd: entry.cwd.clone(),
                created_at: entry.created_at,
                title: None,
            })
            .await
            .expect("bookmark subset");
    }
    let mut meta = crate::acp::protocol::Meta::new();
    meta.insert("remoteNodeIds".into(), serde_json::json!([node_id]));
    let request = |cursor: Option<String>| {
        crate::acp::protocol::ListSessionsRequest::new()
            .cwd("/remote/work")
            .meta(meta.clone())
            .cursor(cursor)
    };
    let offline = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        request(None),
    )
    .await
    .expect("offline first page");
    assert_eq!(offline.sessions.len(), 10);
    assert_eq!(
        offline.next_cursor.as_deref(),
        Some("remote:0:10:bookmarks")
    );

    f.handle.set_mesh(mesh.clone());
    let online = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        request(offline.next_cursor),
    )
    .await
    .expect("online restart");
    assert_eq!(
        online
            .sessions
            .iter()
            .map(|entry| entry.session_id.to_string())
            .collect::<Vec<_>>(),
        owner_page
            .sessions
            .iter()
            .take(10)
            .map(|entry| entry.session_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(online.next_cursor.as_deref(), Some("remote:0:10:live"));

    f.handle.clear_mesh();
    let offline_again = crate::api::AgentSessions::list_for_acp_with_runtime(
        &f.handle,
        f.storage.view_store().expect("view store"),
        request(online.next_cursor),
    )
    .await
    .expect("offline restart");
    assert_eq!(offline_again.sessions, offline.sessions);
    assert_eq!(
        offline_again.next_cursor.as_deref(),
        Some("remote:0:10:bookmarks")
    );
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_create_session_without_attach_returns_structured_result() {
    let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
    let f = RealStorageHandleFixture::new().await;
    f.handle.set_mesh(mesh.clone());

    let remote = RealStorageHandleFixture::new().await;
    let (node_id, _node_manager_ref) =
        register_remote_node(mesh, &remote, "peer-create", false).await;

    let created = ext_method_json(
        &f.handle,
        "querymt/remote/createSession",
        serde_json::json!({ "node_id": node_id, "cwd": "/tmp/work", "attach": false }),
    )
    .await;

    assert_eq!(created["node_id"], node_id);
    assert_eq!(created["attached"], false);
    assert!(created["session_id"].is_string());
    assert_eq!(created["config_options"], serde_json::json!([]));
    assert_eq!(created.get("snapshot"), None);
}

#[cfg(feature = "remote")]
#[tokio::test]
async fn test_querymt_remote_dismiss_session_returns_structured_result() {
    let f = RealStorageHandleFixture::new().await;

    let dismissed = ext_method_json(
        &f.handle,
        "querymt/remote/dismissSession",
        serde_json::json!({ "session_id": "remote-session-1" }),
    )
    .await;

    assert_eq!(dismissed["success"], true);
    assert_eq!(dismissed["session_id"], "remote-session-1");
}
