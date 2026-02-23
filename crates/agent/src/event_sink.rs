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
use std::sync::Arc;

/// Single ingress API for all event producers.
///
/// Owns the journal (storage boundary) and fanout (transport boundary).
pub struct EventSink {
    journal: Arc<dyn EventJournal>,
    fanout: Arc<EventFanout>,
}

impl EventSink {
    /// Create a new EventSink.
    pub fn new(journal: Arc<dyn EventJournal>, fanout: Arc<EventFanout>) -> Self {
        Self { journal, fanout }
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

    /// Emit a durable event with explicit origin metadata (for remote events).
    pub async fn emit_durable_with_origin(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
    ) -> SessionResult<DurableEvent> {
        let new_event = NewDurableEvent {
            session_id: session_id.to_string(),
            origin: origin.clone(),
            source_node: source_node.clone(),
            kind: kind.clone(),
        };

        let persisted = self.journal.append_durable(&new_event).await?;

        // Publish to fanout for live subscribers
        self.fanout
            .publish(EventEnvelope::Durable(persisted.clone()));

        Ok(persisted)
    }

    /// Emit an ephemeral event. Published to fanout only — never persisted.
    pub fn emit_ephemeral(&self, session_id: &str, kind: AgentEventKind) {
        self.emit_ephemeral_with_origin(session_id, kind, EventOrigin::Local, None);
    }

    /// Emit an ephemeral event with explicit origin.
    pub fn emit_ephemeral_with_origin(
        &self,
        session_id: &str,
        kind: AgentEventKind,
        origin: EventOrigin,
        source_node: Option<String>,
    ) {
        let ephemeral = EphemeralEvent {
            session_id: session_id.to_string(),
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            origin: origin.clone(),
            source_node: source_node.clone(),
            kind: kind.clone(),
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
                _after_seq: Option<u64>,
                _limit: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                Ok(vec![])
            }

            async fn load_global_stream(
                &self,
                _after_seq: Option<u64>,
                _limit: Option<usize>,
            ) -> SessionResult<Vec<DurableEvent>> {
                Ok(vec![])
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
}
