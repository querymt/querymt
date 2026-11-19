//! `snapshot_loaded`: parse the load response and rebuild the view from it.

use crate::decode::{Decoded, Event, decode_event};
use crate::view::InputError;
use querymt_agent_protocol_types::accounting::AccountingSnapshot;
use querymt_agent_protocol_types::delegation_update::DelegationUpdateNotification;
use querymt_agent_protocol_types::runtime_status::SessionRuntimeStatus;
use querymt_agent_protocol_types::session::history::{
    HistoryInfo, SessionHistorySummary, TruncatedEntry,
};
use querymt_agent_protocol_types::session::load_snapshot::UserPromptRecord;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Key of the snapshot inside the load response's `_meta`.
pub(crate) const SNAPSHOT_META_KEY: &str = "querymt/sessionLoadSnapshot.v1";

/// A snapshot whose envelope decoded; events are decoded one by one.
pub(crate) struct ParsedSnapshot {
    pub events: Vec<Event>,
    /// Unknown event kinds skipped while decoding.
    pub unknown: u32,
    /// Per-event decode failures, with their `seq`.
    pub event_errors: Vec<InputError>,
    pub runtime_status: SessionRuntimeStatus,
    pub accounting: AccountingSnapshot,
    pub delegation_updates: Vec<DelegationUpdateNotification>,
    pub user_prompts: Vec<UserPromptRecord>,
    pub history: Option<HistoryInfo>,
    pub summary: Option<SessionHistorySummary>,
    pub truncated: Vec<TruncatedEntry>,
}

/// Decode a field that must be present.
pub(crate) fn required<T: DeserializeOwned>(value: &Value, key: &str) -> Result<T, String> {
    let field = value
        .get(key)
        .ok_or_else(|| format!("snapshot has no `{key}`"))?;
    serde_json::from_value(field.clone()).map_err(|error| format!("snapshot `{key}`: {error}"))
}

/// Decode a field that may be absent or null.
pub(crate) fn optional<T: DeserializeOwned>(value: &Value, key: &str) -> Result<Option<T>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(field) => serde_json::from_value(field.clone())
            .map(Some)
            .map_err(|error| format!("snapshot `{key}`: {error}")),
    }
}

/// Decode a list of events one by one: unknown kinds are counted, failures
/// are collected with their `seq`, and the rest is kept.
pub(crate) fn decode_events(raw: &[Value], input_index: u32) -> (Vec<Event>, u32, Vec<InputError>) {
    let mut events = Vec::with_capacity(raw.len());
    let mut unknown = 0;
    let mut errors = Vec::new();
    for value in raw {
        match decode_event(value) {
            (_, Decoded::Value(event)) => events.push(event),
            (_, Decoded::Unknown) => unknown += 1,
            (seq, Decoded::Error(message)) => errors.push(InputError {
                input_index,
                seq,
                message,
            }),
        }
    }
    (events, unknown, errors)
}

/// The snapshot object of a load response, if it carries one.
pub(crate) fn snapshot_value(response: &Value) -> Option<&Value> {
    response.get("_meta")?.get(SNAPSHOT_META_KEY)
}

/// Parse a snapshot. An envelope failure rejects the whole input.
pub(crate) fn parse_snapshot(snapshot: &Value, input_index: u32) -> Result<ParsedSnapshot, String> {
    let raw_events = snapshot
        .get("audit")
        .and_then(|audit| audit.get("events"))
        .and_then(Value::as_array)
        .ok_or_else(|| "snapshot `audit.events` is not an array".to_owned())?;
    let (events, unknown, event_errors) = decode_events(raw_events, input_index);
    Ok(ParsedSnapshot {
        events,
        unknown,
        event_errors,
        runtime_status: required(snapshot, "runtimeStatus")?,
        accounting: required(snapshot, "accounting")?,
        delegation_updates: required(snapshot, "delegationUpdates")?,
        user_prompts: optional(snapshot, "userPrompts")?.unwrap_or_default(),
        history: optional(snapshot, "history")?,
        summary: optional(snapshot, "summary")?,
        truncated: optional(snapshot, "truncated")?.unwrap_or_default(),
    })
}

use crate::core::SessionCore;
use crate::project::event_updates;
use crate::store::Store;
use crate::view::ChangeReport;
use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::{ContentChunk, MessageId, SessionUpdate};
use std::collections::BTreeMap;

impl SessionCore {
    /// Apply a `session/load` response. A snapshot rebuilds the item store;
    /// a response without one keeps the items applied so far.
    pub(crate) fn snapshot_loaded(
        &mut self,
        response: &Value,
        index: u32,
        report: &mut ChangeReport,
    ) {
        let Some(snapshot) = snapshot_value(response) else {
            // No snapshot: keep the items applied so far (replayed updates)
            // and take config options and modes from the response.
            if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
                self.merge_config_options(options.clone());
            }
            if let Some(mode) = response
                .pointer("/modes/currentModeId")
                .and_then(Value::as_str)
            {
                self.header.current_mode = Some(mode.to_owned());
            }
            return;
        };
        let parsed = match parse_snapshot(snapshot, index) {
            Ok(parsed) => parsed,
            Err(message) => {
                report.errors.push(InputError {
                    input_index: index,
                    seq: None,
                    message,
                });
                return;
            }
        };
        report.unknown += parsed.unknown;
        report.errors.extend(parsed.event_errors.iter().cloned());
        let old_ids = self.store.turn_ids();
        self.store = Store::continuing(self.store.revision(), self.store.anon_counter());
        self.seqs.clear();
        self.replay_events(&parsed.events, &parsed.user_prompts);
        self.store.finish_rebuild(&old_ids);
        self.hydrate_header(&parsed);
        self.hydrate_summary(&parsed);
    }

    /// Feed journal events through the live handlers. Structured user
    /// prompts replace the plain prompt text of their message.
    pub(crate) fn replay_events(
        &mut self,
        events: &[crate::decode::Event],
        prompts: &[UserPromptRecord],
    ) {
        let by_message: BTreeMap<&str, &UserPromptRecord> = prompts
            .iter()
            .map(|record| (record.message_id.as_str(), record))
            .collect();
        for event in events {
            self.store.seq_hint = Some(event.seq);
            self.seqs.insert(event.seq);
            let updates = match &event.kind {
                AgentEventKind::PromptReceived {
                    message_id: Some(id),
                    ..
                } if by_message.contains_key(id.as_str()) => by_message[id.as_str()]
                    .blocks
                    .iter()
                    .map(|block| {
                        SessionUpdate::UserMessageChunk(
                            ContentChunk::new(block.clone())
                                .message_id(MessageId::from(id.clone())),
                        )
                    })
                    .collect(),
                _ => event_updates(event),
            };
            for update in updates {
                self.apply_update(update);
            }
        }
        self.store.seq_hint = None;
    }
}

impl SessionCore {
    /// Whole-session state from the snapshot: runtime, accounting,
    /// delegations and, for windowed loads, the summary and paging state.
    fn hydrate_header(&mut self, parsed: &ParsedSnapshot) {
        // Replayed events may have left a run state behind; the snapshot's
        // runtime status is authoritative.
        self.header.run_state = crate::view::RunState::Idle;
        self.header.active_tool_call_id = None;
        self.runtime_observed(&parsed.runtime_status);
        self.set_accounting(&parsed.accounting);
        self.header.delegations = parsed.delegation_updates.clone();
        self.header.history = match &parsed.history {
            Some(info) => crate::view::HistoryPaging {
                has_older: !info.complete,
                before: info.before.clone(),
                stale: info.stale.unwrap_or(false),
            },
            None => crate::view::HistoryPaging::default(),
        };
    }
}

impl SessionCore {
    /// Windowed loads: whole-session state comes from the summary, and
    /// shortened tool content is marked with its full size.
    fn hydrate_summary(&mut self, parsed: &ParsedSnapshot) {
        if let Some(summary) = &parsed.summary {
            let usage = &summary.usage;
            self.header.tokens = Some(crate::view::TokenUsageView {
                latest_request: usage.latest_request.clone(),
                session_totals: usage.session_totals.clone(),
                partial: usage.partial,
            });
            self.header.provider = summary.provider.clone();
            self.header.pending_inputs = summary.pending_inputs.clone();
            if let (Some(used), Some(size)) = (
                usage.context_tokens,
                summary.provider.as_ref().and_then(|p| p.context_limit),
            ) {
                self.header.context = Some(crate::view::ContextUsage { used, size });
            }
            for link in &summary.child_sessions {
                if let Some(tool_id) = &link.tool_call_id {
                    self.link_child_session(tool_id, &link.child_session_id);
                }
            }
        }
        for entry in &parsed.truncated {
            self.mark_truncated(entry);
        }
    }

    pub(crate) fn mark_truncated(&mut self, entry: &TruncatedEntry) {
        use querymt_agent_protocol_types::session::history::ToolContentPart;
        let Some(key) = self.store.idx.tool.get(&entry.tool_call_id).copied() else {
            return;
        };
        let (part, bytes) = (entry.part, entry.full_bytes);
        self.store.modify(key, |kind| {
            let crate::entry::EntryKind::ToolCall(call) = kind else {
                return false;
            };
            match part {
                ToolContentPart::Result => call.result_full_bytes = Some(bytes),
                ToolContentPart::Arguments => call.arguments_full_bytes = Some(bytes),
            }
            true
        });
    }
}
