//! Windowed session history: the recent-turn window, whole-session summary,
//! tool-content truncation and full tool-content lookup.
//!
//! Everything here reads the event journal through paged reads
//! ([`EventJournal::load_history_page`]); the full event vector is never
//! materialised. Positions are *history positions*: `stream_seq` for local
//! sessions and the owner's `source_seq` for remote ones.

use crate::accounting::{AccountingReducer, ScopeTotals};
use crate::acp::protocol::ContentBlock;
use crate::control::delegation_notifications::{
    DelegationUpdateNotification, DelegationUpdateProjector,
};
use crate::events::{AgentEvent, AgentEventKind};
use crate::session::domain::ForkOrigin;
use crate::session::error::SessionResult;
use crate::session::load_snapshot::{StreamCursor, observe_cursor};
use crate::session::projection::{EventJournal, HistoryPageDirection};
use crate::session::turns::turn_start_indices;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use typeshare::typeshare;

pub const DEFAULT_WINDOW_TURNS: usize = 20;
pub const DEFAULT_WINDOW_BYTES: usize = 1024 * 1024;
pub const DEFAULT_TRUNCATE_BYTES: usize = 32 * 1024;
pub const MAX_WINDOW_TURNS: usize = 100;
pub const MAX_WINDOW_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TRUNCATE_BYTES: usize = 256 * 1024;

const BACKWARD_PAGE: usize = 256;
const SCAN_PAGE: usize = 1000;

/// A client's request for a recent-history window.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoadWindow {
    #[typeshare(serialized_as = "number")]
    pub max_turns: usize,
    #[typeshare(serialized_as = "number")]
    pub max_bytes: usize,
    #[typeshare(serialized_as = "number")]
    pub truncate_bytes: usize,
}

impl Default for LoadWindow {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_WINDOW_TURNS,
            max_bytes: DEFAULT_WINDOW_BYTES,
            truncate_bytes: DEFAULT_TRUNCATE_BYTES,
        }
    }
}

impl LoadWindow {
    /// Clamp every value into `1..=` its own agent-side maximum.
    pub fn clamped(self) -> Self {
        Self {
            max_turns: self.max_turns.clamp(1, MAX_WINDOW_TURNS),
            max_bytes: self.max_bytes.clamp(1, MAX_WINDOW_BYTES),
            truncate_bytes: self.truncate_bytes.clamp(1, MAX_TRUNCATE_BYTES),
        }
    }
}

/// Read `_meta.querymt.loadWindow` from a request's `_meta` map.
///
/// Returns `None` when the client did not ask for a window (or sent something
/// that is not an object), so the caller returns the full snapshot. Present
/// values are clamped into the agent's limits; omitted, non-numeric or
/// negative values fall back to the default for that field, and a numeric
/// value above `usize` range clamps to its maximum.
pub fn load_window_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<LoadWindow> {
    let window = meta?
        .get(crate::acp::shared::QUERYMT_META_NAMESPACE)?
        .get("loadWindow")?
        .as_object()?;
    let field = |key: &str, default: usize| -> usize {
        match window.get(key) {
            Some(value) => value
                .as_u64()
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                .or_else(|| {
                    // Fractional values (`20.5`) truncate; negatives and
                    // non-numbers use the default.
                    value
                        .as_f64()
                        .filter(|n| n.is_finite() && *n >= 0.0)
                        .map(|n| n as usize)
                })
                .unwrap_or(default),
            None => default,
        }
    };
    Some(
        LoadWindow {
            max_turns: field("maxTurns", DEFAULT_WINDOW_TURNS),
            max_bytes: field("maxBytes", DEFAULT_WINDOW_BYTES),
            truncate_bytes: field("truncateBytes", DEFAULT_TRUNCATE_BYTES),
        }
        .clamped(),
    )
}

/// Where the returned events sit in the session's history.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryInfo {
    /// No older history exists before the returned events.
    pub complete: bool,
    /// Opaque token for the `session/history` request that returns the turns
    /// preceding the returned events. Absent when `complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The owner was unreachable; the history was served from the local copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

/// Which part of a tool call a truncation marker or content request refers to.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContentPart {
    Result,
    Arguments,
}

/// A tool result or argument string that was shortened in a response.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncatedEntry {
    pub tool_call_id: String,
    pub part: ToolContentPart,
    /// JSON pointer to the shortened string inside the arguments; absent for
    /// results and for arguments that are not JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub full_bytes: u64,
}

/// Exclusive token categories, as in the live usage meta.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    #[typeshare(serialized_as = "number")]
    pub input_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub output_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub reasoning_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub cache_read_tokens: u64,
    #[typeshare(serialized_as = "number")]
    pub cache_write_tokens: u64,
}

impl UsageBreakdown {
    fn from_usage(usage: &querymt::Usage) -> Self {
        Self {
            input_tokens: u64::from(usage.input_tokens),
            output_tokens: u64::from(usage.output_tokens),
            reasoning_tokens: u64::from(usage.reasoning_tokens),
            cache_read_tokens: u64::from(usage.cache_read),
            cache_write_tokens: u64::from(usage.cache_write),
        }
    }

    fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
    }
}

/// Whole-session token usage (the live `_meta.querymt.usage` v1 fields) plus
/// the state a client needs to keep live usage correct after a window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryUsage {
    pub latest_request: Option<UsageBreakdown>,
    pub session_totals: Option<UsageBreakdown>,
    pub partial: bool,
    /// Number of main-request completions in the session.
    #[typeshare(serialized_as = "number")]
    pub request_count: u64,
    /// Context occupancy reported by the last request, and its position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_position: Option<i64>,
    /// Session-lifetime accounting totals from the shared reducer.
    #[typeshare(serialized_as = "any")]
    pub accounting: ScopeTotals,
    /// Usage of the events *before* the returned window. A consumer that
    /// replays the window's own events seeds from this to reach whole-session
    /// totals without counting any event twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_window: Option<PriorUsage>,
}

/// Usage accumulated over the part of history preceding a window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorUsage {
    pub session_totals: Option<UsageBreakdown>,
    pub partial: bool,
    #[typeshare(serialized_as = "number")]
    pub request_count: u64,
    #[typeshare(serialized_as = "any")]
    pub accounting: ScopeTotals,
}

struct PriorAcc {
    reducer: AccountingReducer,
    totals: Option<UsageBreakdown>,
    partial: bool,
    requests: u64,
}

/// The session's current model, from its last `ProviderChanged`.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryProvider {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub context_limit: Option<u64>,
    #[typeshare(serialized_as = "number")]
    pub position: i64,
}

/// A queued or steering input that has not started or been applied yet.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInputSummary {
    pub input_id: String,
    /// `"steer"` or `"queue"`.
    pub delivery: String,
    /// `"accepted"` or `"queued"`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "Option<number>")]
    pub position: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
    /// Acceptance time in milliseconds since the epoch.
    #[typeshare(serialized_as = "number")]
    pub created_at: i64,
}

/// A child session spawned by this session.
#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildSessionLink {
    pub child_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// State computed over the entire session, independent of the window.
#[typeshare]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistorySummary {
    pub usage: SummaryUsage,
    pub provider: Option<SummaryProvider>,
    pub pending_inputs: Vec<PendingInputSummary>,
    pub child_sessions: Vec<ChildSessionLink>,
}

/// Result of the whole-session streaming pass.
pub struct SummaryPass {
    pub summary: SessionHistorySummary,
    pub cursor: StreamCursor,
    pub accounting: crate::accounting::AccountingSnapshot,
    pub delegation_updates: Vec<DelegationUpdateNotification>,
}

/// One window of history (or one older-history page).
pub struct WindowRead {
    /// Events whose `seq` is their history position, ascending.
    pub events: Vec<AgentEvent>,
    pub history: HistoryInfo,
    pub truncated: Vec<TruncatedEntry>,
}

// ── history tokens ─────────────────────────────────────────────────────────

/// Encode "events before `position`, as of history epoch `epoch`".
pub fn history_token(epoch: u64, position: i64) -> String {
    format!("{epoch}.{position}")
}

pub fn parse_history_token(token: &str) -> Option<(u64, i64)> {
    let (epoch, position) = token.split_once('.')?;
    Some((epoch.parse().ok()?, position.parse().ok()?))
}

// ── truncation ─────────────────────────────────────────────────────────────

/// The longest prefix of `s` that is at most `max` bytes and ends on a char
/// boundary.
pub fn utf8_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn truncate_json_strings(
    value: &mut serde_json::Value,
    pointer: &mut String,
    limit: usize,
    tool_call_id: &str,
    out: &mut Vec<TruncatedEntry>,
) {
    match value {
        serde_json::Value::String(text) if text.len() > limit => {
            out.push(TruncatedEntry {
                tool_call_id: tool_call_id.to_string(),
                part: ToolContentPart::Arguments,
                path: (!pointer.is_empty()).then(|| pointer.clone()),
                full_bytes: text.len() as u64,
            });
            let kept = utf8_prefix(text, limit).len();
            text.truncate(kept);
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let len = pointer.len();
                pointer.push_str(&format!("/{index}"));
                truncate_json_strings(item, pointer, limit, tool_call_id, out);
                pointer.truncate(len);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&pointer_escape(key));
                truncate_json_strings(item, pointer, limit, tool_call_id, out);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// Shorten oversized tool results and string tool-call arguments in place.
/// Only the response copy is changed; the journal is never touched.
pub fn truncate_event(kind: &mut AgentEventKind, limit: usize) -> Vec<TruncatedEntry> {
    let mut out = Vec::new();
    match kind {
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            result,
            ..
        } if result.len() > limit => {
            out.push(TruncatedEntry {
                tool_call_id: tool_call_id.clone(),
                part: ToolContentPart::Result,
                path: None,
                full_bytes: result.len() as u64,
            });
            let kept = utf8_prefix(result, limit).len();
            result.truncate(kept);
        }
        AgentEventKind::ToolCallStart {
            tool_call_id,
            arguments,
            ..
        } if arguments.len() > limit => {
            match serde_json::from_str::<serde_json::Value>(arguments) {
                Ok(mut value) => {
                    truncate_json_strings(
                        &mut value,
                        &mut String::new(),
                        limit,
                        tool_call_id,
                        &mut out,
                    );
                    if !out.is_empty()
                        && let Ok(text) = serde_json::to_string(&value)
                    {
                        *arguments = text;
                    }
                }
                Err(_) => {
                    out.push(TruncatedEntry {
                        tool_call_id: tool_call_id.clone(),
                        part: ToolContentPart::Arguments,
                        path: None,
                        full_bytes: arguments.len() as u64,
                    });
                    let kept = utf8_prefix(arguments, limit).len();
                    arguments.truncate(kept);
                }
            }
        }
        _ => {}
    }
    out
}

// ── window walk ────────────────────────────────────────────────────────────

struct Item {
    position: i64,
    event: AgentEvent,
    size: usize,
    truncated: Vec<TruncatedEntry>,
}

/// Turn start indices in `items`. When the walk has not reached the start of
/// history, anything before the first run start belongs to a turn whose
/// beginning is unknown, so it never counts as a start.
fn turn_starts(items: &[Item], at_start: bool) -> Vec<usize> {
    let mut starts = turn_start_indices(items.iter().map(|item| &item.event.kind));
    if !at_start {
        match items
            .iter()
            .position(|item| matches!(item.event.kind, AgentEventKind::RunStarted { .. }))
        {
            Some(first) => starts.retain(|index| *index >= first),
            None => starts.retain(|index| *index != 0),
        }
    }
    starts
}

/// Read the most recent window of history strictly before `before` (`None` =
/// up to the tip): at most `window.max_turns` whole turns and
/// `window.max_bytes` of JSON after truncation, always at least one turn.
pub async fn read_window(
    journal: &dyn EventJournal,
    session_id: &str,
    source_node: Option<&str>,
    before: Option<i64>,
    epoch: u64,
    window: &LoadWindow,
) -> SessionResult<WindowRead> {
    let mut collected: Vec<Item> = Vec::new();
    let mut cursor = before;
    let mut at_start = false;
    loop {
        let page = journal
            .load_history_page(
                session_id,
                source_node,
                HistoryPageDirection::Backward { before: cursor },
                BACKWARD_PAGE,
            )
            .await?;
        if page.is_empty() {
            at_start = true;
            break;
        }
        cursor = Some(page[0].0);
        let short_page = page.len() < BACKWARD_PAGE;
        let items: Vec<Item> = page
            .into_iter()
            .map(|(position, durable)| {
                let mut event = AgentEvent::from(durable);
                event.seq = position;
                let truncated = truncate_event(&mut event.kind, window.truncate_bytes);
                let size = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len());
                Item {
                    position,
                    event,
                    size,
                    truncated,
                }
            })
            .collect();
        collected.splice(0..0, items);
        if short_page {
            at_start = true;
            break;
        }
        let starts = turn_starts(&collected, false);
        if let Some(&first) = starts.first() {
            let bytes: usize = collected[first..].iter().map(|item| item.size).sum();
            if starts.len() >= window.max_turns || bytes > window.max_bytes {
                break;
            }
        }
    }

    let mut starts = turn_starts(&collected, at_start);
    if at_start && !collected.is_empty() && starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    // Newest turns first, within both limits, but never fewer than one.
    let mut cut = collected.len();
    let mut taken = 0usize;
    let mut bytes = 0usize;
    for (index, &start) in starts.iter().enumerate().rev() {
        let end = starts.get(index + 1).copied().unwrap_or(collected.len());
        let size: usize = collected[start..end].iter().map(|item| item.size).sum();
        if taken >= window.max_turns || (taken > 0 && bytes + size > window.max_bytes) {
            break;
        }
        taken += 1;
        bytes += size;
        cut = start;
    }
    if taken == 0 {
        // No turn boundary at all (empty history, or only non-turn events).
        cut = 0;
    }
    let kept = collected.split_off(cut);
    let complete = at_start && cut == 0;
    let before_token = (!complete)
        .then(|| kept.first().map(|item| history_token(epoch, item.position)))
        .flatten();
    let mut truncated = Vec::new();
    let events = kept
        .into_iter()
        .map(|item| {
            truncated.extend(item.truncated);
            item.event
        })
        .collect();
    Ok(WindowRead {
        events,
        history: HistoryInfo {
            complete,
            before: before_token,
            stale: None,
        },
        truncated,
    })
}

// ── whole-session summary ──────────────────────────────────────────────────

#[derive(Default)]
struct SummaryAcc {
    latest: Option<UsageBreakdown>,
    totals: Option<UsageBreakdown>,
    partial: bool,
    requests: u64,
    context: Option<(u64, i64)>,
    provider: Option<SummaryProvider>,
    pending: Vec<PendingInputSummary>,
    children: Vec<ChildSessionLink>,
    delegation_tools: HashMap<String, String>,
}

impl SummaryAcc {
    fn upsert_pending(&mut self, input: PendingInputSummary) {
        match self
            .pending
            .iter_mut()
            .find(|existing| existing.input_id == input.input_id)
        {
            Some(existing) => *existing = input,
            None => self.pending.push(input),
        }
    }

    fn resolve_pending(&mut self, input_id: &str) {
        self.pending.retain(|input| input.input_id != input_id);
    }

    fn observe(&mut self, session_id: &str, position: i64, event: &AgentEvent) {
        match &event.kind {
            AgentEventKind::LlmRequestEnd {
                usage,
                context_tokens,
                ..
            } => {
                self.requests += 1;
                self.context = Some((*context_tokens, position));
                let request = usage.as_ref().map(UsageBreakdown::from_usage);
                match request.as_ref() {
                    Some(request) => self
                        .totals
                        .get_or_insert_with(UsageBreakdown::default)
                        .add(request),
                    None => self.partial = true,
                }
                self.latest = request;
            }
            AgentEventKind::ProviderChanged {
                provider,
                model,
                context_limit,
                provider_node_id,
                ..
            } => {
                self.provider = Some(SummaryProvider {
                    provider: provider.clone(),
                    model: model.clone(),
                    node: provider_node_id.clone(),
                    context_limit: *context_limit,
                    position,
                });
            }
            AgentEventKind::SteeringAccepted {
                run_id,
                input_id,
                position: queue_position,
                blocks,
                accepted_at_ms,
            } => self.upsert_pending(PendingInputSummary {
                input_id: input_id.clone(),
                delivery: "steer".into(),
                state: "accepted".into(),
                run_id: Some(run_id.clone()),
                position: Some(*queue_position),
                blocks: blocks.clone(),
                created_at: accepted_at_ms
                    .map_or(event.timestamp.saturating_mul(1000), |ms| ms as i64),
            }),
            AgentEventKind::InputQueued {
                input_id,
                position: queue_position,
                blocks,
                accepted_at_ms,
            } => self.upsert_pending(PendingInputSummary {
                input_id: input_id.clone(),
                delivery: "queue".into(),
                state: "queued".into(),
                run_id: None,
                position: Some(*queue_position),
                blocks: blocks.clone(),
                created_at: accepted_at_ms
                    .map_or(event.timestamp.saturating_mul(1000), |ms| ms as i64),
            }),
            AgentEventKind::SteeringApplied { input_id, .. }
            | AgentEventKind::SteeringDiscarded { input_id, .. }
            | AgentEventKind::QueuedInputStarted { input_id, .. }
            | AgentEventKind::QueuedInputDiscarded { input_id, .. } => {
                self.resolve_pending(input_id)
            }
            AgentEventKind::DelegationRequested {
                delegation,
                tool_call_id: Some(tool_call_id),
                ..
            } => {
                self.delegation_tools
                    .insert(delegation.public_id.clone(), tool_call_id.clone());
            }
            AgentEventKind::SessionForked {
                parent_session_id,
                child_session_id,
                origin,
                fork_point_ref,
                ..
            } if parent_session_id == session_id => {
                let delegation_id =
                    matches!(origin, ForkOrigin::Delegation).then(|| fork_point_ref.clone());
                if !self
                    .children
                    .iter()
                    .any(|child| child.child_session_id == *child_session_id)
                {
                    self.children.push(ChildSessionLink {
                        child_session_id: child_session_id.clone(),
                        delegation_id,
                        tool_call_id: None,
                    });
                }
            }
            _ => {}
        }
    }
}

fn is_delegation_event(kind: &AgentEventKind) -> bool {
    matches!(
        kind,
        AgentEventKind::DelegationRequested { .. }
            | AgentEventKind::SessionForked { .. }
            | AgentEventKind::DelegationStageChanged { .. }
            | AgentEventKind::DelegationCompleted { .. }
            | AgentEventKind::DelegationFailed { .. }
            | AgentEventKind::DelegationCancelled { .. }
    )
}

/// One streaming pass over the whole journal: cursor, accounting, delegation
/// state and the [`SessionHistorySummary`]. Memory is bounded by one page.
pub async fn summarize_history(
    journal: &dyn EventJournal,
    session_id: &str,
    source_node: Option<&str>,
    window_start: Option<i64>,
) -> SessionResult<SummaryPass> {
    let mut prior = window_start.map(|_| PriorAcc {
        reducer: AccountingReducer::new(session_id),
        totals: None,
        partial: false,
        requests: 0,
    });
    let mut reducer = AccountingReducer::new(session_id);
    let mut projector = DelegationUpdateProjector::default();
    let mut cursor = StreamCursor::default();
    let mut acc = SummaryAcc::default();
    let mut after = 0i64;
    loop {
        let page = journal
            .load_history_page(
                session_id,
                source_node,
                HistoryPageDirection::Forward { after },
                SCAN_PAGE,
            )
            .await?;
        let Some(&(last, _)) = page.last() else { break };
        let page_len = page.len();
        after = last;
        for (position, durable) in page {
            let event = AgentEvent::from(durable);
            observe_cursor(&mut cursor, &event);
            if let Err(conflict) = reducer.apply(&event.kind) {
                tracing::warn!(
                    session.id = %session_id,
                    attempt.id = %conflict.attempt_id,
                    detail = conflict.detail,
                    "skipping conflicting accounting record in snapshot"
                );
            }
            if is_delegation_event(&event.kind) {
                projector.project_event(&event);
            }
            acc.observe(session_id, position, &event);
            if let (Some(prior), Some(start)) = (prior.as_mut(), window_start)
                && position < start
            {
                let _ = prior.reducer.apply(&event.kind);
                if let AgentEventKind::LlmRequestEnd { usage, .. } = &event.kind {
                    prior.requests += 1;
                    match usage.as_ref().map(UsageBreakdown::from_usage) {
                        Some(request) => prior
                            .totals
                            .get_or_insert_with(UsageBreakdown::default)
                            .add(&request),
                        None => prior.partial = true,
                    }
                }
            }
        }
        if page_len < SCAN_PAGE {
            break;
        }
    }

    for child in &mut acc.children {
        child.tool_call_id = child
            .delegation_id
            .as_ref()
            .and_then(|id| acc.delegation_tools.get(id))
            .cloned();
    }
    let usage = SummaryUsage {
        latest_request: acc.latest,
        session_totals: acc.totals,
        partial: acc.partial,
        request_count: acc.requests,
        context_tokens: acc.context.map(|(tokens, _)| tokens),
        context_position: acc.context.map(|(_, position)| position),
        accounting: reducer.session().clone(),
        before_window: prior.map(|prior| PriorUsage {
            session_totals: prior.totals,
            partial: prior.partial,
            request_count: prior.requests,
            accounting: prior.reducer.session().clone(),
        }),
    };
    Ok(SummaryPass {
        accounting: crate::accounting::AccountingSnapshot::from_reducer(&reducer),
        delegation_updates: projector.snapshots(),
        cursor,
        summary: SessionHistorySummary {
            usage,
            provider: acc.provider,
            pending_inputs: acc.pending,
            child_sessions: acc.children,
        },
    })
}

// ── full tool content ──────────────────────────────────────────────────────

/// The complete result or arguments of one tool call, found by a bounded
/// forward scan of the session's journal. `None` when no such call exists.
pub async fn find_tool_content(
    journal: &dyn EventJournal,
    session_id: &str,
    source_node: Option<&str>,
    tool_call_id: &str,
    part: ToolContentPart,
) -> SessionResult<Option<String>> {
    let mut after = 0i64;
    loop {
        let page = journal
            .load_history_page(
                session_id,
                source_node,
                HistoryPageDirection::Forward { after },
                SCAN_PAGE,
            )
            .await?;
        let Some(&(last, _)) = page.last() else {
            return Ok(None);
        };
        let page_len = page.len();
        after = last;
        for (_, durable) in page {
            match (part, durable.kind) {
                (
                    ToolContentPart::Result,
                    AgentEventKind::ToolCallEnd {
                        tool_call_id: id,
                        result,
                        ..
                    },
                ) if id == tool_call_id => return Ok(Some(result)),
                (
                    ToolContentPart::Arguments,
                    AgentEventKind::ToolCallStart {
                        tool_call_id: id,
                        arguments,
                        ..
                    },
                ) if id == tool_call_id => return Ok(Some(arguments)),
                _ => {}
            }
        }
        if page_len < SCAN_PAGE {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventOrigin, ExecutionMetrics};
    use crate::session::projection::NewDurableEvent;
    use crate::session::sqlite_storage::SqliteStorage;

    const SID: &str = "s1";

    async fn append(journal: &dyn EventJournal, kind: AgentEventKind) {
        journal
            .append_durable(&NewDurableEvent {
                session_seq: None,
                session_id: SID.into(),
                origin: EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind,
            })
            .await
            .unwrap();
    }

    fn run(n: usize) -> AgentEventKind {
        AgentEventKind::RunStarted {
            run_id: format!("r{n}"),
            origin: "interactive".into(),
        }
    }

    fn request_end(input: u32) -> AgentEventKind {
        AgentEventKind::LlmRequestEnd {
            usage: Some(querymt::Usage {
                input_tokens: input,
                output_tokens: 1,
                reasoning_tokens: 0,
                cache_read: 0,
                cache_write: 0,
                cache_write_tiers: None,
            }),
            tool_calls: 0,
            finish_reason: None,
            cost_usd: None,
            cumulative_cost_usd: None,
            context_tokens: u64::from(input),
            metrics: ExecutionMetrics { steps: 1, turns: 1 },
            accounting: None,
        }
    }

    fn tool_end(id: &str, result: String) -> AgentEventKind {
        AgentEventKind::ToolCallEnd {
            tool_call_id: id.into(),
            tool_name: "shell".into(),
            is_error: false,
            result,
        }
    }

    /// `turns` runs of: run, prompt, request end.
    async fn seed_turns(journal: &dyn EventJournal, turns: usize) {
        for n in 0..turns {
            append(journal, run(n)).await;
            append(
                journal,
                AgentEventKind::PromptReceived {
                    content: format!("p{n}"),
                    message_id: None,
                },
            )
            .await;
            append(journal, request_end(10)).await;
        }
    }

    async fn storage() -> SqliteStorage {
        SqliteStorage::connect(":memory:".into()).await.unwrap()
    }

    #[tokio::test]
    async fn long_session_returns_last_turns_and_pages_back_exactly_once() {
        let s = storage().await;
        seed_turns(&s, 200).await;
        let window = LoadWindow::default();
        let first = read_window(&s, SID, None, None, 0, &window).await.unwrap();
        let runs = |events: &[AgentEvent]| {
            events
                .iter()
                .filter(|e| matches!(e.kind, AgentEventKind::RunStarted { .. }))
                .count()
        };
        assert_eq!(runs(&first.events), 20);
        assert!(!first.history.complete);
        assert!(matches!(
            first.events[0].kind,
            AgentEventKind::RunStarted { .. }
        ));

        let mut all: Vec<i64> = first.events.iter().map(|e| e.seq).collect();
        let mut token = first.history.before.clone();
        while let Some(t) = token {
            let (epoch, before) = parse_history_token(&t).unwrap();
            let page = read_window(&s, SID, None, Some(before), epoch, &window)
                .await
                .unwrap();
            let seqs: Vec<i64> = page.events.iter().map(|e| e.seq).collect();
            assert!(seqs.iter().all(|seq| *seq < all[0]));
            all.splice(0..0, seqs);
            token = page.history.before;
        }
        assert_eq!(all, (1..=600).collect::<Vec<i64>>());
    }

    #[tokio::test]
    async fn short_session_is_complete() {
        let s = storage().await;
        seed_turns(&s, 5).await;
        let read = read_window(&s, SID, None, None, 0, &LoadWindow::default())
            .await
            .unwrap();
        assert_eq!(read.events.len(), 15);
        assert!(read.history.complete);
        assert!(read.history.before.is_none());
    }

    #[tokio::test]
    async fn byte_cap_shortens_window_at_turn_boundary() {
        let s = storage().await;
        for n in 0..10 {
            append(&s, run(n)).await;
            append(&s, tool_end("t", "x".repeat(20_000))).await;
        }
        let window = LoadWindow {
            max_turns: 20,
            max_bytes: 50_000,
            truncate_bytes: 32 * 1024,
        };
        let read = read_window(&s, SID, None, None, 0, &window).await.unwrap();
        assert!(!read.history.complete);
        assert!(matches!(
            read.events[0].kind,
            AgentEventKind::RunStarted { .. }
        ));
        let turns = read.events.len() / 2;
        assert!((1..10).contains(&turns), "turns = {turns}");
        let bytes: usize = read
            .events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap().len())
            .sum();
        assert!(bytes <= 50_000);
    }

    #[tokio::test]
    async fn single_oversized_turn_is_still_returned_whole() {
        let s = storage().await;
        append(&s, run(0)).await;
        append(&s, run(1)).await;
        for i in 0..5 {
            append(&s, tool_end(&format!("t{i}"), "y".repeat(30_000))).await;
        }
        let window = LoadWindow {
            max_turns: 20,
            max_bytes: 1000,
            truncate_bytes: 32 * 1024,
        };
        let read = read_window(&s, SID, None, None, 0, &window).await.unwrap();
        assert_eq!(read.events.len(), 6, "run r1 and its five tool results");
        assert!(matches!(
            read.events[0].kind,
            AgentEventKind::RunStarted { .. }
        ));
        assert!(!read.history.complete);
    }

    #[test]
    fn truncates_large_result_with_marker() {
        let mut kind = tool_end("c1", "a".repeat(500 * 1024));
        let marks = truncate_event(&mut kind, DEFAULT_TRUNCATE_BYTES);
        let AgentEventKind::ToolCallEnd { result, .. } = &kind else {
            unreachable!()
        };
        assert_eq!(result.len(), DEFAULT_TRUNCATE_BYTES);
        assert_eq!(
            marks,
            vec![TruncatedEntry {
                tool_call_id: "c1".into(),
                part: ToolContentPart::Result,
                path: None,
                full_bytes: 500 * 1024,
            }]
        );
    }

    #[test]
    fn truncates_large_string_argument_with_path() {
        let args = serde_json::json!({"path": "f.txt", "content": "z".repeat(5_000_000)});
        let mut kind = AgentEventKind::ToolCallStart {
            tool_call_id: "c2".into(),
            tool_name: "write_file".into(),
            arguments: args.to_string(),
        };
        let marks = truncate_event(&mut kind, DEFAULT_TRUNCATE_BYTES);
        let AgentEventKind::ToolCallStart { arguments, .. } = &kind else {
            unreachable!()
        };
        let value: serde_json::Value = serde_json::from_str(arguments).unwrap();
        assert_eq!(
            value["content"].as_str().unwrap().len(),
            DEFAULT_TRUNCATE_BYTES
        );
        assert_eq!(value["path"], "f.txt");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].path.as_deref(), Some("/content"));
        assert_eq!(marks[0].part, ToolContentPart::Arguments);
        assert_eq!(marks[0].full_bytes, 5_000_000);
    }

    #[test]
    fn exactly_the_limit_is_unchanged_and_one_over_is_cut() {
        let mut at = tool_end("c", "a".repeat(DEFAULT_TRUNCATE_BYTES));
        assert!(truncate_event(&mut at, DEFAULT_TRUNCATE_BYTES).is_empty());
        let mut over = tool_end("c", "a".repeat(DEFAULT_TRUNCATE_BYTES + 1));
        assert_eq!(truncate_event(&mut over, DEFAULT_TRUNCATE_BYTES).len(), 1);
    }

    #[test]
    fn multibyte_boundary_keeps_valid_utf8() {
        // 3-byte chars: the 32 KiB cut falls inside a character.
        let text = "€".repeat(20_000);
        let mut kind = tool_end("c", text);
        truncate_event(&mut kind, DEFAULT_TRUNCATE_BYTES);
        let AgentEventKind::ToolCallEnd { result, .. } = &kind else {
            unreachable!()
        };
        assert!(result.len() <= DEFAULT_TRUNCATE_BYTES);
        assert_eq!(result.len() % 3, 0);
        assert!(result.chars().all(|c| c == '€'));
    }

    #[tokio::test]
    async fn truncation_leaves_the_journal_untouched_and_full_content_is_fetchable() {
        let s = storage().await;
        append(&s, run(0)).await;
        append(&s, tool_end("big", "q".repeat(100_000))).await;
        let read = read_window(&s, SID, None, None, 0, &LoadWindow::default())
            .await
            .unwrap();
        assert_eq!(read.truncated.len(), 1);
        let stored = s.load_session_stream(SID, None, None).await.unwrap();
        let AgentEventKind::ToolCallEnd { result, .. } = &stored[1].kind else {
            unreachable!()
        };
        assert_eq!(result.len(), 100_000);
        let full = find_tool_content(&s, SID, None, "big", ToolContentPart::Result)
            .await
            .unwrap();
        assert_eq!(full.unwrap().len(), 100_000);
        assert!(
            find_tool_content(&s, SID, None, "nope", ToolContentPart::Result)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn summary_covers_whole_session_including_model_before_window() {
        let s = storage().await;
        append(
            &s,
            AgentEventKind::ProviderChanged {
                provider: "anthropic".into(),
                model: "claude".into(),
                config_id: 1,
                context_limit: Some(1000),
                provider_node_id: None,
            },
        )
        .await;
        append(
            &s,
            AgentEventKind::InputQueued {
                input_id: "q1".into(),
                position: 0,
                blocks: vec![],
                accepted_at_ms: Some(5),
            },
        )
        .await;
        seed_turns(&s, 40).await;
        let window = LoadWindow {
            max_turns: 5,
            ..LoadWindow::default()
        };
        let read = read_window(&s, SID, None, None, 0, &window).await.unwrap();
        assert!(!read.history.complete);
        assert!(
            !read
                .events
                .iter()
                .any(|e| matches!(e.kind, AgentEventKind::ProviderChanged { .. }))
        );
        let start = read.events.first().map(|e| e.seq);
        let windowed = summarize_history(&s, SID, None, start).await.unwrap();
        let full = summarize_history(&s, SID, None, None).await.unwrap();
        assert_eq!(windowed.summary.provider, full.summary.provider);
        assert_eq!(windowed.summary.provider.as_ref().unwrap().model, "claude");
        assert_eq!(windowed.summary.pending_inputs.len(), 1);
        assert_eq!(windowed.summary.pending_inputs, full.summary.pending_inputs);
        let totals = full.summary.usage.session_totals.clone().unwrap();
        assert_eq!(totals.input_tokens, 400);
        assert_eq!(windowed.summary.usage.session_totals, Some(totals));
        assert_eq!(windowed.summary.usage.request_count, 40);
        let prior = windowed.summary.usage.before_window.unwrap();
        assert_eq!(prior.request_count, 35);
        assert_eq!(prior.session_totals.unwrap().input_tokens, 350);
        assert_eq!(windowed.accounting, full.accounting);
        assert_eq!(windowed.cursor.local_seq, full.cursor.local_seq);
    }

    /// 50k-event journal: the window build and the tool-content lookup stay
    /// within generous time bounds (checked in a debug build, so a real
    /// regression to a full-vector read would exceed them by a wide margin).
    #[tokio::test]
    async fn fifty_thousand_event_window_and_tool_content_are_fast() {
        let s = storage().await;
        let mut batch = Vec::new();
        for n in 0..10_000usize {
            for kind in [
                run(n),
                AgentEventKind::ToolCallStart {
                    tool_call_id: format!("c{n}"),
                    tool_name: "shell".into(),
                    arguments: "{}".into(),
                },
                tool_end(&format!("c{n}"), "ok".repeat(50)),
                request_end(10),
                AgentEventKind::Cancelled,
            ] {
                batch.push(NewDurableEvent {
                    session_seq: None,
                    session_id: SID.into(),
                    origin: EventOrigin::Local,
                    source_node: None,
                    source_node_id: None,
                    source_seq: None,
                    kind,
                });
            }
            if batch.len() >= 1000 {
                for r in s.append_durable_batch(&batch).await {
                    r.unwrap();
                }
                batch.clear();
            }
        }
        assert_eq!(s.max_stream_seq(SID).await.unwrap(), 50_000);

        let started = std::time::Instant::now();
        let read = read_window(&s, SID, None, None, 0, &LoadWindow::default())
            .await
            .unwrap();
        let window_time = started.elapsed();
        assert_eq!(read.events.len(), 100);
        assert!(!read.history.complete);
        assert!(
            window_time < std::time::Duration::from_secs(2),
            "{window_time:?}"
        );

        let started = std::time::Instant::now();
        let pass = summarize_history(&s, SID, None, Some(read.events[0].seq))
            .await
            .unwrap();
        let summary_time = started.elapsed();
        assert_eq!(pass.summary.usage.request_count, 10_000);
        assert!(
            summary_time < std::time::Duration::from_secs(10),
            "{summary_time:?}"
        );

        // Worst case for the bounded scan: the last tool call.
        let started = std::time::Instant::now();
        let content = find_tool_content(&s, SID, None, "c9999", ToolContentPart::Result)
            .await
            .unwrap();
        let lookup_time = started.elapsed();
        assert_eq!(content.unwrap().len(), 100);
        assert!(
            lookup_time < std::time::Duration::from_secs(5),
            "{lookup_time:?}"
        );
    }

    #[tokio::test]
    async fn summary_links_child_sessions_to_tool_calls() {
        let s = storage().await;
        append(
            &s,
            AgentEventKind::SessionForked {
                parent_session_id: SID.into(),
                child_session_id: "child-1".into(),
                target_agent_id: "a".into(),
                origin: ForkOrigin::Delegation,
                fork_point_type: crate::session::domain::ForkPointType::ProgressEntry,
                fork_point_ref: "d1".into(),
                instructions: None,
                selected_model_id: None,
                selected_provider_node_id: None,
            },
        )
        .await;
        let pass = summarize_history(&s, SID, None, None).await.unwrap();
        assert_eq!(pass.summary.child_sessions.len(), 1);
        assert_eq!(pass.summary.child_sessions[0].child_session_id, "child-1");
        assert_eq!(
            pass.summary.child_sessions[0].delegation_id.as_deref(),
            Some("d1")
        );
    }

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn load_window_absent_means_full_load() {
        assert_eq!(load_window_from_meta(None), None);
        assert_eq!(
            load_window_from_meta(Some(&meta(serde_json::json!({})))),
            None
        );
        assert_eq!(
            load_window_from_meta(Some(&meta(serde_json::json!({"querymt": {}})))),
            None
        );
        assert_eq!(
            load_window_from_meta(Some(&meta(
                serde_json::json!({"querymt": {"loadWindow": "yes"}})
            ))),
            None
        );
    }

    #[test]
    fn load_window_parses_and_defaults_missing_fields() {
        let window = load_window_from_meta(Some(&meta(serde_json::json!({
            "querymt": {"loadWindow": {"maxTurns": 5}}
        }))))
        .unwrap();
        assert_eq!(window.max_turns, 5);
        assert_eq!(window.max_bytes, DEFAULT_WINDOW_BYTES);
        assert_eq!(window.truncate_bytes, DEFAULT_TRUNCATE_BYTES);

        let window = load_window_from_meta(Some(&meta(serde_json::json!({
            "querymt": {"loadWindow": {"maxTurns": 20, "maxBytes": 1048576, "truncateBytes": 32768}}
        }))))
        .unwrap();
        assert_eq!(window, LoadWindow::default());
    }

    #[test]
    fn load_window_clamps_out_of_range_values() {
        let window = load_window_from_meta(Some(&meta(serde_json::json!({
            "querymt": {"loadWindow": {
                "maxTurns": 1_000_000, "maxBytes": 18446744073709551615u64, "truncateBytes": 0
            }}
        }))))
        .unwrap();
        assert_eq!(window.max_turns, MAX_WINDOW_TURNS);
        assert_eq!(window.max_bytes, MAX_WINDOW_BYTES);
        assert_eq!(window.truncate_bytes, 1);

        // Negative and non-numeric values fall back to defaults, fractional
        // values truncate.
        let window = load_window_from_meta(Some(&meta(serde_json::json!({
            "querymt": {"loadWindow": {"maxTurns": -3, "maxBytes": "big", "truncateBytes": 100.9}}
        }))))
        .unwrap();
        assert_eq!(window.max_turns, DEFAULT_WINDOW_TURNS);
        assert_eq!(window.max_bytes, DEFAULT_WINDOW_BYTES);
        assert_eq!(window.truncate_bytes, 100);
    }
}
