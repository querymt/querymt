//! Progress and task-status routing for MCP tool calls.
//!
//! A tool call reports progress through a per-call [`ProgressSink`] installed
//! in the [`CURRENT_PROGRESS`] task-local, so the [`CallFunctionTool`] trait
//! stays unchanged. Server notifications reach the call through a
//! [`McpNotificationRouter`], which the owner of the connection's client
//! handler feeds from `on_progress` / `on_task_status`.
//!
//! [`CallFunctionTool`]: crate::tool_decorator::CallFunctionTool

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use rmcp::model::{ProgressNotificationParam, ProgressToken, TaskStatusNotificationParams};
use tokio::sync::{Notify, mpsc};

/// One progress report of an in-flight tool call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressUpdate {
    pub progress: Option<f64>,
    pub total: Option<f64>,
    pub message: Option<String>,
}

impl From<ProgressNotificationParam> for ProgressUpdate {
    fn from(p: ProgressNotificationParam) -> Self {
        Self {
            progress: Some(p.progress),
            total: p.total,
            message: p.message,
        }
    }
}

/// Receiver of [`ProgressUpdate`]s for the current tool call.
pub trait ProgressSink: Send + Sync {
    fn report(&self, update: ProgressUpdate);
}

tokio::task_local! {
    /// Sink for the tool call running in this task, if any. Progress is only
    /// forwarded when this is set and the adapter has a router.
    pub static CURRENT_PROGRESS: Arc<dyn ProgressSink>;
}

/// Notifications that arrived for a token nobody subscribed to (yet).
///
/// rmcp picks the progress token when it sends a request, so a subscriber can
/// only register after the request is on the wire. Notifications racing that
/// window are kept here, bounded, and handed over on subscribe.
const EARLY_BUFFER: usize = 64;

#[derive(Default)]
struct ProgressState {
    subscribers: HashMap<ProgressToken, mpsc::UnboundedSender<ProgressUpdate>>,
    early: VecDeque<ProgressNotificationParam>,
}

/// Routes server notifications to in-flight calls. Cheap to clone.
#[derive(Clone, Default)]
pub struct McpNotificationRouter {
    inner: Arc<RouterInner>,
}

#[derive(Default)]
struct RouterInner {
    progress: Mutex<ProgressState>,
    tasks: Mutex<HashMap<String, Arc<Notify>>>,
}

impl std::fmt::Debug for McpNotificationRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpNotificationRouter")
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl McpNotificationRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a `notifications/progress`. Delivered to the token's subscriber,
    /// or kept briefly for a subscriber that has not registered yet.
    pub fn handle_progress(&self, params: ProgressNotificationParam) {
        let mut state = lock(&self.inner.progress);
        if let Some(tx) = state.subscribers.get(&params.progress_token) {
            if tx.send(params.clone().into()).is_err() {
                state.subscribers.remove(&params.progress_token);
            }
            return;
        }
        if state.early.len() == EARLY_BUFFER {
            state.early.pop_front();
        }
        state.early.push_back(params);
    }

    /// Subscribe to a token. Dropping the subscription unsubscribes.
    pub fn subscribe_progress(&self, token: ProgressToken) -> ProgressSubscription {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = lock(&self.inner.progress);
        let (mine, rest): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut state.early)
            .into_iter()
            .partition(|p| p.progress_token == token);
        state.early = rest;
        for early in mine {
            let _ = tx.send(early.into());
        }
        state.subscribers.insert(token.clone(), tx);
        ProgressSubscription {
            router: self.clone(),
            token,
            rx,
        }
    }

    /// Feed a `notifications/tasks`; wakes the poller registered for the task.
    ///
    /// A push that arrives before the poller registered (racing the
    /// `CreateTaskResult`) is dropped; the poll interval covers it.
    pub fn handle_task_status(&self, params: &TaskStatusNotificationParams) {
        let waiter = lock(&self.inner.tasks)
            .get(&params.task.task.task_id)
            .map(Arc::clone);
        if let Some(notify) = waiter {
            // `notify_one` stores a permit, so a push that lands between two
            // polls is not lost.
            notify.notify_one();
        }
    }

    /// Register interest in status pushes for `task_id` until the waker drops.
    pub fn register_task(&self, task_id: impl Into<String>) -> TaskWaker {
        let id = task_id.into();
        let notify = Arc::new(Notify::new());
        lock(&self.inner.tasks).insert(id.clone(), Arc::clone(&notify));
        TaskWaker {
            router: self.clone(),
            id,
            notify,
        }
    }
}

/// Progress of one call; unsubscribes on drop.
pub struct ProgressSubscription {
    router: McpNotificationRouter,
    token: ProgressToken,
    rx: mpsc::UnboundedReceiver<ProgressUpdate>,
}

impl ProgressSubscription {
    /// Next update; pending until one arrives.
    pub async fn recv(&mut self) -> Option<ProgressUpdate> {
        self.rx.recv().await
    }

    /// An update that is already queued, if any.
    pub fn try_recv(&mut self) -> Option<ProgressUpdate> {
        self.rx.try_recv().ok()
    }
}

impl Drop for ProgressSubscription {
    fn drop(&mut self) {
        lock(&self.router.inner.progress)
            .subscribers
            .remove(&self.token);
    }
}

/// Registration of a task poller; unregisters on drop.
pub struct TaskWaker {
    router: McpNotificationRouter,
    id: String,
    notify: Arc<Notify>,
}

impl TaskWaker {
    /// Resolves when a status notification for the task arrives.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

impl Drop for TaskWaker {
    fn drop(&mut self) {
        lock(&self.router.inner.tasks).remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{DetailedTask, NumberOrString, Task, TaskPayload, TaskStatus};
    use std::time::Duration;

    fn token(s: &str) -> ProgressToken {
        ProgressToken(NumberOrString::String(s.into()))
    }

    fn status(id: &str) -> TaskStatusNotificationParams {
        TaskStatusNotificationParams::new(DetailedTask::new(
            Task::new(
                id,
                TaskStatus::Working,
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            ),
            TaskPayload::Working,
        ))
    }

    #[tokio::test]
    async fn progress_reaches_only_the_subscribed_token() {
        let router = McpNotificationRouter::new();
        let mut sub = router.subscribe_progress(token("a"));
        router.handle_progress(ProgressNotificationParam::new(token("b"), 1.0));
        router.handle_progress(ProgressNotificationParam::new(token("a"), 2.0));
        assert_eq!(sub.recv().await.unwrap().progress, Some(2.0));
        assert!(sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn early_notifications_are_handed_to_a_late_subscriber() {
        let router = McpNotificationRouter::new();
        router.handle_progress(ProgressNotificationParam::new(token("a"), 1.0));
        router.handle_progress(ProgressNotificationParam::new(token("b"), 7.0));
        let mut sub = router.subscribe_progress(token("a"));
        assert_eq!(sub.try_recv().unwrap().progress, Some(1.0));
        assert!(sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn early_buffer_is_bounded() {
        let router = McpNotificationRouter::new();
        for i in 0..(EARLY_BUFFER + 10) {
            router.handle_progress(ProgressNotificationParam::new(token("a"), i as f64));
        }
        let mut sub = router.subscribe_progress(token("a"));
        let mut n = 0;
        while sub.try_recv().is_some() {
            n += 1;
        }
        assert_eq!(n, EARLY_BUFFER);
    }

    #[tokio::test]
    async fn dropped_subscription_stops_delivery() {
        let router = McpNotificationRouter::new();
        let sub = router.subscribe_progress(token("a"));
        drop(sub);
        router.handle_progress(ProgressNotificationParam::new(token("a"), 1.0));
        assert!(lock(&router.inner.progress).subscribers.is_empty());
    }

    #[tokio::test]
    async fn task_status_wakes_registered_waker_and_unregisters_on_drop() {
        let router = McpNotificationRouter::new();
        let waker = router.register_task("t1");
        router.handle_task_status(&status("t1"));
        tokio::time::timeout(Duration::from_secs(1), waker.notified())
            .await
            .expect("woken by status push");
        drop(waker);
        assert!(lock(&router.inner.tasks).is_empty());
        // No registered poller: ignored, no panic.
        router.handle_task_status(&status("t1"));
    }

    #[tokio::test]
    async fn status_push_for_another_task_does_not_wake() {
        let router = McpNotificationRouter::new();
        let waker = router.register_task("t1");
        router.handle_task_status(&status("t2"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), waker.notified())
                .await
                .is_err()
        );
    }
}
