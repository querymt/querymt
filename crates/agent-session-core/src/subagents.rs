//! Subagents: `subagent_update` patches and tool calls that reference a child
//! session.

use crate::core::SessionCore;
use crate::view::SubagentView;
use querymt_agent_protocol_types::protocol::{MaybeUndefined, SubagentUpdate, ToolCallContent};
use querymt_agent_protocol_types::replay::QUERYMT_META_NAMESPACE;

/// Apply an ACP patch field: omitted keeps the value, `null` clears it.
fn patch<T, U>(slot: &mut Option<U>, patch: &MaybeUndefined<T>, convert: impl FnOnce(&T) -> U) {
    match patch {
        MaybeUndefined::Undefined => {}
        MaybeUndefined::Null => *slot = None,
        MaybeUndefined::Value(value) => *slot = Some(convert(value)),
    }
}

/// `_meta.querymt.stage` of a `_meta` object.
fn stage_of(meta: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    meta.get(QUERYMT_META_NAMESPACE)?
        .get("stage")?
        .as_str()
        .map(str::to_owned)
}

impl SessionCore {
    /// `subagent_update` upserts the child by session id and patches the
    /// fields it carries; every other field keeps its value.
    pub(crate) fn subagent_update(&mut self, update: &SubagentUpdate) {
        let session_id = update.session_id.0.to_string();
        let index = match self
            .header
            .subagents
            .iter()
            .position(|subagent| subagent.session_id == session_id)
        {
            Some(index) => index,
            None => {
                self.header.subagents.push(SubagentView {
                    session_id,
                    ..SubagentView::default()
                });
                self.header.subagents.len() - 1
            }
        };
        let subagent = &mut self.header.subagents[index];
        patch(&mut subagent.title, &update.title, String::clone);
        patch(
            &mut subagent.description,
            &update.description,
            String::clone,
        );
        patch(&mut subagent.capabilities, &update.capabilities, |value| {
            serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
        });
        patch(&mut subagent.cwd, &update.cwd, |path| {
            path.to_string_lossy().into_owned()
        });
        patch(
            &mut subagent.additional_directories,
            &update.additional_directories,
            |paths| {
                paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect()
            },
        );
        patch(&mut subagent.meta, &update.meta, |meta| {
            serde_json::Value::Object(meta.clone())
        });

        // The state is a whole-object snapshot; its stage rides in the
        // snapshot's own `_meta` or in the update's.
        match &update.state {
            MaybeUndefined::Undefined => {}
            MaybeUndefined::Null => {
                subagent.state = None;
                subagent.stage = None;
            }
            MaybeUndefined::Value(state) => {
                let wire = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
                subagent.state = wire
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                let state_meta = wire.get("_meta").and_then(serde_json::Value::as_object);
                let update_meta = match &update.meta {
                    MaybeUndefined::Value(meta) => Some(meta),
                    _ => None,
                };
                subagent.stage = update_meta
                    .and_then(stage_of)
                    .or_else(|| state_meta.and_then(stage_of));
            }
        }
    }

    /// Link the tool call to every child session its content references.
    pub(crate) fn link_session_content(&mut self, tool_id: &str, content: &[ToolCallContent]) {
        for item in content {
            if let ToolCallContent::Session(reference) = item {
                self.link_child_session(tool_id, &reference.session_id.0);
            }
        }
    }
}
