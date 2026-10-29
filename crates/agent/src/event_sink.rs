//! Single producer-facing API for all event emission.
//!
//! `EventSink` is the only API domain code should use to emit events.
//! It classifies events as durable or ephemeral, routes them to the
//! appropriate backend, and publishes to the fanout for live delivery.
//!
//! Rules:
//! - `emit_durable` persists to the journal (awaited), then publishes to fanout.
//! - `emit_ephemeral` publishes to fanout only — never persisted.
//! - Domain code MUST NOT call EventJournal directly.

use crate::event_fanout::EventFanout;
use crate::events::{
    AgentEventKind, Durability, DurableEvent, EphemeralEvent, EventEnvelope, EventOrigin,
    classify_durability,
};
use crate::session::error::SessionResult;
use crate::session::projection::{EventJournal, NewDurableEvent};
use crate::session_event_stream::SessionEventStream;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Single ingress API for all event producers.
///
/// Owns the journal (storage boundary) and fanout (transport boundary), and
/// one [`SessionEventStream`] per session, which numbers and orders every
/// locally produced event of that session.
pub struct EventSink {
    journal: Arc<dyn EventJournal>,
    fanout: Arc<EventFanout>,
    streams: Mutex<StreamRegistry>,
    /// One shared accounting projection per live session, so every component
    /// that records or reads spend (execution, compaction, delegation
    /// summaries) sees the same totals. Released with the session's stream.
    accounting: Mutex<
        std::collections::HashMap<
            String,
            Arc<tokio::sync::OnceCell<Arc<crate::accounting::SessionAccounting>>>,
        >,
    >,
}

#[derive(Default)]
struct StreamRegistry {
    live: HashMap<String, Arc<SessionEventStream>>,
    /// Last sequence number assigned by a retired stream. Ephemeral events
    /// consume numbers that the journal never sees, so a replacement stream
    /// must continue after this tip, not just after the journal's maximum.
    retired_tips: HashMap<String, i64>,
}

impl EventSink {
    /// Create a new EventSink.
    pub fn new(journal: Arc<dyn EventJournal>, fanout: Arc<EventFanout>) -> Self {
        Self {
            journal,
            fanout,
            streams: Mutex::new(StreamRegistry::default()),
            accounting: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The session's shared accounting projection, hydrated from the journal
    /// on first use. Concurrent callers share one hydration.
    pub async fn session_accounting(
        &self,
        session_id: &str,
    ) -> Result<Arc<crate::accounting::SessionAccounting>, crate::accounting::AccountingError> {
        let cell = self
            .accounting
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .clone();
        cell.get_or_try_init(|| {
            crate::accounting::SessionAccounting::hydrate(session_id, self.journal.clone())
        })
        .await
        .cloned()
    }

    pub(crate) fn live_delegation_activity(
        &self,
        session_id: &str,
    ) -> Vec<crate::control::delegation_notifications::DelegationUpdateNotification> {
        self.fanout.live_delegation_activity(session_id)
    }

    /// The ordered emit handle of a session, created on first use.
    ///
    /// All emitters of one session (actor, execution task, delegation,
    /// scheduler) share this handle, so they share one `session_seq` counter.
    /// Must be called inside a Tokio runtime.
    pub fn session_stream(&self, session_id: &str) -> Arc<SessionEventStream> {
        if let Some(stream) = self.streams.lock().live.get(session_id) {
            return stream.clone();
        }

        // The journal lookup may wait on the storage connection, so it runs
        // without the registry lock. It can be stale by the time the lock is
        // retaken; the retained tip of a retired stream covers that case.
        let durable_base = self.journal.max_session_seq_blocking(session_id).max(0);

        let mut registry = self.streams.lock();
        // A concurrent caller may have created the stream meanwhile.
        if let Some(stream) = registry.live.get(session_id) {
            return stream.clone();
        }
        let retired_tip = registry.retired_tips.remove(session_id).unwrap_or(0);
        let stream = SessionEventStream::with_base(
            session_id,
            self.journal.clone(),
            self.fanout.clone(),
            durable_base.max(retired_tip),
        );
        registry.live.insert(session_id.to_string(), stream.clone());
        stream
    }

    /// Release a session's ordered stream (and its writer task) when the
    /// session actor stops.
    ///
    /// Waits until everything already emitted is persisted and published, then
    /// removes the stream only if it is idle and nobody else holds it. A later
    /// emit creates a fresh stream that continues after the highest number the
    /// retired stream assigned (and the journal's highest number), so numbers
    /// consumed by ephemeral events are never reused. Returns whether the
    /// stream was released.
    pub async fn retire_session_stream(&self, session_id: &str) -> bool {
        let Some(stream) = self.streams.lock().live.get(session_id).cloned() else {
            // No ordered stream, but the accounting projection must not outlive
            // the session's retirement.
            self.accounting.lock().remove(session_id);
            return false;
        };
        stream.flush().await;

        let mut registry = self.streams.lock();
        // The map's entry plus our local clone are the two expected owners.
        let released = registry
            .live
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, &stream))
            && stream.is_idle()
            && Arc::strong_count(&stream) == 2;
        if released {
            self.accounting.lock().remove(session_id);
            registry.live.remove(session_id);
            registry
                .retired_tips
                .insert(session_id.to_string(), stream.tip());
        }
        released
    }

    /// Number of live per-session streams (diagnostics and tests).
    pub fn session_stream_count(&self) -> usize {
        self.streams.lock().live.len()
    }

    /// Emit a durable event. Persists to journal (awaited), then publishes to fanout.
    ///
    /// Returns the persisted `DurableEvent` with DB-assigned `event_id` and `stream_seq`.
    pub async fn emit_durable(
        &self,
        session_id: &str,
        kind: AgentEventKind,
    ) -> SessionResult<DurableEvent> {
        self.emit_durable_with_origin(session_id, kind, EventOrigin::Local, None)
            .await
    }

    /// Emit a durable event with explicit origin metadata.
    ///
    /// Locally produced events go through the session's ordered stream.
    /// Events of another origin carry no local production order: they are
    /// stored unsequenced (`session_seq` unset) and published after the write.
    pub async fn emit_durable_with_origin(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
    ) -> SessionResult<DurableEvent> {
        if origin == EventOrigin::Local {
            return self
                .session_stream(session_id)
                .emit_persisted_with(kind, origin, source_node)
                .await;
        }

        let new_event = NewDurableEvent {
            session_id: session_id.to_string(),
            origin,
            source_node,
            source_node_id: None,
            source_seq: None,
            session_seq: None,
            kind,
        };
        let persisted = self.journal.append_durable(&new_event).await?;
        self.fanout
            .publish(EventEnvelope::Durable(persisted.clone()));
        Ok(persisted)
    }

    /// Emit a durable event that carries remote source identity (plan §15/§16).
    ///
    /// Persists through the idempotent source-identity path: when an event with
    /// the same `(session_id, source_node_id, source_seq)` is already journaled,
    /// this is a full no-op — `Ok(None)` is returned and the fanout is NOT
    /// republished, so duplicate replay after reconnect never doubles local
    /// delivery.
    ///
    /// `session_seq` is the host's per-session sequence; it is stored as-is so
    /// one cursor describes the session on every node. `None` (or `0`) for
    /// events from nodes that do not number them.
    pub async fn emit_durable_from_source(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        source_node: Option<String>,
        source_node_id: String,
        source_seq: i64,
        session_seq: Option<i64>,
    ) -> SessionResult<Option<DurableEvent>> {
        let new_event = NewDurableEvent {
            session_id: session_id.to_string(),
            origin: EventOrigin::Remote,
            source_node,
            source_node_id: Some(source_node_id),
            source_seq: Some(source_seq),
            session_seq: session_seq.filter(|seq| *seq > 0),
            kind,
        };

        match self.journal.append_durable_from_source(&new_event).await? {
            // Duplicate replay: no insert, no fanout republish.
            None => Ok(None),
            Some(persisted) => {
                // Publish to fanout for live subscribers
                self.fanout
                    .publish(EventEnvelope::Durable(persisted.clone()));
                Ok(Some(persisted))
            }
        }
    }

    /// Emit a locally produced ephemeral event through the session's ordered
    /// stream. Never persisted.
    pub fn emit_ephemeral(&self, session_id: &str, kind: AgentEventKind) {
        self.session_stream(session_id).emit_ephemeral(kind);
    }

    /// Emit an unsequenced ephemeral event with explicit origin (connection
    /// annotations such as remote-stream status). Published to fanout only.
    pub fn emit_ephemeral_with_origin(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
    ) {
        self.publish_ephemeral(session_id, kind, origin, source_node, 0);
    }

    /// Republish an ephemeral event relayed from a remote host, keeping the
    /// host's `session_seq`.
    pub fn emit_ephemeral_relayed(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        source_node: Option<String>,
        session_seq: i64,
    ) {
        self.publish_ephemeral(
            session_id,
            kind,
            EventOrigin::Remote,
            source_node,
            session_seq.max(0),
        );
    }

    fn publish_ephemeral(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
        session_seq: i64,
    ) {
        let ephemeral = EphemeralEvent {
            session_id: session_id.to_string(),
            session_seq,
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            origin,
            source_node,
            kind,
        };

        self.fanout.publish(EventEnvelope::Ephemeral(ephemeral));
    }

    /// Auto-classify and emit: inspects the kind and routes to durable or ephemeral.
    ///
    /// This is the recommended single entry point for most producers.
    pub async fn emit(
        &self,
        session_id: &str,
        kind: AgentEventKind,
    ) -> SessionResult<Option<DurableEvent>> {
        match classify_durability(&kind) {
            Durability::Durable => {
                let persisted = self.emit_durable(session_id, kind).await?;
                Ok(Some(persisted))
            }
            Durability::Ephemeral => {
                self.emit_ephemeral(session_id, kind);
                Ok(None)
            }
        }
    }

    /// Access the underlying fanout for subscribing.
    pub fn fanout(&self) -> &Arc<EventFanout> {
        &self.fanout
    }

    /// Access the underlying journal.
    pub fn journal(&self) -> &Arc<dyn EventJournal> {
        &self.journal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn make_sink() -> EventSink {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let journal = storage.event_journal();
        let fanout = Arc::new(EventFanout::new());
        EventSink::new(journal, fanout)
    }

    // ── session stream release ─────────────────────────────────────────

    #[tokio::test]
    async fn retiring_a_stream_drains_it_and_a_new_one_never_reuses_numbers() {
        let sink = make_sink().await;
        for _ in 0..3 {
            // Queued, not awaited: retire must wait for the writer.
            sink.session_stream("s1").emit(AgentEventKind::Cancelled);
        }
        assert_eq!(sink.session_stream_count(), 1);

        assert!(sink.retire_session_stream("s1").await);
        assert_eq!(sink.session_stream_count(), 0, "no stream left behind");
        assert!(!sink.retire_session_stream("s1").await, "already released");

        let next = sink
            .emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        assert_eq!(next.session_seq, 4, "numbering continues after the drain");
        let stored = sink
            .journal()
            .load_session_stream("s1", None, None)
            .await
            .unwrap();
        let seqs: Vec<i64> = stored.iter().map(|e| e.session_seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn retired_stream_tip_covers_ephemeral_numbers() {
        let sink = make_sink().await;
        sink.emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantContentDelta {
                content: "t".into(),
                message_id: "m1".into(),
            },
        );
        let tip = sink.session_stream("s1").tip();
        assert_eq!(tip, 2);

        assert!(sink.retire_session_stream("s1").await);
        let next = sink
            .emit_durable("s1", AgentEventKind::Cancelled)
            .await
            .unwrap();
        assert_eq!(next.session_seq, 3, "ephemeral number 2 is not reused");
    }

    #[tokio::test]
    async fn concurrent_session_stream_calls_share_one_stream() {
        let sink = Arc::new(make_sink().await);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let sink = sink.clone();
                tokio::spawn(async move { sink.session_stream("s1") })
            })
            .collect();
        let mut streams = Vec::new();
        for handle in handles {
            streams.push(handle.await.unwrap());
        }
        assert!(streams.iter().all(|s| Arc::ptr_eq(s, &streams[0])));
        assert_eq!(sink.session_stream_count(), 1);
    }

    #[tokio::test]
    async fn retire_keeps_a_stream_that_is_still_in_use() {
        let sink = make_sink().await;
        let held = sink.session_stream("s1");
        assert!(!sink.retire_session_stream("s1").await);
        assert_eq!(sink.session_stream_count(), 1);
        drop(held);
        assert!(sink.retire_session_stream("s1").await);
    }

    // ── emit_durable ───────────────────────────────────────────────────

    #[tokio::test]
    async fn emit_durable_persists_and_returns_event() {
        let sink = make_sink().await;
        let result = sink
            .emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();

        assert_eq!(result.session_id, "s1");
        assert!(result.stream_seq >= 1);
        assert!(!result.event_id.is_empty());
        assert!(matches!(result.kind, AgentEventKind::SessionCreated));
    }

    #[tokio::test]
    async fn emit_durable_publishes_to_fanout() {
        let sink = make_sink().await;
        let mut rx = sink.fanout().subscribe();

        sink.emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();

        let env = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert!(env.is_durable());
        assert_eq!(env.session_id(), "s1");
    }

    #[tokio::test]
    async fn emit_durable_is_replayable_from_journal() {
        let sink = make_sink().await;

        sink.emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        sink.emit_durable("s1", AgentEventKind::Cancelled)
            .await
            .unwrap();

        let journal = sink.journal();
        let events = journal.load_session_stream("s1", None, None).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].kind, AgentEventKind::SessionCreated));
        assert!(matches!(events[1].kind, AgentEventKind::Cancelled));
    }

    // ── emit_ephemeral ─────────────────────────────────────────────────

    #[tokio::test]
    async fn emit_ephemeral_publishes_to_fanout() {
        let sink = make_sink().await;
        let mut rx = sink.fanout().subscribe();

        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantContentDelta {
                content: "hello".into(),
                message_id: "m1".into(),
            },
        );

        let env = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv");

        assert!(env.is_ephemeral());
    }

    #[tokio::test]
    async fn emit_ephemeral_never_persisted_to_journal() {
        let sink = make_sink().await;

        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantContentDelta {
                content: "tok".into(),
                message_id: "m1".into(),
            },
        );

        let journal = sink.journal();
        let events = journal.load_session_stream("s1", None, None).await.unwrap();
        assert!(
            events.is_empty(),
            "ephemeral events must never appear in journal"
        );
    }

    // ── Source-identity dedup (plan §15/§16) ───────────────────────────

    #[tokio::test]
    async fn emit_durable_from_source_dedups_and_skips_fanout() {
        let sink = make_sink().await;
        let mut rx = sink.fanout().subscribe();

        let first = sink
            .emit_durable_from_source(
                "s1",
                AgentEventKind::SessionCreated,
                Some("peer-a".to_string()),
                "node-a".to_string(),
                7,
                None,
            )
            .await
            .unwrap();
        assert!(
            first.is_some(),
            "fresh source identity persists and returns the event"
        );

        // Same (session, node, source_seq): no insert, no fanout republish —
        // duplicate replay after reconnect must never double local delivery.
        let duplicate = sink
            .emit_durable_from_source(
                "s1",
                AgentEventKind::SessionCreated,
                Some("peer-a".to_string()),
                "node-a".to_string(),
                7,
                None,
            )
            .await
            .unwrap();
        assert!(duplicate.is_none(), "duplicate replay is a no-op");

        let received = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("fresh insert publishes to fanout")
            .unwrap();
        assert_eq!(received.session_id(), "s1");
        let late = tokio::time::timeout(std::time::Duration::from_millis(80), rx.recv()).await;
        assert!(late.is_err(), "duplicate must not be republished to fanout");

        // A different source sequence is a distinct event.
        let third = sink
            .emit_durable_from_source(
                "s1",
                AgentEventKind::Cancelled,
                Some("peer-a".to_string()),
                "node-a".to_string(),
                8,
                None,
            )
            .await
            .unwrap();
        assert!(third.is_some());
    }

    // ── emit (auto-classify) ───────────────────────────────────────────

    #[tokio::test]
    async fn emit_auto_classifies_durable() {
        let sink = make_sink().await;
        let result = sink
            .emit("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        assert!(
            result.is_some(),
            "SessionCreated should be classified as durable"
        );
    }

    #[tokio::test]
    async fn emit_auto_classifies_ephemeral() {
        let sink = make_sink().await;
        let result = sink
            .emit(
                "s1",
                AgentEventKind::AssistantContentDelta {
                    content: "x".into(),
                    message_id: "m".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "ContentDelta should be classified as ephemeral"
        );
    }

    #[tokio::test]
    async fn emit_durable_with_origin_preserves_remote_metadata() {
        let sink = make_sink().await;
        let result = sink
            .emit_durable_with_origin(
                "s1",
                AgentEventKind::SessionCreated,
                EventOrigin::Remote,
                Some("peer-42".into()),
            )
            .await
            .unwrap();

        assert!(matches!(result.origin, EventOrigin::Remote));
        assert_eq!(result.source_node.as_deref(), Some("peer-42"));
    }

    // ── Invariant: no duplicate delivery on replay+live boundary ──────

    #[tokio::test]
    async fn durable_replay_plus_live_no_duplicates() {
        let sink = make_sink().await;

        // Emit 3 durable events
        let e1 = sink
            .emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        let _e2 = sink
            .emit_durable("s1", AgentEventKind::Cancelled)
            .await
            .unwrap();
        let _e3 = sink
            .emit_durable(
                "s1",
                AgentEventKind::Error {
                    message: "oops".into(),
                },
            )
            .await
            .unwrap();

        // Simulate replay from cursor after e1
        let replayed = sink
            .journal()
            .load_session_stream("s1", Some(e1.stream_seq), None)
            .await
            .unwrap();
        assert_eq!(replayed.len(), 2, "should get events after e1 only");

        // A live subscriber that started with cursor at e1 would receive
        // these same events. Dedup is done by cursor comparison.
        // The key contract: journal never returns e1 again after cursor.
    }

    // ── Invariant: durable ordering is monotonic per session stream ────

    #[tokio::test]
    async fn durable_ordering_monotonic_per_session() {
        let sink = make_sink().await;

        let e1 = sink
            .emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        let e2 = sink
            .emit_durable("s1", AgentEventKind::Cancelled)
            .await
            .unwrap();
        let e3 = sink
            .emit_durable(
                "s1",
                AgentEventKind::Error {
                    message: "x".into(),
                },
            )
            .await
            .unwrap();

        assert!(
            e1.stream_seq < e2.stream_seq,
            "seq must be monotonically increasing: {} < {}",
            e1.stream_seq,
            e2.stream_seq
        );
        assert!(
            e2.stream_seq < e3.stream_seq,
            "seq must be monotonically increasing: {} < {}",
            e2.stream_seq,
            e3.stream_seq
        );

        // Also verify via journal reload
        let events = sink
            .journal()
            .load_session_stream("s1", None, None)
            .await
            .unwrap();
        for window in events.windows(2) {
            assert!(
                window[0].stream_seq < window[1].stream_seq,
                "journal replay must be monotonically ordered"
            );
        }
    }

    // ── Invariant: DB append failure must NOT publish to fanout ────────

    #[tokio::test]
    async fn db_append_failure_does_not_publish_to_fanout() {
        use crate::session::error::SessionError;

        /// A journal that always fails on append.
        struct FailingJournal;

        #[async_trait::async_trait]
        impl EventJournal for FailingJournal {
            async fn append_durable(
                &self,
                _event: &NewDurableEvent,
            ) -> SessionResult<DurableEvent> {
                Err(SessionError::DatabaseError("simulated DB failure".into()))
            }

            async fn load_session_stream(
                &self,
                _session_id: &str,
                _after_seq: Option<i64>,
                _limit: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                Ok(vec![])
            }

            async fn load_global_stream(
                &self,
                _after_seq: Option<i64>,
                _limit: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                Ok(vec![])
            }

            async fn delete_session_events_from(
                &self,
                _session_id: &str,
                _from_seq: i64,
            ) -> SessionResult<usize> {
                Ok(0)
            }

            async fn append_durable_from_source(
                &self,
                event: &NewDurableEvent,
            ) -> SessionResult<Option<DurableEvent>> {
                Ok(Some(self.append_durable(event).await?))
            }

            async fn latest_source_seq(
                &self,
                _session_id: &str,
                _source_node_id: &str,
            ) -> SessionResult<Option<i64>> {
                Ok(None)
            }

            async fn remote_sync_cursor(&self, _: &str, _: &str) -> SessionResult<Option<i64>> {
                Ok(None)
            }
            async fn advance_remote_sync_cursor(
                &self,
                _: &str,
                _: &str,
                _: i64,
                _: bool,
            ) -> SessionResult<()> {
                Ok(())
            }
            async fn load_remote_session_stream(
                &self,
                _: &str,
                _: &str,
            ) -> SessionResult<Vec<DurableEvent>> {
                Ok(vec![])
            }

            async fn max_stream_seq(&self, _session_id: &str) -> SessionResult<i64> {
                Ok(0)
            }
        }

        let fanout = Arc::new(EventFanout::new());
        let mut rx = fanout.subscribe();
        let sink = EventSink::new(Arc::new(FailingJournal), fanout);

        // Attempt durable emit — should fail
        let result = sink
            .emit_durable("s1", AgentEventKind::SessionCreated)
            .await;
        assert!(result.is_err(), "emit_durable must propagate DB failure");

        // Fanout must NOT have received anything
        let recv_result =
            tokio::time::timeout(tokio::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            recv_result.is_err(),
            "fanout must not receive event when DB append fails"
        );
    }

    // ── Invariant: ephemeral never appears in journal replay ──────────

    #[tokio::test]
    async fn ephemeral_never_in_journal_even_with_mixed_emit() {
        let sink = make_sink().await;

        // Emit durable, ephemeral, durable — interleaved
        sink.emit_durable("s1", AgentEventKind::SessionCreated)
            .await
            .unwrap();
        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantContentDelta {
                content: "tok1".into(),
                message_id: "m1".into(),
            },
        );
        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantThinkingDelta {
                content: "think".into(),
                message_id: "m2".into(),
                part_id: None,
            },
        );
        sink.emit_durable("s1", AgentEventKind::Cancelled)
            .await
            .unwrap();

        let events = sink
            .journal()
            .load_session_stream("s1", None, None)
            .await
            .unwrap();

        assert_eq!(events.len(), 2, "only durable events in journal");
        for ev in &events {
            assert!(
                !matches!(
                    ev.kind,
                    AgentEventKind::AssistantContentDelta { .. }
                        | AgentEventKind::AssistantThinkingDelta { .. }
                ),
                "ephemeral event kind must never appear in journal replay"
            );
        }
    }

    // ── Invariant: ephemeral events DO appear on live fanout ──────────

    #[tokio::test]
    async fn ephemeral_events_appear_on_live_fanout() {
        let sink = make_sink().await;
        let mut rx = sink.fanout().subscribe();

        sink.emit_ephemeral(
            "s1",
            AgentEventKind::AssistantContentDelta {
                content: "hello".into(),
                message_id: "m1".into(),
            },
        );

        let env = tokio::time::timeout(tokio::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout — ephemeral event must appear on fanout")
            .expect("recv");

        assert!(env.is_ephemeral());
    }

    #[tokio::test]
    async fn structured_prompt_blocks_precede_awaited_summary_and_stay_out_of_journal() {
        let sink = make_sink().await;
        let mut rx = sink.fanout().subscribe();
        let blocks = [
            crate::acp::protocol::ContentBlock::Text(crate::acp::protocol::TextContent::new(
                "before",
            )),
            crate::acp::protocol::ContentBlock::Image(crate::acp::protocol::ImageContent::new(
                "AQID",
                "image/png",
            )),
        ];

        for block in blocks {
            sink.emit_ephemeral(
                "s1",
                AgentEventKind::UserPromptBlock {
                    message_id: "m1".into(),
                    client_prompt_id: Some("client-1".into()),
                    block,
                },
            );
        }
        sink.emit_durable(
            "s1",
            AgentEventKind::PromptReceived {
                content: "before\n\n[Image: image/png]".into(),
                message_id: Some("m1".into()),
            },
        )
        .await
        .unwrap();

        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        let third = rx.recv().await.unwrap();
        assert!(matches!(
            first.kind(),
            AgentEventKind::UserPromptBlock { .. }
        ));
        assert!(matches!(
            second.kind(),
            AgentEventKind::UserPromptBlock { .. }
        ));
        assert!(matches!(
            third.kind(),
            AgentEventKind::PromptReceived { .. }
        ));

        let journal = sink
            .journal()
            .load_session_stream("s1", None, None)
            .await
            .unwrap();
        assert_eq!(journal.len(), 1);
        assert!(matches!(
            journal[0].kind,
            AgentEventKind::PromptReceived { .. }
        ));
        assert!(!serde_json::to_string(&journal).unwrap().contains("AQID"));
    }
}
