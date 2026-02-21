use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::broadcast;
use tokio::task;
use tokio::task::JoinSet;

const EVENT_BUS_BUFFER: usize = 1024;

/// Unified event bus for broadcasting agent events.
pub struct EventBus {
    sender: broadcast::Sender<AgentEvent>,
    observers: Arc<Mutex<Vec<Arc<dyn EventObserver>>>>,
    sequence: AtomicU64,
    observer_tasks: Arc<TokioMutex<JoinSet<()>>>,
}

impl EventBus {
    /// Create a new event bus with a bounded broadcast channel.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_BUS_BUFFER);
        Self {
            sender,
            observers: Arc::new(Mutex::new(Vec::new())),
            sequence: AtomicU64::new(1),
            observer_tasks: Arc::new(TokioMutex::new(JoinSet::new())),
        }
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.sender.subscribe()
    }

    /// Register an event observer.
    pub fn add_observer(&self, observer: Arc<dyn EventObserver>) {
        self.observers.lock().push(observer);
    }

    /// Register multiple observers.
    pub fn add_observers(&self, observers: Vec<Arc<dyn EventObserver>>) {
        self.observers.lock().extend(observers);
    }

    /// Return the number of currently registered observers.
    pub fn observer_count(&self) -> usize {
        self.observers.lock().len()
    }

    /// Publish an event to all subscribers and observers.
    pub fn publish(&self, session_id: &str, kind: AgentEventKind) {
        let event = self.build_event(session_id, kind);
        let _ = self.sender.send(event.clone());

        let observers = { self.observers.lock().clone() };

        // Spawn observer tasks and track them for cleanup
        let tasks = self.observer_tasks.clone();
        task::spawn(async move {
            let mut tasks_guard = tasks.lock().await;
            for observer in observers {
                let observer = Arc::clone(&observer);
                let event = event.clone();
                tasks_guard.spawn(async move {
                    let _ = observer.on_event(&event).await;
                });
            }
        });
    }

    /// Shutdown the event bus and abort all pending observer tasks.
    pub async fn shutdown(&self) {
        log::debug!("EventBus: Shutting down and aborting all observer tasks");
        let mut tasks = self.observer_tasks.lock().await;
        tasks.shutdown().await;
        log::debug!("EventBus: All observer tasks aborted");
    }

    fn build_event(&self, session_id: &str, kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq: self.sequence.fetch_add(1, Ordering::Relaxed),
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            session_id: session_id.to_string(),
            kind,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::AgentEventKind;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    // Mock observer for testing
    struct MockObserver {
        received_events: Arc<TokioMutex<Vec<AgentEvent>>>,
    }

    impl MockObserver {
        fn new() -> Self {
            Self {
                received_events: Arc::new(TokioMutex::new(Vec::new())),
            }
        }

        async fn get_received_events(&self) -> Vec<AgentEvent> {
            self.received_events.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl EventObserver for MockObserver {
        async fn on_event(&self, event: &AgentEvent) -> Result<(), querymt::error::LLMError> {
            self.received_events.lock().await.push(event.clone());
            Ok(())
        }
    }

    // ── Basic functionality ────────────────────────────────────────────────

    #[tokio::test]
    async fn new_creates_working_bus() {
        let bus = EventBus::new();
        assert_eq!(bus.observer_count(), 0);
    }

    #[tokio::test]
    async fn subscribe_receives_published_events() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish("sess-1", AgentEventKind::SessionCreated);

        let event = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("event received");

        assert_eq!(event.session_id, "sess-1");
        assert!(matches!(event.kind, AgentEventKind::SessionCreated));
    }

    #[tokio::test]
    async fn add_observer_gets_notified() {
        let bus = EventBus::new();
        let observer = Arc::new(MockObserver::new());

        bus.add_observer(observer.clone());
        assert_eq!(bus.observer_count(), 1);

        bus.publish("sess-test", AgentEventKind::Cancelled);

        // Give observer time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let received = observer.get_received_events().await;
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].session_id, "sess-test");
        assert!(matches!(received[0].kind, AgentEventKind::Cancelled));
    }

    #[tokio::test]
    async fn add_observers_bulk_registration() {
        let bus = EventBus::new();
        let observer1 = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;
        let observer2 = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;

        bus.add_observers(vec![observer1, observer2]);
        assert_eq!(bus.observer_count(), 2);
    }

    #[tokio::test]
    async fn observer_count_accuracy() {
        let bus = EventBus::new();
        assert_eq!(bus.observer_count(), 0);

        let observer1 = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;
        bus.add_observer(observer1);
        assert_eq!(bus.observer_count(), 1);

        let observer2 = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;
        let observer3 = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;
        bus.add_observers(vec![observer2, observer3]);
        assert_eq!(bus.observer_count(), 3);
    }

    #[tokio::test]
    async fn sequence_numbers_increment_monotonically() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.publish("sess-1", AgentEventKind::SessionCreated);
        bus.publish("sess-1", AgentEventKind::Cancelled);
        bus.publish("sess-1", AgentEventKind::SessionCreated);

        let event1 = rx.recv().await.unwrap();
        let event2 = rx.recv().await.unwrap();
        let event3 = rx.recv().await.unwrap();

        assert_eq!(event1.seq, 1);
        assert_eq!(event2.seq, 2);
        assert_eq!(event3.seq, 3);
    }

    #[tokio::test]
    async fn publish_to_bus_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new();
        // No subscribers or observers registered
        bus.publish(
            "sess-no-sub",
            AgentEventKind::Error {
                message: "test".to_string(),
            },
        );
        // Should not panic
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive_events() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish("sess-multi", AgentEventKind::SessionCreated);

        let event1 = rx1.recv().await.unwrap();
        let event2 = rx2.recv().await.unwrap();

        assert_eq!(event1.seq, event2.seq);
        assert_eq!(event1.session_id, "sess-multi");
        assert_eq!(event2.session_id, "sess-multi");
    }

    #[tokio::test]
    async fn shutdown_completes_without_error() {
        let bus = EventBus::new();
        let observer = Arc::new(MockObserver::new()) as Arc<dyn EventObserver>;
        bus.add_observer(observer);

        bus.publish("sess-shutdown", AgentEventKind::SessionCreated);
        bus.shutdown().await;
        // Should complete without hanging or panicking
    }

    #[tokio::test]
    async fn default_creates_same_as_new() {
        let bus1 = EventBus::new();
        let bus2 = EventBus::default();

        assert_eq!(bus1.observer_count(), bus2.observer_count());
    }
}
