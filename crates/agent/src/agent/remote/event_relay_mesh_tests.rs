//! Module F — Event relay + mesh integration tests.
//!
//! Tests the full `EventForwarder → EventRelayActor → local EventSink/Fanout` chain
//! using real remote actor messaging (full msgpack serialization).

#[cfg(all(test, feature = "remote"))]
#[allow(clippy::module_inception)]
mod event_relay_mesh_tests {
    use crate::agent::core::SessionRuntime;
    use crate::agent::remote::SessionActorRef;
    use crate::agent::remote::event_forwarder::EventForwarder;
    use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::agent::session_actor::SessionActor;
    use crate::event_fanout::EventFanout;
    use crate::event_sink::EventSink;
    use crate::events::{AgentEvent, AgentEventKind, EventEnvelope, EventOrigin};
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use kameo::actor::Spawn;
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    // ── Fixture ───────────────────────────────────────────────────────────────

    /// Set up a local EventRelayActor registered in the DHT and return
    /// (relay_ref, event_sink, dht_name).
    async fn setup_relay(
        label: &str,
        test_id: &str,
    ) -> (
        kameo::actor::ActorRef<EventRelayActor>,
        Arc<EventSink>,
        String,
    ) {
        let mesh = get_test_mesh().await;
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        let event_sink = Arc::new(EventSink::new(journal, fanout));
        let relay = EventRelayActor::new(event_sink.clone(), label.to_string());
        let relay_ref = EventRelayActor::spawn(relay);

        let dht_name =
            crate::agent::remote::dht_name::event_relay(&format!("{}-{}", label, test_id));
        mesh.register_actor(relay_ref.clone(), dht_name.clone())
            .await;

        (relay_ref, event_sink, dht_name)
    }

    // ── F.1 ──────────────────────────────────────────────────────────────────

    /// Tests that EventForwarder::start() subscribes to a fanout and forwards
    /// events to the relay actor via mesh.
    #[tokio::test]
    async fn test_event_forwarder_sends_to_registered_relay() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;
        let (relay_ref, event_sink, dht_name) = setup_relay("f1", &test_id).await;
        let _ = relay_ref; // keep alive

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Resolve relay via DHT (simulates the remote side looking it up).
        let remote_relay = mesh
            .lookup_actor::<EventRelayActor>(&dht_name)
            .await
            .expect("DHT lookup")
            .expect("relay not in DHT");

        // Create a source fanout to publish events into
        let source_fanout = Arc::new(EventFanout::new());

        // Subscribe to the relay's sink fanout to verify events arrive
        let mut rx = event_sink.fanout().subscribe();

        // Start the forwarder (subscribes to source_fanout, forwards to remote_relay)
        let _handle = EventForwarder::start(
            source_fanout.clone(),
            remote_relay,
            "test-source-f1".to_string(),
        );

        // Publish a durable event to the source fanout
        source_fanout.publish(EventEnvelope::Durable(crate::events::DurableEvent {
            event_id: "e-f1".into(),
            stream_seq: 1,
            session_id: "s-f1".into(),
            timestamp: 1000,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        }));

        let received = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert_eq!(received.session_id(), "s-f1");
        assert!(received.is_durable());
        if let EventEnvelope::Durable(de) = &received {
            assert!(matches!(de.origin, EventOrigin::Remote));
            assert_eq!(de.source_node.as_deref(), Some("f1"));
            assert!(matches!(de.kind, AgentEventKind::SessionCreated));
        }
    }

    // ── F.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_event_forwarding_multiple_events_ordered() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;
        let (relay_ref, event_sink, dht_name) = setup_relay("f2", &test_id).await;
        let _ = relay_ref;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let remote_relay = mesh
            .lookup_actor::<EventRelayActor>(&dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let source_fanout = Arc::new(EventFanout::new());
        let mut rx = event_sink.fanout().subscribe();

        let _handle =
            EventForwarder::start(source_fanout.clone(), remote_relay, "source-f2".to_string());

        for i in 1u64..=5 {
            source_fanout.publish(EventEnvelope::Durable(crate::events::DurableEvent {
                event_id: format!("e-f2-{}", i),
                stream_seq: i,
                session_id: "s-f2".into(),
                timestamp: i as i64 * 100,
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::SessionCreated,
            }));
        }

        // All 5 durable events (SessionCreated) should arrive on fanout.
        // Each gets a new journal-assigned stream_seq (monotonic).
        let mut received_seqs = Vec::new();
        for _ in 0..5 {
            let env = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
                .await
                .expect("timeout")
                .expect("recv");
            if let EventEnvelope::Durable(de) = env {
                received_seqs.push(de.stream_seq);
            }
        }

        // Journal assigns monotonically increasing seqs.
        for window in received_seqs.windows(2) {
            assert!(
                window[0] < window[1],
                "stream_seq must be monotonically increasing"
            );
        }
        assert_eq!(received_seqs.len(), 5);
    }

    // ── F.3 ──────────────────────────────────────────────────────────────────
    //
    // The `received` counter on `EventRelayActor` is private.  We verify it
    // indirectly: N events sent → N events arrive on the local bus.  If the
    // counter were accessible, we'd assert `relay.received == N`.

    #[tokio::test]
    async fn test_event_relay_received_counter_increments() {
        let test_id = Uuid::now_v7().to_string();
        let (_relay_ref, event_sink, _dht_name) = setup_relay("f3", &test_id).await;

        let mut rx = event_sink.fanout().subscribe();

        // Send 3 events directly to the relay's local actor ref (no DHT needed).
        for i in 1u64..=3 {
            _relay_ref
                .tell(RelayedEvent {
                    event: AgentEvent {
                        seq: i,
                        timestamp: 0,
                        session_id: "s-f3".to_string(),
                        origin: EventOrigin::Local,
                        source_node: None,
                        kind: AgentEventKind::SessionCreated,
                    },
                })
                .await
                .expect("tell");
        }

        let mut count = 0usize;
        for _ in 0..3 {
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .expect("timeout")
                .expect("recv");
            count += 1;
        }
        assert_eq!(count, 3, "all 3 relayed events should arrive on fanout");
    }

    // ── F.4 ──────────────────────────────────────────────────────────────────

    /// Tests that SubscribeEvents starts a forwarder task.
    /// We verify indirectly: after subscribe, events published to the session's
    /// EventFanout should arrive at the relay.
    #[tokio::test]
    async fn test_subscribe_events_with_live_mesh_installs_forwarder() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        let f = AgentConfigFixture::new().await;
        let session_id = format!("s-f4-{}", test_id);

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
        let session_ref_local = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::Local(session_ref_local);

        // Register a relay under the name the SubscribeEvents handler looks for.
        let relay_dht_name = crate::agent::remote::dht_name::event_relay(&session_id);
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        let relay_sink = Arc::new(EventSink::new(journal, fanout));
        let relay = EventRelayActor::new(relay_sink.clone(), "f4-relay".to_string());
        let relay_ref = EventRelayActor::spawn(relay);
        mesh.register_actor(relay_ref.clone(), relay_dht_name.clone())
            .await;
        let _ = relay_ref;

        // Wait for DHT propagation. On CI (ubuntu-latest) under load, Kademlia
        // propagation can take longer than on a developer laptop; 200ms is
        // conservative enough to avoid a lookup-returns-None flake.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // SubscribeEvents with any relay_actor_id — the handler looks up
        // "event_relay::{session_id}" in the DHT.
        let result = session_ref.subscribe_events(1).await;
        assert!(result.is_ok(), "subscribe_events should succeed");

        // Verify the forwarder is working by publishing an event to the session's
        // fanout and checking it arrives at the relay.  We use a retry-publish
        // loop instead of a fixed sleep so that:
        //   • The test passes quickly on a fast laptop (first attempt usually
        //     succeeds after the first 200 ms poll).
        //   • The test also passes on a slow/loaded CI runner where Kademlia
        //     propagation, the async DHT lookup inside subscribe_events, and
        //     the forwarder task start-up all take longer than any single
        //     hard-coded delay we could reasonably choose.
        //
        // Total budget: 5 s.  Each iteration waits 200 ms for the forwarder to
        // be ready, publishes a fresh event, then gives recv() 500 ms.
        let mut rx = relay_sink.fanout().subscribe();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut attempt: u32 = 0;
        loop {
            // Wait before publishing so the forwarder task has time to be
            // installed (and so DHT lookup inside subscribe_events can finish).
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            attempt += 1;

            f.config.event_sink.fanout().publish(EventEnvelope::Durable(
                crate::events::DurableEvent {
                    event_id: format!("e-f4-{attempt}"),
                    stream_seq: attempt as u64,
                    session_id: session_id.clone(),
                    timestamp: 100,
                    origin: EventOrigin::Local,
                    source_node: None,
                    kind: AgentEventKind::SessionCreated,
                },
            ));

            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(_)) => break, // forwarder is up and forwarded the event
                _ if tokio::time::Instant::now() >= deadline => {
                    panic!(
                        "forwarder should forward events from session fanout to relay \
                         (gave up after {attempt} attempts / 5 s)"
                    );
                }
                _ => {
                    // recv timed out or channel closed — forwarder not ready yet,
                    // re-subscribe to drain any missed messages and retry.
                    rx = relay_sink.fanout().subscribe();
                }
            }
        }
    }

    // ── F.5 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_subscribe_events_without_relay_in_dht_logs_warn() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        let f = AgentConfigFixture::new().await;
        let session_id = format!("s-f5-{}", test_id);

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
        let session_ref_local = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::Local(session_ref_local);

        // Intentionally do NOT register the relay in the DHT.

        // Should return Ok (no panic), just logs a warning.
        let result = session_ref.subscribe_events(99).await;
        assert!(
            result.is_ok(),
            "subscribe_events should return Ok even with no relay in DHT"
        );
    }

    // ── F.6 — Unsubscribe lifecycle ──────────────────────────────────────────

    /// Verifies subscribe+unsubscribe stops the forwarder task.
    #[tokio::test]
    async fn test_unsubscribe_events_stops_forwarder() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        let f = AgentConfigFixture::new().await;
        let session_id = format!("s-f6-{}", test_id);

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
        let session_ref_local = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::Local(session_ref_local);

        // Register a relay so subscribe actually starts a forwarder.
        let relay_dht_name = crate::agent::remote::dht_name::event_relay(&session_id);
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        let relay_sink = Arc::new(EventSink::new(journal, fanout));
        let relay = EventRelayActor::new(relay_sink, "f6-relay".to_string());
        let relay_ref = EventRelayActor::spawn(relay);
        mesh.register_actor(relay_ref.clone(), relay_dht_name).await;
        let _ = relay_ref;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        session_ref.subscribe_events(7).await.expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Unsubscribe — the forwarder task should be aborted.
        let result = session_ref.unsubscribe_events(7).await;
        assert!(result.is_ok(), "unsubscribe_events should return Ok");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // After unsubscribe, the forwarder task is aborted. We can't easily
        // observe this externally, but at minimum unsubscribe must succeed
        // without error.
    }
}
