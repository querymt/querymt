use super::*;
use crate::mesh_runtime::{guarded_test_behaviour, messaging_config};
use crate::peer_access::{
    GrantStore, GrantStoreError, ModelSelection, PeerGrant, PeerGrantKey, PeerGrantState,
    ProviderGrant,
};
use futures_util::StreamExt;
use kameo::actor::ActorId;
use libp2p::swarm::SwarmEvent;
use tokio::sync::mpsc;

struct RecordingGate(mpsc::UnboundedSender<PeerId>);

#[async_trait]
impl MeshIngress for RecordingGate {
    async fn handle_legacy(&self, peer: PeerId, request: SwarmRequest) -> SwarmResponse {
        self.0.send(peer).unwrap();
        Legacy::overloaded(request)
    }

    async fn handle_v1(
        &self,
        peer: PeerId,
        _: SwarmRequest,
    ) -> Result<SwarmResponse, SharingRejection> {
        self.0.send(peer).unwrap();
        Err(SharingRejection::Denied)
    }
}

fn lan<B: NetworkBehaviour>(behaviour: impl FnOnce(PeerId) -> B) -> libp2p::Swarm<B> {
    libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|key| behaviour(key.public().to_peer_id()))
        .unwrap()
        .build()
}

async fn listening<B: NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    addr: Multiaddr,
) -> Multiaddr {
    let listener = swarm.listen_on(addr).unwrap();
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } if listener_id == listener => return address,
            SwarmEvent::ListenerError { listener_id, error } if listener_id == listener => {
                panic!("test listener failed: {error}")
            }
            _ => {}
        }
    }
}

async fn round_trip<B: NetworkBehaviour>(
    mut client: libp2p::Swarm<remote::Behaviour>,
    mut server: libp2p::Swarm<B>,
    addr: Multiaddr,
    mut calls: mpsc::UnboundedReceiver<PeerId>,
) {
    let server_peer = *server.local_peer_id();
    let client_peer = *client.local_peer_id();
    client
        .dial(addr.with(libp2p::multiaddr::Protocol::P2p(server_peer)))
        .unwrap();
    loop {
        tokio::select! {
            event = client.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == server_peer => break,
                SwarmEvent::OutgoingConnectionError { error, .. } => panic!("test dial failed: {error}"),
                _ => {}
            },
            _ = server.select_next_some() => {}
        }
    }
    let actor = ActorId::new_with_peer_id(17, server_peer);
    let request = client.behaviour_mut().messaging.ask(
        actor,
        "test::Echo".into(),
        "test::EchoRequest".into(),
        vec![],
        None,
        None,
        false,
    );
    loop {
        tokio::select! {
            event = client.select_next_some() => {
                if let SwarmEvent::Behaviour(remote::Event::Messaging(remote::messaging::Event::AskResult { peer, request_id, result, .. })) = event {
                    assert_eq!(request_id, request);
                    assert_eq!(peer, server_peer);
                    assert!(matches!(result, Err(RemoteSendError::MailboxFull)), "{result:?}");
                    break;
                }
            },
            _ = server.select_next_some() => {}
        }
    }
    assert_eq!(calls.try_recv().unwrap(), client_peer);
    assert!(calls.try_recv().is_err());
}

async fn exchange<B: NetworkBehaviour>(
    client: &mut libp2p::Swarm<SharingWire>,
    server: &mut libp2p::Swarm<B>,
    request: SharingRequest,
) -> Result<SharingResponse, SharingCallError> {
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    client
        .behaviour_mut()
        .send(*server.local_peer_id(), request, tx);
    loop {
        tokio::select! {
            result = &mut rx => return result.unwrap(),
            _ = client.select_next_some() => {},
            _ = server.select_next_some() => {},
        }
    }
}

#[tokio::test]
async fn sharing_probe_bypasses_the_actor_task_limit() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (tx, mut calls) = mpsc::unbounded_channel();
        let gate = Arc::new(RecordingGate(tx));
        let mut server = lan(|_| SharingWire::new(gate.clone(), Duration::from_secs(3)));
        let mut client = lan(|_| SharingWire::new(gate, Duration::from_secs(3)));
        let addr = listening(&mut server, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
        let host = *server.local_peer_id();
        client.add_peer_address(host, addr);
        for _ in 0..MAX_IN_FLIGHT {
            server.behaviour_mut().tasks.spawn(std::future::pending());
        }
        let ask = SwarmRequest::Ask {
            actor_id: ActorId::new_with_peer_id(17, host),
            actor_remote_id: "test::Echo".into(),
            message_remote_id: "test::EchoRequest".into(),
            payload: vec![],
            mailbox_timeout: None,
            reply_timeout: None,
            immediate: false,
        };
        assert!(matches!(
            exchange(
                &mut client,
                &mut server,
                SharingRequest::Actor(Box::new(ask))
            )
            .await,
            Ok(SharingResponse::Actor(Err(SharingRejection::Busy)))
        ));
        assert!(matches!(
            exchange(&mut client, &mut server, SharingRequest::Probe).await,
            Ok(SharingResponse::Supported)
        ));
        assert!(calls.try_recv().is_err());
    })
    .await
    .expect("sharing capacity test timed out");
}

#[tokio::test]
async fn sharing_wire_expiry_drops_handler_and_reclaims_slot() {
    struct PendingGate(tokio::sync::Mutex<Option<mpsc::UnboundedSender<()>>>);

    #[async_trait]
    impl MeshIngress for PendingGate {
        async fn handle_legacy(&self, _: PeerId, request: SwarmRequest) -> SwarmResponse {
            Legacy::overloaded(request)
        }

        async fn handle_v1(
            &self,
            _: PeerId,
            _: SwarmRequest,
        ) -> Result<SwarmResponse, SharingRejection> {
            let _guard = self.0.lock().await.take().unwrap();
            std::future::pending().await
        }
    }

    let short = Duration::from_millis(250);
    let long = Duration::from_secs(10);
    for (wire_budget, reply_budget) in [(short, None), (long, Some(short))] {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (tx, mut dropped) = mpsc::unbounded_channel();
            let gate = Arc::new(PendingGate(tokio::sync::Mutex::new(Some(tx))));
            let mut server = lan(|_| SharingWire::new(gate.clone(), wire_budget));
            let mut client = lan(|_| SharingWire::new(gate, long));
            let addr = listening(&mut server, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
            let host = *server.local_peer_id();
            client.add_peer_address(host, addr);
            let ask = SwarmRequest::Ask {
                actor_id: ActorId::new_with_peer_id(17, host),
                actor_remote_id: "test::Echo".into(),
                message_remote_id: "test::EchoRequest".into(),
                payload: vec![],
                mailbox_timeout: None,
                reply_timeout: reply_budget,
                immediate: false,
            };
            assert!(
                exchange(
                    &mut client,
                    &mut server,
                    SharingRequest::Actor(Box::new(ask))
                )
                .await
                .is_err()
            );
            assert!(dropped.recv().await.is_none());
            assert!(matches!(
                exchange(&mut client, &mut server, SharingRequest::Probe).await,
                Ok(SharingResponse::Supported)
            ));
            assert!(server.behaviour().tasks.is_empty());
            assert!(server.behaviour().pending_dispatch.is_empty());
        })
        .await
        .expect("handler was not reclaimed on wire or V1 exchange budget expiry");
    }
}

#[tokio::test]
async fn sharing_wire_distinguishes_legacy_negotiation_from_denial() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (tx, mut calls) = mpsc::unbounded_channel();
        let gate = Arc::new(RecordingGate(tx));
        let mut server = lan(|peer| guarded_test_behaviour(peer, gate.clone()));
        let mut client = lan(|_| SharingWire::new(gate.clone(), Duration::from_secs(3)));
        let addr = listening(&mut server, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
        let host = *server.local_peer_id();
        client.add_peer_address(host, addr);
        assert!(matches!(
            exchange(&mut client, &mut server, SharingRequest::Probe).await,
            Ok(SharingResponse::Supported)
        ));
        let ask = SwarmRequest::Ask {
            actor_id: ActorId::new_with_peer_id(17, host),
            actor_remote_id: "test::Echo".into(),
            message_remote_id: "test::EchoRequest".into(),
            payload: vec![],
            mailbox_timeout: None,
            reply_timeout: None,
            immediate: false,
        };
        assert!(matches!(
            exchange(
                &mut client,
                &mut server,
                SharingRequest::Actor(Box::new(ask))
            )
            .await,
            Ok(SharingResponse::Actor(Err(SharingRejection::Denied)))
        ));
        assert_eq!(calls.try_recv().unwrap(), *client.local_peer_id());
        assert!(calls.try_recv().is_err());

        let mut legacy =
            lan(|peer| remote::Behaviour::new(peer, messaging_config(Duration::from_secs(3))));
        let addr = listening(&mut legacy, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
        client.add_peer_address(*legacy.local_peer_id(), addr);
        assert!(matches!(
            exchange(&mut client, &mut legacy, SharingRequest::Probe).await,
            Err(SharingCallError::UnsupportedProtocol)
        ));
    })
    .await
    .expect("sharing negotiation test timed out");
}

#[derive(Default)]
struct FixtureGrants(parking_lot::RwLock<std::collections::HashMap<PeerGrantKey, PeerGrantState>>);

#[async_trait]
impl GrantStore for FixtureGrants {
    async fn grant(&self, key: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
        Ok(self.0.read().get(key).cloned().unwrap_or_default())
    }

    async fn replace_grant(
        &self,
        key: &PeerGrantKey,
        expected: uuid::Uuid,
        grant: PeerGrant,
    ) -> Result<PeerGrantState, GrantStoreError> {
        let mut grants = self.0.write();
        let actual = grants.get(key).cloned().unwrap_or_default().revision;
        if expected != actual {
            return Err(GrantStoreError::Conflict { expected, actual });
        }
        let state = PeerGrantState {
            grant,
            revision: uuid::Uuid::now_v7(),
        };
        grants.insert(key.clone(), state.clone());
        Ok(state)
    }

    async fn managed_peers(
        &self,
        host: &crate::NodeId,
    ) -> Result<Vec<crate::NodeId>, GrantStoreError> {
        Ok(self
            .0
            .read()
            .keys()
            .filter(|key| &key.host == host)
            .map(|key| key.peer.clone())
            .collect())
    }
}

#[tokio::test]
async fn mesh_provider_v1_requests_and_revoked_controls_do_not_downgrade() {
    use crate::mesh_runtime::bootstrap_mesh_handle_with_ingress;
    use crate::mesh_runtime_config::{
        DirectoryMode, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
    };
    use crate::provider_share::guarded::MeteredChat;
    use crate::{
        KameoMeshClientTransport, NodeId, ProviderChatRequest, ProviderChatResponse,
        ProviderHostActor, ProviderStreamRequest, ProviderStreamRouterActor,
        RemoteProviderClientTransport, RemoteProviderHostError, RenewProviderStreamLease,
    };
    use kameo::actor::RemoteActorRef;
    use querymt::error::{LLMError, ProviderErrorKind};
    use tokio::sync::oneshot;

    type StreamStart = ProviderStreamRequest<RemoteActorRef<ProviderStreamRouterActor>>;

    fn chat_reply(output_tokens: u32) -> ProviderChatResponse {
        ProviderChatResponse {
            output: querymt::chat::ChatOutput {
                usage: Some(querymt::Usage {
                    output_tokens,
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    fn assert_denied(error: LLMError) {
        let LLMError::ProviderResponseError(failure) = error else {
            panic!("expected a typed access denial, got {error:?}");
        };
        assert_eq!(failure.kind(), ProviderErrorKind::Authentication);
        assert_eq!(failure.code(), Some("mesh_access_denied"));
    }

    tokio::time::timeout(Duration::from_secs(15), async {
        let grants = Arc::new(FixtureGrants::default());
        let (start_tx, mut starts) = mpsc::unbounded_channel();
        let (legacy_start_tx, mut legacy_starts) = mpsc::unbounded_channel();
        let (renew_tx, mut renewals) = mpsc::unbounded_channel();
        let mut host = lan(|peer| {
            let router = Arc::new(InboundRouter::new(peer, grants.clone()));
            let provider = ActorId::new_with_peer_id(1, peer);
            router.register::<ProviderHostActor, MeteredChat, _, RemoteProviderHostError, _, _>(
                provider,
                InboundDelivery::V1Ask,
                InboundAccess::Providers,
                |_, msg| async move {
                    assert_eq!(msg.request.model, "echo-model");
                    assert!(!msg.request_id.is_nil());
                    Ok(chat_reply(7))
                },
            );
            router.register::<ProviderHostActor, StreamStart, _, RemoteProviderHostError, _, _>(
                provider,
                InboundDelivery::V1Ask,
                InboundAccess::Providers,
                move |peer, request| {
                    start_tx.send((peer.key().clone(), request)).unwrap();
                    async { Ok(()) }
                },
            );
            router.register::<ProviderHostActor, RenewProviderStreamLease, _, kameo::error::Infallible, _, _>(
                provider,
                InboundDelivery::V1Ask,
                InboundAccess::Providers,
                move |_, request| {
                    let (finish, reply) = oneshot::channel();
                    renew_tx.send((request, finish)).unwrap();
                    async move { Ok(reply.await.unwrap()) }
                },
            );
            guarded_test_behaviour(peer, router)
        });
        let mut legacy = lan(|peer| {
            let router = Arc::new(InboundRouter::new(peer, grants.clone()));
            router.register::<ProviderHostActor, ProviderChatRequest, _, RemoteProviderHostError, _, _>(
                ActorId::new_with_peer_id(1, peer),
                InboundDelivery::LegacyAsk,
                InboundAccess::Full,
                |_, _| async { Ok(chat_reply(3)) },
            );
            router.register::<ProviderHostActor, StreamStart, _, RemoteProviderHostError, _, _>(
                ActorId::new_with_peer_id(1, peer),
                InboundDelivery::LegacyTellV1Ask,
                InboundAccess::Full,
                move |_, request| {
                    legacy_start_tx.send(request).unwrap();
                    async { Ok(()) }
                },
            );
            router.register::<ProviderHostActor, RenewProviderStreamLease, _, kameo::error::Infallible, _, _>(
                ActorId::new_with_peer_id(1, peer),
                InboundDelivery::LegacyAsk,
                InboundAccess::Full,
                |_, request| async move { Ok(request.lease_ttl_secs == 30) },
            );
            LegacyWire::new(router, Duration::from_secs(3))
        });
        let loopback: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
        let host_addr = listening(&mut host, loopback.clone()).await;
        let legacy_addr = listening(&mut legacy, loopback).await;
        let (host_peer, legacy_peer) = (*host.local_peer_id(), *legacy.local_peer_id());
        let (unused, _calls) = mpsc::unbounded_channel();
        let identity = tempfile::tempdir().unwrap();
        let mesh = bootstrap_mesh_handle_with_ingress(&MeshRuntimeConfig {
            enabled: true,
            lan: Some(LanMeshConfig {
                listen: Some("/ip4/127.0.0.1/tcp/0".into()),
                discovery: LanDiscovery::None,
                directory: DirectoryMode::Kademlia,
            }),
            iroh_enabled: false,
            iroh_gso: false,
            iroh_dns: None,
            iroh_scopes: vec![],
            identity_file: Some(identity.path().join("identity")),
            request_timeout: Duration::from_secs(3),
            stream_reconnect_grace: Duration::from_secs(3),
            node_name: None,
            peers: vec![host_addr.with(libp2p::multiaddr::Protocol::P2p(host_peer)).to_string(), legacy_addr.with(libp2p::multiaddr::Protocol::P2p(legacy_peer)).to_string()],
            auto_fallback: false,
            access: Default::default(),
        }, Arc::new(RecordingGate(unused))).await.unwrap();
        let run = async {
            for peer in [host_peer, legacy_peer] {
                let mut events = mesh.subscribe_peer_events();
                while !mesh.is_peer_alive(&peer) {
                    events.recv().await.unwrap();
                }
            }
            let transport = KameoMeshClientTransport::new(mesh.clone());
            // Address the fixtures directly; DHT lookup is not the boundary under test.
            let host_ref = |peer| -> RemoteActorRef<ProviderHostActor> {
                rmp_serde::from_slice(
                    &rmp_serde::to_vec(&(ActorId::new_with_peer_id(1, peer),)).unwrap(),
                ).unwrap()
            };
            let host = host_ref(host_peer);
            let key_for = |peer| PeerGrantKey { host: NodeId::from_peer_id(peer), peer: NodeId::from_peer_id(*mesh.peer_id()) };
            let request = ProviderChatRequest {
                provider: "fixture".into(), model: "echo-model".into(), messages: vec![],
                tools: None, params: None, item_aware_contract_version: None, options: None,
            };
            grants.replace_grant(&key_for(legacy_peer), uuid::Uuid::nil(), PeerGrant::Full).await.unwrap();
            let legacy_reply = transport.send_chat_request(&host_ref(legacy_peer), &request).await.unwrap();
            assert_eq!(legacy_reply.output.usage.unwrap().output_tokens, 3);
            assert_denied(transport.send_chat_request(&host, &request).await.unwrap_err());
            let key = key_for(host_peer);
            let grant = grants.replace_grant(&key, uuid::Uuid::nil(), PeerGrant::Custom {
                sessions: false, filesystem: false, shell: false,
                providers: vec![ProviderGrant { provider_id: request.provider.clone(), models: ModelSelection::All }],
            }).await.unwrap();
            let reply = transport.send_chat_request(&host, &request).await.unwrap();
            assert_eq!(reply.output.usage.unwrap().output_tokens, 7);

            let (session, id) = ("mesh-stream-session", "mesh-stream-request");
            let (chunks, _consumer) = mpsc::unbounded_channel();
            let (router, callback) = transport.prepare_stream_router(&host, session, id, chunks).await.unwrap();
            let stream_request = ProviderStreamRequest {
                provider: request.provider, model: request.model, messages: vec![], tools: None,
                session_id: session.into(), request_id: id.into(), stream_router_ref: callback,
                reconnect_grace_secs: 3, heartbeat_interval_secs: 5, lease_ttl_secs: 30,
                params: None, item_aware_contract_version: None, options: None,
            };
            let legacy_host = host_ref(legacy_peer);
            transport.send_stream_request(&legacy_host, stream_request.clone()).await.unwrap();
            assert_eq!(legacy_starts.try_recv().unwrap().request_id, id);
            assert!(transport.renew_stream_lease(&legacy_host, session, id, 30).await.unwrap());

            transport.send_stream_request(&host, stream_request.clone()).await.unwrap();
            let (caller, started) = starts.try_recv().unwrap();
            assert_eq!(caller, key);
            assert_eq!(started.request_id, id);
            assert_eq!(started.stream_router_ref.id(), router.id());
            let (renewed, ()) = tokio::join!(transport.renew_stream_lease(&host, session, id, 30), async {
                let (renewed, reply) = renewals.recv().await.unwrap();
                assert_eq!((renewed.session_id.as_str(), renewed.request_id.as_str()), (session, id));
                reply.send(true).unwrap();
            });
            assert!(renewed.unwrap());

            // Revoke after admission, before the handler can return success.
            let (renewed, ()) = tokio::join!(transport.renew_stream_lease(&host, session, id, 30), async {
                let (_, reply) = renewals.recv().await.unwrap();
                grants.replace_grant(&key, grant.revision, PeerGrant::None).await.unwrap();
                reply.send(true).unwrap();
            });
            assert_denied(renewed.unwrap_err());
            assert_denied(transport.renew_stream_lease(&host, session, id, 30).await.unwrap_err());
            assert_denied(transport.send_stream_request(&host, stream_request).await.unwrap_err());
            assert!(matches!(renewals.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
            assert!(matches!(starts.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
            router.kill();
            mesh.shutdown().await;
        };
        tokio::pin!(run);
        loop {
            tokio::select! {
                _ = &mut run => break,
                _ = host.select_next_some() => {},
                _ = legacy.select_next_some() => {},
            }
        }
    })
    .await
    .expect("mesh provider loopback test timed out");
}

#[tokio::test]
async fn legacy_lan_requests_reach_only_the_authenticated_ingress() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut server = lan(|peer| guarded_test_behaviour(peer, Arc::new(RecordingGate(tx))));
        let client =
            lan(|peer| remote::Behaviour::new(peer, messaging_config(Duration::from_secs(3))));
        let addr = listening(&mut server, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
        round_trip(client, server, addr, rx).await;
    })
    .await
    .expect("LAN ingress test timed out");
}

#[tokio::test]
async fn legacy_iroh_requests_reach_only_the_authenticated_ingress() {
    use iroh::address_lookup::MemoryLookup;
    use libp2p::Transport as _;

    tokio::time::timeout(Duration::from_secs(15), async {
        let config = || libp2p_iroh::TransportConfig {
            relay_mode: iroh::RelayMode::Disabled,
            enable_gso: false,
            timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let server = libp2p_iroh::Transport::with_config(None, config())
            .await
            .unwrap();
        let client = libp2p_iroh::Transport::with_config(None, config())
            .await
            .unwrap();
        let server_endpoint = server.endpoint().clone();
        let client_endpoint = client.endpoint().clone();
        server_endpoint.address_lookup().unwrap().clear();
        let lookup = client_endpoint.address_lookup().unwrap();
        lookup.clear();
        lookup.add(MemoryLookup::from_endpoint_info([server_endpoint.addr()]));
        let server_id = server.peer_id;
        let client_id = client.peer_id;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut server = libp2p::Swarm::new(
            server.boxed(),
            guarded_test_behaviour(server_id, Arc::new(RecordingGate(tx))),
            server_id,
            libp2p::swarm::Config::with_tokio_executor(),
        );
        let client = libp2p::Swarm::new(
            client.boxed(),
            remote::Behaviour::new(client_id, messaging_config(Duration::from_secs(3))),
            client_id,
            libp2p::swarm::Config::with_tokio_executor(),
        );
        // The Iroh transport binds before building the swarm; listen_on starts
        // its protocol router. Dial by the authenticated peer ID.
        server.listen_on(Multiaddr::empty()).unwrap();
        round_trip(client, server, Multiaddr::empty(), rx).await;
        server_endpoint.close().await;
        client_endpoint.close().await;
    })
    .await
    .expect("Iroh ingress test timed out");
}

/// Drive `swarm` until `future` completes.
async fn driving<B: NetworkBehaviour, F: std::future::Future>(
    swarm: &mut libp2p::Swarm<B>,
    future: F,
) -> F::Output {
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return output,
            _ = swarm.select_next_some() => {}
        }
    }
}

#[tokio::test]
async fn host_defaults_follow_private_lan_connections_and_presented_trust_tokens() {
    use crate::NodeId;
    use crate::mesh_runtime_config::{
        DirectoryMode, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
    };
    use crate::peer_access::defaults::issue_trust_token;
    use crate::peer_access::{GrantSource, MeshAccessConfig, PolicyEffect, TrustScopeConfig};
    use tokio::sync::oneshot;

    let (unused, _calls) = mpsc::unbounded_channel();
    // Keep the idle connection open; the default swarm closes it at once.
    let mut client = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|_| {
            SharingWire::new(Arc::new(RecordingGate(unused)), Duration::from_secs(3))
        })
        .unwrap()
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
    let client_addr = listening(&mut client, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
    let client_peer = *client.local_peer_id();
    let authority = libp2p::identity::Keypair::generate_ed25519();
    let providers = PeerGrant::Custom {
        sessions: false,
        filesystem: false,
        shell: false,
        providers: vec![ProviderGrant {
            provider_id: "fixture".into(),
            models: ModelSelection::All,
        }],
    };
    let sessions = PeerGrant::Custom {
        sessions: true,
        filesystem: false,
        shell: false,
        providers: vec![],
    };
    let identity = tempfile::tempdir().unwrap();
    let runtime = crate::bootstrap_mesh_runtime_guarded(&MeshRuntimeConfig {
        enabled: true,
        lan: Some(LanMeshConfig {
            listen: Some("/ip4/127.0.0.1/tcp/0".into()),
            discovery: LanDiscovery::None,
            directory: DirectoryMode::Cached,
        }),
        iroh_enabled: false,
        iroh_gso: false,
        iroh_dns: None,
        iroh_scopes: vec![],
        identity_file: Some(identity.path().join("identity")),
        request_timeout: Duration::from_secs(3),
        stream_reconnect_grace: Duration::from_secs(3),
        node_name: None,
        peers: vec![
            client_addr
                .with(libp2p::multiaddr::Protocol::P2p(client_peer))
                .to_string(),
        ],
        auto_fallback: false,
        access: MeshAccessConfig {
            lan: providers.clone(),
            trust: vec![TrustScopeConfig {
                name: "eng".into(),
                authority: authority.public().to_peer_id().to_string(),
                access: sessions.clone(),
                token: None,
            }],
        },
    })
    .await
    .unwrap();
    let mesh = runtime.as_mesh_handle().clone();
    let host_peer = *mesh.peer_id();
    let store = mesh.quota_store().unwrap().clone();
    let key = PeerGrantKey {
        host: NodeId::from_peer_id(host_peer),
        peer: NodeId::from_peer_id(client_peer),
    };
    let mut changes = store.subscribe_peer_changes(&key).unwrap();
    let deadline = Duration::from_secs(5);

    // The host dials the client on loopback: a private-network LAN connection.
    tokio::time::timeout(deadline, async {
        loop {
            if let SwarmEvent::ConnectionEstablished { peer_id, .. } =
                client.select_next_some().await
                && peer_id == host_peer
            {
                break;
            }
        }
    })
    .await
    .expect("host connects to the client");
    let change = driving(&mut client, tokio::time::timeout(deadline, changes.recv()))
        .await
        .expect("LAN default publishes a policy change")
        .unwrap();
    assert!(matches!(&change.effect, PolicyEffect::Grant(grant) if *grant == providers));
    assert_eq!(store.grant(&key).await.unwrap().grant, providers);
    assert_eq!(
        store.grant_source(&key).await.unwrap(),
        GrantSource::Default {
            defaults: vec!["lan".into()]
        }
    );

    // Only the token issued to the authenticated sender counts.
    let token = issue_trust_token(&authority, "eng", &client_peer.to_string(), None).unwrap();
    let for_other =
        issue_trust_token(&authority, "eng", &PeerId::random().to_string(), None).unwrap();
    let (tx, rx) = oneshot::channel();
    client
        .behaviour_mut()
        .send(host_peer, SharingRequest::Trust(vec![for_other, token]), tx);
    let reply = driving(&mut client, tokio::time::timeout(deadline, rx))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(reply, SharingResponse::Trust(scopes) if scopes == ["eng"]));
    assert_eq!(
        store.grant(&key).await.unwrap().grant,
        providers.clone().union(sessions)
    );
    assert_eq!(
        store.grant_source(&key).await.unwrap(),
        GrantSource::Default {
            defaults: vec!["lan".into(), "trust:eng".into()]
        }
    );

    // Defaults end with the connection.
    drop(client);
    tokio::time::timeout(deadline, async {
        loop {
            let change = changes.recv().await.unwrap();
            if matches!(change.effect, PolicyEffect::Grant(PeerGrant::None)) {
                break;
            }
        }
    })
    .await
    .expect("disconnect revokes default access");
    assert_eq!(store.grant(&key).await.unwrap().grant, PeerGrant::None);
    mesh.shutdown().await;
}

struct TrustGate(mpsc::UnboundedSender<(PeerId, Vec<String>)>);

#[async_trait]
impl MeshIngress for TrustGate {
    async fn handle_legacy(&self, _: PeerId, request: SwarmRequest) -> SwarmResponse {
        Legacy::overloaded(request)
    }

    async fn present_trust(&self, peer: PeerId, tokens: Vec<String>) -> Vec<String> {
        self.0.send((peer, tokens)).unwrap();
        vec!["eng".into()]
    }
}

#[tokio::test]
async fn a_member_presents_its_trust_token_on_each_new_connection() {
    use crate::mesh_runtime_config::{
        DirectoryMode, LanDiscovery, LanMeshConfig, MeshRuntimeConfig,
    };
    use crate::peer_access::defaults::issue_trust_token;
    use crate::peer_access::{MeshAccessConfig, TrustScopeConfig};

    let (presented_tx, mut presented) = mpsc::unbounded_channel();
    let mut host = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .unwrap()
        .with_behaviour(|_| {
            SharingWire::new(Arc::new(TrustGate(presented_tx)), Duration::from_secs(3))
        })
        .unwrap()
        .with_swarm_config(|config| config.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
    let host_addr = listening(&mut host, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).await;
    let host_peer = *host.local_peer_id();

    let identity = tempfile::tempdir().unwrap();
    let keypair =
        crate::identity::load_or_generate_keypair(Some(&identity.path().join("identity"))).unwrap();
    let member = keypair.public().to_peer_id();
    let authority = libp2p::identity::Keypair::generate_ed25519();
    let token = issue_trust_token(&authority, "eng", &member.to_string(), Some(3600)).unwrap();
    let runtime = crate::bootstrap_mesh_runtime_guarded(&MeshRuntimeConfig {
        enabled: true,
        lan: Some(LanMeshConfig {
            listen: Some("/ip4/127.0.0.1/tcp/0".into()),
            discovery: LanDiscovery::None,
            directory: DirectoryMode::Cached,
        }),
        iroh_enabled: false,
        iroh_gso: false,
        iroh_dns: None,
        iroh_scopes: vec![],
        identity_file: Some(identity.path().join("identity")),
        request_timeout: Duration::from_secs(3),
        stream_reconnect_grace: Duration::from_secs(3),
        node_name: None,
        peers: vec![
            host_addr
                .with(libp2p::multiaddr::Protocol::P2p(host_peer))
                .to_string(),
        ],
        auto_fallback: false,
        access: MeshAccessConfig {
            lan: PeerGrant::None,
            trust: vec![TrustScopeConfig {
                name: "eng".into(),
                authority: authority.public().to_peer_id().to_string(),
                access: PeerGrant::None,
                token: Some(token.clone()),
            }],
        },
    })
    .await
    .unwrap();
    let mesh = runtime.as_mesh_handle().clone();
    assert_eq!(*mesh.peer_id(), member);

    let (peer, tokens) = driving(
        &mut host,
        tokio::time::timeout(Duration::from_secs(5), presented.recv()),
    )
    .await
    .expect("the member presents its token")
    .unwrap();
    assert_eq!(peer, member);
    assert_eq!(tokens, [token]);
    mesh.shutdown().await;
}
