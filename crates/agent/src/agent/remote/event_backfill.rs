//! Cursor-based event backfill for remote sessions (plan §10/§16).
//!
//! After a fresh attachment commit, the local journal may be missing durable
//! events that the host emitted while the relay was disconnected. Backfill:
//!
//! 1. Reads the last persisted source cursor `(session_id, source_node_id)`.
//! 2. Pages the host stream via `GetEventStreamSince` after that cursor.
//! 3. Inserts through the same idempotent source-identity path as the live
//!    relay (`EventJournal::append_durable_from_source`), so overlap between
//!    the live subscription and historical pages is deduplicated — never
//!    double-inserted, never republished twice.
//! 4. Publishes an ephemeral `RemoteSessionSyncCompleted` when done.
//!
//! Ordering guarantee: the live subscription is established during attachment
//! prepare (before commit), and backfill starts after commit — newly generated
//! events cannot be lost, and overlap is deduplicated. Prefer overlap over gap.
//!
//! Mixed-version: a host that predates cursor backfill fails the page query
//! with a typed transport failure (unknown message). That marks history
//! potentially stale but never fails the established connection (plan
//! compatibility rule).
//!
//! Legacy boundary (§16): when no source cursor exists for the session/node,
//! source sequences are never guessed. The first post-upgrade successful
//! attachment is the new synchronization boundary; the authoritative snapshot
//! served at open covers that opening, and every subsequent event persists
//! with a cursor.

use std::sync::Arc;

use tracing::Instrument;

use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, EventOrigin};
use crate::session::projection::NewDurableEvent;

use super::actor_ref::SessionActorRef;

/// Page size for backfill requests. Bounded so a large gap cannot produce a
/// single huge message; the loop pages until the host tip is reached. Pinned
/// to the host-side `MAX_EVENT_STREAM_PAGE_SIZE` cap so every page is served
/// in full.
const BACKFILL_PAGE_SIZE: usize = crate::agent::messages::MAX_EVENT_STREAM_PAGE_SIZE;

/// Backfill durable events for a freshly (re)attached remote session.
///
/// Spawn-and-forget: must never block connection establishment nor fail it.
pub(crate) async fn backfill_remote_events(
    event_sink: Arc<EventSink>,
    session_ref: SessionActorRef,
    session_id: String,
    source_node_id: String,
    peer_label: String,
    attachment_id: u64,
) {
    let span = tracing::info_span!(
        "remote.session.sync",
        session_id = %session_id,
        node_id = %source_node_id,
        peer_label = %peer_label,
        attachment_id,
    );
    async move {
        let journal = event_sink.journal().clone();

        // §16.1: last persisted source sequence for this session/node.
        let cursor = match journal
            .latest_source_seq(&session_id, &source_node_id)
            .await
        {
            Ok(cursor) => cursor,
            Err(error) => {
                tracing::warn!(
                    target: "remote::event_backfill",
                    session_id = %session_id,
                    node_id = %source_node_id,
                    attachment_id,
                    error = %error,
                    "backfill failed to read the persisted source cursor"
                );
                return;
            }
        };

        let Some(mut after) = cursor else {
            // §16 legacy boundary: rows persisted before source identity
            // existed carry no cursor. Do not guess source sequences — treat
            // this first post-upgrade attachment as a new synchronization
            // boundary; cursor-based persistence is guaranteed for every
            // event from now on.
            tracing::info!(
                target: "remote::event_backfill",
                session_id = %session_id,
                node_id = %source_node_id,
                attachment_id,
                "backfill skipped: no source cursor for session/node (legacy sync boundary)"
            );
            event_sink.emit_ephemeral_with_origin(
                &session_id,
                AgentEventKind::RemoteSessionSyncCompleted {
                    backfilled: 0,
                    boundary: true,
                    node_id: Some(source_node_id.clone()),
                },
                EventOrigin::Local,
                None,
            );
            return;
        };

        tracing::info!(
            target: "remote::event_backfill",
            session_id = %session_id,
            node_id = %source_node_id,
            attachment_id,
            after,
            "backfill started"
        );

        let mut backfilled: u64 = 0;
        let mut duplicates: u64 = 0;
        loop {
            let page = match session_ref
                .get_event_stream_since(Some(after), BACKFILL_PAGE_SIZE)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    // Unsupported backfill (mixed-version host) and transient
                    // transport failures both mark history potentially stale;
                    // neither fails the established connection (plan compat).
                    match error.transport_failure() {
                        Some(failure) => tracing::info!(
                            target: "remote::event_backfill",
                            session_id = %session_id,
                            node_id = %source_node_id,
                            attachment_id,
                            failure_kind = ?failure.kind,
                            delivery = ?failure.delivery,
                            "backfill unavailable; history may be stale"
                        ),
                        None => tracing::warn!(
                            target: "remote::event_backfill",
                            session_id = %session_id,
                            node_id = %source_node_id,
                            attachment_id,
                            error = %error,
                            "backfill failed; history may be stale"
                        ),
                    }
                    return;
                }
            };

            let tip = page.latest_source_seq;
            let last_seq = page.events.last().map(|event| event.seq);

            for event in page.events {
                if event.seq <= 0 {
                    // No usable source cursor on this event; cannot dedup.
                    continue;
                }
                let new_event = NewDurableEvent {
                    session_id: session_id.clone(),
                    origin: EventOrigin::Remote,
                    // Display metadata only (§15); live relay uses the same
                    // label, so backfilled rows look identical to live rows.
                    source_node: event
                        .source_node
                        .clone()
                        .or_else(|| Some(peer_label.clone())),
                    source_node_id: Some(source_node_id.clone()),
                    source_seq: Some(event.seq),
                    kind: event.kind.clone(),
                };
                match journal.append_durable_from_source(&new_event).await {
                    Ok(Some(_)) => backfilled += 1,
                    // Overlap with the live relay deduplicated (§16.4/§16.5).
                    Ok(None) => duplicates += 1,
                    Err(error) => {
                        tracing::warn!(
                            target: "remote::event_backfill",
                            session_id = %session_id,
                            node_id = %source_node_id,
                            attachment_id,
                            error = %error,
                            "backfill failed to persist page; history may be stale"
                        );
                        return;
                    }
                }
            }

            match last_seq {
                // Reached the host tip (§16.5).
                Some(seq) if seq >= tip => break,
                // Defend against non-monotonic pages: the cursor must advance,
                // otherwise the same page would be requested forever.
                Some(seq) if seq > after => after = seq,
                Some(seq) => {
                    tracing::warn!(
                        target: "remote::event_backfill",
                        session_id = %session_id,
                        node_id = %source_node_id,
                        attachment_id,
                        after,
                        page_last_seq = seq,
                        "backfill stopped: host page did not advance the cursor"
                    );
                    break;
                }
                // Empty page: stream exhausted.
                None => break,
            }
        }

        tracing::info!(
            target: "remote::event_backfill",
            session_id = %session_id,
            node_id = %source_node_id,
            attachment_id,
            backfilled,
            duplicates,
            "backfill completed"
        );
        event_sink.emit_ephemeral_with_origin(
            &session_id,
            AgentEventKind::RemoteSessionSyncCompleted {
                backfilled,
                boundary: false,
                node_id: Some(source_node_id.clone()),
            },
            EventOrigin::Local,
            None,
        );
    }
    .instrument(span)
    .await;
}
