//! Per-session ordered event stream.
//!
//! One [`SessionEventStream`] exists per session and is the single ordering
//! authority for that session's locally produced events:
//!
//! 1. **Sequence at emit time.** Emitting assigns the next `session_seq`
//!    synchronously, before any spawn or write. Durable and ephemeral events
//!    share the counter.
//! 2. **Ordered durable writer.** Durable events are queued to one writer task
//!    per session, persisted in batches (one transaction per batch), and only
//!    published after they are persisted.
//! 3. **Ephemeral reorder buffer.** Ephemeral events publish inline unless an
//!    earlier durable event is still unpublished, in which case they wait for
//!    exactly the events with a lower `session_seq` (never for later ones).
//!
//! Fanout order therefore equals `session_seq` order, and the journal stores
//! the same order.

use crate::event_fanout::EventFanout;
use crate::events::{
    AgentEventKind, Durability, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
    classify_durability,
};
use crate::session::error::{SessionError, SessionResult};
use crate::session::projection::{EventJournal, NewDurableEvent};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Most durable events the writer persists in one transaction.
const BATCH_LIMIT: usize = 64;

/// Something waiting for its turn to be published.
enum Slot {
    /// A durable write failed: nothing to publish, but do not block later events.
    Skip,
    Publish(Box<(EventEnvelope, Option<tracing::Span>)>),
}

struct State {
    /// Next sequence number to hand out.
    next_seq: i64,
    /// Lowest sequence number not yet published (or skipped).
    next_publish: i64,
    /// Completed slots that cannot be published yet, keyed by sequence.
    ready: BTreeMap<i64, Slot>,
}

impl State {
    fn complete(&mut self, seq: i64, slot: Slot, fanout: &EventFanout) {
        if seq == self.next_publish && self.ready.is_empty() {
            Self::publish(slot, fanout);
            self.next_publish += 1;
            return;
        }
        self.ready.insert(seq, slot);
        while let Some(slot) = self.ready.remove(&self.next_publish) {
            Self::publish(slot, fanout);
            self.next_publish += 1;
        }
    }

    fn publish(slot: Slot, fanout: &EventFanout) {
        if let Slot::Publish(publication) = slot {
            let (envelope, parent) = *publication;
            fanout.publish_with_context(envelope, parent);
        }
    }
}

struct Shared {
    session_id: String,
    journal: Arc<dyn EventJournal>,
    fanout: Arc<EventFanout>,
    state: Mutex<State>,
}

/// Messages to a session's writer task, processed strictly in order.
// `Write` is the common message; boxing it would add an allocation per durable
// event on the emit path just to shrink the rare `Flush` variant.
#[allow(clippy::large_enum_variant)]
enum WriterMsg {
    Write(PendingWrite),
    /// Acknowledged once every write queued before it has been persisted and
    /// published.
    Flush(oneshot::Sender<()>),
}

struct PendingWrite {
    seq: i64,
    event: NewDurableEvent,
    message: Option<crate::model::AgentMessage>,
    prompt_blocks: Vec<EphemeralEvent>,
    parent: Option<tracing::Span>,
    reply: Option<oneshot::Sender<SessionResult<DurableEvent>>>,
}

/// Emit handle and ordering authority for one session.
///
/// Shared (via `Arc`) by every emitter of the session: the session actor, the
/// execution task, delegation and the scheduler.
pub struct SessionEventStream {
    shared: Arc<Shared>,
    writer: mpsc::UnboundedSender<WriterMsg>,
}

impl SessionEventStream {
    /// Create the stream for `session_id`, continuing after whatever the
    /// journal already holds, and start its writer task.
    ///
    /// Must be called inside a Tokio runtime.
    #[cfg(test)]
    pub(crate) fn new(
        session_id: &str,
        journal: Arc<dyn EventJournal>,
        fanout: Arc<EventFanout>,
    ) -> Arc<Self> {
        let base = journal.max_session_seq_blocking(session_id).max(0);
        Self::with_base(session_id, journal, fanout, base)
    }

    /// Create the stream for `session_id`, numbering from `base + 1`, and
    /// start its writer task. The caller supplies `base` (the journal's
    /// highest number, or a retired stream's tip if higher) so the lookup can
    /// run outside any lock.
    ///
    /// Must be called inside a Tokio runtime.
    pub(crate) fn with_base(
        session_id: &str,
        journal: Arc<dyn EventJournal>,
        fanout: Arc<EventFanout>,
        base: i64,
    ) -> Arc<Self> {
        let shared = Arc::new(Shared {
            session_id: session_id.to_string(),
            journal,
            fanout,
            state: Mutex::new(State {
                next_seq: base + 1,
                next_publish: base + 1,
                ready: BTreeMap::new(),
            }),
        });
        let (writer, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_writer(shared.clone(), rx));
        Arc::new(Self { shared, writer })
    }

    /// The session this stream orders.
    pub fn session_id(&self) -> &str {
        &self.shared.session_id
    }

    /// Last `session_seq` assigned so far (`0` when nothing was emitted).
    ///
    /// Everything up to and including this number is either published or
    /// queued ahead of any later publication, so a consumer that has handled
    /// this position has seen every event produced so far.
    pub fn tip(&self) -> i64 {
        self.shared.state.lock().next_seq - 1
    }

    /// Wait until every event emitted so far is persisted (durable) and
    /// published. Events emitted while waiting are not waited for.
    pub async fn flush(&self) {
        let (ack, done) = oneshot::channel();
        if self.writer.send(WriterMsg::Flush(ack)).is_ok() {
            let _ = done.await;
        }
    }

    /// True when every assigned number has been published or skipped.
    pub(crate) fn is_idle(&self) -> bool {
        let state = self.shared.state.lock();
        state.next_publish == state.next_seq
    }

    /// Highest `session_seq` already published to the fanout (`0` when none).
    #[cfg(test)]
    pub(crate) fn published_tip(&self) -> i64 {
        self.shared.state.lock().next_publish - 1
    }

    /// Emit without waiting: ephemeral events publish inline (or once earlier
    /// durable events are published); durable events are queued to the writer.
    pub fn emit(&self, kind: AgentEventKind) {
        match classify_durability(&kind) {
            Durability::Durable => self.enqueue(kind, EventOrigin::Local, None, None, None),
            Durability::Ephemeral => self.emit_ephemeral(kind),
        }
    }

    /// Emit a durable event and wait until it is persisted and published.
    pub async fn emit_persisted(&self, kind: AgentEventKind) -> SessionResult<DurableEvent> {
        self.emit_persisted_with(kind, EventOrigin::Local, None)
            .await
    }

    pub(crate) async fn emit_persisted_with(
        &self,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
    ) -> SessionResult<DurableEvent> {
        let (reply, receive) = oneshot::channel();
        self.enqueue(kind, origin, source_node, Some(reply), None);
        receive.await.map_err(|_| {
            SessionError::Other("event writer stopped before persisting the event".into())
        })?
    }

    /// Commit history and its acceptance event together. Structured blocks keep
    /// their earlier sequence positions, but publish only after admission commits.
    pub async fn admit_prompt(
        &self,
        message: crate::model::AgentMessage,
        content: String,
        client_prompt_id: Option<String>,
    ) -> SessionResult<DurableEvent> {
        if message.session_id != self.shared.session_id
            || !matches!(message.role, querymt::chat::ChatRole::User)
            || !message.parts.iter().any(|part| {
                matches!(
                    part,
                    crate::model::MessagePart::Prompt { .. }
                        | crate::model::MessagePart::Steering { .. }
                )
            })
        {
            return Err(SessionError::Other(
                "invalid prompt admission message".into(),
            ));
        }
        let kind = AgentEventKind::PromptReceived {
            content,
            message_id: Some(message.id.clone()),
        };
        let (reply, receive) = oneshot::channel();
        self.enqueue(
            kind,
            EventOrigin::Local,
            None,
            Some(reply),
            Some((message, client_prompt_id)),
        );
        receive.await.map_err(|_| {
            SessionError::Other("event writer stopped before admitting the prompt".into())
        })?
    }

    /// Publish an ephemeral event with the next sequence number.
    pub fn emit_ephemeral(&self, kind: AgentEventKind) {
        let mut state = self.shared.state.lock();
        let seq = state.next_seq;
        state.next_seq += 1;
        let envelope = EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: self.shared.session_id.clone(),
            session_seq: seq,
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        });
        state.complete(
            seq,
            Slot::Publish(Box::new((envelope, None))),
            &self.shared.fanout,
        );
    }

    fn enqueue(
        &self,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
        reply: Option<oneshot::Sender<SessionResult<DurableEvent>>>,
        admission: Option<(crate::model::AgentMessage, Option<String>)>,
    ) {
        let parent = (origin == EventOrigin::Local && EventFanout::is_delegation_event(&kind))
            .then(tracing::Span::current);

        // Assign the number and enqueue under one lock so the writer queue is
        // in sequence order.
        let mut state = self.shared.state.lock();
        let mut prompt_blocks = Vec::new();
        let message = admission.map(|(message, client_prompt_id)| {
            for part in &message.parts {
                let blocks = match part {
                    crate::model::MessagePart::Prompt { blocks }
                    | crate::model::MessagePart::Steering { blocks, .. } => blocks,
                    _ => continue,
                };
                for block in blocks {
                    let seq = state.next_seq;
                    state.next_seq += 1;
                    prompt_blocks.push(EphemeralEvent {
                        session_id: self.shared.session_id.clone(),
                        session_seq: seq,
                        timestamp: message.created_at,
                        origin: EventOrigin::Local,
                        source_node: None,
                        kind: AgentEventKind::UserPromptBlock {
                            message_id: message.id.clone(),
                            client_prompt_id: client_prompt_id.clone(),
                            block: block.clone(),
                        },
                    });
                }
            }
            message
        });
        let seq = state.next_seq;
        state.next_seq += 1;
        let write = PendingWrite {
            seq,
            event: NewDurableEvent {
                session_id: self.shared.session_id.clone(),
                origin,
                source_node,
                source_node_id: None,
                source_seq: None,
                session_seq: Some(seq),
                kind,
            },
            message,
            prompt_blocks,
            parent,
            reply,
        };
        if let Err(mpsc::error::SendError(WriterMsg::Write(write))) =
            self.writer.send(WriterMsg::Write(write))
        {
            // Writer task is gone: do not wedge later events behind this one.
            for block in write.prompt_blocks {
                state.complete(block.session_seq, Slot::Skip, &self.shared.fanout);
            }
            state.complete(seq, Slot::Skip, &self.shared.fanout);
            if let Some(reply) = write.reply {
                let _ = reply.send(Err(SessionError::Other(
                    "event writer is not running".into(),
                )));
            }
        }
    }
}

async fn run_writer(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<WriterMsg>) {
    let mut queue: Vec<PendingWrite> = Vec::with_capacity(BATCH_LIMIT);
    let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();
    while let Some(first) = rx.recv().await {
        let mut next = Some(first);
        while let Some(msg) = next.take() {
            match msg {
                WriterMsg::Write(write) => queue.push(write),
                WriterMsg::Flush(ack) => flushes.push(ack),
            }
            if queue.len() < BATCH_LIMIT {
                next = rx.try_recv().ok();
            }
        }
        if queue.is_empty() {
            for ack in flushes.drain(..) {
                let _ = ack.send(());
            }
            continue;
        }

        let mut events = Vec::with_capacity(queue.len());
        let mut pending = Vec::with_capacity(queue.len());
        let mut results = Vec::with_capacity(queue.len());
        for write in queue.drain(..) {
            if let Some(message) = write.message {
                if !events.is_empty() {
                    results.extend(shared.journal.append_durable_batch(&events).await);
                    events.clear();
                }
                results.push(shared.journal.append_prompt(&write.event, &message).await);
            } else {
                events.push(write.event);
            }
            pending.push((write.seq, write.prompt_blocks, write.parent, write.reply));
        }
        if !events.is_empty() {
            results.extend(shared.journal.append_durable_batch(&events).await);
        }
        let mut results = results.into_iter();

        let mut outcomes = Vec::with_capacity(pending.len());
        {
            let mut state = shared.state.lock();
            for (seq, prompt_blocks, parent, reply) in pending {
                let result = results.next().unwrap_or_else(|| {
                    Err(SessionError::Other(
                        "journal returned fewer results than events".into(),
                    ))
                });
                for block in prompt_blocks {
                    let seq = block.session_seq;
                    let slot = if result.is_ok() {
                        Slot::Publish(Box::new((EventEnvelope::Ephemeral(block), None)))
                    } else {
                        Slot::Skip
                    };
                    state.complete(seq, slot, &shared.fanout);
                }
                match &result {
                    Ok(persisted) => state.complete(
                        seq,
                        Slot::Publish(Box::new((
                            EventEnvelope::Durable(persisted.clone()),
                            parent,
                        ))),
                        &shared.fanout,
                    ),
                    Err(err) => {
                        log::warn!(
                            "failed to persist durable event for session {}: {}",
                            shared.session_id,
                            err
                        );
                        state.complete(seq, Slot::Skip, &shared.fanout);
                    }
                }
                outcomes.push((reply, result));
            }
        }
        // Reply only after publication, so an awaited emit observes its event
        // on the fanout.
        for (reply, result) in outcomes {
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
        for ack in flushes.drain(..) {
            let _ = ack.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use std::sync::atomic::{AtomicBool, Ordering};

    async fn make_stream(session_id: &str) -> (Arc<SessionEventStream>, Arc<EventFanout>) {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let fanout = Arc::new(EventFanout::new());
        let stream = SessionEventStream::new(session_id, storage.event_journal(), fanout.clone());
        (stream, fanout)
    }

    fn delta(n: usize) -> AgentEventKind {
        AgentEventKind::AssistantContentDelta {
            content: "t".into(),
            message_id: n.to_string(),
        }
    }

    fn stored(n: usize) -> AgentEventKind {
        AgentEventKind::AssistantMessageStored {
            content: "x".into(),
            thinking: None,
            reasoning_parts: Vec::new(),
            message_id: Some(n.to_string()),
        }
    }

    fn message_id(envelope: &EventEnvelope) -> usize {
        match envelope.kind() {
            AgentEventKind::AssistantContentDelta { message_id, .. } => message_id.parse().unwrap(),
            AgentEventKind::AssistantMessageStored {
                message_id: Some(message_id),
                ..
            } => message_id.parse().unwrap(),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_emitters_get_strictly_increasing_numbers() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 100;
        let (stream, fanout) = make_stream("s-conc").await;
        let mut rx = fanout.subscribe();

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let stream = stream.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_THREAD {
                    // Alternate durable and ephemeral from every thread.
                    let n = t * PER_THREAD + i;
                    stream.emit(if i % 3 == 0 { stored(n) } else { delta(n) });
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let total = THREADS * PER_THREAD;
        assert_eq!(stream.tip(), total as i64);
        let mut seqs = Vec::with_capacity(total);
        while seqs.len() < total {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("timeout")
                .expect("recv");
            seqs.push(envelope.session_seq());
        }
        let expected: Vec<i64> = (1..=total as i64).collect();
        assert_eq!(seqs, expected, "fanout order must equal session_seq order");
    }

    #[tokio::test]
    async fn ephemeral_waits_only_for_earlier_durable() {
        let (stream, fanout) = make_stream("s-eph").await;
        let mut rx = fanout.subscribe();
        // durable(1), ephemeral(2), ephemeral(3) emitted back to back.
        stream.emit(stored(1));
        stream.emit(delta(2));
        stream.emit(delta(3));
        let mut order = Vec::new();
        for _ in 0..3 {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            order.push(message_id(&envelope));
        }
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn ephemeral_without_pending_durable_publishes_inline() {
        let (stream, fanout) = make_stream("s-inline").await;
        let mut rx = fanout.subscribe();
        stream.emit(delta(1));
        // try_recv: no await point between emit and receive.
        let envelope = rx.try_recv().expect("published inline");
        assert_eq!(envelope.session_seq(), 1);
    }

    #[tokio::test]
    async fn emit_persisted_returns_after_publication() {
        let (stream, fanout) = make_stream("s-persisted").await;
        let mut rx = fanout.subscribe();
        let persisted = stream.emit_persisted(stored(1)).await.unwrap();
        assert_eq!(persisted.session_seq, 1);
        let envelope = rx.try_recv().expect("already published when awaited");
        assert_eq!(envelope.session_seq(), 1);
    }

    #[tokio::test]
    async fn prompt_admission_rolls_back_history_and_events_before_publication() {
        use crate::model::{AgentMessage, MessagePart};
        use crate::session::store::SessionStore;
        for (table, condition) in [
            ("messages", "NEW.public_id = 'admission'"),
            ("event_journal", "NEW.kind = 'prompt_received'"),
        ] {
            let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
            let session = storage
                .create_session(None, None, None, None)
                .await
                .unwrap();
            let fanout = Arc::new(EventFanout::new());
            let stream = SessionEventStream::new(
                &session.public_id,
                storage.event_journal(),
                fanout.clone(),
            );
            stream
                .emit_persisted(AgentEventKind::SessionCreated)
                .await
                .unwrap();
            let mut receiver = fanout.subscribe();
            let message = AgentMessage {
                id: "admission".into(),
                session_id: session.public_id.clone(),
                role: querymt::chat::ChatRole::User,
                parts: vec![MessagePart::Prompt {
                    blocks: vec!["task".into()],
                }],
                created_at: 100,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            };
            storage.conn().lock().unwrap().execute_batch(&format!(
                "CREATE TEMP TRIGGER reject_admission BEFORE INSERT ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT, 'injected admission failure'); END;"
            )).unwrap();
            let error = stream
                .admit_prompt(message.clone(), "task".into(), Some("client".into()))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("injected admission failure"));
            assert!(
                storage
                    .get_history(&session.public_id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                storage
                    .load_session_stream(&session.public_id, None, None)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                receiver.try_recv().is_err(),
                "a rejected prompt must not publish blocks or acceptance"
            );
            storage
                .conn()
                .lock()
                .unwrap()
                .execute_batch("DROP TRIGGER reject_admission;")
                .unwrap();
            let accepted = stream
                .admit_prompt(message, "task".into(), Some("client".into()))
                .await
                .unwrap();
            let block = receiver.try_recv().unwrap();
            let prompt = receiver.try_recv().unwrap();
            assert!(
                matches!(block.kind(), AgentEventKind::UserPromptBlock { client_prompt_id: Some(id), .. } if id == "client")
            );
            assert!(
                matches!(prompt.kind(), AgentEventKind::PromptReceived { message_id: Some(id), .. } if id == "admission")
            );
            assert!(block.session_seq() < prompt.session_seq());
            assert_eq!(prompt.session_seq(), accepted.session_seq);
            let history = storage.get_history(&session.public_id).await.unwrap();
            assert_eq!(history.len(), 1);
            assert_eq!(history[0].id, "admission");
            stream.emit(delta(1));
            stream.flush().await;
            assert!(receiver.try_recv().unwrap().session_seq() > accepted.session_seq);
        }
    }

    #[tokio::test]
    async fn stream_continues_after_existing_journal_events() {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let fanout = Arc::new(EventFanout::new());
        let first = SessionEventStream::new("s-resume", storage.event_journal(), fanout.clone());
        first.emit_persisted(stored(1)).await.unwrap();
        first.emit_persisted(stored(2)).await.unwrap();
        drop(first);

        let second = SessionEventStream::new("s-resume", storage.event_journal(), fanout);
        let persisted = second.emit_persisted(stored(3)).await.unwrap();
        assert_eq!(persisted.session_seq, 3);
    }

    #[tokio::test]
    async fn failed_write_does_not_block_later_events() {
        struct FailOnce {
            inner: Arc<dyn EventJournal>,
            failed: AtomicBool,
        }
        #[async_trait::async_trait]
        impl EventJournal for FailOnce {
            async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
                if !self.failed.swap(true, Ordering::SeqCst) {
                    return Err(SessionError::DatabaseError("boom".into()));
                }
                self.inner.append_durable(event).await
            }
            async fn load_session_stream(
                &self,
                s: &str,
                a: Option<i64>,
                l: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                self.inner.load_session_stream(s, a, l).await
            }
            async fn load_global_stream(
                &self,
                a: Option<i64>,
                l: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                self.inner.load_global_stream(a, l).await
            }
            async fn delete_session_events_from(&self, s: &str, f: i64) -> SessionResult<usize> {
                self.inner.delete_session_events_from(s, f).await
            }
            async fn append_durable_from_source(
                &self,
                e: &NewDurableEvent,
            ) -> SessionResult<Option<DurableEvent>> {
                self.inner.append_durable_from_source(e).await
            }
            async fn latest_source_seq(&self, s: &str, n: &str) -> SessionResult<Option<i64>> {
                self.inner.latest_source_seq(s, n).await
            }
            async fn remote_sync_cursor(&self, s: &str, n: &str) -> SessionResult<Option<i64>> {
                self.inner.remote_sync_cursor(s, n).await
            }
            async fn advance_remote_sync_cursor(
                &self,
                s: &str,
                n: &str,
                q: i64,
                c: bool,
            ) -> SessionResult<()> {
                self.inner.advance_remote_sync_cursor(s, n, q, c).await
            }
            async fn load_remote_session_stream(
                &self,
                s: &str,
                n: &str,
            ) -> SessionResult<Vec<DurableEvent>> {
                self.inner.load_remote_session_stream(s, n).await
            }
            async fn max_stream_seq(&self, s: &str) -> SessionResult<i64> {
                self.inner.max_stream_seq(s).await
            }
        }

        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal: Arc<dyn EventJournal> = Arc::new(FailOnce {
            inner: storage.event_journal(),
            failed: AtomicBool::new(false),
        });
        let fanout = Arc::new(EventFanout::new());
        let mut rx = fanout.subscribe();
        let stream = SessionEventStream::new("s-fail", journal, fanout);

        let first = stream.emit_persisted(stored(1)).await;
        assert!(first.is_err());
        stream.emit(delta(2));
        let second = stream.emit_persisted(stored(3)).await.unwrap();
        assert_eq!(second.session_seq, 3);

        let a = rx.recv().await.unwrap();
        let b = rx.recv().await.unwrap();
        assert_eq!((a.session_seq(), b.session_seq()), (2, 3));
    }
}
