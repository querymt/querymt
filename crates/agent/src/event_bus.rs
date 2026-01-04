use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::task;
use tokio::task::JoinSet;

const EVENT_BUS_BUFFER: usize = 1024;

/// Unified event bus for broadcasting agent events.
pub struct EventBus {
    sender: broadcast::Sender<AgentEvent>,
    observers: Arc<StdMutex<Vec<Arc<dyn EventObserver>>>>,
    sequence: AtomicU64,
    observer_tasks: Arc<Mutex<JoinSet<()>>>,
}

impl EventBus {
    /// Create a new event bus with a bounded broadcast channel.
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(EVENT_BUS_BUFFER);
        Self {
            sender,
            observers: Arc::new(StdMutex::new(Vec::new())),
            sequence: AtomicU64::new(1),
            observer_tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    /// Subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.sender.subscribe()
    }

    /// Register an event observer.
    pub fn add_observer(&self, observer: Arc<dyn EventObserver>) {
        if let Ok(mut observers) = self.observers.lock() {
            observers.push(observer);
        }
    }

    /// Register multiple observers.
    pub fn add_observers(&self, observers: Vec<Arc<dyn EventObserver>>) {
        if let Ok(mut list) = self.observers.lock() {
            list.extend(observers);
        }
    }

    /// Publish an event to all subscribers and observers.
    pub fn publish(&self, session_id: &str, kind: AgentEventKind) {
        let event = self.build_event(session_id, kind);
        let _ = self.sender.send(event.clone());

        let observers = {
            if let Ok(list) = self.observers.lock() {
                list.clone()
            } else {
                Vec::new()
            }
        };

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
