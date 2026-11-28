//! Projection of `querymt_agent_session_core::SessionCore` into the `Item`
//! rows the UI reads (design D1, D2).
//!
//! The core is the only writer of conversation state. A turn projects to a
//! contiguous slice of `items`; after each batch only the slices of turns the
//! change report names are rebuilt, so unchanged rows keep their keys,
//! revisions and `Arc<ToolCall>` pointers.

// Not wired into the store yet (tasks 5.x); the allow goes with the wiring.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use querymt_agent_session_core::entry::{
    CompactionEntry, CompactionState, Entry, EntryKind, PromptStatus, ToolCallEntry,
};
use querymt_agent_session_core::input::{SessionInput, UndoFrame as CoreUndoFrame};
use querymt_agent_session_core::view::{ChangeReport, RunState, SessionHeader, SlashCommandView};
use querymt_agent_session_core::{SessionCore, TurnRef};
use serde_json::Value;

use super::overlay::{Notice, Overlay};
use super::{
    Compaction, CompactionStatus, ContentFetch, Diff, Item, ItemKind, PendingInput, PlanEntry,
    Thought, ToolCall, UndoFrame, UserCommand, WorkEntry, content_text, diff_stats, prompt_parts,
    synthesized_diffs,
};
use crate::usage::{Accounting, Breakdown, TokenUsage};

/// Where one turn's rows sit in `Projection::items`.
#[derive(Clone, Debug)]
pub struct TurnSlice {
    pub id: String,
    pub revision: u64,
    pub len: usize,
}

/// The header fields the UI reads, mapped from `SessionHeader` (design D2).
#[derive(Clone, Debug, Default)]
pub struct HeaderFields {
    /// Runtime phase vocabulary of `runtimeChanged`: `idle`, `starting`,
    /// `model`, `tools`, `waiting`.
    pub phase: String,
    pub run_id: Option<String>,
    pub plan: Vec<PlanEntry>,
    /// The core's context measurement `(used, size)`; `None` also after a
    /// model change made the last one stale.
    pub usage: Option<(u64, u64)>,
    /// Context size of the session's current model, when the agent named it
    /// (load summary or the model-change signal).
    pub context_limit: Option<u64>,
    pub breakdown: Option<Breakdown>,
    pub accounting: Option<Accounting>,
    /// Run time of the ended runs, in milliseconds (the core's `active_time`).
    pub active_ms: f64,
    /// Host time (ms since the epoch) the active run started, if one is
    /// active; the UI adds `now - active_since_ms` while it runs.
    pub active_since_ms: Option<f64>,
    pub title: Option<String>,
    pub mode: Option<String>,
    pub options: Vec<Value>,
    /// The agent's latest `available_commands_update`, names without `/`.
    pub slash_commands: Vec<SlashCommandView>,
    pub history_more: bool,
    pub history_before: Option<String>,
    pub undo_stack: Vec<UndoFrame>,
    /// Inputs the core lists; the overlay adds unconfirmed ones (task 3.4).
    pub pending_inputs: Vec<PendingInput>,
}

pub struct Projection {
    pub core: SessionCore,
    /// Client-only state; its notices are rows of the turn they follow.
    pub overlay: Overlay,
    pub items: Vec<Item>,
    /// Rows before the first turn's slice (notices pushed before any turn).
    lead: usize,
    slices: Vec<TurnSlice>,
    /// `Arc<ToolCall>` per tool entry id and revision (design D2).
    cache: Cache,
    /// Turns rebuilt by the latest `feed`; lets tests prove that unchanged
    /// turns are not touched.
    pub rebuilt_turns: usize,
}

impl Default for Projection {
    fn default() -> Self {
        Self::new("")
    }
}

impl Projection {
    pub fn new(session_id: &str) -> Self {
        Self {
            core: SessionCore::new(session_id),
            overlay: Overlay::default(),
            items: Vec::new(),
            lead: 0,
            slices: Vec::new(),
            cache: Cache::default(),
            rebuilt_turns: 0,
        }
    }

    /// Apply a batch to the core and re-project what changed.
    pub fn feed(&mut self, inputs: &[SessionInput]) -> ChangeReport {
        let report = self.core.apply(inputs);
        let loaded = inputs
            .iter()
            .any(|i| matches!(i, SessionInput::SnapshotLoaded { .. }));
        let mut synced = report.clone();
        if loaded {
            for id in self.mark_interrupted() {
                if !synced.added.contains(&id) && !synced.updated.contains(&id) {
                    synced.updated.push(id);
                }
            }
        }
        self.sync(&synced);
        report
    }

    /// Take over a core that was hydrated elsewhere (a large load decoded on
    /// a background thread) and project all of its turns. The session had no
    /// turns before: nothing but client notices is replaced.
    pub fn replace_core(&mut self, core: SessionCore) -> ChangeReport {
        self.core = core;
        self.items.clear();
        self.slices.clear();
        self.lead = 0;
        self.cache = Cache::default();
        let report = ChangeReport {
            added: self.core.turn_order().map(str::to_owned).collect(),
            header_changed: true,
            ..ChangeReport::default()
        };
        self.mark_interrupted();
        self.sync(&report);
        report
    }

    /// Test reference: drop every row and cached tool call and project all
    /// turns again from the core, keeping the interrupted entries. Whatever
    /// incremental updates built must equal this.
    #[cfg(test)]
    pub fn rebuild_all(&mut self) {
        let interrupted = std::mem::take(&mut self.cache.interrupted);
        self.cache = Cache {
            interrupted,
            ..Cache::default()
        };
        self.items.clear();
        self.slices.clear();
        self.lead = 0;
        let report = ChangeReport {
            added: self.core.turn_order().map(str::to_owned).collect(),
            ..ChangeReport::default()
        };
        self.reassemble(&report);
    }

    /// After a load that found the session idle, tools and compactions that
    /// still run are interrupted ones (the run ended without recording their
    /// result). Returns the turns whose rows depend on a changed flag.
    fn mark_interrupted(&mut self) -> Vec<String> {
        let before = std::mem::take(&mut self.cache.interrupted);
        if !self.running() {
            for id in self.core.turn_order() {
                let Some(turn) = self.core.turn_ref(id) else {
                    continue;
                };
                for entry in turn.entries() {
                    let unfinished = match &entry.kind {
                        EntryKind::ToolCall(call) => {
                            matches!(call.status.as_str(), "pending" | "in_progress" | "")
                        }
                        EntryKind::Compaction(c) => c.state == CompactionState::InProgress,
                        _ => false,
                    };
                    if unfinished {
                        self.cache
                            .interrupted
                            .insert(entry.id.clone(), entry.revision);
                    }
                }
            }
        }
        let after = &self.cache.interrupted;
        let changed: HashSet<&String> = before
            .iter()
            .filter(|(id, rev)| after.get(*id) != Some(*rev))
            .map(|(id, _)| id)
            .chain(
                after
                    .iter()
                    .filter(|(id, rev)| before.get(*id) != Some(*rev))
                    .map(|(id, _)| id),
            )
            .collect();
        let mut turns = Vec::new();
        if changed.is_empty() {
            return turns;
        }
        for id in self.core.turn_order() {
            if let Some(turn) = self.core.turn_ref(id)
                && turn.entries().any(|e| changed.contains(&e.id))
            {
                turns.push(id.to_owned());
            }
        }
        turns
    }

    /// Turn slices in history order.
    pub fn slices(&self) -> &[TurnSlice] {
        &self.slices
    }

    /// Item range of turn `id`.
    pub fn turn_items(&self, id: &str) -> Option<std::ops::Range<usize>> {
        let at = self.slices.iter().position(|s| s.id == id)?;
        let start: usize = self.lead + self.slices[..at].iter().map(|s| s.len).sum::<usize>();
        Some(start..start + self.slices[at].len)
    }

    fn sync(&mut self, report: &ChangeReport) {
        self.rebuilt_turns = 0;
        if report.added.is_empty() && report.updated.is_empty() && report.removed.is_empty() {
            return;
        }
        let same_order = self.core.turn_order().len() == self.slices.len()
            && self
                .core
                .turn_order()
                .zip(&self.slices)
                .all(|(id, slice)| id == slice.id);
        if same_order && report.added.is_empty() && report.removed.is_empty() {
            self.rebuild_in_place(report);
        } else {
            self.reassemble(report);
        }
    }

    /// Same turns in the same order: replace the slices of updated turns.
    fn rebuild_in_place(&mut self, report: &ChangeReport) {
        for id in &report.updated {
            // Streaming updates the last turn: search from the end.
            let Some(at) = self.slices.iter().rposition(|s| s.id == *id) else {
                continue;
            };
            let Some(turn) = self.core.turn_ref(id) else {
                continue;
            };
            let is_last = at + 1 == self.slices.len();
            let notices: Vec<Notice> = notices_for(&self.overlay.notices, id, is_last, &|a| {
                self.slices.iter().any(|s| s.id == a)
            })
            .into_iter()
            .cloned()
            .collect();
            let rows = project_turn(&mut self.cache, &self.overlay.fetches, &notices, turn);
            let start: usize = self.lead + self.slices[..at].iter().map(|s| s.len).sum::<usize>();
            let old = self.slices[at].len;
            let revision = turn.revision();
            self.slices[at].len = rows.len();
            self.slices[at].revision = revision;
            self.items.splice(start..start + old, rows);
            self.rebuilt_turns += 1;
        }
    }

    /// Turns were added or removed (a load, an older page, a new run):
    /// reuse the rows of unchanged turns and rebuild the rest, in core order.
    fn reassemble(&mut self, report: &ChangeReport) {
        let changed: HashSet<&str> = report
            .added
            .iter()
            .chain(&report.updated)
            .chain(&report.removed)
            .map(String::as_str)
            .collect();
        let mut old_rows: HashMap<String, Vec<Item>> = HashMap::new();
        let mut rest = std::mem::take(&mut self.items).into_iter();
        // Leading notices are regenerated below.
        rest.by_ref().take(self.lead).for_each(drop);
        self.lead = 0;
        for slice in std::mem::take(&mut self.slices) {
            let rows: Vec<Item> = rest.by_ref().take(slice.len).collect();
            old_rows.insert(slice.id, rows);
        }

        let order: Vec<String> = self.core.turn_order().map(str::to_owned).collect();
        let live: HashSet<&str> = order.iter().map(String::as_str).collect();
        // A note whose turn is gone follows the last turn; that turn is
        // rebuilt so the note is not lost.
        let has_orphans = self
            .overlay
            .notices
            .iter()
            .any(|n| n.anchor.as_deref().is_some_and(|a| !live.contains(a)));
        let last = order.len().saturating_sub(1);
        for (at, id) in order.iter().enumerate() {
            let Some(turn) = self.core.turn_ref(id) else {
                continue;
            };
            let force = has_orphans && at == last;
            let kept = if force || changed.contains(id.as_str()) {
                None
            } else {
                old_rows.remove(id)
            };
            let rows = match kept {
                Some(rows) => rows,
                None => {
                    self.rebuilt_turns += 1;
                    let notices: Vec<Notice> =
                        notices_for(&self.overlay.notices, id, at == last, &|a| live.contains(a))
                            .into_iter()
                            .cloned()
                            .collect();
                    project_turn(&mut self.cache, &self.overlay.fetches, &notices, turn)
                }
            };
            self.slices.push(TurnSlice {
                id: id.clone(),
                revision: turn.revision(),
                len: rows.len(),
            });
            self.items.extend(rows);
        }
        let lead = lead_rows(&self.overlay.notices, self.slices.is_empty());
        self.lead = lead.len();
        self.items.splice(0..0, lead);
        if !report.removed.is_empty() {
            let core = &self.core;
            self.cache.tools.retain(|id, _| core.entry(id).is_some());
        }
    }

    /// The header fields the UI reads. `pending_inputs` merges the core's
    /// list with the overlay's unconfirmed inputs.
    pub fn header_fields(&self) -> HeaderFields {
        let mut fields = map_header(self.core.header());
        fields.pending_inputs = self
            .overlay
            .merge_inputs(std::mem::take(&mut fields.pending_inputs));
        fields
    }

    /// A prompt or run is in flight (design D2: `running()` from the core).
    pub fn running(&self) -> bool {
        !matches!(
            self.core.header().run_state,
            RunState::Idle | RunState::Cancelled | RunState::Failed
        )
    }

    /// Settle the overlay's inputs to match `running()`.
    pub fn sync_inputs(&mut self) {
        let running = self.running();
        self.overlay.sync_inputs(running);
    }

    /// Item index range of each turn, in order (trailing notices included).
    pub fn turn_bounds(&self) -> Vec<(&str, std::ops::Range<usize>)> {
        let mut at = self.lead;
        self.slices
            .iter()
            .map(|s| {
                let range = at..at + s.len;
                at += s.len;
                (s.id.as_str(), range)
            })
            .collect()
    }

    /// The turn holding item `ix`: its id and item range. Leading notices
    /// belong to no turn.
    pub fn turn_containing(&self, ix: usize) -> Option<(&str, std::ops::Range<usize>)> {
        let mut at = self.lead;
        for s in &self.slices {
            if ix < at + s.len {
                return (ix >= at).then(|| (s.id.as_str(), at..at + s.len));
            }
            at += s.len;
        }
        None
    }

    /// Start of the newest turn's rows (0 without turns).
    pub fn last_turn_start(&self) -> usize {
        self.slices.last().map_or(0, |s| {
            self.lead + self.slices.iter().map(|s| s.len).sum::<usize>() - s.len
        })
    }

    /// The current tool call `tool_call_id` as the rows show it.
    pub fn tool(&self, tool_call_id: &str) -> Option<&ToolCall> {
        self.cache
            .tools
            .get(&format!("tool-{tool_call_id}"))
            .map(|c| &*c.call)
    }

    /// Show (or clear) the progress of fetching a tool call's full content,
    /// rebuilding only the turn that holds it.
    pub fn set_fetch(&mut self, tool_call_id: &str, fetch: Option<ContentFetch>) {
        match fetch {
            Some(f) => {
                self.overlay.fetches.insert(tool_call_id.to_owned(), f);
            }
            None => {
                self.overlay.fetches.remove(tool_call_id);
            }
        }
        let entry = format!("tool-{tool_call_id}");
        // A fetch is for a recent tool call: search from the newest turn.
        let order: Vec<&str> = self.core.turn_order().collect();
        let turn = order.into_iter().rev().find(|id| {
            self.core
                .turn_ref(id)
                .is_some_and(|t| t.entries().any(|e| e.id == entry))
        });
        if let Some(id) = turn.map(str::to_owned) {
            let report = ChangeReport {
                updated: vec![id],
                ..ChangeReport::default()
            };
            self.rebuilt_turns = 0;
            self.rebuild_in_place(&report);
        }
    }

    /// Add a client note after the last turn.
    pub fn push_notice(&mut self, text: impl Into<String>, error: bool) {
        let anchor = self.slices.last().map(|s| s.id.clone());
        self.overlay.push_notice(text, error, anchor.clone());
        match anchor {
            Some(id) => {
                self.rebuilt_turns = 0;
                let mut report = ChangeReport::default();
                report.updated.push(id);
                self.rebuild_in_place(&report);
            }
            None => {
                let lead = lead_rows(&self.overlay.notices, true);
                let old = self.lead;
                self.lead = lead.len();
                self.items.splice(0..old, lead);
            }
        }
    }
}

/// Notices rendered after turn `id`: those anchored to it, and, on the last
/// turn, those whose turn no longer exists.
fn notices_for<'a>(
    notices: &'a [Notice],
    id: &str,
    is_last: bool,
    live: &dyn Fn(&str) -> bool,
) -> Vec<&'a Notice> {
    notices
        .iter()
        .filter(|n| match n.anchor.as_deref() {
            Some(anchor) => anchor == id || (is_last && !live(anchor)),
            None => false,
        })
        .collect()
}

fn notice_item(notice: &Notice) -> Item {
    Item {
        key: notice.key.clone(),
        rev: 1,
        kind: ItemKind::Notice {
            text: notice.text.clone(),
            error: notice.error,
        },
    }
}

/// Notices shown before the first turn: pushed before any turn existed, or
/// (with no turns at all) anchored to a turn that is gone.
fn lead_rows(notices: &[Notice], no_turns: bool) -> Vec<Item> {
    notices
        .iter()
        .filter(|n| n.anchor.is_none() || no_turns)
        .map(notice_item)
        .collect()
}

/// Per-session caches the row builders read.
#[derive(Default)]
struct Cache {
    /// `Arc<ToolCall>` per tool entry id, revision and interrupted flag
    /// (design D2).
    tools: HashMap<String, CachedTool>,
    /// Entries that were still running when a load found the session idle:
    /// entry id → the revision that was loaded. They show as failed until the
    /// agent updates them.
    interrupted: HashMap<String, u64>,
}

/// The entries of a turn in display order: reasoning that arrives after its
/// message's reply started joins the work group before that reply instead of
/// splitting it.
fn ordered_entries<'a>(turn: TurnRef<'a>) -> Vec<&'a Entry> {
    // A snapshot can hold both streamed reasoning (no part id) and the stored
    // message's parts for the same text: the parts stand for it.
    let mut parts: HashMap<&str, Vec<&str>> = HashMap::new();
    for entry in turn.entries() {
        if let EntryKind::Reasoning(part) = &entry.kind
            && let Some(message) = part.message_id.as_deref()
            && !part.part_id.is_empty()
        {
            parts.entry(message).or_default().push(part.text.as_str());
        }
    }
    let duplicate = |part: &querymt_agent_session_core::entry::ReasoningPart| {
        part.part_id.is_empty()
            && part
                .message_id
                .as_deref()
                .and_then(|m| parts.get(m))
                .is_some_and(|texts| {
                    texts.contains(&part.text.as_str()) || texts.concat() == part.text
                })
    };
    let mut order: Vec<&Entry> = Vec::new();
    for entry in turn.entries() {
        // The stored part takes the place the streamed reasoning had.
        if let EntryKind::Reasoning(part) = &entry.kind
            && !part.part_id.is_empty()
            && let Some(at) = order.iter().position(|e| {
                matches!(&e.kind, EntryKind::Reasoning(p)
                    if p.message_id == part.message_id && duplicate(p))
            })
        {
            order[at] = entry;
            continue;
        }
        if let EntryKind::Reasoning(part) = &entry.kind
            && let Some(message) = &part.message_id
            && let Some(at) = order.iter().position(|e| {
                matches!(&e.kind, EntryKind::AssistantMessage(a) if a.message_id.as_ref() == Some(message))
            })
        {
            order.insert(at, entry);
            continue;
        }
        order.push(entry);
    }
    order
}

/// Rows of one turn: user and agent messages, work groups, compactions.
fn project_turn(
    cache: &mut Cache,
    fetches: &HashMap<String, ContentFetch>,
    notices: &[Notice],
    turn: TurnRef<'_>,
) -> Vec<Item> {
    let mut rows = Vec::new();
    let mut work = WorkGroup::default();
    for entry in ordered_entries(turn) {
        match &entry.kind {
            EntryKind::UserMessage(message) => {
                work.flush(&mut rows);
                let blocks: Vec<Value> = message
                    .blocks
                    .iter()
                    .filter_map(|b| serde_json::to_value(b).ok())
                    .collect();
                let (text, attachments) = prompt_parts(&blocks);
                // A record says what the user typed; the blocks are then the
                // expansion the model received.
                let (text, command) = match &message.slash_command {
                    Some(record) => (
                        typed_command(&record.name, &record.arguments),
                        Some(UserCommand {
                            name: record.name.clone(),
                            arguments: record.arguments.clone(),
                            expansion: text,
                        }),
                    ),
                    None => (text, None),
                };
                rows.push(Item {
                    key: entry.id.clone(),
                    rev: entry.revision,
                    kind: ItemKind::User {
                        text,
                        attachments,
                        local: message.status != PromptStatus::Sent,
                        command,
                    },
                });
                if message.status == PromptStatus::Failed {
                    rows.push(Item {
                        key: format!("notice-{}", entry.id),
                        rev: entry.revision,
                        kind: ItemKind::Notice {
                            text: message
                                .error
                                .clone()
                                .unwrap_or_else(|| "Failed to send the prompt.".into()),
                            error: true,
                        },
                    });
                }
            }
            EntryKind::AssistantMessage(message) => {
                work.flush(&mut rows);
                rows.push(Item {
                    key: entry.id.clone(),
                    rev: entry.revision,
                    kind: ItemKind::Agent {
                        text: message.text.clone(),
                    },
                });
            }
            EntryKind::Reasoning(part) => {
                let message = part.message_id.clone().unwrap_or_else(|| entry.id.clone());
                let part_id = (!part.part_id.is_empty()).then(|| part.part_id.clone());
                work.push(
                    &entry.id,
                    entry.revision,
                    WorkEntry::Thought(Thought::new(message, part_id, part.text.clone())),
                );
            }
            EntryKind::ToolCall(call) => {
                let interrupted = cache.interrupted.get(&entry.id) == Some(&entry.revision);
                let call = tool_call(
                    &mut cache.tools,
                    &entry.id,
                    entry.revision,
                    interrupted,
                    fetches.get(&call.tool_call_id),
                    call,
                );
                work.push(&entry.id, entry.revision, WorkEntry::Tool(call));
            }
            EntryKind::Compaction(compaction) => {
                work.flush(&mut rows);
                rows.push(Item {
                    key: entry.id.clone(),
                    rev: entry.revision,
                    kind: ItemKind::Compaction(Box::new(compaction_row(
                        compaction,
                        cache.interrupted.get(&entry.id) == Some(&entry.revision),
                    ))),
                });
            }
            // The core may add entry kinds; an unknown one has no row.
            _ => {}
        }
    }
    work.flush(&mut rows);
    rows.extend(notices.iter().map(notice_item));
    rows
}

/// Consecutive reasoning and tool entries, one `ItemKind::Work`.
#[derive(Default)]
struct WorkGroup {
    key: Option<String>,
    rev: u64,
    entries: Vec<WorkEntry>,
}

impl WorkGroup {
    fn push(&mut self, entry_id: &str, revision: u64, entry: WorkEntry) {
        if self.key.is_none() {
            self.key = Some(entry_id.to_owned());
        }
        self.rev = self.rev.max(revision);
        self.entries.push(entry);
    }

    fn flush(&mut self, rows: &mut Vec<Item>) {
        let Some(key) = self.key.take() else {
            return;
        };
        rows.push(Item {
            key,
            rev: std::mem::take(&mut self.rev),
            kind: ItemKind::Work(std::mem::take(&mut self.entries)),
        });
    }
}

/// The cached `Arc<ToolCall>` for `(entry id, revision)`; built once per
/// revision (design D2).
/// A built tool call with what it was built from.
struct CachedTool {
    revision: u64,
    interrupted: bool,
    fetch: Option<ContentFetch>,
    call: Arc<ToolCall>,
}

fn tool_call(
    cache: &mut HashMap<String, CachedTool>,
    entry_id: &str,
    revision: u64,
    interrupted: bool,
    fetch: Option<&ContentFetch>,
    entry: &ToolCallEntry,
) -> Arc<ToolCall> {
    if let Some(cached) = cache.get(entry_id)
        && cached.revision == revision
        && cached.interrupted == interrupted
        && cached.fetch.as_ref() == fetch
    {
        return Arc::clone(&cached.call);
    }
    let mut built = build_tool_call(entry);
    built.fetch = fetch.cloned();
    if interrupted {
        built.status = "failed".into();
        if built.output().is_empty() {
            built.set_output("Run ended before a tool result was recorded.".into());
        }
    }
    let call = Arc::new(built);
    cache.insert(
        entry_id.to_owned(),
        CachedTool {
            revision,
            interrupted,
            fetch: fetch.cloned(),
            call: Arc::clone(&call),
        },
    );
    call
}

fn build_tool_call(entry: &ToolCallEntry) -> ToolCall {
    let mut call = ToolCall {
        title: entry.title.clone(),
        kind: entry.kind.clone(),
        status: entry.status.clone(),
        input: entry.raw_input.clone().filter(|v| !v.is_null()),
        truncated_result: entry.result_full_bytes,
        truncated_args: entry.arguments_full_bytes,
        child_session: entry.child_session_id.clone(),
        ..ToolCall::new(&entry.tool_call_id)
    };
    // The agent's raw output is the result; the core also writes the full
    // content fetched after a truncated load there (`ToolContent`), while the
    // ACP content keeps the prefix. Content text is the fallback.
    let raw_output = entry
        .raw_output
        .as_ref()
        .filter(|v| !v.is_null())
        .map(|raw| {
            raw.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| raw.to_string())
        });
    if !entry.content.is_empty() {
        let mut output = String::new();
        let mut diffs = Vec::new();
        for c in &entry.content {
            match c.get("type").and_then(Value::as_str) {
                Some("content") => {
                    if let Some(inner) = c.get("content") {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&content_text(inner));
                    }
                }
                Some("diff") => diffs.push(Diff::whole_file(
                    c.get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    c.get("oldText").and_then(Value::as_str).map(str::to_owned),
                    c.get("newText")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                )),
                _ => {}
            }
        }
        call.set_output(raw_output.unwrap_or(output));
        if !diffs.is_empty() {
            call.stats = diffs
                .iter()
                .map(diff_stats)
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            call.diffs = diffs;
        }
    } else if let Some(output) = raw_output {
        call.set_output(output);
    }
    // Edit tools report a receipt, not a diff: rebuild one from the arguments.
    if call.diffs.is_empty() {
        let diffs = synthesized_diffs(&call);
        if !diffs.is_empty() {
            call.synthesized = true;
            call.stats = diffs
                .iter()
                .map(diff_stats)
                .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            call.diffs = diffs;
        }
    }
    call
}

/// `/name arguments`, as the user typed it.
fn typed_command(name: &str, arguments: &str) -> String {
    if arguments.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {arguments}")
    }
}

fn compaction_row(entry: &CompactionEntry, interrupted: bool) -> Compaction {
    let status = match entry.state {
        CompactionState::Completed => CompactionStatus::Completed,
        CompactionState::Failed => CompactionStatus::Failed,
        CompactionState::Cancelled => CompactionStatus::Cancelled,
        // An unknown status is never treated as completed.
        _ if interrupted => CompactionStatus::Failed,
        _ => CompactionStatus::InProgress,
    };
    let summary = entry
        .summary
        .iter()
        .filter_map(|b| serde_json::to_value(b).ok())
        .map(|b| content_text(&b))
        .collect::<String>();
    let counts = entry
        .meta
        .as_ref()
        .and_then(|m| m.get("querymt"))
        .and_then(|q| q.get("compaction"));
    let count = |key: &str| counts.and_then(|c| c.get(key)).and_then(Value::as_u64);
    Compaction {
        id: entry.compaction_id.clone(),
        status,
        summary,
        error: entry.error.clone().or_else(|| {
            interrupted.then(|| "Compaction was interrupted before it finished.".to_owned())
        }),
        tokens_before: count("tokensBefore"),
        tokens_after: count("tokensAfter"),
        requested: entry
            .slash_command
            .as_ref()
            .map(|record| record.arguments.clone()),
    }
}

fn map_header(header: &SessionHeader) -> HeaderFields {
    let phase = match header.run_state {
        RunState::Submitting => "starting",
        RunState::Thinking | RunState::Streaming => "model",
        RunState::ToolRunning => "tools",
        RunState::WaitingForUser => "waiting",
        _ => "idle",
    };
    let plan = header
        .plan
        .as_ref()
        .map(|p| {
            p.entries
                .iter()
                .map(|e| PlanEntry {
                    content: e.content.clone(),
                    status: serde_json::to_value(&e.status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    // `UsageBreakdown` serializes in the live shape `TokenUsage::read` takes.
    let breakdown = header.tokens.as_ref().map(|t| Breakdown {
        latest: t
            .latest_request
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok())
            .and_then(|v| TokenUsage::read(&v, false)),
        totals: t
            .session_totals
            .as_ref()
            .and_then(|u| serde_json::to_value(u).ok())
            .and_then(|v| TokenUsage::read(&v, false)),
        partial: t.partial,
    });
    let accounting = header
        .accounting
        .as_ref()
        .and_then(|a| serde_json::to_value(a).ok())
        .and_then(|v| Accounting::read(Some(&v)));
    HeaderFields {
        phase: phase.into(),
        run_id: header.active_run_id.clone(),
        plan,
        usage: header.context.as_ref().map(|c| (c.used, c.size)),
        context_limit: header
            .provider
            .as_ref()
            .and_then(|p| p.context_limit)
            .filter(|size| *size > 0),
        breakdown,
        accounting,
        active_ms: header
            .active_time
            .as_ref()
            .map_or(0., |t| t.total_ms as f64),
        active_since_ms: header
            .active_time
            .as_ref()
            .and_then(|t| t.active_since_ms)
            .map(|t| t as f64),
        title: header.title.clone(),
        mode: header.current_mode.clone(),
        options: header.config_options.clone(),
        slash_commands: header.available_commands.clone(),
        history_more: header.history.has_older,
        history_before: header.history.before.clone(),
        undo_stack: header
            .undo_stack
            .iter()
            .map(|f: &CoreUndoFrame| UndoFrame {
                message_id: f.message_id.clone(),
                restore_failed: f.restore_failed,
            })
            .collect(),
        pending_inputs: header
            .pending_inputs
            .iter()
            .map(|i| PendingInput {
                input_id: i.input_id.clone(),
                delivery: serde_json::to_value(i.delivery)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                state: serde_json::to_value(i.state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                text: i
                    .blocks
                    .iter()
                    .filter_map(|b| serde_json::to_value(b).ok())
                    .map(|b| content_text(&b))
                    .collect(),
                attachments: i
                    .blocks
                    .iter()
                    .filter_map(|b| serde_json::to_value(b).ok())
                    .filter(|b| b.get("type").and_then(Value::as_str) != Some("text"))
                    .count(),
                discard_pending: false,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live(update: Value) -> SessionInput {
        SessionInput::AcpNotification {
            notification: json!({ "sessionId": "s1", "update": update }),
            replayed: false,
            at_ms: 0,
        }
    }

    fn with_run(mut update: Value, run: Option<&str>) -> Value {
        if let Some(run) = run {
            update["_meta"] = json!({ "querymt": { "runId": run } });
        }
        update
    }

    fn user(id: &str, run: Option<&str>) -> SessionInput {
        live(with_run(
            json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
                    "content": { "type": "text", "text": id } }),
            run,
        ))
    }

    fn agent(id: &str, text: &str, run: Option<&str>) -> SessionInput {
        live(with_run(
            json!({ "sessionUpdate": "agent_message_chunk", "messageId": id,
                    "content": { "type": "text", "text": text } }),
            run,
        ))
    }

    fn state(state: &str, run: &str) -> SessionInput {
        SessionInput::StateUpdate {
            notification: json!({ "sessionId": "s1", "state": state, "runId": run }),
            at_ms: 0,
        }
    }

    fn keys(p: &Projection) -> Vec<String> {
        p.items.iter().map(|i| i.key.clone()).collect()
    }

    #[test]
    fn unchanged_turns_keep_their_items() {
        let mut p = Projection::new("s1");
        let mut batch = Vec::new();
        for n in 0..200 {
            let run = format!("r{n}");
            batch.push(user(&format!("u{n}"), Some(&run)));
            batch.push(agent(&format!("a{n}"), "answer", Some(&run)));
        }
        p.feed(&batch);
        assert_eq!(p.slices().len(), 200);
        assert_eq!(p.items.len(), 400);
        let before: Vec<(String, u64)> = p.items.iter().map(|i| (i.key.clone(), i.rev)).collect();

        // Streaming into the last turn rebuilds only that turn.
        let report = p.feed(&[agent("a199", " more", Some("r199"))]);
        assert_eq!(report.updated, ["run-r199"]);
        assert_eq!(p.rebuilt_turns, 1);
        assert_eq!(p.items.len(), 400);
        let after: Vec<(String, u64)> = p.items.iter().map(|i| (i.key.clone(), i.rev)).collect();
        assert_eq!(before[..399], after[..399], "older rows are untouched");
        assert_eq!(before[399].0, after[399].0);
        assert!(after[399].1 > before[399].1, "the revision only grows");
        let ItemKind::Agent { text } = &p.items[399].kind else {
            panic!("agent row");
        };
        assert_eq!(text, "answer more");

        // A new run appends one slice and rebuilds one turn.
        p.feed(&[user("u200", Some("r200"))]);
        assert_eq!(p.rebuilt_turns, 1);
        assert_eq!(p.slices().len(), 201);
        assert_eq!(p.items.len(), 401);
    }

    #[test]
    fn steering_stays_in_the_running_turn() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", Some("r1")),
            agent("a1", "first", Some("r1")),
            user("steer", Some("r1")),
            agent("a2", "second", Some("r1")),
        ]);
        assert_eq!(p.slices().len(), 1);
        assert_eq!(
            keys(&p),
            ["user-u1", "assistant-a1", "user-steer", "assistant-a2"]
        );
        assert_eq!(p.turn_items("run-r1"), Some(0..4));
    }

    #[test]
    fn run_without_a_prompt_is_its_own_turn() {
        let mut p = Projection::new("s1");
        p.feed(&[user("u1", Some("r1")), agent("a1", "hi", Some("r1"))]);
        // A scheduled run: the state reports it, its output carries no run id.
        p.feed(&[state("running", "r2"), agent("a2", "scheduled", None)]);
        let ids: Vec<&str> = p.slices().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["run-r1", "run-r2"]);
        assert_eq!(p.turn_items("run-r2"), Some(2..3));
        assert_eq!(p.items[2].key, "assistant-a2");
        assert_eq!(p.header_fields().phase, "model");
    }

    fn history_event(seq: i64, kind: &str, data: Value) -> Value {
        json!({ "seq": seq, "session_seq": seq, "timestamp": 1000 + seq, "session_id": "s1",
                "origin": "local", "kind": { "type": kind, "data": data } })
    }

    /// A `session/load` response carrying a snapshot of `events`.
    fn snapshot(events: Vec<Value>, extra: Value) -> SessionInput {
        let mut snapshot = json!({
            "runtimeStatus": { "phase": "idle", "active_run_id": null, "steerable": false,
                "pending_steering_count": 0, "queued_input_count": 0, "run_started_at_ms": null,
                "observation": { "availability": "live" } },
            "audit": { "session_id": "s1", "events": events, "tasks": [], "intent_snapshots": [],
                "decisions": [], "progress_entries": [], "artifacts": [], "delegations": [],
                "generated_at": "2026-01-01T00:00:00Z" },
            "cursor": { "local_seq": 0, "remote_seq_by_source": {} },
            "accounting": { "version": 1, "session": { "knownSubtotalUsd": 0.0,
                "completeTotalUsd": 0.0, "complete": true, "billing": "empty", "reasons": [],
                "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
                "incompleteAttempts": 0, "unaccountedGenerations": 0 }, "cycle": null },
            "subagentUpdates": []
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            snapshot[key] = value.clone();
        }
        SessionInput::SnapshotLoaded {
            response: json!({ "_meta": { "querymt/sessionLoadSnapshot.v1": snapshot } }),
        }
    }

    fn tool_of(p: &Projection, id: &str) -> Arc<ToolCall> {
        p.items
            .iter()
            .find_map(|item| match &item.kind {
                ItemKind::Work(entries) => entries.iter().find_map(|e| match e {
                    WorkEntry::Tool(call) if call.id == id => Some(Arc::clone(call)),
                    _ => None,
                }),
                _ => None,
            })
            .expect("tool call row")
    }

    #[test]
    fn same_revision_reuses_the_tool_call() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", Some("r1")),
            live(with_run(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run shell",
                        "kind": "execute", "status": "in_progress" }),
                Some("r1"),
            )),
        ]);
        let first = tool_of(&p, "t1");

        // Another entry of the same turn changes; the tool call does not.
        p.feed(&[agent("a1", "text", Some("r1"))]);
        assert_eq!(p.rebuilt_turns, 1);
        assert!(Arc::ptr_eq(&first, &tool_of(&p, "t1")), "pointer kept");

        // A new revision of the tool call is a new value.
        p.feed(&[live(with_run(
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                    "status": "completed",
                    "content": [{ "type": "content",
                                  "content": { "type": "text", "text": "ok" } }] }),
            Some("r1"),
        ))]);
        let second = tool_of(&p, "t1");
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.status, "completed");
        assert_eq!(second.output(), "ok");
    }

    #[test]
    fn load_a_truncated_result() {
        let mut p = Projection::new("s1");
        let events = vec![
            history_event(
                1,
                "prompt_received",
                json!({ "content": "x", "message_id": "u1" }),
            ),
            history_event(
                2,
                "tool_call_start",
                json!({ "tool_call_id": "t1", "tool_name": "shell",
                        "arguments": "{\"cmd\":\"ls\"}" }),
            ),
            history_event(
                3,
                "tool_call_end",
                json!({ "tool_call_id": "t1", "tool_name": "shell", "is_error": false,
                        "result": "pre" }),
            ),
        ];
        let extra = json!({
            "truncated": [{ "tool_call_id": "t1", "part": "result", "full_bytes": 400000 }]
        });
        p.feed(&[snapshot(events, extra)]);
        let call = tool_of(&p, "t1");
        assert_eq!(call.truncated_result, Some(400_000));
        assert_eq!(call.truncated_args, None);
        assert_eq!(call.output(), "pre");

        // The full content replaces the prefix and clears the marker.
        let content: SessionInput = serde_json::from_value(json!({
            "type": "tool_content",
            "data": { "tool_call_id": "t1", "part": "result", "content": "the whole result" }
        }))
        .expect("tool content input");
        p.feed(&[content]);
        let call = tool_of(&p, "t1");
        assert_eq!(call.truncated_result, None);
        assert_eq!(call.output(), "the whole result");
    }

    #[test]
    fn delegate_links_its_child() {
        let mut p = Projection::new("s1");
        p.feed(&[
            live(
                json!({ "sessionUpdate": "subagent_update", "sessionId": "child-1",
                         "title": "Verifier" }),
            ),
            user("u1", Some("r1")),
            live(with_run(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "delegate",
                        "content": [{ "type": "session", "sessionId": "child-1" }] }),
                Some("r1"),
            )),
        ]);
        assert_eq!(tool_of(&p, "t1").child_session.as_deref(), Some("child-1"));

        // The reference can arrive on a later update.
        p.feed(&[
            live(with_run(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "delegate" }),
                Some("r1"),
            )),
            live(with_run(
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2",
                        "content": [{ "type": "session", "sessionId": "child-2" }] }),
                Some("r1"),
            )),
        ]);
        assert_eq!(tool_of(&p, "t2").child_session.as_deref(), Some("child-2"));
    }

    fn compaction_of(p: &Projection) -> Compaction {
        let found: Vec<&Compaction> = p
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Compaction(c) => Some(&**c),
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "one compaction row");
        found[0].clone()
    }

    fn request_end(seq: i64, context_tokens: u64) -> Value {
        history_event(
            seq,
            "llm_request_end",
            json!({ "usage": null, "tool_calls": 0, "finish_reason": null, "cost_usd": null,
                    "cumulative_cost_usd": null, "context_tokens": context_tokens,
                    "metrics": { "steps": 1, "turns": 1 } }),
        )
    }

    fn scope(complete: bool, total: Option<f64>) -> Value {
        json!({ "knownSubtotalUsd": total.unwrap_or(0.0), "completeTotalUsd": total,
                "complete": complete, "billing": "metered", "reasons": [],
                "pricedAttempts": 1, "excludedAttempts": 0, "unresolvedAttempts": 0,
                "incompleteAttempts": 0, "unaccountedGenerations": 0 })
    }

    #[test]
    fn compaction_live_and_after_reload() {
        let counts = |update: Value| {
            live(
                json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                         "status": update["status"], "_meta": { "querymt": {
                             "compaction": update["counts"] } } }),
            )
        };
        let mut live_view = Projection::new("s1");
        live_view.feed(&[
            user("u1", None),
            counts(json!({ "status": "in_progress", "counts": { "tokensBefore": 150000 } })),
            live(
                json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
                         "content": { "type": "text", "text": "short" } }),
            ),
            counts(json!({ "status": "completed",
                           "counts": { "tokensBefore": 150000, "tokensAfter": 20000 } })),
        ]);
        let live_row = compaction_of(&live_view);
        assert_eq!(live_row.status, CompactionStatus::Completed);
        assert_eq!(live_row.summary, "short");
        assert_eq!(live_row.tokens_before, Some(150_000));
        assert_eq!(live_row.tokens_after, Some(20_000));

        let mut reloaded = Projection::new("s1");
        reloaded.feed(&[snapshot(
            vec![
                history_event(
                    1,
                    "prompt_received",
                    json!({ "content": "hi", "message_id": "u1" }),
                ),
                request_end(2, 150_000),
                history_event(
                    3,
                    "compaction_start",
                    json!({ "token_estimate": 90000, "compaction_id": "c1" }),
                ),
                history_event(
                    4,
                    "compaction_end",
                    json!({ "summary": "short", "summary_len": 5, "compaction_id": "c1",
                            "context_tokens": 20000 }),
                ),
            ],
            json!({}),
        )]);
        let row = compaction_of(&reloaded);
        assert_eq!(row.status, CompactionStatus::Completed);
        assert_eq!(row.summary, "short");
        assert_eq!(
            (row.tokens_before, row.tokens_after),
            (live_row.tokens_before, live_row.tokens_after)
        );
        assert_eq!(row.detail(), live_row.detail());
    }

    fn record(name: &str, arguments: &str, source: &str, effect: &str) -> Value {
        json!({ "name": name, "arguments": arguments, "source": source, "effect": effect })
    }

    /// A live user message whose blocks are `text`, with an optional record.
    fn user_text(id: &str, text: &str, command: Option<Value>) -> SessionInput {
        let mut update = json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
                                 "content": { "type": "text", "text": text } });
        if let Some(command) = command {
            update["_meta"] = json!({ "querymt": { "slashCommand": command } });
        }
        live(update)
    }

    fn user_rows(p: &Projection) -> Vec<(String, Option<UserCommand>)> {
        p.items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::User { text, command, .. } => Some((text.clone(), command.clone())),
                _ => None,
            })
            .collect()
    }

    const TEMPLATE: &str = "The user invoked the slash command `/explain` with: closures";

    #[test]
    fn user_defined_command_shows_what_was_typed() {
        let mut p = Projection::new("s1");
        p.feed(&[user_text(
            "u1",
            TEMPLATE,
            Some(record("explain", "closures", "project", "prompt")),
        )]);
        assert_eq!(
            user_rows(&p),
            vec![(
                "/explain closures".to_owned(),
                Some(UserCommand {
                    name: "explain".into(),
                    arguments: "closures".into(),
                    expansion: TEMPLATE.into(),
                })
            )]
        );
        // A command with no arguments reads as `/name`.
        let mut bare = Projection::new("s1");
        bare.feed(&[user_text(
            "u1",
            "Do the thing.",
            Some(record("thing", "", "global", "prompt")),
        )]);
        assert_eq!(user_rows(&bare)[0].0, "/thing");
    }

    #[test]
    fn reloaded_prompt_command() {
        let mut live_view = Projection::new("s1");
        live_view.feed(&[user_text(
            "u1",
            TEMPLATE,
            Some(record("explain", "closures", "project", "prompt")),
        )]);
        let mut reloaded = Projection::new("s1");
        reloaded.feed(&[snapshot(
            vec![history_event(
                1,
                "prompt_received",
                json!({ "content": TEMPLATE, "message_id": "u1",
                        "slash_command": record("explain", "closures", "project", "prompt") }),
            )],
            json!({}),
        )]);
        assert_eq!(user_rows(&reloaded), user_rows(&live_view));
        assert!(user_rows(&reloaded)[0].1.is_some());
    }

    #[test]
    fn compact_with_instructions() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", None),
            live(json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                         "status": "in_progress",
                         "_meta": { "querymt": { "slashCommand":
                             record("compact", "keep the API decisions", "builtin", "action") } } })),
        ]);
        assert_eq!(
            compaction_of(&p).requested.as_deref(),
            Some("keep the API decisions")
        );
    }

    #[test]
    fn compact_without_instructions() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", None),
            live(
                json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                         "status": "in_progress",
                         "_meta": { "querymt": { "slashCommand":
                             record("compact", "", "builtin", "action") } } }),
            ),
        ]);
        assert_eq!(compaction_of(&p).requested.as_deref(), Some(""));
    }

    #[test]
    fn automatic_compaction() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", None),
            live(
                json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
                         "status": "in_progress" }),
            ),
        ]);
        assert_eq!(compaction_of(&p).requested, None);
    }

    #[test]
    fn agent_without_records() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user_text("u1", "/worktree list", None),
            agent("a1", "No worktrees.", None),
        ]);
        assert_eq!(user_rows(&p), vec![("/worktree list".to_owned(), None)]);
        assert!(
            p.items
                .iter()
                .any(|i| matches!(&i.kind, ItemKind::Agent { text } if text == "No worktrees."))
        );
    }

    #[test]
    fn prompt_that_only_starts_with_a_slash() {
        let mut p = Projection::new("s1");
        p.feed(&[user_text("u1", "/tmp is full, why?", None)]);
        assert_eq!(user_rows(&p), vec![("/tmp is full, why?".to_owned(), None)]);
    }

    #[test]
    fn todo_list_after_reload() {
        let args = r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"in_progress"}]}"#;
        let mut p = Projection::new("s1");
        p.feed(&[snapshot(
            vec![
                history_event(
                    1,
                    "prompt_received",
                    json!({ "content": "go", "message_id": "u1" }),
                ),
                history_event(
                    2,
                    "tool_call_start",
                    json!({ "tool_call_id": "t1", "tool_name": "todowrite", "arguments": args }),
                ),
                history_event(
                    3,
                    "tool_call_end",
                    json!({ "tool_call_id": "t1", "tool_name": "todowrite",
                            "is_error": false, "result": "ok" }),
                ),
            ],
            json!({}),
        )]);
        let plan = p.header_fields().plan;
        let got: Vec<(&str, &str)> = plan
            .iter()
            .map(|e| (e.content.as_str(), e.status.as_str()))
            .collect();
        assert_eq!(got, [("a", "pending"), ("b", "in_progress")]);
        // The todo list is the plan, not a tool call row.
        assert!(
            !p.items
                .iter()
                .any(|i| matches!(i.kind, ItemKind::Work(ref w) if !w.is_empty())),
            "no work group for the todo tool"
        );
    }

    #[test]
    fn subscription_usage() {
        let excluded = json!({
            "knownSubtotalUsd": 0.0, "completeTotalUsd": null, "complete": false,
            "billing": "excluded", "reasons": [],
            "pricedAttempts": 0, "excludedAttempts": 3, "unresolvedAttempts": 0,
            "incompleteAttempts": 0, "unaccountedGenerations": 0
        });
        let mut p = Projection::new("s1");
        p.feed(&[snapshot(
            vec![history_event(
                1,
                "prompt_received",
                json!({ "content": "x", "message_id": "u1" }),
            )],
            json!({ "accounting": { "version": 1, "session": excluded, "cycle": null } }),
        )]);
        let accounting = p.header_fields().accounting.expect("accounting");
        assert_eq!(accounting.session.billing, "excluded");
        assert_eq!(accounting.session.complete_total_usd, None);
        assert!(!accounting.session.complete);

        // Live accounting replaces the snapshot's.
        p.feed(&[live(json!({
            "sessionUpdate": "usage_update", "used": 10, "size": 100,
            "_meta": { "querymt": { "accounting": {
                "version": 1, "session": scope(true, Some(0.42)), "cycle": null } } }
        }))]);
        let accounting = p.header_fields().accounting.expect("accounting");
        assert_eq!(accounting.session.complete_total_usd, Some(0.42));
        assert_eq!(p.header_fields().usage, Some((10, 100)));
    }

    #[test]
    fn model_change_keeps_the_new_limit() {
        let context = |context_position: i64, model_position: i64| {
            snapshot(
                vec![history_event(
                    5,
                    "prompt_received",
                    json!({ "content": "x", "message_id": "u1" }),
                )],
                json!({ "summary": {
                    "usage": { "latestRequest": null, "sessionTotals": null, "partial": false,
                               "requestCount": 1, "contextTokens": 12000,
                               "contextPosition": context_position,
                               // `ScopeTotals` of the summary (snake case).
                               "accounting": {
                                   "known_subtotal_usd": 0.0, "budget_known_subtotal_usd": 0.0,
                                   "components": { "input_usd": 0.0, "output_usd": 0.0,
                                       "cache_read_usd": 0.0, "cache_write_usd": 0.0,
                                       "cache_write_1h_usd": 0.0 },
                                   "priced_attempts": 0, "excluded_attempts": 0,
                                   "unresolved_attempts": 0, "incomplete_attempts": 0,
                                   "budget_incomplete_attempts": 0,
                                   "settled_reasons": [] } },
                    "provider": { "provider": "p", "model": "m", "contextLimit": 200000,
                                  "position": model_position },
                    "pendingInputs": [],
                    "childSessions": []
                } }),
            )
        };
        // The measurement predates the model change: the core drops it, the
        // new model's size stays known (the view shows the last usage).
        let mut stale = Projection::new("s1");
        let report = stale.feed(&[context(3, 7)]);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(stale.header_fields().usage, None);
        assert_eq!(stale.header_fields().context_limit, Some(200_000));

        // The measurement follows the model change: kept.
        let mut fresh = Projection::new("s1");
        fresh.feed(&[context(8, 7)]);
        assert_eq!(fresh.header_fields().usage, Some((12_000, 200_000)));
    }

    #[test]
    fn no_cost_without_model_calls() {
        let empty = json!({
            "knownSubtotalUsd": 0.0, "completeTotalUsd": 0.0, "complete": true,
            "billing": "empty", "reasons": [],
            "pricedAttempts": 0, "excludedAttempts": 0, "unresolvedAttempts": 0,
            "incompleteAttempts": 0, "unaccountedGenerations": 0
        });
        let mut p = Projection::new("s1");
        p.feed(&[snapshot(
            vec![history_event(
                1,
                "prompt_received",
                json!({ "content": "x", "message_id": "u1" }),
            )],
            json!({ "accounting": { "version": 1, "session": empty, "cycle": null } }),
        )]);
        assert!(p.header_fields().accounting.is_none());
        assert!(p.core.header().cost.is_none());
    }

    fn input_note(id: &str, state: &str) -> SessionInput {
        SessionInput::InputState {
            notification: serde_json::from_value(json!({
                "version": 1, "session_id": "s1", "input_id": id,
                "delivery": "queue", "state": state
            }))
            .expect("input state"),
        }
    }

    fn sending(id: &str, text: &str) -> PendingInput {
        PendingInput {
            input_id: id.into(),
            delivery: "steer".into(),
            state: "sending".into(),
            text: text.into(),
            attachments: 0,
            discard_pending: false,
        }
    }

    #[test]
    fn stopped_run_note_follows_its_turn() {
        let mut p = Projection::new("s1");
        p.feed(&[user("u1", Some("r1")), agent("a1", "one", Some("r1"))]);
        p.push_notice("Stopped.", false);
        assert_eq!(keys(&p), ["user-u1", "assistant-a1", "notice-1"]);

        // The next run comes after the note, and streaming into it leaves
        // the note where it was.
        p.feed(&[user("u2", Some("r2")), agent("a2", "two", Some("r2"))]);
        p.feed(&[agent("a2", " more", Some("r2"))]);
        assert_eq!(
            keys(&p),
            [
                "user-u1",
                "assistant-a1",
                "notice-1",
                "user-u2",
                "assistant-a2"
            ]
        );

        // A note pushed now follows the second run.
        p.push_notice("Prompt failed", true);
        assert_eq!(keys(&p).last().map(String::as_str), Some("notice-2"));
        assert_eq!(p.items.len(), 6);

        // Updating the first run keeps its note.
        p.feed(&[agent("a1", "!", Some("r1"))]);
        assert_eq!(keys(&p)[..3], ["user-u1", "assistant-a1", "notice-1"]);
    }

    #[test]
    fn a_note_before_any_turn_stays_first() {
        let mut p = Projection::new("s1");
        p.push_notice("The agent is not connected.", true);
        p.feed(&[user("u1", Some("r1"))]);
        assert_eq!(keys(&p), ["notice-1", "user-u1"]);
        p.feed(&[agent("a1", "x", Some("r1"))]);
        assert_eq!(keys(&p), ["notice-1", "user-u1", "assistant-a1"]);
    }

    #[test]
    fn input_state_overtakes_the_submit_response() {
        let mut p = Projection::new("s1");
        p.overlay.add_input(sending("gpui-1", "steer me"));

        // The agent's notification arrives before the submit response.
        p.feed(&[input_note("agent-9", "queued")]);
        p.overlay.note_input_state("agent-9", "queue", "queued");
        assert!(p.overlay.input_notes.contains_key("agent-9"));

        // The response re-keys the input; both lists name it, shown once with
        // the text only this client has.
        p.overlay.rekey_input("gpui-1", "agent-9");
        p.overlay.set_input_state("agent-9", "queue", "accepted");
        p.overlay.apply_input_notes("agent-9", "gpui-1");
        let inputs = p.header_fields().pending_inputs;
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].input_id, "agent-9");
        assert_eq!(inputs[0].state, "queued");
        assert_eq!(inputs[0].text, "steer me");

        // The terminal state arriving first: nothing is left waiting.
        p.overlay.add_input(sending("gpui-2", "second"));
        p.feed(&[input_note("agent-10", "started")]);
        p.overlay.note_input_state("agent-10", "queue", "started");
        p.overlay.rekey_input("gpui-2", "agent-10");
        p.overlay.set_input_state("agent-10", "queue", "accepted");
        p.overlay.apply_input_notes("agent-10", "gpui-2");
        let ids: Vec<String> = p
            .header_fields()
            .pending_inputs
            .into_iter()
            .map(|i| i.input_id)
            .collect();
        assert_eq!(ids, ["agent-9"]);
    }

    #[test]
    fn idle_session_drops_inputs_that_were_never_confirmed() {
        let mut p = Projection::new("s1");
        p.overlay.add_input(sending("gpui-1", "in flight"));
        p.overlay.add_input(PendingInput {
            state: "queued".into(),
            ..sending("agent-2", "queued")
        });
        p.sync_inputs();
        assert!(!p.running());
        let ids: Vec<String> = p
            .header_fields()
            .pending_inputs
            .into_iter()
            .map(|i| i.input_id)
            .collect();
        assert_eq!(ids, ["gpui-1"], "only the request still in flight stays");
    }

    #[test]
    fn active_time_is_the_cores() {
        let mut p = Projection::new("s1");
        p.feed(&[SessionInput::StateUpdate {
            notification: json!({ "sessionId": "s1", "state": "running", "runId": "r1",
                                  "atMs": 1_000 }),
            at_ms: 1_200,
        }]);
        let f = p.header_fields();
        assert_eq!(f.active_since_ms, Some(1_000.0), "the host's start");
        p.feed(&[SessionInput::StateUpdate {
            notification: json!({ "sessionId": "s1", "state": "idle", "runId": "r1",
                                  "stopReason": "end_turn", "atMs": 3_500 }),
            at_ms: 3_700,
        }]);
        let f = p.header_fields();
        assert_eq!(f.active_ms, 2_500.0);
        assert_eq!(f.active_since_ms, None);
    }

    #[test]
    fn notices_keep_their_turn_after_an_older_page() {
        let mut p = Projection::new("s1");
        p.feed(&[snapshot(
            vec![
                history_event(
                    20,
                    "prompt_received",
                    json!({ "content": "x", "message_id": "u20" }),
                ),
                history_event(
                    21,
                    "assistant_message_stored",
                    json!({ "content": "y", "message_id": "a20" }),
                ),
            ],
            json!({ "history": { "complete": false, "before": "t1" } }),
        )]);
        p.push_notice("Stopped.", false);
        assert_eq!(keys(&p).last().map(String::as_str), Some("notice-1"));

        let older: Vec<Value> = (0..3)
            .flat_map(|n| {
                let s = 1 + n * 2;
                [
                    history_event(
                        s,
                        "prompt_received",
                        json!({ "content": "q", "message_id": format!("u{s}") }),
                    ),
                    history_event(
                        s + 1,
                        "assistant_message_stored",
                        json!({ "content": "r", "message_id": format!("a{s}") }),
                    ),
                ]
            })
            .collect();
        let report = p.feed(&[SessionInput::HistoryPage {
            response: json!({ "events": older,
                              "history": { "complete": true, "before": null },
                              "truncated": [] }),
        }]);
        assert_eq!(report.added.len(), 3, "{report:?}");
        assert_eq!(p.slices().len(), 4);
        // Only the new turns were projected; the note still ends the
        // original turn, which is now last.
        assert_eq!(p.rebuilt_turns, 3);
        let keys = keys(&p);
        assert_eq!(keys.last().map(String::as_str), Some("notice-1"));
        assert_eq!(keys.iter().filter(|k| k.starts_with("notice-")).count(), 1);
        assert_eq!(keys[keys.len() - 2], "assistant-a20");
    }

    #[test]
    fn a_note_whose_turn_disappears_follows_the_last_turn() {
        let mut p = Projection::new("s1");
        p.feed(&[user("u1", None)]);
        p.push_notice("note", false);
        // A reload replaces the history with other turns.
        p.feed(&[snapshot(
            vec![history_event(
                1,
                "prompt_received",
                json!({ "content": "again", "message_id": "u9" }),
            )],
            json!({}),
        )]);
        let keys = keys(&p);
        assert_eq!(keys.iter().filter(|k| k.starts_with("notice-")).count(), 1);
        assert_eq!(keys.last().map(String::as_str), Some("notice-1"));
    }

    #[test]
    fn reasoning_and_tools_form_one_work_group() {
        let mut p = Projection::new("s1");
        p.feed(&[
            user("u1", Some("r1")),
            live(with_run(
                json!({ "sessionUpdate": "agent_thought_chunk", "messageId": "a1",
                        "content": { "type": "text", "text": "hmm" } }),
                Some("r1"),
            )),
            live(with_run(
                json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run shell",
                        "kind": "execute", "status": "in_progress" }),
                Some("r1"),
            )),
            agent("a1", "done", Some("r1")),
        ]);
        assert_eq!(p.items.len(), 3);
        let ItemKind::Work(entries) = &p.items[1].kind else {
            panic!("work group");
        };
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], WorkEntry::Thought(_)));
        assert!(matches!(entries[1], WorkEntry::Tool(_)));
        assert!(p.items[1].key.starts_with("reasoning-") || !p.items[1].key.is_empty());
    }
}
