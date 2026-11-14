//! Stream cursor: how far a client has read the local and remote event streams.

use crate::events::AgentEvent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamCursor {
    #[typeshare(serialized_as = "number")]
    pub local_seq: i64,
    #[typeshare(serialized_as = "Record<string, number>")]
    pub remote_seq_by_source: HashMap<String, i64>,
}

/// Fold one event into a stream cursor.
pub fn observe_cursor(cursor: &mut StreamCursor, event: &AgentEvent) {
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
