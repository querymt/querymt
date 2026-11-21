use super::SessionActorRef;
use crate::agent::messages;
use crate::error::AgentError;
use agent_client_protocol::ErrorCode;
use querymt::error::LLMErrorPayload;
use querymt_remote::{DeliveryCertainty, RemoteTransportFailureKind};

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

// ── Typed remote transport failures (Phase 2) ──────────────────────────────

#[test]
fn remote_reply_timeout_is_typed_ambiguous_transport_failure() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::<AgentError>::ReplyTimeout,
        "SubmitInput timed out on remote session",
    );

    let failure = mapped
        .transport_failure()
        .expect("reply timeout must surface as a typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ReplyTimeout);
    assert_eq!(failure.delivery, DeliveryCertainty::Unknown);
    assert_eq!(failure.message, "SubmitInput timed out on remote session");
}

#[test]
fn remote_handler_errors_pass_through_unwrapped() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::HandlerError(AgentError::SessionTimeout {
            details: "handler-reported timeout".to_string(),
        }),
        "caller timeout message must not be used",
    );

    match mapped {
        AgentError::SessionTimeout { details } => {
            assert_eq!(details, "handler-reported timeout")
        }
        other => panic!("handler errors must pass through unchanged, got {other}"),
    }
}

#[test]
fn remote_actor_unavailable_is_proven_not_delivered() {
    let mapped = SessionActorRef::map_infallible_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ActorNotRunning,
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ActorUnavailable);
    assert!(failure.proven_not_delivered());
    assert!(failure.is_retryable_kind());
}

#[test]
fn remote_connection_closed_is_ambiguous_delivery() {
    let mapped = SessionActorRef::map_infallible_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ConnectionClosed,
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ConnectionClosed);
    assert!(!failure.proven_not_delivered());
}

#[test]
fn remote_protocol_mismatch_is_never_retryable() {
    let mapped = SessionActorRef::map_agent_timeout_remote_send_error(
        kameo::error::RemoteSendError::<AgentError>::BadActorType,
        "ignored",
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ProtocolMismatch);
    assert!(!failure.is_retryable_kind());
}

#[test]
fn timeout_message_override_applies_only_to_reply_timeout() {
    let mapped = SessionActorRef::map_infallible_timeout_remote_send_error(
        kameo::error::RemoteSendError::<kameo::error::Infallible>::ConnectionClosed,
        "GetMode timed out on remote session",
    );

    let failure = mapped.transport_failure().expect("typed transport failure");
    assert_eq!(failure.kind, RemoteTransportFailureKind::ConnectionClosed);
    // Non-timeout failures keep the original kameo message.
    assert_eq!(failure.message, "connection closed");
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
        let store = Arc::new(
            querymt_agent_sqlite::mesh_access::SqliteMeshAccess::open_exclusive(
                fixture._tempdir.path().join("mesh.sqlite3"),
            )
            .await
            .unwrap(),
        );
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(host),
            peer: NodeId::from_peer_id(peer),
        };
        let grant = |filesystem| PeerGrant::Custom {
            sessions: true,
            filesystem,
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
        assert_selected_line(
            local
                .read_remote_file("example.txt".into(), 1, 1)
                .await
                .unwrap(),
        );
    }
}
