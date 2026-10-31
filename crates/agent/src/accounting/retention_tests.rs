//! Accounting survives conversation-history changes (task 5.1).
//!
//! All tests use fresh databases. Range deletion is the single primitive
//! behind pruning and rewind, so it is exercised directly and through the
//! real rewind cleanup entry point.

use crate::accounting::tests::{sample_settlement, sample_start};
use crate::accounting::{AttemptRecorder, SessionAccounting};
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EventOrigin, ExecutionMetrics};
use crate::session::backend::StorageBackend;
use crate::session::projection::{EventJournal, NewDurableEvent};
use crate::session::sqlite_storage::SqliteStorage;
use crate::session_event_stream::SessionEventStream;
use std::sync::Arc;

fn cost() -> f64 {
    (100.0 * 3.0 + 15.0 * 15.0 + 50.0 * 0.3 + 20.0 * 3.75) / 1e6
}

fn prompt(id: &str) -> AgentEventKind {
    AgentEventKind::PromptReceived {
        content: "hello".into(),
        message_id: Some(id.into()),
    }
}

fn request_end(accounting: Option<crate::accounting::RequestAccounting>) -> AgentEventKind {
    AgentEventKind::LlmRequestEnd {
        usage: None,
        tool_calls: 0,
        finish_reason: None,
        cost_usd: Some(1.0),
        cumulative_cost_usd: Some(1.0),
        context_tokens: 0,
        metrics: ExecutionMetrics { steps: 1, turns: 1 },
        accounting,
    }
}

fn derived() -> Option<crate::accounting::RequestAccounting> {
    Some(crate::accounting::RequestAccounting {
        operation_id: "op".into(),
        attempt_ids: vec!["a1".into()],
        known_subtotal_usd: cost(),
        complete: true,
        reasons: vec![],
    })
}

async fn kinds(journal: &Arc<dyn EventJournal>) -> Vec<(i64, String)> {
    journal
        .load_session_stream("sess-1", None, None)
        .await
        .unwrap()
        .into_iter()
        .map(|e| {
            let tag = serde_json::to_value(&e.kind).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            (e.session_seq, tag)
        })
        .collect()
}

/// prompt, started a1, settled a1, request_end, then a second turn.
async fn two_turns(storage: &SqliteStorage) -> (Arc<dyn EventJournal>, i64) {
    let journal = storage.event_journal();
    let accounting = SessionAccounting::hydrate("sess-1", journal.clone())
        .await
        .unwrap();
    let stream = SessionEventStream::new("sess-1", journal.clone(), Arc::new(EventFanout::new()));
    let recorder = AttemptRecorder::with_projection(stream.clone(), accounting);
    stream.emit_persisted(prompt("m1")).await.unwrap();
    let mut start = sample_start();
    start.attempt.attempt_id = "a1".into();
    let handle = recorder.begin(start).await.unwrap();
    let mut settled = sample_settlement();
    settled.attempt.attempt_id = "a1".into();
    recorder.settle(handle, settled).await.unwrap();
    stream.emit_persisted(request_end(derived())).await.unwrap();
    // Second turn begins at this prompt: the rewind frontier.
    let frontier = stream.emit_persisted(prompt("m2")).await.unwrap();
    stream.emit_persisted(request_end(derived())).await.unwrap();
    (journal, frontier.stream_seq)
}

#[tokio::test]
async fn range_deletion_keeps_accounting_and_drops_conversation() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (journal, _) = two_turns(&storage).await;
    let before = kinds(&journal).await;

    // Delete everything from the very first event.
    let deleted = journal
        .delete_session_events_from("sess-1", 0)
        .await
        .unwrap();
    let after = kinds(&journal).await;

    let accounting: Vec<_> = before
        .iter()
        .filter(|(_, k)| k.starts_with("llm_attempt_"))
        .cloned()
        .collect();
    assert_eq!(accounting.len(), 2);
    assert_eq!(
        deleted, 4,
        "two prompts and two derived request-ends go; the ledger stays"
    );
    for kept in &accounting {
        assert!(after.contains(kept), "ledger record {kept:?} must survive");
    }
    assert!(!after.iter().any(|(_, k)| k == "prompt_received"));
    // Retained records keep their original numbers (gaps are valid).
    assert!(after.iter().all(|row| before.contains(row)));
}

#[tokio::test]
async fn rewind_leaves_incurred_totals_intact_without_renumbering() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let (journal, frontier) = two_turns(&storage).await;
    let live = SessionAccounting::hydrate("sess-1", journal.clone())
        .await
        .unwrap();
    let totals_before = live.read(|r| r.session().clone());
    assert!((totals_before.complete_total_usd().unwrap() - cost()).abs() < 1e-12);

    // Rewind to before turn 2 and before turn 1 (the generated turn).
    for from in [frontier, 0] {
        journal
            .delete_session_events_from("sess-1", from)
            .await
            .unwrap();
        let rehydrated = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap();
        assert_eq!(rehydrated.read(|r| r.session().clone()), totals_before);
    }
    let rows = kinds(&journal).await;
    let seqs: Vec<i64> = rows.iter().map(|(s, _)| *s).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "order preserved: {seqs:?}"
    );
    assert!(
        seqs.contains(&2) && seqs.contains(&3),
        "ledger numbers unchanged: {seqs:?}"
    );
}

#[tokio::test]
async fn legacy_request_end_evidence_is_retained_but_new_request_end_is_derived() {
    let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
    let journal = storage.event_journal();
    let append = |kind: AgentEventKind, seq: i64| {
        let journal = journal.clone();
        async move {
            journal
                .append_durable(&NewDurableEvent {
                    session_id: "sess-1".into(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    session_seq: Some(seq),
                    kind,
                })
                .await
                .unwrap();
        }
    };
    let derived = crate::accounting::RequestAccounting {
        operation_id: "op".into(),
        attempt_ids: vec!["a1".into()],
        known_subtotal_usd: 0.0,
        complete: false,
        reasons: vec![],
    };
    append(request_end(None), 1).await; // legacy: no accounting basis
    append(request_end(Some(derived)), 2).await; // derived presentation
    append(prompt("m1"), 3).await;

    journal
        .delete_session_events_from("sess-1", 0)
        .await
        .unwrap();
    let rows = kinds(&journal).await;
    assert_eq!(rows, vec![(1, "llm_request_end".to_string())]);
}

#[tokio::test]
async fn explicit_full_session_deletion_is_still_allowed() {
    let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
    let session = storage
        .session_store()
        .create_session(None, None, None, None)
        .await
        .unwrap();
    let id = session.public_id.clone();
    let journal = storage.event_journal();
    let stream = SessionEventStream::new(&id, journal.clone(), Arc::new(EventFanout::new()));
    let accounting = SessionAccounting::hydrate(&id, journal.clone())
        .await
        .unwrap();
    let recorder = AttemptRecorder::with_projection(stream, accounting);
    let mut start = sample_start();
    start.attempt.session_id = id.clone();
    let handle = recorder.begin(start).await.unwrap();
    let mut settled = sample_settlement();
    settled.attempt.session_id = id.clone();
    recorder.settle(handle, settled).await.unwrap();

    storage.session_store().delete_session(&id).await.unwrap();
    let remaining = journal.load_session_stream(&id, None, None).await.unwrap();
    // Whatever the schema does on session deletion, the guarantee is only
    // that range deletion never removes ledger rows; full deletion is the
    // documented exception and must not error.
    let _ = remaining;
}

/// Fork isolation (task 5.2). A fork copies conversation only: inherited
/// messages and their usage create no fork charges, new fork generation has
/// its own identities, and the source keeps its spend.
mod fork {
    use super::*;
    use crate::model::{AgentMessage, MessagePart};
    use crate::session::domain::ForkOrigin;
    use crate::session::store::SessionStore;
    use querymt::chat::ChatRole;

    struct Forked {
        storage: Arc<SqliteStorage>,
        source: String,
        fork: String,
    }

    async fn forked() -> Forked {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let source = storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        let mut message = AgentMessage::new(source.clone(), ChatRole::Assistant);
        message.id = "assistant-1".into();
        message.parts.push(MessagePart::Text {
            content: "generated".into(),
        });
        storage.add_message(&source, message).await.unwrap();

        // The source generated that message at a recorded cost.
        let journal = storage.event_journal();
        let accounting = SessionAccounting::hydrate(&source, journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new(&source, journal, Arc::new(EventFanout::new()));
        let recorder = AttemptRecorder::with_projection(stream.clone(), accounting);
        let mut start = sample_start();
        start.attempt.session_id = source.clone();
        let handle = recorder.begin(start).await.unwrap();
        let mut settled = sample_settlement();
        settled.attempt.session_id = source.clone();
        recorder.settle(handle, settled).await.unwrap();
        stream
            .emit_persisted(AgentEventKind::AssistantMessageStored {
                content: "generated".into(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("assistant-1".into()),
            })
            .await
            .unwrap();
        stream.emit_persisted(request_end(derived())).await.unwrap();

        let fork = storage
            .fork_session(&source, "assistant-1", ForkOrigin::User)
            .await
            .unwrap();
        Forked {
            storage,
            source,
            fork,
        }
    }

    #[tokio::test]
    async fn fork_inherits_conversation_but_no_accounting_records() {
        let f = forked().await;
        let journal = f.storage.event_journal();
        let copied = journal
            .load_session_stream(&f.fork, None, None)
            .await
            .unwrap();
        assert!(
            copied
                .iter()
                .any(|e| matches!(e.kind, AgentEventKind::AssistantMessageStored { .. })),
            "conversation is copied"
        );
        assert!(copied.iter().all(|e| !matches!(
            e.kind,
            AgentEventKind::LlmAttemptStarted { .. }
                | AgentEventKind::LlmAttemptSettled { .. }
                | AgentEventKind::LlmRequestEnd { .. }
        )));

        let fork_totals = SessionAccounting::hydrate(&f.fork, journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(fork_totals, Default::default(), "no inherited charges");
        assert_eq!(fork_totals.complete_total_usd(), None);
        assert_eq!(fork_totals.unresolved_attempts, 0);
    }

    #[tokio::test]
    async fn source_keeps_its_spend_and_fork_generation_is_independent() {
        let f = forked().await;
        let journal = f.storage.event_journal();
        let source_before = SessionAccounting::hydrate(&f.source, journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert!((source_before.complete_total_usd().unwrap() - cost()).abs() < 1e-12);

        // New generation in the fork gets its own identities and scope.
        let fork_accounting = SessionAccounting::hydrate(&f.fork, journal.clone())
            .await
            .unwrap();
        let stream =
            SessionEventStream::new(&f.fork, journal.clone(), Arc::new(EventFanout::new()));
        let recorder = AttemptRecorder::with_projection(stream, fork_accounting.clone());
        let mut start = sample_start();
        start.attempt.session_id = f.fork.clone();
        start.attempt.attempt_id = "fork-attempt".into();
        let handle = recorder.begin(start).await.unwrap();
        let mut settled = sample_settlement();
        settled.attempt.session_id = f.fork.clone();
        settled.attempt.attempt_id = "fork-attempt".into();
        recorder.settle(handle, settled).await.unwrap();

        let fork_totals = fork_accounting.read(|r| r.session().clone());
        assert_eq!(fork_totals.priced_attempts, 1);
        assert!((fork_totals.known_subtotal_usd - cost()).abs() < 1e-12);

        // Source is unchanged by fork spend, and not charged for it.
        let source_after = SessionAccounting::hydrate(&f.source, journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(source_after, source_before);

        // The parent's reducer refuses a record owned by the fork.
        let parent = SessionAccounting::hydrate(&f.source, journal.clone())
            .await
            .unwrap();
        let fork_events = journal
            .load_session_stream(&f.fork, None, None)
            .await
            .unwrap();
        for event in &fork_events {
            assert_eq!(
                parent.observe(event),
                Ok(crate::accounting::Applied::NotAccounting)
            );
        }
        assert_eq!(parent.read(|r| r.session().clone()), source_before);
    }

    #[tokio::test]
    async fn delegated_child_fork_does_not_charge_the_parent_scope() {
        let f = forked().await;
        let child = f
            .storage
            .fork_session(&f.source, "assistant-1", ForkOrigin::Delegation)
            .await
            .unwrap();
        let journal = f.storage.event_journal();
        let child_totals = SessionAccounting::hydrate(&child, journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(child_totals, Default::default());
        // Parent budget scope (its own session reducer) is untouched.
        let parent = SessionAccounting::hydrate(&f.source, journal)
            .await
            .unwrap();
        assert_eq!(parent.read(|r| r.session().priced_attempts), 1);
    }
}

/// Historical coverage (task 5.3): local generation with no accounting basis
/// is disclosed as unaccounted, never repriced or trusted.
mod coverage {
    use super::*;
    use crate::accounting::AccountingReason;
    use crate::session::domain::ForkOrigin;
    use crate::session::store::SessionStore;

    async fn append(
        journal: &Arc<dyn EventJournal>,
        session: &str,
        seq: i64,
        kind: AgentEventKind,
    ) {
        journal
            .append_durable(&NewDurableEvent {
                session_id: session.into(),
                origin: EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                session_seq: Some(seq),
                kind,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn legacy_shaped_history_is_partial_and_untrusted() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        // Legacy request-ends claim a cost (1.0) that must not be trusted.
        append(&journal, "sess-1", 1, prompt("m1")).await;
        append(&journal, "sess-1", 2, request_end(None)).await;
        append(&journal, "sess-1", 3, request_end(None)).await;

        let totals = SessionAccounting::hydrate("sess-1", journal)
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(totals.unaccounted_generations, 2);
        assert_eq!(
            totals.known_subtotal_usd, 0.0,
            "old amounts are not trusted"
        );
        assert_eq!(totals.complete_total_usd(), None);
        assert!(
            totals
                .reasons()
                .contains(&AccountingReason::UnaccountedHistory)
        );
    }

    #[test]
    fn documented_reason_name_matches_serialization() {
        // docs/docs/agent/event-ordering.md names this reason.
        let json = serde_json::to_value(AccountingReason::UnaccountedHistory).unwrap();
        assert_eq!(json["kind"], "unaccounted_history");
        let doc = include_str!("../../../../docs/docs/agent/event-ordering.md");
        assert!(doc.contains("`unaccounted_history`"));
        for forbidden in [
            "delete the charge",
            "delete accounting",
            "rewrite the database",
        ] {
            assert!(
                !doc.to_lowercase().contains(&forbidden.to_lowercase())
                    || doc.contains("Do not delete accounting events"),
                "docs must not instruct removing charges: {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn new_spend_on_a_legacy_session_cannot_claim_a_complete_total() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let journal = storage.event_journal();
        append(&journal, "sess-1", 1, request_end(None)).await;
        let accounting = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap();
        let stream = SessionEventStream::new("sess-1", journal, Arc::new(EventFanout::new()));
        let recorder = AttemptRecorder::with_projection(stream, accounting.clone());
        let handle = recorder.begin(sample_start()).await.unwrap();
        recorder.settle(handle, sample_settlement()).await.unwrap();

        let totals = accounting.read(|r| r.session().clone());
        assert!((totals.known_subtotal_usd - cost()).abs() < 1e-12);
        assert_eq!(
            totals.complete_total_usd(),
            None,
            "lifetime total is partial"
        );
    }

    #[tokio::test]
    async fn derived_request_ends_and_empty_sessions_are_clean() {
        let storage = SqliteStorage::connect(":memory:".into()).await.unwrap();
        let (journal, _) = two_turns(&storage).await;
        let totals = SessionAccounting::hydrate("sess-1", journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(totals.unaccounted_generations, 0);
        assert!(totals.complete_total_usd().is_some());

        let empty = SessionAccounting::hydrate("never-used", journal)
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(empty, Default::default());
        assert!(empty.reasons().is_empty());
    }

    #[tokio::test]
    async fn fork_of_a_legacy_session_with_inherited_history_stays_clean() {
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let source = storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        let mut message =
            crate::model::AgentMessage::new(source.clone(), querymt::chat::ChatRole::Assistant);
        message.id = "assistant-1".into();
        message.parts.push(crate::model::MessagePart::Text {
            content: "old".into(),
        });
        storage.add_message(&source, message).await.unwrap();
        let journal = storage.event_journal();
        append(
            &journal,
            &source,
            1,
            AgentEventKind::AssistantMessageStored {
                content: "old".into(),
                thinking: None,
                reasoning_parts: Vec::new(),
                message_id: Some("assistant-1".into()),
            },
        )
        .await;
        append(&journal, &source, 2, request_end(None)).await;

        let source_totals = SessionAccounting::hydrate(&source, journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(source_totals.unaccounted_generations, 1);

        let fork = storage
            .fork_session(&source, "assistant-1", ForkOrigin::User)
            .await
            .unwrap();
        let fork_totals = SessionAccounting::hydrate(&fork, journal)
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(
            fork_totals,
            Default::default(),
            "inherited history is not local generation"
        );
    }
}
