use crate::agent::LocalAgentHandle;
use crate::events::AgentEvent;
use crate::session::error::SessionResult;
use crate::session::projection::{AuditView, ViewStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamCursor {
    #[typeshare(serialized_as = "number")]
    pub local_seq: i64,
    #[typeshare(serialized_as = "Record<string, number>")]
    pub remote_seq_by_source: HashMap<String, i64>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLoadSnapshot {
    pub audit: AuditView,
    pub cursor: StreamCursor,
}

pub fn cursor_from_events(events: &[AgentEvent]) -> StreamCursor {
    let mut cursor = StreamCursor::default();

    for event in events {
        match event.origin {
            crate::events::EventOrigin::Local => {
                cursor.local_seq = cursor.local_seq.max(event.seq);
            }
            crate::events::EventOrigin::Remote => {
                if let Some(source) = event.source_node.as_ref() {
                    cursor
                        .remote_seq_by_source
                        .entry(source.clone())
                        .and_modify(|seq| *seq = (*seq).max(event.seq))
                        .or_insert(event.seq);
                }
            }
            crate::events::EventOrigin::Unknown(_) => {
                cursor.local_seq = cursor.local_seq.max(event.seq);
            }
        }
    }

    cursor
}

pub async fn load_session_snapshot(
    agent: &LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
) -> SessionResult<SessionLoadSnapshot> {
    let is_remote_attached = {
        let registry = agent.registry.lock().await;
        registry.get(session_id).is_some_and(|r| r.is_remote())
    };

    // Load the same snapshot the web UI uses. Remote attached sessions may not
    // have a full local projection row yet, so fall back to journal events.
    let audit = match view_store.get_audit_view(session_id, false).await {
        Ok(audit) => audit,
        Err(e) if is_remote_attached => {
            let events: Vec<AgentEvent> = agent
                .config
                .event_sink
                .journal()
                .load_session_stream(session_id, None, None)
                .await?
                .into_iter()
                .map(AgentEvent::from)
                .collect();

            tracing::debug!(
                session_id,
                error = %e,
                event_count = events.len(),
                "remote session missing local audit projection; loaded journal-backed snapshot"
            );

            AuditView {
                session_id: session_id.to_string(),
                events,
                tasks: Vec::new(),
                intent_snapshots: Vec::new(),
                decisions: Vec::new(),
                progress_entries: Vec::new(),
                artifacts: Vec::new(),
                delegations: Vec::new(),
                generated_at: OffsetDateTime::now_utc(),
            }
        }
        Err(e) => return Err(e),
    };

    let cursor = cursor_from_events(&audit.events);
    Ok(SessionLoadSnapshot { audit, cursor })
}
