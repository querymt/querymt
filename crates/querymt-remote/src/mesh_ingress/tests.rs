use super::*;
use crate::mesh_runtime::{guarded_test_behaviour, messaging_config};
use futures_util::StreamExt;
use kameo::actor::ActorId;
use libp2p::swarm::SwarmEvent;
use tokio::sync::mpsc;

struct RecordingGate(mpsc::UnboundedSender<PeerId>);

#[async_trait]
impl MeshIngress for RecordingGate {
    async fn handle_legacy(&self, peer: PeerId, request: SwarmRequest) -> SwarmResponse {
        self.0.send(peer).unwrap();
        overloaded(request)
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
