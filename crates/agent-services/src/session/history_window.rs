//! Windowed session history: the recent-turn window, whole-session summary,
//! tool-content truncation and full tool-content lookup.
//!
//! Everything here reads the event journal through paged reads
//! ([`EventJournal::load_history_page`]); the full event vector is never
//! materialised. Positions are *history positions*: `stream_seq` for local
//! sessions and the owner's `source_seq` for remote ones.

use crate::accounting::AccountingReducer;
use crate::control::delegation_notifications::{
    DelegationUpdateNotification, DelegationUpdateProjector,
};
use crate::session::domain::ForkOrigin;
use crate::session::error::SessionResult;
use crate::session::projection::{EventJournal, HistoryPageDirection};
use crate::session::turns::turn_start_indices;
use querymt_agent_core::events::{AgentEvent, AgentEventKind};
pub use querymt_agent_core::session::history::{
    ChildSessionLink, DEFAULT_TRUNCATE_BYTES, DEFAULT_WINDOW_BYTES, DEFAULT_WINDOW_TURNS,
    HistoryInfo, InputDelivery, LoadWindow, MAX_TRUNCATE_BYTES, MAX_WINDOW_BYTES, MAX_WINDOW_TURNS,
    PendingInputState, PendingInputSummary, PriorUsage, SessionHistorySummary, SummaryProvider,
    SummaryUsage, ToolContentPart, TruncatedEntry, UsageBreakdown,
};
use querymt_agent_core::stream_cursor::{StreamCursor, observe_cursor};
use std::collections::HashMap;

const BACKWARD_PAGE: usize = 256;
const SCAN_PAGE: usize = 1000;

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
        .get(querymt_agent_core::meta::QUERYMT_META_NAMESPACE)?
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

struct PriorAcc {
    reducer: AccountingReducer,
    totals: Option<UsageBreakdown>,
    partial: bool,
    requests: u64,
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
    /// Start time (ms) of each run that has started and not ended.
    run_starts: HashMap<String, i64>,
    /// Total time of the ended runs, in ms.
    active_ms: u64,
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
            // A finished compaction reports the context the session holds
            // afterwards; live delivery and replay use it as the measurement,
            // so the summary must too.
            AgentEventKind::CompactionEnd {
                context_tokens: Some(tokens),
                ..
            } => {
                self.context = Some((*tokens, position));
            }
            // Active time is run time: the sum over ended runs of the time
            // from `RunStarted` to `RunCompleted`, paired by run id. A start
            // without an end is the active run (the runtime status reports
            // it); an end without a start is ignored. Event timestamps are
            // whole seconds, so the total is exact to one second per run.
            AgentEventKind::RunStarted { run_id, .. } => {
                self.run_starts
                    .insert(run_id.clone(), event.timestamp.saturating_mul(1000));
            }
            AgentEventKind::RunCompleted { run_id, .. } => {
                if let Some(start) = self.run_starts.remove(run_id) {
                    let ended = event.timestamp.saturating_mul(1000);
                    self.active_ms = self
                        .active_ms
                        .saturating_add(u64::try_from(ended.saturating_sub(start)).unwrap_or(0));
                }
            }
            AgentEventKind::ProviderChanged {
                provider,
                model,
                context_limit,
                provider_node_id,
                ..
            } => {
                // Selecting the model the session already uses (another
                // configuration or node) keeps the position: the last
                // measurement still describes that model.
                let position = match &self.provider {
                    Some(current)
                        if current.provider == *provider
                            && current.model == *model
                            && current.context_limit == *context_limit =>
                    {
                        current.position
                    }
                    _ => position,
                };
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
                delivery: InputDelivery::Steer,
                state: PendingInputState::Accepted,
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
                delivery: InputDelivery::Queue,
                state: PendingInputState::Queued,
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
    let mut fold = SummaryFold::new(session_id, window_start);
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
            fold.observe(position, &AgentEvent::from(durable));
        }
        if page_len < SCAN_PAGE {
            break;
        }
    }
    Ok(fold.finish())
}

/// The same pass over events the caller already holds, each with its history
/// position, in ascending order. A snapshot that read its history once builds
/// its summary from those events, so both describe one and the same cut.
pub fn summarize_events<'a>(
    session_id: &str,
    events: impl IntoIterator<Item = (i64, &'a AgentEvent)>,
) -> SummaryPass {
    let mut fold = SummaryFold::new(session_id, None);
    for (position, event) in events {
        fold.observe(position, event);
    }
    fold.finish()
}

/// State of the whole-session pass: fed event by event, in order.
struct SummaryFold {
    session_id: String,
    window_start: Option<i64>,
    prior: Option<PriorAcc>,
    reducer: AccountingReducer,
    projector: DelegationUpdateProjector,
    cursor: StreamCursor,
    acc: SummaryAcc,
}

impl SummaryFold {
    fn new(session_id: &str, window_start: Option<i64>) -> Self {
        Self {
            session_id: session_id.to_owned(),
            window_start,
            prior: window_start.map(|_| PriorAcc {
                reducer: AccountingReducer::new(session_id),
                totals: None,
                partial: false,
                requests: 0,
            }),
            reducer: AccountingReducer::new(session_id),
            projector: DelegationUpdateProjector::default(),
            cursor: StreamCursor::default(),
            acc: SummaryAcc::default(),
        }
    }

    fn observe(&mut self, position: i64, event: &AgentEvent) {
        observe_cursor(&mut self.cursor, event);
        if let Err(conflict) = self.reducer.apply(&event.kind) {
            tracing::warn!(
                session.id = %self.session_id,
                attempt.id = %conflict.attempt_id,
                detail = conflict.detail,
                "skipping conflicting accounting record in snapshot"
            );
        }
        if is_delegation_event(&event.kind) {
            self.projector.project_event(event);
        }
        self.acc.observe(&self.session_id, position, event);
        if let (Some(prior), Some(start)) = (self.prior.as_mut(), self.window_start)
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

    fn finish(self) -> SummaryPass {
        let Self {
            window_start,
            prior,
            reducer,
            projector,
            cursor,
            mut acc,
            ..
        } = self;
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
                window_start,
                session_totals: prior.totals,
                partial: prior.partial,
                request_count: prior.requests,
                accounting: prior.reducer.session().clone(),
            }),
        };
        SummaryPass {
            accounting: crate::accounting::snapshot_from_reducer(&reducer),
            delegation_updates: projector.snapshots(),
            cursor,
            summary: SessionHistorySummary {
                usage,
                provider: acc.provider,
                pending_inputs: acc.pending,
                child_sessions: acc.children,
                active_ms: acc.active_ms,
            },
        }
    }
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
    use crate::session::projection::NewDurableEvent;
    use querymt_agent_core::events::{EventOrigin, ExecutionMetrics};
    use querymt_agent_sqlite::session::sqlite_storage::SqliteStorage;

    const SID: &str = "s1";

    #[test]
    fn typed_state_fields_serialize_unchanged() {
        let summary = |delivery, state| PendingInputSummary {
            input_id: "i".into(),
            delivery,
            state,
            run_id: None,
            position: None,
            blocks: vec![],
            created_at: 0,
        };
        let steer =
            serde_json::to_value(summary(InputDelivery::Steer, PendingInputState::Accepted))
                .unwrap();
        assert_eq!(steer["delivery"], "steer");
        assert_eq!(steer["state"], "accepted");
        let queue =
            serde_json::to_value(summary(InputDelivery::Queue, PendingInputState::Queued)).unwrap();
        assert_eq!(queue["delivery"], "queue");
        assert_eq!(queue["state"], "queued");
    }

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
            prefix_change: None,
            idle_secs: None,
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

    #[tokio::test]
    async fn summary_over_held_events_equals_the_journal_pass() {
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
        seed_turns(&s, 6).await;
        let events: Vec<AgentEvent> = s
            .load_session_stream(SID, None, None)
            .await
            .unwrap()
            .into_iter()
            .map(AgentEvent::from)
            .collect();
        let held = summarize_events(SID, events.iter().map(|event| (event.seq, event)));
        let journal = summarize_history(&s, SID, None, None).await.unwrap();
        assert_eq!(held.summary, journal.summary);
        assert_eq!(held.accounting, journal.accounting);
        assert_eq!(held.cursor.local_seq, journal.cursor.local_seq);
        assert_eq!(held.summary.usage.request_count, 6);
    }

    /// A run boundary at `timestamp` seconds, for the active-time tests.
    fn run_event(seq: i64, timestamp: i64, kind: AgentEventKind) -> AgentEvent {
        AgentEvent {
            seq,
            session_seq: seq,
            timestamp,
            session_id: SID.into(),
            origin: EventOrigin::Local,
            source_node: None,
            kind,
        }
    }

    fn run_end(run_id: &str) -> AgentEventKind {
        AgentEventKind::RunCompleted {
            run_id: run_id.into(),
            outcome: "completed".into(),
            stop_type: None,
            reason: None,
        }
    }

    fn active_ms(events: &[AgentEvent], window_start: Option<i64>) -> u64 {
        let mut fold = SummaryFold::new(SID, window_start);
        for event in events {
            fold.observe(event.seq, event);
        }
        fold.finish().summary.active_ms
    }

    #[test]
    fn ended_runs_are_summed() {
        let events = vec![
            run_event(1, 100, run(0)),
            run_event(2, 130, run_end("r0")),
            run_event(3, 200, run(1)),
            run_event(4, 290, run_end("r1")),
        ];
        assert_eq!(active_ms(&events, None), 120_000);
        // The total covers the whole session, not the window.
        assert_eq!(active_ms(&events, Some(3)), 120_000);
        let held = summarize_events(SID, events.iter().map(|event| (event.seq, event)));
        assert_eq!(held.summary.active_ms, 120_000);
    }

    #[test]
    fn running_run_is_not_counted() {
        let events = vec![
            run_event(1, 100, run(0)),
            run_event(2, 130, run_end("r0")),
            run_event(3, 200, run(1)),
            run_event(4, 290, run_end("r1")),
            run_event(5, 400, run(2)),
        ];
        assert_eq!(active_ms(&events, None), 120_000);
    }

    #[test]
    fn an_end_without_a_start_is_ignored_and_pairs_are_per_run_id() {
        let events = vec![
            run_event(1, 100, run_end("orphan")),
            run_event(2, 200, run(1)),
            run_event(3, 205, run(2)),
            run_event(4, 210, run_end("r2")),
            run_event(5, 230, run_end("r1")),
        ];
        // r2: 5 s, r1: 30 s.
        assert_eq!(active_ms(&events, None), 35_000);
    }

    #[tokio::test]
    async fn reselecting_the_model_keeps_its_position() {
        let s = storage().await;
        let select = |config_id: i64, limit: u64| AgentEventKind::ProviderChanged {
            provider: "p".into(),
            model: "m".into(),
            config_id,
            context_limit: Some(limit),
            provider_node_id: None,
        };
        let position = |pass: SummaryPass| pass.summary.provider.expect("a model").position;
        append(&s, select(1, 1000)).await;
        let first = position(summarize_history(&s, SID, None, None).await.unwrap());
        // Another configuration of the same model: the measurement taken
        // since the first selection still describes it.
        seed_turns(&s, 1).await;
        append(&s, select(2, 1000)).await;
        let again = position(summarize_history(&s, SID, None, None).await.unwrap());
        assert_eq!(again, first);
        // Another context limit is another model.
        append(&s, select(3, 2000)).await;
        let other = position(summarize_history(&s, SID, None, None).await.unwrap());
        assert!(other > again);
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
