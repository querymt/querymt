use super::SessionActorRef;
use crate::agent::messages;
use crate::error::AgentError;
use agent_client_protocol::ErrorCode;
use querymt::error::LLMErrorPayload;

#[test]
fn local_prompt_handler_error_preserves_structured_provider_data() {
    let error = kameo::error::SendError::HandlerError(AgentError::ProviderFailure {
        message: "LLM streaming error".to_string(),
        provider: Some("openrouter".to_string()),
        model: Some("qwen/qwen3.5-122b-a10b".to_string()),
        retryable: false,
        error: Box::new(LLMErrorPayload::NotImplemented {
            message: "Streaming request construction not supported by this HTTP provider"
                .to_string(),
        }),
    });

    let acp = SessionActorRef::map_local_prompt_send_error(error);

    assert_eq!(acp.code, ErrorCode::new(-32010));
    assert_eq!(
        acp.data,
        Some(serde_json::json!({
            "category": "provider",
            "kind": "unsupported_operation",
            "message": "Streaming request construction not supported by this HTTP provider",
            "provider": "openrouter",
            "model": "qwen/qwen3.5-122b-a10b",
            "retryable": false,
            "error": {
                "type": "not_implemented",
                "message": "Streaming request construction not supported by this HTTP provider"
            }
        }))
    );
}

#[test]
fn local_prompt_transport_error_remains_internal() {
    let error = kameo::error::SendError::<messages::Prompt, AgentError>::ActorStopped;

    let acp = SessionActorRef::map_local_prompt_send_error(error);

    assert_eq!(acp.code, ErrorCode::INTERNAL_ERROR);
    assert!(acp.message.contains("actor stopped"));
}

#[cfg(feature = "remote")]
mod remote_paths {
    use super::SessionActorRef;
    use crate::agent::{
        core::{AgentMode, McpToolState, SessionRuntime},
        file_proxy::{FileProxyError, GetFileIndexResponse, ReadRemoteFileResponse},
        messages,
        remote::{
            remote_actor_impl::register_file_proxy,
            test_helpers::fixtures::{AgentConfigFixture, get_test_mesh},
        },
        session_actor::SessionActor,
    };
    use kameo::{
        actor::{ActorId, ActorRef, Spawn},
        remote::{
            RemoteActor, RemoteMessage,
            messaging::{SwarmRequest, SwarmResponse},
        },
    };
    use querymt_agent_index::{WorkspaceIndexManagerActor, get_or_create_workspace_with_timeout};
    use querymt_remote::{
        DeliveryCertainty, NodeId, RemoteTransportFailureKind,
        mesh_ingress::{InboundRouter, MeshIngress, SharingRejection},
        peer_access::{GrantStore, PeerGrant, PeerGrantKey},
    };
    use std::{collections::HashMap, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use uuid::Uuid;

    const TIMEOUT: Duration = Duration::from_secs(15);

    async fn workspace_session(
        fixture: &AgentConfigFixture,
        workspace: &TempDir,
    ) -> ActorRef<SessionActor> {
        let cwd = workspace.path().canonicalize().unwrap();
        std::fs::write(
            cwd.join("example.txt"),
            "first line\nselected line\nlast line\n",
        )
        .unwrap();
        std::fs::write(cwd.join(".local-note"), "inert local note\n").unwrap();
        std::fs::write(cwd.join("Cargo.toml"), "[workspace]\n").unwrap();
        let manager = WorkspaceIndexManagerActor::new(Default::default());
        let index = get_or_create_workspace_with_timeout(&manager, cwd.clone())
            .await
            .expect("workspace index ready");
        let runtime = SessionRuntime::new(Some(cwd), HashMap::new(), McpToolState::empty());
        assert!(runtime.workspace_handle.set(index).is_ok());
        let session = fixture
            .config
            .provider
            .history_store()
            .create_session(Some("actor reference fixture".into()), None, None, None)
            .await
            .expect("persist fixture session");
        SessionActor::spawn(SessionActor::new(
            fixture.config.clone(),
            session.public_id,
            runtime,
        ))
    }

    fn assert_selected_line(response: ReadRemoteFileResponse) {
        let ReadRemoteFileResponse::Text(text) = response else {
            panic!("expected a text file, got {response:?}");
        };
        assert!(text.contains("selected line"), "wrong page: {text}");
        assert!(
            !text.contains("first line") && !text.contains("last line"),
            "read must honor offset and limit: {text}"
        );
    }

    #[tokio::test]
    async fn remote_reference_reads_workspace_and_reports_stopped_actor() {
        let mesh = get_test_mesh().await;
        let fixture = AgentConfigFixture::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let local = workspace_session(&fixture, &workspace).await;
        let dht_name = format!("test-file-proxy-{}", Uuid::new_v4());
        mesh.register_actor(local.clone(), &dht_name).await;
        let actor = mesh
            .lookup_actor_with_timeout::<SessionActor>(&dht_name, TIMEOUT)
            .await
            .expect("lookup succeeds")
            .expect("session registered");
        mesh.deregister_actor(&dht_name);
        let reference = SessionActorRef::remote(actor, dht_name.clone());
        assert!(reference.is_remote());
        assert_eq!(reference.node_label(), dht_name);
        assert!(reference.remote_node_id().is_none());

        let index = reference.get_file_index().await.expect("remote index");
        assert_eq!(
            index.workspace_root,
            workspace
                .path()
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        assert!(index.files.iter().any(|entry| entry.path == "example.txt"));
        assert_selected_line(
            reference
                .read_remote_file("example.txt".into(), 1, 1)
                .await
                .unwrap(),
        );
        assert!(matches!(
            reference.read_remote_file("missing.txt".into(), 0, 1).await,
            Err(FileProxyError::PathResolution(_))
        ));

        SessionActorRef::from(local)
            .shutdown_with_timeout(TIMEOUT, TIMEOUT)
            .await
            .expect("owner stops the session");
        let Err(FileProxyError::Transport(failure)) = reference.get_file_index().await else {
            panic!("a stopped remote actor must produce a typed transport failure");
        };
        assert_eq!(failure.kind, RemoteTransportFailureKind::ActorUnavailable);
        assert_eq!(failure.delivery, DeliveryCertainty::NotDelivered);
    }

    /// A node issues passes only for its own owner's prompts: a turn that runs
    /// under a peer's authority is refused before it reaches a remote session.
    #[tokio::test]
    async fn a_node_issues_no_pass_for_a_peers_turn() {
        use crate::agent::session_actor::PeerTurn;
        use crate::error::AgentError;

        let mesh = get_test_mesh().await;
        let fixture = AgentConfigFixture::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let local = SessionActorRef::from(workspace_session(&fixture, &workspace).await);
        let SessionActorRef::Local(actor) = &local else {
            unreachable!()
        };
        let reference = SessionActorRef::Remote {
            actor_ref: actor.into_remote_ref().await,
            peer_label: "fixture owner".into(),
            remote_node_id: Some(mesh.peer_id().to_string()),
            mesh: Some(Arc::new(mesh.clone())),
        };
        let store = Arc::new(querymt_remote::MeshAccessStore::new(
            querymt_remote::MeshStateStore::shared(
                &workspace.path().join("peer-turn.mesh_state.json"),
            )
            .unwrap(),
        ));
        let authority =
            PeerTurn::test_authority(querymt_remote::peer_access::PeerGrant::Full, store);
        let session_id = actor.id().to_string();
        let error = reference
            .prompt_with_authority(
                crate::acp::protocol::PromptRequest::new(session_id, vec![]),
                &authority,
            )
            .await
            .unwrap_err();
        let expected: crate::acp::protocol::Error = AgentError::MeshAccessDenied.into();
        assert_eq!(error.code, expected.code);
        local.shutdown_with_timeout(TIMEOUT, TIMEOUT).await.unwrap();
    }

    #[tokio::test]
    async fn remote_reference_reads_session_control_history_and_idle_state() {
        use querymt_agent_core::runtime_status::{RuntimeAvailability, SessionRuntimePhase};

        let mesh = get_test_mesh().await;
        let fixture = AgentConfigFixture::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let local = SessionActorRef::from(workspace_session(&fixture, &workspace).await);
        let SessionActorRef::Local(actor) = &local else {
            unreachable!()
        };
        let node_id = mesh.peer_id().to_string();
        let reference = SessionActorRef::Remote {
            actor_ref: actor.into_remote_ref().await,
            peer_label: "fixture owner".into(),
            remote_node_id: Some(node_id.clone()),
            mesh: Some(Arc::new(mesh.clone())),
        };
        assert_eq!(reference.remote_node_id(), Some(node_id.as_str()));
        reference.check_history_access().await.unwrap();
        let control = reference.get_session_control().await.unwrap();
        assert_eq!(control.active_mode, AgentMode::Build);
        assert!(reference.get_session_limits().await.unwrap().is_none());
        assert_eq!(reference.get_reasoning_effort().await.unwrap(), None);
        assert!(reference.get_history().await.unwrap().is_empty());
        assert!(reference.get_user_prompts(None).await.unwrap().is_empty());
        assert!(
            reference
                .list_pending_interactions(None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            reference
                .claim_interaction_delivery(
                    None,
                    "fixture-connection".into(),
                    Some("fixture-authority".into()),
                )
                .await
                .unwrap()
                .is_empty()
        );
        let discarded = reference
            .discard_queued_input("already-delivered".into())
            .await
            .unwrap();
        assert!(
            matches!(discarded, messages::DiscardQueuedInputResult::NotPending { input_id } if input_id == "already-delivered")
        );

        let status = reference.get_runtime_status().await.unwrap();
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
        assert_eq!(status.observation.availability, RuntimeAvailability::Live);
        assert_eq!(
            reference
                .get_runtime_status_with_timeout(TIMEOUT, TIMEOUT)
                .await
                .unwrap(),
            status
        );
        let observation = reference.session_delivery_barrier().await.unwrap();
        assert_eq!(observation.runtime, status);
        assert!(observation.tip > 0);
        assert!(reference.owner_supports_chunks().await.unwrap());
        assert!(reference.owner_supports_chunks().await.unwrap());
        let chunk = reference
            .get_event_stream_chunk(messages::GetEventStreamChunk {
                after_source_seq: Some(i64::MAX),
                before_source_seq: None,
                tail: false,
                max_events: 1,
                max_bytes: messages::MAX_EVENT_STREAM_CHUNK_BYTES,
            })
            .await
            .unwrap();
        assert!(chunk.events.is_empty());
        assert_eq!(
            reference.get_event_stream().await.unwrap().len(),
            local.get_event_stream().await.unwrap().len()
        );

        local.shutdown_with_timeout(TIMEOUT, TIMEOUT).await.unwrap();
        let error = reference
            .get_runtime_status_with_timeout(TIMEOUT, TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            error.transport_failure().unwrap().kind,
            RemoteTransportFailureKind::ActorUnavailable
        );
    }

    fn session_ask<M: serde::Serialize>(actor_id: ActorId, message: &M) -> SwarmRequest
    where
        SessionActor: RemoteMessage<M>,
    {
        SwarmRequest::Ask {
            actor_id,
            actor_remote_id: <SessionActor as RemoteActor>::REMOTE_ID.into(),
            message_remote_id: <SessionActor as RemoteMessage<M>>::REMOTE_ID.into(),
            payload: rmp_serde::to_vec_named(message).unwrap(),
            mailbox_timeout: None,
            reply_timeout: None,
            immediate: false,
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn file_proxy_lists_visible_files_without_a_warm_index() {
        let host = *get_test_mesh().await.peer_id();
        let peer = libp2p::PeerId::random();
        let fixture = AgentConfigFixture::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let cwd = workspace.path().to_owned();
        std::fs::create_dir(cwd.join("nested")).unwrap();
        std::fs::write(cwd.join("nested/notes.txt"), "notes").unwrap();
        std::fs::write(cwd.join(".local-note"), "local note").unwrap();
        let runtime = SessionRuntime::new(Some(cwd.clone()), HashMap::new(), McpToolState::empty());
        let actor = SessionActor::spawn(SessionActor::new(
            fixture.config.clone(),
            "cold-index".into(),
            runtime.clone(),
        ));
        let store = Arc::new(querymt_remote::MeshAccessStore::new(
            querymt_remote::MeshStateStore::shared(
                &fixture._tempdir.path().join("mesh_state.json"),
            )
            .unwrap(),
        ));
        store
            .replace_grant(
                &PeerGrantKey {
                    host: NodeId::from_peer_id(host),
                    peer: NodeId::from_peer_id(peer),
                },
                Uuid::nil(),
                PeerGrant::Custom {
                    sessions: true,
                    filesystem: true,
                    shell: false,
                    providers: vec![],
                },
            )
            .await
            .unwrap();
        let router = InboundRouter::new(host, store);
        register_file_proxy(&router, &actor);
        let reply = router
            .handle_v1(peer, session_ask(actor.id(), &messages::GetFileIndex))
            .await;
        let Ok(SwarmResponse::Ask(Ok(bytes))) = reply else {
            panic!("expected the confined index: {reply:?}")
        };
        let response: GetFileIndexResponse = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(response.workspace_root, cwd.display().to_string());
        assert_eq!(
            response
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["nested", "nested/notes.txt"]
        );
        assert!(runtime.workspace_handle.get().is_none());
    }

    #[tokio::test]
    async fn file_proxy_filters_index_and_rechecks_filesystem_grant() {
        let host = *get_test_mesh().await.peer_id();
        let fixture = AgentConfigFixture::new().await;
        let workspace = tempfile::tempdir().unwrap();
        let actor = workspace_session(&fixture, &workspace).await;
        let local = SessionActorRef::from(actor.clone());
        let index = local.get_file_index().await.unwrap();
        assert!(
            index.files.iter().any(|entry| entry.path == ".local-note"),
            "fixture must include a local-only entry before filtering"
        );

        let peer = libp2p::PeerId::random();
        let store = Arc::new(querymt_remote::MeshAccessStore::new(
            querymt_remote::MeshStateStore::shared(
                &fixture._tempdir.path().join("mesh_state.json"),
            )
            .unwrap(),
        ));
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(host),
            peer: NodeId::from_peer_id(peer),
        };
        let grant = |filesystem| PeerGrant::Custom {
            sessions: true,
            filesystem,
            shell: false,
            providers: vec![],
        };
        let allowed = store
            .replace_grant(&key, Uuid::nil(), grant(true))
            .await
            .unwrap();
        let router = InboundRouter::new(host, store.clone());
        register_file_proxy(&router, &actor);
        let index_request = || session_ask(actor.id(), &messages::GetFileIndex);
        let read_request = |path: &str| {
            session_ask(
                actor.id(),
                &messages::ReadRemoteFile {
                    path: path.into(),
                    offset: 1,
                    limit: 1,
                },
            )
        };

        let reply = router.handle_v1(peer, index_request()).await;
        let Ok(SwarmResponse::Ask(Ok(bytes))) = reply else {
            panic!("granted peer must receive the filtered index: {reply:?}");
        };
        let remote: GetFileIndexResponse = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(remote.workspace_root, index.workspace_root);
        assert_eq!(remote.generated_at, index.generated_at);
        assert!(remote.files.iter().any(|entry| entry.path == "example.txt"));
        assert!(!remote.files.iter().any(|entry| entry.path == ".local-note"));

        #[cfg(target_os = "linux")]
        {
            let reply = router.handle_v1(peer, read_request("example.txt")).await;
            let Ok(SwarmResponse::Ask(Ok(bytes))) = reply else {
                panic!("granted peer must receive a text page: {reply:?}");
            };
            assert_selected_line(rmp_serde::from_slice(&bytes).unwrap());
            let reply = router.handle_v1(peer, read_request("missing.txt")).await;
            let Ok(SwarmResponse::Ask(Err(kameo::error::RemoteSendError::HandlerError(bytes)))) =
                reply
            else {
                panic!("missing file must return a typed handler error: {reply:?}");
            };
            assert!(matches!(
                rmp_serde::from_slice(&bytes).unwrap(),
                FileProxyError::ReadError(_)
            ));
        }

        assert_selected_line(
            local
                .read_remote_file("example.txt".into(), 1, 1)
                .await
                .unwrap(),
        );
        local.shutdown_with_timeout(TIMEOUT, TIMEOUT).await.unwrap();
        for request in [index_request(), read_request("example.txt")] {
            let response = router.handle_v1(peer, request).await;
            let Ok(SwarmResponse::Ask(Err(kameo::error::RemoteSendError::HandlerError(bytes)))) =
                response
            else {
                panic!("a stopped file actor must return a typed error: {response:?}");
            };
            let error: FileProxyError = rmp_serde::from_slice(&bytes).unwrap();
            let FileProxyError::Transport(failure) = error else {
                panic!("expected transport typing: {error:?}")
            };
            assert_eq!(failure.kind, RemoteTransportFailureKind::ActorUnavailable);
            assert_eq!(
                failure.delivery,
                querymt_remote::DeliveryCertainty::NotDelivered
            );
        }

        store
            .replace_grant(&key, allowed.revision, grant(false))
            .await
            .unwrap();
        for request in [index_request(), read_request("example.txt")] {
            assert!(matches!(
                router.handle_v1(peer, request).await,
                Err(SharingRejection::Denied)
            ));
        }
    }
}
