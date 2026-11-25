//! `snapshot_loaded`: parse the load response and rebuild the view from it.

use crate::decode::{Decoded, Event, decode_event};
use crate::view::InputError;
use querymt_agent_protocol_types::accounting::AccountingSnapshot;
use querymt_agent_protocol_types::protocol::{CursorUpdate, SubagentUpdate};
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
    /// The latest `subagent_update` per child. A snapshot without the key
    /// (an agent that does not report subagents) has none; the old
    /// `delegationUpdates` key is not read.
    pub subagent_updates: Vec<SubagentUpdate>,
    /// The cursors of the returned window (`session.cursors` clients).
    pub cursor_updates: Vec<CursorUpdate>,
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

/// Decode a list of ACP updates under `key` one by one: an element that does
/// not decode is reported and skipped, the rest is kept. An absent key is an
/// empty list.
pub(crate) fn decode_update_list<T: DeserializeOwned>(
    holder: &Value,
    key: &str,
    input_index: u32,
    errors: &mut Vec<InputError>,
) -> Vec<T> {
    let mut error = |message: String| {
        errors.push(InputError {
            input_index,
            seq: None,
            message,
        });
    };
    let raw = match holder.get(key) {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::Array(raw)) => raw,
        Some(_) => {
            error(format!("`{key}` is not an array"));
            return Vec::new();
        }
    };
    let mut updates = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        match serde_json::from_value::<T>(value.clone()) {
            Ok(update) => updates.push(update),
            Err(failure) => error(format!("`{key}[{index}]`: {failure}")),
        }
    }
    updates
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
    let (events, unknown, mut event_errors) = decode_events(raw_events, input_index);
    let subagent_updates =
        decode_update_list(snapshot, "subagentUpdates", input_index, &mut event_errors);
    let cursor_updates =
        decode_update_list(snapshot, "cursorUpdates", input_index, &mut event_errors);
    Ok(ParsedSnapshot {
        events,
        unknown,
        event_errors,
        runtime_status: required(snapshot, "runtimeStatus")?,
        accounting: required(snapshot, "accounting")?,
        subagent_updates,
        cursor_updates,
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
use querymt_agent_protocol_types::replay::CompactionIds;
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
        let old_turns = self.store.all_turns();
        let client_local = self.client_local_prompts();
        self.store = Store::continuing(self.store.revision(), self.store.anon_counter());
        self.seqs.clear();
        self.replay_events(&parsed.events, &parsed.user_prompts);
        self.restore_client_local_prompts(client_local);
        self.store.finish_rebuild(&old_turns);
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
        let mut compaction_ids = CompactionIds::default();
        for event in events {
            self.store.seq_hint = Some(event.seq);
            self.seqs.insert(event.seq);
            // Events between a run's start and end belong to that run.
            if let AgentEventKind::RunStarted { run_id, .. } = &event.kind {
                self.store.active_run = Some(run_id.clone());
            }
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
                _ => event_updates(event, &mut compaction_ids),
            };
            for mut update in updates {
                // The agent's replay stamps the enclosing run on its updates
                // with this same function; a snapshot builds the same entries.
                if let Some(run_id) = &self.store.active_run {
                    querymt_agent_protocol_types::replay::stamp_run_id(&mut update, run_id);
                }
                self.apply_update(update);
            }
            if matches!(event.kind, AgentEventKind::RunCompleted { .. }) {
                self.store.active_run = None;
            }
        }
        self.store.seq_hint = None;
    }

    /// The pending and failed prompts the client holds: they are local to
    /// this client and survive a load.
    fn client_local_prompts(&self) -> Vec<crate::entry::Entry> {
        self.store
            .pending_turn_ids()
            .iter()
            .filter_map(|id| self.store.turn(id))
            .flat_map(|turn| turn.entries)
            .collect()
    }

    /// Put the client-local prompts back after the rebuilt turns.
    fn restore_client_local_prompts(&mut self, entries: Vec<crate::entry::Entry>) {
        for entry in entries {
            let crate::entry::EntryKind::UserMessage(user) = &entry.kind else {
                continue;
            };
            // A prompt settled without a turn (a native command) is not in
            // the agent's history; like its ephemeral reply, it is gone.
            if user.status == crate::entry::PromptStatus::Sent {
                continue;
            }
            let Some(prompt_id) = user.client_prompt_id.clone() else {
                continue;
            };
            let key = self.store.push(
                entry.id,
                entry.kind,
                crate::store::TurnHint::Pending(prompt_id.clone()),
            );
            self.store.idx.prompt.insert(prompt_id, key);
        }
    }
}

impl SessionCore {
    /// Whole-session state from the snapshot: runtime, accounting,
    /// subagents and, for windowed loads, the summary and paging state.
    fn hydrate_header(&mut self, parsed: &ParsedSnapshot) {
        // Replayed events may have left a run state behind; the snapshot's
        // runtime status is authoritative.
        self.header.run_state = crate::view::RunState::Idle;
        self.header.active_tool_call_id = None;
        self.header.runtime = Some(parsed.runtime_status.clone());
        self.apply_runtime_phase(&parsed.runtime_status);
        self.hydrate_active_time(parsed);
        self.set_accounting(&parsed.accounting);
        self.header.subagents.clear();
        for update in &parsed.subagent_updates {
            self.subagent_update(update);
        }
        // A load discards the cursors held and rebuilds them from its own.
        self.header.cursors.clear();
        for update in &parsed.cursor_updates {
            self.cursor_update(update);
        }
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
    /// Active time: the total of the ended runs from the summary, and the
    /// start of the run active now from the runtime status (both on the
    /// host's clock). A snapshot without a summary keeps the total held.
    fn hydrate_active_time(&mut self, parsed: &ParsedSnapshot) {
        let since = parsed
            .runtime_status
            .run_started_at_ms
            .and_then(|ms| i64::try_from(ms).ok());
        let total = parsed.summary.as_ref().map(|summary| summary.active_ms);
        if total.is_none() && since.is_none() && self.header.active_time.is_none() {
            self.active_start_is_host = false;
            return;
        }
        let held = self
            .header
            .active_time
            .as_ref()
            .map_or(0, |time| time.total_ms);
        self.header.active_time = Some(crate::view::ActiveTime {
            total_ms: total.unwrap_or(held),
            active_since_ms: since,
        });
        self.active_start_is_host = since.is_some();
    }
}

impl SessionCore {
    /// Whole-session state comes from the summary every snapshot carries,
    /// windowed or not; shortened tool content is marked with its full size.
    fn hydrate_summary(&mut self, parsed: &ParsedSnapshot) {
        if let Some(summary) = &parsed.summary {
            let usage = &summary.usage;
            // A session without a model request has no token breakdown, as
            // live where no `usage_update` arrived.
            self.header.tokens = (usage.request_count > 0).then(|| crate::view::TokenUsageView {
                latest_request: usage.latest_request.clone(),
                session_totals: usage.session_totals.clone(),
                partial: usage.partial,
            });
            self.header.provider = summary.provider.clone();
            self.header.pending_inputs = summary.pending_inputs.clone();
            // A measurement taken before the current model was selected does
            // not describe the new model (the agent's `reliable_used` rule).
            let stale = match (usage.context_position, &summary.provider) {
                (Some(position), Some(provider)) => position < provider.position,
                _ => false,
            };
            self.header.context = match (
                usage.context_tokens,
                summary.provider.as_ref().and_then(|p| p.context_limit),
            ) {
                (Some(used), Some(size)) if !stale => {
                    Some(crate::view::ContextUsage { used, size })
                }
                _ => None,
            };
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
