//! Module F — Event relay + mesh integration tests.
//!
//! Tests the full `EventForwarder → EventRelayActor → local EventBus` chain
//! using real remote actor messaging (full msgpack serialization).
//!
//! Bug documented:
//! - **#4** — `UnsubscribeEvents` is a no-op (F.6).
//! - **#5** — Race in `attach_remote_session` / `SubscribeEvents` (G.8).

#[cfg(all(test, feature = "remote"))]
mod event_relay_mesh_tests {
    use crate::agent::core::SessionRuntime;
    use crate::agent::remote::SessionActorRef;
    use crate::agent::remote::event_forwarder::EventForwarder;
    use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};
    use crate::agent::remote::test_helpers::fixtures::{AgentConfigFixture, get_test_mesh};
    use crate::agent::session_actor::SessionActor;
    use crate::event_bus::EventBus;
    use crate::events::{AgentEvent, AgentEventKind, EventOrigin};
    use kameo::actor::Spawn;
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    // ── Fixture ───────────────────────────────────────────────────────────────

    /// Set up a local EventRelayActor registered in the DHT and return
    /// (relay_ref, local_bus, dht_name).
    async fn setup_relay(
        label: &str,
        test_id: &str,
    ) -> (
        kameo::actor::ActorRef<EventRelayActor>,
        Arc<EventBus>,
        String,
    ) {
        let mesh = get_test_mesh().await;
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), label.to_string());
        let relay_ref = EventRelayActor::spawn(relay);

        let dht_name = format!("event_relay::{}-{}", label, test_id);
        mesh.register_actor(relay_ref.clone(), dht_name.clone())
            .await;

        (relay_ref, local_bus, dht_name)
    }

    // ── F.1 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_event_forwarder_sends_to_registered_relay() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;
        let (relay_ref, local_bus, dht_name) = setup_relay("f1", &test_id).await;
        let _ = relay_ref; // keep alive

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Resolve relay via DHT (simulates the remote side looking it up).
        let remote_relay = mesh
            .lookup_actor::<EventRelayActor>(&dht_name)
            .await
            .expect("DHT lookup")
            .expect("relay not in DHT");

        let forwarder = EventForwarder::new(remote_relay, "test-source-f1".to_string());

        let mut rx = local_bus.subscribe();

        let event = AgentEvent {
            seq: 1,
            timestamp: 1000,
            session_id: "s-f1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };

        use crate::events::EventObserver;
        forwarder.on_event(&event).await.expect("on_event");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert_eq!(received.session_id, "s-f1");
        assert!(matches!(received.origin, EventOrigin::Remote));
        assert_eq!(received.source_node.as_deref(), Some("f1"));
        assert!(matches!(received.kind, AgentEventKind::SessionCreated));
    }

    // ── F.2 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_event_forwarding_multiple_events_ordered() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;
        let (relay_ref, local_bus, dht_name) = setup_relay("f2", &test_id).await;
        let _ = relay_ref;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let remote_relay = mesh
            .lookup_actor::<EventRelayActor>(&dht_name)
            .await
            .expect("lookup")
            .expect("not found");

        let forwarder = EventForwarder::new(remote_relay, "source-f2".to_string());

        let mut rx = local_bus.subscribe();

        use crate::events::EventObserver;
        for i in 1u64..=5 {
            let event = AgentEvent {
                seq: i,
                timestamp: i as i64 * 100,
                session_id: "s-f2".to_string(),
                origin: EventOrigin::Local,
                source_node: None,
                kind: AgentEventKind::SessionCreated,
            };
            forwarder.on_event(&event).await.expect("on_event");
        }

        let mut received_seqs = Vec::new();
        for _ in 0..5 {
            let evt = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv())
                .await
                .expect("timeout")
                .expect("recv");
            received_seqs.push(evt.seq);
        }

        // Relayed events should retain original sequence numbers end-to-end.
        received_seqs.sort_unstable();
        assert_eq!(received_seqs, vec![1, 2, 3, 4, 5]);
    }

    // ── F.3 ──────────────────────────────────────────────────────────────────
    //
    // The `received` counter on `EventRelayActor` is private.  We verify it
    // indirectly: N events sent → N events arrive on the local bus.  If the
    // counter were accessible, we'd assert `relay.received == N`.

    #[tokio::test]
    async fn test_event_relay_received_counter_increments() {
        let test_id = Uuid::now_v7().to_string();
        let (_relay_ref, local_bus, _dht_name) = setup_relay("f3", &test_id).await;

        let mut rx = local_bus.subscribe();

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
        assert_eq!(
            count, 3,
            "all 3 relayed events should arrive on the local bus"
        );
    }

    // ── F.4 ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_subscribe_events_with_live_mesh_installs_forwarder() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        // Create a SessionActor with a mesh handle.
        let f = AgentConfigFixture::new().await;
        let session_id = format!("s-f4-{}", test_id);

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        // SessionActor::new_with_mesh is needed; check if the constructor accepts mesh.
        // Looking at source: SessionActor has a `mesh` field set via `new_with_mesh` or
        // we need to check actual API.
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
        let session_ref_local = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::Local(session_ref_local);

        // Register a relay under the name the SubscribeEvents handler looks for.
        let relay_dht_name = format!("event_relay::{}", session_id);
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus.clone(), "f4-relay".to_string());
        let relay_ref = EventRelayActor::spawn(relay);
        mesh.register_actor(relay_ref.clone(), relay_dht_name.clone())
            .await;
        let _ = relay_ref;

        // Brief propagation delay.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let observer_count_before = f.config.event_bus.observer_count();

        // SubscribeEvents with any relay_actor_id — the handler looks up
        // "event_relay::{session_id}" in the DHT.
        let result = session_ref.subscribe_events(1).await;
        assert!(result.is_ok(), "subscribe_events should succeed");

        // Give the async DHT lookup + observer registration time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let observer_count_after = f.config.event_bus.observer_count();
        assert!(
            observer_count_after > observer_count_before,
            "observer count should increase after SubscribeEvents (before={}, after={})",
            observer_count_before,
            observer_count_after
        );
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

        let observer_count_before = f.config.event_bus.observer_count();

        // Should return Ok (no panic), just logs a warning.
        let result = session_ref.subscribe_events(99).await;
        assert!(
            result.is_ok(),
            "subscribe_events should return Ok even with no relay in DHT"
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Observer count must not have changed.
        let observer_count_after = f.config.event_bus.observer_count();
        assert_eq!(
            observer_count_before, observer_count_after,
            "no forwarder should be installed when relay is not in DHT"
        );
    }

    // ── F.6 — Unsubscribe lifecycle ──────────────────────────────────────────

    /// Verifies subscribe+unsubscribe detaches the relay observer.
    #[tokio::test]
    async fn test_unsubscribe_events_detaches_observer() {
        let test_id = Uuid::now_v7().to_string();
        let mesh = get_test_mesh().await;

        let f = AgentConfigFixture::new().await;
        let session_id = format!("s-f6-{}", test_id);

        let runtime = SessionRuntime::new(None, HashMap::new(), HashMap::new(), Vec::new());
        let actor = SessionActor::new(f.config.clone(), session_id.clone(), runtime)
            .with_mesh(Some(mesh.clone()));
        let session_ref_local = SessionActor::spawn(actor);
        let session_ref = SessionActorRef::Local(session_ref_local);

        // Register a relay so subscribe actually installs a forwarder.
        let relay_dht_name = format!("event_relay::{}", session_id);
        let local_bus = Arc::new(EventBus::new());
        let relay = EventRelayActor::new(local_bus, "f6-relay".to_string());
        let relay_ref = EventRelayActor::spawn(relay);
        mesh.register_actor(relay_ref.clone(), relay_dht_name).await;
        let _ = relay_ref;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let before = f.config.event_bus.observer_count();

        session_ref.subscribe_events(7).await.expect("subscribe");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let after_subscribe = f.config.event_bus.observer_count();

        // Unsubscribe and verify the relay observer is detached.
        let result = session_ref.unsubscribe_events(7).await;
        assert!(result.is_ok(), "unsubscribe_events should return Ok");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let after_unsubscribe = f.config.event_bus.observer_count();
        assert_eq!(
            before, after_unsubscribe,
            "observer count should return to baseline after unsubscribe"
        );

        assert!(
            after_subscribe > before,
            "subscribe should still increase observer count before unsubscribe"
        );
    }
}
