//! `build_root`: when a runtime's root may delegate.

use super::member::{DelegateBuildParams, MemberBuildSpec};
use super::root::{RootParts, build_root};
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::config::{ToolRef, ToolSpec};
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::{
    MockLlmProvider, SharedLlmProvider, TestProviderFactory, mock_plugin_registry,
};
use querymt::LLMParams;
use std::sync::Arc;

/// A root with one member `helper`, whose `tools` selection is `tools`.
async fn root_with_member(tools: &[&str]) -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
    root_with_member_and_plans(tools, None).await
}

async fn root_with_member_and_plans(
    tools: &[&str],
    protocol_plans: Option<crate::dotagents::DotagentsSubAgentPlans>,
) -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
    root_with_members(tools, protocol_plans, vec![]).await
}

/// Like [`root_with_member_and_plans`], with members that already exist
/// (remote agents) registered first.
async fn root_with_members(
    tools: &[&str],
    protocol_plans: Option<crate::dotagents::DotagentsSubAgentPlans>,
    preregistered: Vec<super::roster::Member>,
) -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
    let (root, dir, _storage) =
        root_with_members_and_storage(tools, protocol_plans, preregistered).await;
    (root, dir)
}

/// Like [`root_with_members`], also returning the root's storage.
async fn root_with_members_and_storage(
    tools: &[&str],
    protocol_plans: Option<crate::dotagents::DotagentsSubAgentPlans>,
    preregistered: Vec<super::roster::Member>,
) -> (Arc<LocalAgentHandle>, tempfile::TempDir, Arc<SqliteStorage>) {
    root_over(
        MockLlmProvider::new(),
        tools,
        protocol_plans,
        preregistered,
        true,
    )
    .await
}

/// A root whose members and root share `mock`; `helper` adds the configured
/// member `helper`.
pub(crate) async fn root_over(
    mock: MockLlmProvider,
    tools: &[&str],
    protocol_plans: Option<crate::dotagents::DotagentsSubAgentPlans>,
    preregistered: Vec<super::roster::Member>,
    helper: bool,
) -> (Arc<LocalAgentHandle>, tempfile::TempDir, Arc<SqliteStorage>) {
    let factory = Arc::new(TestProviderFactory::new(SharedLlmProvider::new(
        mock,
        vec![],
    )));
    let (registry, dir) = mock_plugin_registry(factory).expect("plugin registry");
    let registry = Arc::new(registry);
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());

    let llm = LLMParams::new().provider("mock").model("mock-model");
    let mut builder = AgentConfigBuilder::new(registry.clone(), storage.clone(), llm.clone());
    if !tools.is_empty() {
        let specs: Vec<ToolSpec> = tools.iter().map(|t| t.parse().unwrap()).collect();
        builder = builder.with_allowed_tools(specs);
    }

    let members = if helper {
        let mut spec = MemberBuildSpec::new(super::roster::AgentId::try_from("helper").unwrap());
        spec.llm_config = Some(llm);
        vec![spec]
    } else {
        vec![]
    };
    let root = build_root(
        builder,
        RootParts {
            enabled: true,
            settings: Default::default(),
            profile_id: None,
            cwd: None,
            members,
            member_params: DelegateBuildParams {
                plugin_registry: registry,
                snapshot_policy: SnapshotPolicy::None,
                skills_project_root: ".".into(),
                workspace_service: None,
                repository_knowledge: None,
            },
            preregistered,
            protocol_plans,
            skills: None,
            middleware_factories: vec![],
            middleware_entries: vec![],
            #[cfg(feature = "remote")]
            peer_delegates: vec![],
            #[cfg(feature = "remote")]
            mesh: None,
        },
    )
    .await
    .expect("root builds");
    (root, dir, storage)
}

fn protocol_plans(id: &str) -> crate::dotagents::DotagentsSubAgentPlans {
    use crate::dotagents::{
        DotagentsAgent, DotagentsAgentConfig, DotagentsAgentConnection, DotagentsAgentRole,
        DotagentsLayer, DotagentsManifest, DotagentsSource,
    };
    let mut manifest = DotagentsManifest::default();
    manifest.agents.insert(
        id.to_string(),
        DotagentsAgent {
            id: id.to_string(),
            name: id.to_string(),
            description: format!("{id} description"),
            enabled: true,
            role: DotagentsAgentRole::DelegationTarget,
            connection: DotagentsAgentConnection::default(),
            capabilities: vec![],
            config: DotagentsAgentConfig::default(),
            body: format!("{id} body"),
            extensions: Default::default(),
            source: DotagentsSource::entry(
                DotagentsLayer::Workspace,
                format!(".agents/agents/{id}/agent.md"),
                id,
            ),
            fingerprint: format!("fp-{id}"),
        },
    );
    crate::dotagents::DotagentsSubAgentPlans::from_manifest(&manifest)
}

#[tokio::test]
async fn every_member_shares_the_runtime_event_stream() {
    let (root, _dir) = root_with_member_and_plans(&[], Some(protocol_plans("writer"))).await;
    let root_fanout = root.config.event_sink.fanout().clone();
    let roster = root.config.roster().expect("root has a roster").clone();

    for id in ["helper", "writer"] {
        let member = roster
            .snapshot()
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("member `{id}` is listed"));
        let member = roster
            .resolve(&member)
            .unwrap_or_else(|e| panic!("member `{id}` resolves: {e}"));
        let member = member.as_local().expect("local member").clone();
        assert!(
            Arc::ptr_eq(member.event_fanout(), &root_fanout),
            "member `{id}` did not use the runtime event stream"
        );
    }
    root.shutdown().await;
}

#[tokio::test]
async fn protocol_target_activity_reaches_the_parents_client() {
    use crate::acp::shared::{SessionOwnerMap, is_event_owned};
    use crate::session::domain::ForkOrigin;
    use querymt_agent_core::events::{AgentEventKind, EventEnvelope};

    let (root, _dir) = root_with_member_and_plans(&[], Some(protocol_plans("writer"))).await;

    // Subscribe the way every ACP transport does: the root's fanout only.
    let mut events = root.config.event_sink.fanout().subscribe();

    // The protocol target is built after the client subscribed.
    let roster = root.config.roster().expect("root has a roster").clone();
    let member = roster.snapshot().get("writer").cloned().expect("listed");
    let member = roster.resolve(&member).expect("writer builds");
    let member = member.as_local().expect("local member").clone();

    let store = root.config.provider.history_store();
    let parent = store
        .create_session(None, None, None, None)
        .await
        .unwrap()
        .public_id;
    let child = store
        .create_session(
            None,
            None,
            Some(parent.clone()),
            Some(ForkOrigin::Delegation),
        )
        .await
        .unwrap()
        .public_id;

    // The client owns the parent session only.
    let owners = SessionOwnerMap::default();
    owners
        .lock()
        .await
        .entry(parent.clone())
        .or_default()
        .insert("conn-1".to_string());

    root.config
        .event_sink
        .emit_durable(
            &parent,
            AgentEventKind::SessionForked {
                parent_session_id: parent.clone(),
                child_session_id: child.clone(),
                target_agent_id: "writer".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: crate::session::domain::ForkPointType::ProgressEntry,
                fork_point_ref: "1".into(),
                instructions: None,
                selected_model_id: None,
                selected_provider_node_id: None,
            },
        )
        .await
        .unwrap();
    // The child's activity goes through the member's own sink.
    member
        .config
        .event_sink
        .emit_durable(
            &child,
            AgentEventKind::ToolCallStart {
                tool_call_id: "call-1".into(),
                tool_name: "read_tool".into(),
                arguments: "{}".into(),
            },
        )
        .await
        .unwrap();
    member
        .config
        .event_sink
        .emit_durable(
            &child,
            AgentEventKind::AssistantMessageStored {
                content: "done".into(),
                thinking: None,
                reasoning_parts: vec![],
                message_id: None,
            },
        )
        .await
        .unwrap();

    let mut delivered = Vec::new();
    while delivered.len() < 3 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("event arrives on the root's fanout")
            .expect("fanout open");
        if is_event_owned(&owners, "conn-1", &event).await {
            delivered.push(event);
        }
    }
    let child_kinds: Vec<&AgentEventKind> = delivered
        .iter()
        .filter(|e| e.session_id() == child)
        .map(EventEnvelope::kind)
        .collect();
    assert!(
        child_kinds
            .iter()
            .any(|k| matches!(k, AgentEventKind::ToolCallStart { tool_name, .. } if tool_name == "read_tool")),
        "tool event of the child reached the parent's client"
    );
    assert!(
        child_kinds
            .iter()
            .any(|k| matches!(k, AgentEventKind::AssistantMessageStored { content, .. } if content == "done")),
        "message event of the child reached the parent's client"
    );
    // A connection that does not own the parent gets nothing.
    assert!(!is_event_owned(&owners, "conn-2", &delivered[1]).await);
    root.shutdown().await;
}

#[tokio::test]
async fn member_agents_are_not_offered_the_delegate_tool() {
    // `helper` has no `tools` restriction, so it gets every built-in that a
    // member may have; `delegate` is not one of them.
    let (root, _dir) = root_with_member(&[]).await;
    assert!(
        root.config.tool_registry.find("delegate").is_some(),
        "the root itself is offered `delegate`"
    );
    let roster = root.config.roster().expect("root has a roster").clone();
    let member = roster
        .snapshot()
        .get("helper")
        .cloned()
        .expect("member `helper` is listed");
    let member = roster.resolve(&member).expect("helper resolves");
    let member = member.as_local().expect("local member").clone();
    assert!(
        member.config.tool_registry.find("delegate").is_none(),
        "a member's tool list must not contain `delegate`"
    );
    assert!(
        member.config.roster().is_none(),
        "a member has no roster of its own"
    );
    assert!(member.delegation_orchestrator().is_none());
    root.shutdown().await;
}

#[tokio::test]
async fn delegation_is_disabled() {
    // The root's tools exclude `delegate`: protocol targets stay out of the
    // roster, no service runs, and configured members are still listed.
    let (root, _dir) =
        root_with_member_and_plans(&["read_tool"], Some(protocol_plans("writer"))).await;
    assert!(root.delegation_orchestrator().is_none());
    assert!(root.config.tool_registry.find("delegate").is_none());
    let snapshot = root.config.roster().expect("roster").snapshot();
    let ids: Vec<String> = snapshot.iter().map(|m| m.id().to_string()).collect();
    assert_eq!(ids, ["helper"]);
    root.shutdown().await;
}

#[tokio::test]
async fn unrestricted_agent_can_delegate() {
    let (root, _dir) = root_with_member(&[]).await;
    assert!(
        root.config.tool_registry.find("delegate").is_some(),
        "root tool list must contain `delegate`"
    );
    assert!(root.config.tool_config.allows(ToolRef::builtin("delegate")));
    assert!(
        root.delegation_orchestrator().is_some(),
        "an unrestricted root must run a delegation service"
    );
    root.shutdown().await;
}

#[tokio::test]
async fn agent_whose_tools_exclude_delegate_cannot_delegate() {
    let (root, _dir) = root_with_member(&["read_tool"]).await;
    assert!(
        root.config.tool_registry.find("delegate").is_none(),
        "root tool list must not contain `delegate`"
    );
    assert!(!root.config.tool_config.allows(ToolRef::builtin("delegate")));
    assert!(
        root.delegation_orchestrator().is_none(),
        "a root that cannot call `delegate` must not run a service"
    );
    root.shutdown().await;
}

#[cfg(feature = "remote")]
mod remote_members {
    use super::*;
    use crate::agent::remote::remote_setup::remote_members_from_config;
    use crate::agent::remote::scope::{MeshScopeId, scoped_node_manager_for_peer};
    use crate::agent::remote::test_helpers::fixtures::{NodeManagerFixture, get_test_mesh};
    use crate::agent::remote::{MeshRuntimeHandle, NodeId};
    use crate::config::{MeshPeerConfig, RemoteAgentConfig};
    use crate::delegation::roster::{MemberLocation, MemberOrigin};

    fn random_peer_id() -> libp2p::PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    /// `[[remote_agents]]` for `gpu` on peer `gpu-node`, whose configured
    /// address names `peer_id` as the owner.
    fn gpu_config(
        peer_id: libp2p::PeerId,
    ) -> (Vec<RemoteAgentConfig>, Vec<MeshPeerConfig>, String) {
        let node_id = NodeId::from_peer_id(peer_id).to_string();
        (
            vec![RemoteAgentConfig {
                id: "gpu".into(),
                name: "GPU".into(),
                description: "Remote GPU agent".into(),
                peer: "gpu-node".into(),
                capabilities: vec![],
            }],
            vec![MeshPeerConfig {
                name: "gpu-node".into(),
                addr: format!("/ip4/127.0.0.1/tcp/41999/p2p/{node_id}"),
            }],
            node_id,
        )
    }

    /// A root on the shared test mesh with the configured remote members.
    async fn root_with_remote(
        remotes: &[RemoteAgentConfig],
        peers: &[MeshPeerConfig],
    ) -> (Arc<LocalAgentHandle>, tempfile::TempDir) {
        let members = remote_members_from_config(remotes, peers).expect("remote members");
        let (root, dir) = root_with_members(&[], None, members).await;
        root.set_mesh(get_test_mesh().await.clone());
        (root, dir)
    }

    async fn open_session(
        root: &LocalAgentHandle,
        id: &str,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), crate::acp::protocol::Error> {
        let roster = root.config.roster().expect("roster").clone();
        let member = roster
            .snapshot()
            .get(id)
            .cloned()
            .expect("member is listed");
        let agent = roster.resolve(&member).expect("remote member resolves");
        agent
            .create_delegation_session(root, None, "parent-session".into())
            .await
    }

    #[tokio::test]
    async fn remote_member_is_listed_while_its_peer_is_unreachable() {
        let (remotes, peers, _node_id) = gpu_config(random_peer_id());
        let (root, _dir) = root_with_remote(&remotes, &peers).await;

        // Listed although no node manager of that peer exists.
        let snapshot = root.config.roster().expect("roster").snapshot();
        let member = snapshot.get("gpu").expect("gpu is listed");
        assert_eq!(member.location(), MemberLocation::Remote);
        assert_eq!(member.origin(), MemberOrigin::Config);

        // Delegating fails with an error that names the peer.
        let error = match open_session(&root, "gpu").await {
            Ok(_) => panic!("the peer is not reachable"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("gpu-node"),
            "error must name the peer: {}",
            error.message
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn delegation_succeeds_once_the_peer_appears() {
        // The in-process peer is hosted by the test mesh, so its owner id is
        // the mesh's own peer id.
        let (remotes, peers, node_id) = gpu_config(*get_test_mesh().await.peer_id());
        let (root, _dir) = root_with_remote(&remotes, &peers).await;
        assert!(open_session(&root, "gpu").await.is_err(), "not there yet");

        // The peer's node manager is published after the runtime started.
        let peer = NodeManagerFixture::new_with_mesh().await;
        MeshRuntimeHandle::from(get_test_mesh().await.clone())
            .register_actor(
                peer.actor_ref.clone(),
                scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &node_id),
            )
            .await;

        let (session_id, _) = open_session(&root, "gpu")
            .await
            .expect("the next delegation reaches the peer without a restart");
        let hosted = peer
            .actor_ref
            .ask(crate::agent::remote::ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list peer sessions");
        assert!(
            hosted.sessions.iter().any(|s| s.session_id == session_id),
            "the session was created on the peer"
        );
        root.shutdown().await;
    }

    /// Covers agent-roster / members from every source are listed together.
    #[tokio::test]
    async fn members_from_every_source_are_listed_together() {
        let (remotes, peers, _node_id) = gpu_config(random_peer_id());
        let members = remote_members_from_config(&remotes, &peers).expect("remote members");
        let (root, _dir) = root_with_members(&[], Some(protocol_plans("writer")), members).await;

        let snapshot = root.config.roster().expect("roster").snapshot();
        let listed: Vec<(String, MemberOrigin, MemberLocation)> = snapshot
            .iter()
            .map(|m| (m.id().to_string(), m.origin(), m.location()))
            .collect();
        assert_eq!(
            listed,
            [
                ("gpu".into(), MemberOrigin::Config, MemberLocation::Remote),
                ("helper".into(), MemberOrigin::Config, MemberLocation::Local),
                (
                    "writer".into(),
                    MemberOrigin::Protocol,
                    MemberLocation::Local
                ),
            ]
        );
        root.shutdown().await;
    }

    /// A delegated session on a remote member: the peer hosts the session,
    /// the root is attached to it, and a client owns the parent session only.
    struct RemoteDelegation {
        root: Arc<LocalAgentHandle>,
        peer: NodeManagerFixture,
        parent: String,
        child: String,
        owners: crate::acp::shared::SessionOwnerMap,
        events: tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
        _dir: tempfile::TempDir,
    }

    impl RemoteDelegation {
        async fn start() -> Self {
            use crate::session::domain::ForkOrigin;
            use querymt_agent_core::events::AgentEventKind;

            let (remotes, peers, node_id) = gpu_config(*get_test_mesh().await.peer_id());
            let (root, dir) = root_with_remote(&remotes, &peers).await;
            let peer = NodeManagerFixture::new_with_mesh().await;
            MeshRuntimeHandle::from(get_test_mesh().await.clone())
                .register_actor(
                    peer.actor_ref.clone(),
                    scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &node_id),
                )
                .await;

            // Subscribe the way every ACP transport does: the root's fanout only.
            let events = root.config.event_sink.fanout().subscribe();

            let parent = root
                .config
                .provider
                .history_store()
                .create_session(None, None, None, None)
                .await
                .unwrap()
                .public_id;
            let roster = root.config.roster().expect("roster").clone();
            let member = roster.snapshot().get("gpu").cloned().expect("listed");
            let (child, _session_ref) = roster
                .resolve(&member)
                .expect("remote member resolves")
                .create_delegation_session(&root, None, parent.clone())
                .await
                .expect("the peer is reachable");

            // The client owns the parent session; the fork propagates ownership.
            let owners = crate::acp::shared::SessionOwnerMap::default();
            owners
                .lock()
                .await
                .entry(parent.clone())
                .or_default()
                .insert("conn-1".to_string());
            root.config
                .event_sink
                .emit_durable(
                    &parent,
                    AgentEventKind::SessionForked {
                        parent_session_id: parent.clone(),
                        child_session_id: child.clone(),
                        target_agent_id: "gpu".into(),
                        origin: ForkOrigin::Delegation,
                        fork_point_type: crate::session::domain::ForkPointType::ProgressEntry,
                        fork_point_ref: "1".into(),
                        instructions: None,
                        selected_model_id: None,
                        selected_provider_node_id: None,
                    },
                )
                .await
                .unwrap();
            Self {
                root,
                peer,
                parent,
                child,
                owners,
                events,
                _dir: dir,
            }
        }

        /// Waits for an event of the child session on the root's fanout that
        /// `conn-1` owns and `wanted` accepts.
        async fn next_child_event(
            &mut self,
            wanted: impl Fn(&querymt_agent_core::events::AgentEventKind) -> bool,
        ) -> querymt_agent_core::events::EventEnvelope {
            loop {
                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(10), self.events.recv())
                        .await
                        .expect("the child's event arrives on the root's fanout")
                        .expect("fanout open");
                // Every event goes through the ownership check: the parent's
                // `SessionForked` is what extends ownership to the child.
                let owned =
                    crate::acp::shared::is_event_owned(&self.owners, "conn-1", &event).await;
                if owned && event.session_id() == self.child && wanted(event.kind()) {
                    return event;
                }
            }
        }
    }

    /// A delegated session is an attached, bookmarked remote session, yet it
    /// belongs to its parent: neither the bookmark listing nor the owner's
    /// session listing may offer it as a user session.
    #[tokio::test]
    async fn delegated_remote_session_is_not_listed_as_a_user_session() {
        use crate::api::{AgentSessions, ListSessionsOptions, RemoteSessionMode};

        let (remotes, peers, node_id) = gpu_config(*get_test_mesh().await.peer_id());
        let members = remote_members_from_config(&remotes, &peers).expect("remote members");
        let (root, _dir, storage) = root_with_members_and_storage(&[], None, members).await;
        root.set_mesh(get_test_mesh().await.clone());
        let peer = NodeManagerFixture::new_with_mesh().await;
        MeshRuntimeHandle::from(get_test_mesh().await.clone())
            .register_actor(
                peer.actor_ref.clone(),
                scoped_node_manager_for_peer(&MeshScopeId::lan_default(), &node_id),
            )
            .await;
        let (child, _) = open_session(&root, "gpu")
            .await
            .expect("the peer is reachable");

        // The session is attached and bookmarked, like any remote session.
        assert!(
            root.registry
                .lock()
                .await
                .remote_sessions()
                .iter()
                .any(|(id, _, _)| *id == child)
        );
        use crate::session::backend::StorageBackend;
        let sessions = AgentSessions::new(
            root.clone(),
            storage.view_store().expect("view store"),
            storage.session_store(),
            None,
        );

        // Bookmark and attached-session listing.
        let page = sessions
            .list(ListSessionsOptions {
                remote: RemoteSessionMode::Bookmarks,
                ..Default::default()
            })
            .await
            .expect("list with remote bookmarks");
        let listed: Vec<&str> = page
            .groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .map(|session| session.session_id.as_str())
            .collect();
        assert!(
            !listed.contains(&child.as_str()),
            "delegated session listed from bookmarks: {listed:?}"
        );

        // Owner listing, as `session/list` with `remoteNodeIds` does.
        let request: crate::acp::protocol::ListSessionsRequest = serde_json::from_value(
            serde_json::json!({"_meta": {"remoteNodeIds": [node_id], "session_scope": "root"}}),
        )
        .unwrap();
        let page = sessions
            .list_for_acp(request)
            .await
            .expect("list owner sessions");
        assert!(
            page.sessions
                .iter()
                .all(|session| session.session_id.to_string() != child),
            "delegated session listed from the owner"
        );

        // The owner knows it is a delegated session: any viewer of that peer
        // sees the origin, and the owner's own root listing leaves it out.
        let hosted = peer
            .actor_ref
            .ask(crate::agent::remote::ListRemoteSessions {
                offset: None,
                limit: None,
            })
            .await
            .expect("list peer sessions");
        let on_peer = hosted
            .sessions
            .iter()
            .find(|session| session.session_id == child)
            .expect("the peer hosts the session");
        assert_eq!(on_peer.fork_origin.as_deref(), Some("delegation"));
        let roots = peer
            .actor_ref
            .ask(crate::agent::remote::ListRemoteSessionsWithLifecycle {
                offset: None,
                limit: None,
                cwd: None,
                scope: crate::session::projection::SessionScope::Root,
                lifecycle: Default::default(),
            })
            .await
            .expect("list peer root sessions");
        assert!(
            roots
                .sessions
                .iter()
                .all(|row| row.session.session_id != child),
            "the owner lists a delegated session as a root session"
        );

        // Startup reattachment skips it.
        let bookmark = storage
            .session_store()
            .get_remote_session_bookmark(&child)
            .await
            .unwrap()
            .expect("the delegated session is bookmarked");
        assert_eq!(
            bookmark.fork_origin,
            Some(crate::session::domain::ForkOrigin::Delegation)
        );
        let error = match root.reattach_from_bookmark(&bookmark).await {
            Ok(_) => panic!("a delegated session must not be reattached at startup"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("delegated remote session"),
            "{error}"
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn remote_member_activity_reaches_the_parents_client() {
        use querymt_agent_core::events::AgentEventKind;

        let mut delegation = RemoteDelegation::start().await;

        // The peer's session runs and emits on the peer's own event stream.
        let peer_sink = delegation.peer.config.event_sink.clone();
        peer_sink
            .emit_durable(
                &delegation.child,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_tool".into(),
                    arguments: "{}".into(),
                },
            )
            .await
            .unwrap();
        peer_sink
            .emit_durable(
                &delegation.child,
                AgentEventKind::AssistantMessageStored {
                    content: "done".into(),
                    thinking: None,
                    reasoning_parts: vec![],
                    message_id: None,
                },
            )
            .await
            .unwrap();

        let envelope = delegation
            .next_child_event(|kind| {
                matches!(kind, AgentEventKind::ToolCallStart { tool_name, .. } if tool_name == "read_tool")
            })
            .await;
        delegation
            .next_child_event(|kind| {
                matches!(kind, AgentEventKind::AssistantMessageStored { content, .. } if content == "done")
            })
            .await;

        // A connection that does not own the parent gets nothing.
        assert!(!crate::acp::shared::is_event_owned(&delegation.owners, "conn-2", &envelope).await);
        delegation.root.shutdown().await;
    }

    #[tokio::test]
    async fn remote_member_interaction_reaches_the_user() {
        use crate::acp::protocol::{
            PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionRequest,
            ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        use crate::elicitation::{
            ElicitationResolution, ElicitationResponse, claim_live_elicitation_delivery,
            register_pending_permission, resolve_claimed_elicitation,
        };
        use querymt_agent_core::events::AgentEventKind;

        let mut delegation = RemoteDelegation::start().await;
        let child = delegation.child.clone();

        // The remote turn asks for a permission and waits for the answer.
        let request = RequestPermissionRequest::new(
            child.clone(),
            ToolCallUpdate::new(
                ToolCallId::from("remote-call".to_string()),
                ToolCallUpdateFields::new().title("Run shell"),
            ),
            vec![PermissionOption::new(
                PermissionOptionId::from("allow_once"),
                "Allow once",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let (tx, waiter) = tokio::sync::oneshot::channel();
        register_pending_permission(
            &delegation.peer.config.pending_elicitations,
            "remote-perm".to_string(),
            child.clone(),
            request.clone(),
            Some("run".to_string()),
            Some("remote-call".to_string()),
            tx,
        )
        .await;
        delegation
            .peer
            .config
            .event_sink
            .emit_durable(
                &child,
                AgentEventKind::PermissionRequested {
                    permission_id: "remote-perm".into(),
                    task_id: None,
                    tool_name: "shell".into(),
                    reason: "needs permission".into(),
                    tool_call_id: "remote-call".into(),
                    request: Some(request),
                },
            )
            .await
            .unwrap();

        // The request reaches the client that owns the parent session ...
        delegation
            .next_child_event(|kind| {
                matches!(kind, AgentEventKind::PermissionRequested { permission_id, .. } if permission_id == "remote-perm")
            })
            .await;
        assert_ne!(child, delegation.parent);

        // ... which claims it through the root and answers it.
        let claim =
            claim_live_elicitation_delivery(&delegation.root, &child, "remote-perm", "conn-1")
                .await
                .expect("the root reaches the pending request on the peer");
        let resolution = resolve_claimed_elicitation(
            &delegation.root,
            &child,
            "remote-perm",
            "conn-1",
            claim.delivery_generation,
            ElicitationResponse::permission_selected("allow_once"),
        )
        .await
        .expect("answer routed to the peer");
        assert_eq!(resolution, ElicitationResolution::Resolved);

        // The answer resumes the remote turn.
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("remote turn resumed")
            .expect("answer");
        assert_eq!(answer.selected_permission_option(), Some("allow_once"));
        delegation.root.shutdown().await;
    }
}

/// Changing the roster of a running root (design D9).
mod roster_changes {
    use super::*;
    use crate::acp::protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
    use crate::delegation::member::MemberSpec;
    use crate::delegation::roster::{MemberOrigin, RosterError};
    use crate::send_agent::SendAgent as _;
    use crate::session::domain::{Delegation, DelegationStatus};
    use crate::test_utils::MockChatResponse;
    use crate::tools::{ToolContext, ToolError};
    use async_trait::async_trait;
    use querymt::chat::Tool;
    use querymt_agent_core::events::AgentEventKind;
    use std::sync::Mutex;

    type Seen = Arc<Mutex<Vec<Vec<Tool>>>>;

    struct NoContext;

    #[async_trait]
    impl ToolContext for NoContext {
        fn session_id(&self) -> &str {
            "sess-test"
        }
        fn cwd(&self) -> Option<&std::path::Path> {
            None
        }
        async fn record_progress(
            &self,
            _kind: &str,
            _content: String,
            _metadata: Option<serde_json::Value>,
        ) -> Result<String, ToolError> {
            Ok(String::new())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    fn spec(id: &str) -> MemberSpec {
        serde_json::from_value(serde_json::json!({
            "kind": "local", "id": id, "provider": "mock", "model": "mock-model",
            "description": format!("{id} description")
        }))
        .expect("member spec")
    }

    /// A mock that answers every call with plain text and records the tools of
    /// every request. `gate` makes each answer wait for a release.
    fn scripted(
        seen: &Seen,
        gate: Option<(Arc<tokio::sync::Notify>, std::sync::mpsc::Receiver<()>)>,
    ) -> MockLlmProvider {
        let mut mock = MockLlmProvider::new();
        let gate = gate.map(|(entered, release)| (entered, Mutex::new(release)));
        let gate = Arc::new(gate);
        let wait = move || {
            if let Some((entered, release)) = gate.as_ref() {
                entered.notify_one();
                let _ = release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10));
            }
        };
        let wait = Arc::new(wait);
        let on_chat = wait.clone();
        mock.expect_chat().returning(move |_| {
            on_chat();
            Ok(MockChatResponse::text_only("Task complete").into())
        });
        let record = seen.clone();
        mock.expect_chat_with_tools().returning(move |_, tools| {
            record
                .lock()
                .unwrap()
                .push(tools.map(<[Tool]>::to_vec).unwrap_or_default());
            wait();
            Ok(MockChatResponse::text_only("Task complete").into())
        });
        mock.expect_tools().return_const(None).times(0..);
        mock
    }

    async fn plain_root() -> (Arc<LocalAgentHandle>, tempfile::TempDir, Seen) {
        let seen = Seen::default();
        let (root, dir, _) = root_over(scripted(&seen, None), &[], None, vec![], false).await;
        (root, dir, seen)
    }

    async fn new_session(root: &LocalAgentHandle) -> String {
        root.new_session(NewSessionRequest::new(std::env::temp_dir()))
            .await
            .expect("session")
            .session_id
            .to_string()
    }

    async fn run_turn(root: &LocalAgentHandle, session: &str) {
        let request = PromptRequest::new(
            session.to_string(),
            vec![ContentBlock::Text(TextContent::new("hello"))],
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), root.prompt(request))
            .await
            .expect("turn finishes")
            .expect("turn succeeds");
    }

    /// The enum of the `delegate` tool in the last request the model saw.
    fn offered_targets(seen: &Seen) -> Vec<String> {
        let seen = seen.lock().unwrap();
        let tools = seen.last().expect("the model was called");
        let delegate = tools
            .iter()
            .find(|tool| tool.function.name == "delegate")
            .expect("delegate tool is offered");
        delegate.function.parameters["properties"]["target_agent_id"]["enum"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .map(|id| id.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records a delegation request from a new root session, as the `delegate`
    /// tool does, and returns its public ID.
    async fn request_delegation(root: &LocalAgentHandle, target: &str) -> String {
        let parent = new_session(root).await;
        let store = root.config.provider.history_store();
        let parent_row = store.get_session(&parent).await.unwrap().unwrap();
        let delegation = store
            .create_delegation(Delegation {
                id: 0,
                public_id: String::new(),
                session_id: parent_row.id,
                task_id: None,
                target_agent_id: target.into(),
                objective: format!("fixture task for {target}"),
                objective_hash: Default::default(),
                context: None,
                constraints: None,
                expected_output: None,
                verification_spec: None,
                planning_summary: None,
                status: DelegationStatus::Requested,
                retry_count: 0,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
                isolation: Default::default(),
            })
            .await
            .unwrap();
        let public_id = delegation.public_id.clone();
        root.config
            .emit_event_persisted(
                &parent,
                AgentEventKind::DelegationRequested {
                    delegation,
                    tool_call_id: Some("call-1".into()),
                    batch_id: None,
                    cycle_id: None,
                },
            )
            .await
            .unwrap();
        public_id
    }

    async fn status_of(root: &LocalAgentHandle, delegation_id: &str) -> DelegationStatus {
        root.config
            .provider
            .history_store()
            .get_delegation(delegation_id)
            .await
            .unwrap()
            .expect("delegation row")
            .status
    }

    async fn wait_terminal(root: &LocalAgentHandle, delegation_id: &str) -> DelegationStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = status_of(root, delegation_id).await;
            if !matches!(
                status,
                DelegationStatus::Requested | DelegationStatus::Running
            ) || std::time::Instant::now() > deadline
            {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn member_added_to_a_plain_agent_can_be_delegated_to() {
        let (root, _dir, _seen) = plain_root().await;
        assert!(root.config.roster().unwrap().snapshot().is_empty());

        let generation = root.add_roster_member(spec("reviewer")).await.unwrap();
        assert_eq!(generation.get(), 1);
        let snapshot = root.config.roster().unwrap().snapshot();
        assert_eq!(
            snapshot.get("reviewer").unwrap().origin(),
            MemberOrigin::Runtime
        );

        let delegation = request_delegation(&root, "reviewer").await;
        assert_eq!(
            wait_terminal(&root, &delegation).await,
            DelegationStatus::Complete
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn removed_member_is_no_longer_a_delegation_target() {
        let (root, _dir, _seen) = plain_root().await;
        root.add_roster_member(spec("coder")).await.unwrap();
        let generation = root.remove_roster_member("coder").await.unwrap();
        assert_eq!(generation.get(), 2);

        let tool = root
            .tool_registry()
            .find("delegate")
            .expect("delegate tool");
        let error = tool
            .call(
                serde_json::json!({"target_agent_id": "coder", "objective": "work"}),
                &NoContext,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Unknown agent: 'coder'"),
            "{error}"
        );
        root.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn running_delegation_survives_removal_of_its_member() {
        let seen = Seen::default();
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, gate) = std::sync::mpsc::channel();
        let mock = scripted(&seen, Some((entered.clone(), gate)));
        let (root, _dir, _) = root_over(mock, &[], None, vec![], false).await;
        root.add_roster_member(spec("coder")).await.unwrap();

        let delegation = request_delegation(&root, "coder").await;
        tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
            .await
            .expect("the delegation reached the member's model call");

        root.remove_roster_member("coder")
            .await
            .expect("removal succeeds while the delegation runs");
        release.send(()).unwrap();

        assert_eq!(
            wait_terminal(&root, &delegation).await,
            DelegationStatus::Complete
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn root_agent_is_told_about_a_member_added_between_turns() {
        let (root, _dir, seen) = plain_root().await;
        root.add_roster_member(spec("coder")).await.unwrap();
        let session = new_session(&root).await;
        run_turn(&root, &session).await;
        assert_eq!(offered_targets(&seen), ["coder"]);

        let mut events = root.config.subscribe_events();
        root.add_roster_member(spec("reviewer")).await.unwrap();
        run_turn(&root, &session).await;

        assert_eq!(offered_targets(&seen), ["coder", "reviewer"]);
        let mut announced = false;
        while let Ok(event) = events.try_recv() {
            announced |= event.session_id() == session
                && matches!(event.kind(), AgentEventKind::ToolsAvailable { .. });
        }
        assert!(
            announced,
            "a tools_available event was recorded for the turn"
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn root_agent_is_told_when_a_member_was_removed() {
        let (root, _dir, seen) = plain_root().await;
        root.add_roster_member(spec("coder")).await.unwrap();
        root.add_roster_member(spec("writer")).await.unwrap();
        let session = new_session(&root).await;
        run_turn(&root, &session).await;
        assert_eq!(offered_targets(&seen), ["coder", "writer"]);

        root.remove_roster_member("coder").await.unwrap();
        run_turn(&root, &session).await;

        assert_eq!(offered_targets(&seen), ["writer"]);
        root.shutdown().await;
    }

    #[tokio::test]
    async fn activity_of_a_member_added_after_connect_reaches_the_client() {
        use crate::acp::shared::{SessionOwnerMap, is_event_owned};
        use crate::session::domain::{ForkOrigin, ForkPointType};

        let (root, _dir, _seen) = plain_root().await;
        // The client subscribes first, the way every ACP transport does.
        let mut events = root.config.event_sink.fanout().subscribe();

        root.add_roster_member(spec("reviewer")).await.unwrap();
        let roster = root.config.roster().unwrap().clone();
        let member = roster.snapshot().get("reviewer").cloned().unwrap();
        let member = roster.resolve(&member).unwrap().as_local().unwrap().clone();

        let store = root.config.provider.history_store();
        let parent = store
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        let child = store
            .create_session(
                None,
                None,
                Some(parent.clone()),
                Some(ForkOrigin::Delegation),
            )
            .await
            .unwrap()
            .public_id;
        let owners = SessionOwnerMap::default();
        owners
            .lock()
            .await
            .entry(parent.clone())
            .or_default()
            .insert("conn-1".to_string());

        root.config
            .event_sink
            .emit_durable(
                &parent,
                AgentEventKind::SessionForked {
                    parent_session_id: parent.clone(),
                    child_session_id: child.clone(),
                    target_agent_id: "reviewer".into(),
                    origin: ForkOrigin::Delegation,
                    fork_point_type: ForkPointType::ProgressEntry,
                    fork_point_ref: "1".into(),
                    instructions: None,
                    selected_model_id: None,
                    selected_provider_node_id: None,
                },
            )
            .await
            .unwrap();
        member
            .config
            .event_sink
            .emit_durable(
                &child,
                AgentEventKind::ToolCallStart {
                    tool_call_id: "call-1".into(),
                    tool_name: "read_tool".into(),
                    arguments: "{}".into(),
                },
            )
            .await
            .unwrap();

        let mut reached = false;
        while !reached {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event arrives on the root's fanout")
                .expect("fanout open");
            // Every event goes through the ownership check, as in a
            // transport: the fork event is what gives the client the child.
            let owned = is_event_owned(&owners, "conn-1", &event).await;
            reached = owned
                && event.session_id() == child
                && matches!(event.kind(), AgentEventKind::ToolCallStart { .. });
        }
        root.shutdown().await;
    }

    #[tokio::test]
    async fn isolated_member_without_a_workspace_service_is_rejected() {
        let (root, _dir, _seen) = plain_root().await;
        let mut notifications = root.subscribe_ext_notifications();
        let member: MemberSpec = serde_json::from_value(serde_json::json!({
            "kind": "local", "id": "worker", "provider": "mock", "model": "mock-model",
            "isolation": "isolated"
        }))
        .unwrap();

        let error = root.add_roster_member(member).await.unwrap_err();

        assert!(
            matches!(error, RosterError::IsolationWithoutWorkspace { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("workspace service"), "{error}");
        assert_eq!(
            root.config.roster().unwrap().snapshot().generation().get(),
            0
        );
        assert!(
            notifications.try_recv().is_err(),
            "a failed add sends nothing"
        );
        root.shutdown().await;
    }

    #[tokio::test]
    async fn member_with_a_file_prompt_part_is_rejected() {
        let (root, _dir, _seen) = plain_root().await;
        let mut notifications = root.subscribe_ext_notifications();
        let member: MemberSpec = serde_json::from_value(serde_json::json!({
            "kind": "local", "id": "worker", "provider": "mock", "model": "mock-model",
            "system": [{"file": "/etc/passwd"}]
        }))
        .unwrap();

        let error = root.add_roster_member(member).await.unwrap_err();

        assert!(
            matches!(error, RosterError::FilePromptPart { .. }),
            "{error}"
        );
        assert!(root.config.roster().unwrap().snapshot().is_empty());
        assert!(
            notifications.try_recv().is_err(),
            "a failed add sends nothing"
        );
        root.shutdown().await;
    }
}
