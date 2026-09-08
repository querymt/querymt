//! Historical synchronization uses a durable checkpoint captured before subscription.
//! Live events cannot advance it; successful pages advance it only after persistence.
//! Cursorless sessions fetch authoritative host pages from the beginning, without
//! assigning guessed source identities to legacy rows. Fresh events publish once.

use std::sync::Arc;

use tracing::Instrument;

use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, EventOrigin};

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
    cursor: Option<i64>,
) -> Result<(), crate::error::AgentError> {
    let span = tracing::info_span!(
        "remote.session.sync",
        session_id = %session_id,
        node_id = %source_node_id,
        attachment_id,
    );
    async move {
        let journal = event_sink.journal();
        // This checkpoint was captured BEFORE subscription. Never replace it
        // with MAX(source_seq): a live event may already be beyond a gap.
        let mut after = cursor.unwrap_or(0);
        let boundary = cursor.is_none();
        let mut backfilled = 0;
        loop {
            let page = match session_ref
                .get_event_stream_since(Some(after), BACKFILL_PAGE_SIZE)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(session_id, %error, "backfill unavailable; history may be stale");
                    return Err(error);
                }
            };
            let last = page.events.last().map(|event| event.seq);
            if last.is_some_and(|seq| seq <= after)
                || (last.is_none() && page.latest_source_seq > after)
            {
                tracing::warn!(session_id, after, "backfill page did not advance; retaining checkpoint");
                return Err(crate::error::AgentError::Internal("invalid backfill page".into()));
            }
            let mut previous = after;
            for event in page.events {
                if event.seq <= previous || event.session_id != session_id {
                    tracing::warn!(session_id, "invalid backfill page; retaining checkpoint");
                    return Err(crate::error::AgentError::Internal("invalid backfill page".into()));
                }
                previous = event.seq;
                match event_sink.emit_durable_from_source(
                    &session_id,
                    event.kind,
                    Some(peer_label.clone()),
                    source_node_id.clone(),
                    event.seq,
                ).await {
                    Ok(Some(_)) => backfilled += 1,
                    Ok(None) => {},
                    Err(error) => {
                        tracing::warn!(session_id, %error, "backfill persistence failed; retaining checkpoint");
                        return Err(crate::error::AgentError::Internal(error.to_string()));
                    }
                }
            }
            after = last.unwrap_or(after);
            let complete = after >= page.latest_source_seq;
            if let Err(error) = journal.advance_remote_sync_cursor(
                &session_id, &source_node_id, after, complete,
            ).await {
                tracing::warn!(session_id, %error, "backfill checkpoint failed");
                return Err(crate::error::AgentError::Internal(error.to_string()));
            }
            if complete { break; }
        }
        tracing::info!(session_id, backfilled, after, "backfill completed");
        event_sink.emit_ephemeral_with_origin(
            &session_id,
            AgentEventKind::RemoteSessionSyncCompleted {
                backfilled,
                boundary,
                node_id: Some(source_node_id),
            },
            EventOrigin::Local,
            None,
        );
        Ok(())
    }.instrument(span).await
}
