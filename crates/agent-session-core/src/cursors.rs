//! Session cursors: `cursor_update` patches and where each cursor sits.
//!
//! Cursors are opaque to the reducer. It never builds or parses a cursor id;
//! it keeps the first anchor, patches capabilities and metadata, and reports
//! whether the anchored entity is in the view.

use crate::core::SessionCore;
use crate::view::{CursorPlacement, CursorView};
use querymt_agent_protocol_types::protocol::{CursorUpdate, MaybeUndefined};
use serde_json::Value;

impl SessionCore {
    /// `cursor_update` upserts by cursor id. The first anchor is kept (a later
    /// one is ignored), capabilities and metadata patch: omitted keeps, `null`
    /// clears, a value replaces. An update for an unknown cursor without an
    /// anchor cannot be placed and is skipped.
    pub(crate) fn cursor_update(&mut self, update: &CursorUpdate) {
        let cursor_id = update.cursor_id.0.to_string();
        let index = match self
            .header
            .cursors
            .iter()
            .position(|cursor| cursor.cursor_id == cursor_id)
        {
            Some(index) => index,
            None => {
                let Some(anchor) = update
                    .anchor
                    .as_ref()
                    .and_then(|anchor| serde_json::to_value(anchor).ok())
                else {
                    return;
                };
                self.header.cursors.push(CursorView {
                    cursor_id,
                    anchor,
                    capabilities: None,
                    meta: None,
                    placement: None,
                });
                self.header.cursors.len() - 1
            }
        };
        let cursor = &mut self.header.cursors[index];
        match &update.capabilities {
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => cursor.capabilities = None,
            MaybeUndefined::Value(value) => cursor.capabilities = serde_json::to_value(value).ok(),
        }
        match &update.meta {
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => cursor.meta = None,
            MaybeUndefined::Value(meta) => cursor.meta = Some(Value::Object(meta.clone())),
        }
        self.refresh_cursor_placement();
    }

    /// Place every cursor next to the entry of its anchored entity; a cursor
    /// whose entity is not in the view stays unplaced, never placed by guess.
    pub(crate) fn refresh_cursor_placement(&mut self) {
        if self.header.cursors.is_empty() {
            return;
        }
        let placements: Vec<Option<CursorPlacement>> = self
            .header
            .cursors
            .iter()
            .map(|cursor| self.place(&cursor.anchor))
            .collect();
        for (cursor, placement) in self.header.cursors.iter_mut().zip(placements) {
            cursor.placement = placement;
        }
    }

    fn place(&self, anchor: &Value) -> Option<CursorPlacement> {
        let text = |key: &str| anchor.get(key).and_then(Value::as_str);
        let position = text("position")?;
        if position != "before" && position != "after" {
            return None;
        }
        let idx = &self.store.idx;
        // The entry's own id is read back from the store: an entry keeps the
        // id it was created with even when it later learns its message id.
        let key = match text("type")? {
            "user_message" => idx.user.get(text("messageId")?).copied(),
            "agent_message" => idx.assistant.get(text("messageId")?).copied(),
            "agent_thought" => {
                let id = text("messageId")?;
                let mut parts = idx
                    .reasoning
                    .range((id.to_owned(), String::new())..)
                    .take_while(|((message, _), _)| message == id)
                    .map(|(_, key)| *key);
                if position == "before" {
                    parts.next()
                } else {
                    parts.last()
                }
            }
            "tool_call" => idx.tool.get(text("toolCallId")?).copied(),
            "compaction" => idx.compaction.get(text("compactionId")?).copied(),
            // A kind this reducer does not know cannot be placed.
            _ => None,
        }?;
        Some(CursorPlacement {
            entry_id: self.store.entry_id(key)?.to_owned(),
            position: position.to_owned(),
        })
    }
}
