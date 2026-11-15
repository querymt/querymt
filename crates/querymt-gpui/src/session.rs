//! Session transcript model: folds ACP `session/update` notifications (both the
//! replay sent during `session/load` and live updates) into display items.
//!
//! Pure data, no UI. Every mutated item gets a fresh `rev` so the view can
//! re-measure only what changed.

use std::collections::HashMap;

use serde_json::Value;

use crate::usage::{Accounting, Breakdown};

#[derive(Clone, Debug)]
pub struct Diff {
    pub path: String,
    pub old: Option<String>,
    pub new: String,
    /// 1-based line where this hunk starts in the old / new file, when known.
    pub old_start: Option<usize>,
    pub new_start: Option<usize>,
    /// `old` / `new` are just the edited snippet (not whole files): show every line.
    pub snippet: bool,
}

impl Diff {
    fn whole_file(path: String, old: Option<String>, new: String) -> Self {
        Self {
            path,
            old,
            new,
            old_start: None,
            new_start: None,
            snippet: false,
        }
    }
}

/// Tools whose arguments describe a file change we can render as a diff.
const EDIT_TOOLS: [&str; 4] = ["edit", "multiedit", "replace_symbol", "write_file"];

/// One `H kind old=a,b new=c,d +x -y` line of an edit receipt.
struct ReceiptHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

/// Per-path hunks of a compact edit receipt (`OK paths=… / P path / H …`).
fn parse_receipt(output: &str) -> HashMap<String, Vec<ReceiptHunk>> {
    let mut files: HashMap<String, Vec<ReceiptHunk>> = HashMap::new();
    let mut lines = output.trim().lines();
    if !lines.next().is_some_and(|l| l.starts_with("OK paths=")) {
        return files;
    }
    let mut current: Option<String> = None;
    for line in lines {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("P ") {
            current = Some(normalize_path(path));
            files.entry(normalize_path(path)).or_default();
        } else if let (Some(rest), Some(path)) = (line.strip_prefix("H "), &current) {
            let field = |key: &str| -> Option<(usize, usize)> {
                let v = rest.split_whitespace().find_map(|t| t.strip_prefix(key))?;
                let (a, b) = v.split_once(',')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            };
            if let (Some((old_start, old_count)), Some((new_start, new_count))) =
                (field("old="), field("new="))
            {
                files.entry(path.clone()).or_default().push(ReceiptHunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                });
            }
        }
    }
    files
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches('/').to_string()
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::to_owned)
}

fn tool_file_path(v: &Value) -> Option<String> {
    str_field(v, &["path", "filePath", "file_path", "file"]).filter(|p| !p.is_empty())
}

/// (old, new) of an edit-like object; `None` when it has neither side.
fn replacement_pair(v: &Value) -> Option<(String, String)> {
    let old = str_field(
        v,
        &["old_string", "oldString", "old_str", "oldText", "old_text"],
    );
    let new = str_field(
        v,
        &["new_string", "newString", "new_str", "newText", "new_text"],
    );
    if old.is_none() && new.is_none() {
        return None;
    }
    Some((old.unwrap_or_default(), new.unwrap_or_default()))
}

fn line_count(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.trim_end_matches('\n').split('\n').count()
    }
}

/// Diffs for a completed edit tool call that returned no ACP `diff` block, built
/// from its arguments; line numbers come from the receipt when it matches.
fn synthesized_diffs(call: &ToolCall) -> Vec<Diff> {
    let name = call.name().to_lowercase();
    if call.running() || call.failed() || !EDIT_TOOLS.contains(&name.as_str()) {
        return vec![];
    }
    let Some(input) = call.input.as_ref().filter(|v| v.is_object()) else {
        return vec![];
    };
    let result = call.output.trim();
    let ok = result.is_empty()
        || result.starts_with("OK ")
        || (name == "replace_symbol" && result.starts_with("Updated "))
        || (name == "write_file" && result.starts_with('{'));
    if !ok {
        return vec![];
    }
    let parent = tool_file_path(input).unwrap_or_else(|| "file".into());
    if name == "write_file" {
        let content = str_field(input, &["content", "contents"]).unwrap_or_default();
        if content.is_empty() {
            return vec![];
        }
        return vec![Diff {
            path: parent,
            old: None,
            new: content,
            old_start: None,
            new_start: Some(1),
            snippet: true,
        }];
    }
    let items: Vec<(String, (String, String))> = match input
        .get("edits")
        .or_else(|| input.get("replacements"))
        .and_then(Value::as_array)
    {
        Some(items) => items
            .iter()
            .filter_map(|it| {
                Some((
                    tool_file_path(it).unwrap_or_else(|| parent.clone()),
                    replacement_pair(it)?,
                ))
            })
            .collect(),
        None => replacement_pair(input)
            .map(|pair| (parent.clone(), pair))
            .into_iter()
            .collect(),
    };
    let mut diffs: Vec<Diff> = items
        .into_iter()
        .map(|(path, (old, new))| Diff {
            path,
            old: Some(old),
            new,
            old_start: None,
            new_start: None,
            snippet: true,
        })
        .collect();
    // Anchor hunks to real line numbers when the receipt lists one hunk per edit
    // with matching sizes (receipts are in file order; edits usually are too).
    let receipts = parse_receipt(result);
    for (path, hunks) in receipts {
        let mine: Vec<usize> = diffs
            .iter()
            .enumerate()
            .filter(|(_, d)| normalize_path(&d.path) == path)
            .map(|(i, _)| i)
            .collect();
        if mine.len() != hunks.len() {
            continue;
        }
        let fits = mine.iter().zip(&hunks).all(|(&i, h)| {
            line_count(diffs[i].old.as_deref().unwrap_or_default()) == h.old_count
                && line_count(&diffs[i].new) == h.new_count
        });
        if !fits {
            continue;
        }
        for (&i, h) in mine.iter().zip(&hunks) {
            diffs[i].old_start = Some(h.old_start);
            diffs[i].new_start = Some(h.new_start);
        }
    }
    diffs
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub input: Option<Value>,
    pub output: String,
    pub diffs: Vec<Diff>,
    pub locations: Vec<String>,
    /// Lines (added, removed) across `diffs`; computed once when the diffs arrive.
    pub stats: (usize, usize),
    /// `diffs` were derived from the arguments rather than sent by the agent.
    pub synthesized: bool,
}

/// Line counts of a diff: (added, removed).
pub fn diff_stats(d: &Diff) -> (usize, usize) {
    let old = d.old.clone().unwrap_or_default();
    let diff = similar::TextDiff::from_lines(&old, &d.new);
    let (mut adds, mut dels) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => adds += 1,
            similar::ChangeTag::Delete => dels += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (adds, dels)
}

impl ToolCall {
    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "in_progress" | "")
    }

    pub fn failed(&self) -> bool {
        self.status == "failed"
    }

    /// Short tool name: `Run shell` -> `shell`.
    pub fn name(&self) -> String {
        let name = self.title.strip_prefix("Run ").unwrap_or(&self.title);
        if name.is_empty() {
            "tool".into()
        } else {
            name.to_string()
        }
    }

    /// Exit code of a shell result, when the output is the usual JSON envelope.
    pub fn exit_code(&self) -> Option<i64> {
        let trimmed = self.output.trim();
        if !trimmed.starts_with('{') {
            return None;
        }
        serde_json::from_str::<Value>(trimmed)
            .ok()?
            .get("exit_code")?
            .as_i64()
    }

    /// Full shell command (all lines), when there is one.
    pub fn command(&self) -> Option<String> {
        self.input
            .as_ref()?
            .get("command")?
            .as_str()
            .map(str::to_owned)
    }

    /// The most telling argument: command, path, pattern, …
    pub fn summary(&self) -> String {
        let Some(input) = &self.input else {
            return self.locations.first().cloned().unwrap_or_default();
        };
        for key in [
            "command",
            "path",
            "file_path",
            "pattern",
            "query",
            "url",
            "name",
            "symbol",
            "prompt",
        ] {
            if let Some(v) = input.get(key).and_then(Value::as_str) {
                return v.lines().next().unwrap_or_default().to_string();
            }
        }
        if let Some(paths) = input.get("paths").and_then(Value::as_array) {
            return paths
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
        }
        let text = input.to_string();
        if text == "{}" || text == "null" {
            String::new()
        } else {
            text.chars().take(160).collect()
        }
    }

    /// Output prettified: shell JSON results become stdout + stderr.
    pub fn pretty_output(&self) -> String {
        let trimmed = self.output.trim();
        if trimmed.starts_with('{')
            && let Ok(v) = serde_json::from_str::<Value>(trimmed)
        {
            let out = v.get("stdout").and_then(Value::as_str);
            let err = v.get("stderr").and_then(Value::as_str);
            if out.is_some() || err.is_some() {
                let mut text = out.unwrap_or_default().trim_end().to_string();
                if let Some(err) = err.filter(|e| !e.trim().is_empty()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(err.trim_end());
                }
                if let Some(code) = v
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .filter(|c| *c != 0)
                {
                    text.push_str(&format!("\n[exit code {code}]"));
                }
                return text;
            }
        }
        self.output.clone()
    }
}

/// One entry of a work group: a reasoning step or a tool call.
#[derive(Clone, Debug)]
pub enum WorkEntry {
    Thought { id: String, text: String },
    Tool(Box<ToolCall>),
}

impl WorkEntry {
    pub fn id(&self) -> &str {
        match self {
            WorkEntry::Thought { id, .. } => id,
            WorkEntry::Tool(call) => &call.id,
        }
    }
}

/// A file or image the user sent with a prompt (image bytes are kept for thumbnails).
#[derive(Clone, Debug, PartialEq)]
pub struct UserAttachment {
    pub name: String,
    pub mime: String,
    pub size: Option<u64>,
    /// Base64 payload, present for inline images.
    pub data: Option<String>,
}

impl UserAttachment {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    /// From an ACP `image` / `resource` / `resource_link` content block.
    fn from_block(block: &Value) -> Option<Self> {
        let s = |p: &str| block.pointer(p).and_then(Value::as_str).map(str::to_owned);
        let size = block.pointer("/_meta/querymt/size").and_then(Value::as_u64);
        match block.get("type").and_then(Value::as_str)? {
            "image" => Some(Self {
                name: s("/_meta/querymt/filename").unwrap_or_else(|| "image".into()),
                mime: s("/mimeType").unwrap_or_else(|| "image/png".into()),
                size,
                data: s("/data"),
            }),
            "resource" | "resource_link" => {
                let uri = s("/_meta/querymt/filename")
                    .or_else(|| s("/resource/uri"))
                    .or_else(|| s("/name"))
                    .or_else(|| s("/uri"))
                    .unwrap_or_else(|| "file".into());
                let name = uri.rsplit('/').next().unwrap_or(&uri).to_string();
                let mime = s("/resource/mimeType")
                    .or_else(|| s("/mimeType"))
                    .unwrap_or_default();
                let data = mime
                    .starts_with("image/")
                    .then(|| s("/resource/blob"))
                    .flatten();
                Some(Self {
                    name,
                    mime,
                    size,
                    data,
                })
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    User {
        text: String,
        attachments: Vec<UserAttachment>,
        local: bool,
    },
    Agent {
        text: String,
    },
    /// Consecutive reasoning steps and tool calls between two replies, shown as
    /// one collapsible work group (like the desktop app).
    Work(Vec<WorkEntry>),
    Notice {
        text: String,
        error: bool,
    },
    /// Context-compaction boundary (desktop `SessionCompactionDivider`).
    Compaction(Box<Compaction>),
}

/// Lifecycle of a context compaction (ACP `compaction_update.status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

impl CompactionStatus {
    fn parse(status: Option<&str>) -> Self {
        match status {
            Some("completed") => Self::Completed,
            Some("failed") => Self::Failed,
            Some("cancelled") => Self::Cancelled,
            _ => Self::InProgress,
        }
    }
}

/// One context compaction, shown as a divider in the transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    pub id: String,
    pub status: CompactionStatus,
    /// Summary that replaced the compacted context (streams while running).
    pub summary: String,
    pub error: Option<String>,
    /// Context tokens when the compaction started.
    pub tokens_before: Option<u64>,
    /// Context tokens after it completed.
    pub tokens_after: Option<u64>,
    /// Completed live: the next `usage_update` carries the reduced context size.
    awaiting_usage: bool,
}

pub const COMPACTING_LABEL: &str = "Compacting context…";

/// True for the native `/compact [instructions]` command.
pub fn is_compact_command(prompt: &str) -> bool {
    let p = prompt.trim_start();
    p.get(..8)
        .is_some_and(|head| head.eq_ignore_ascii_case("/compact"))
        && p[8..].chars().next().is_none_or(char::is_whitespace)
}

/// `12`, `950`, `12.5k`, `120k` (desktop `formatTokens`).
fn format_tokens(value: u64) -> String {
    if value < 1000 {
        return value.to_string();
    }
    let k = value as f64 / 1000.;
    if k >= 100. {
        format!("{}k", k.round())
    } else {
        let k = (k * 10.).round() / 10.;
        if k.fract() == 0. {
            format!("{k:.0}k")
        } else {
            format!("{k:.1}k")
        }
    }
}

impl Compaction {
    pub fn title(&self) -> &'static str {
        match self.status {
            CompactionStatus::InProgress => COMPACTING_LABEL,
            CompactionStatus::Completed => "Context compacted",
            CompactionStatus::Failed => "Compaction failed",
            CompactionStatus::Cancelled => "Compaction cancelled",
        }
    }

    /// Secondary text next to the title: token counts, or the failure reason.
    pub fn detail(&self) -> Option<String> {
        match self.status {
            CompactionStatus::InProgress => self
                .tokens_before
                .map(|b| format!("{} tokens in context", format_tokens(b))),
            CompactionStatus::Failed => self.error.clone().filter(|e| !e.is_empty()),
            CompactionStatus::Cancelled => None,
            CompactionStatus::Completed => match (self.tokens_before, self.tokens_after) {
                (Some(b), Some(a)) => Some(format!(
                    "{} → {} tokens",
                    format_tokens(b),
                    format_tokens(a)
                )),
                (None, Some(a)) => Some(format!("{} tokens remaining", format_tokens(a))),
                _ => None,
            },
        }
    }
}

/// Where a compaction from the history audit sits in the transcript: after
/// the item it followed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CompactionAnchor {
    /// User or agent message item key.
    Item(String),
    /// Tool call id (resolved to its work group).
    Tool(String),
}

/// Fold the compaction lifecycle events of a raw history (`compaction_start`,
/// `compaction_summary_chunk`, `compaction_end`, `compaction_failed`), each
/// with the transcript item it followed. Legacy records without an id pair
/// their terminal with the latest open start, like the desktop app.
fn fold_compactions(events: &[Value]) -> Vec<(Option<CompactionAnchor>, Compaction)> {
    let mut out: Vec<(Option<CompactionAnchor>, Compaction)> = Vec::new();
    let mut anchor: Option<CompactionAnchor> = None;
    let mut used: Option<u64> = None;
    let mut open_legacy: Option<String> = None;
    for (n, ev) in events.iter().enumerate() {
        let kind = ev
            .pointer("/kind/type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = ev.pointer("/kind/data").unwrap_or(&Value::Null);
        let s = |k: &str| {
            data.get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        match kind {
            "prompt_received" | "assistant_message_stored" => {
                if let Some(id) = s("message_id") {
                    anchor = Some(CompactionAnchor::Item(id.to_string()));
                }
            }
            "tool_call_start" | "tool_call_end" => {
                if let Some(id) = s("tool_call_id") {
                    anchor = Some(CompactionAnchor::Tool(id.to_string()));
                }
            }
            "llm_request_end" => {
                if let Some(c) = data.get("context_tokens").and_then(Value::as_u64) {
                    used = Some(c);
                }
            }
            "compaction_start"
            | "compaction_summary_chunk"
            | "compaction_end"
            | "compaction_failed" => {
                let id = match s("compaction_id") {
                    Some(id) => id.to_string(),
                    None => {
                        if kind == "compaction_start" || open_legacy.is_none() {
                            let seq = ev.get("seq").and_then(Value::as_i64).unwrap_or(n as i64);
                            open_legacy = Some(format!("querymt-compaction-legacy-{seq}"));
                        }
                        open_legacy.clone().unwrap_or_default()
                    }
                };
                let at = match out.iter().position(|(_, c)| c.id == id) {
                    Some(at) => at,
                    None => {
                        out.push((
                            anchor.clone(),
                            Compaction {
                                id: id.clone(),
                                tokens_before: used,
                                ..Default::default()
                            },
                        ));
                        out.len() - 1
                    }
                };
                let c = &mut out[at].1;
                match kind {
                    "compaction_summary_chunk" => {
                        c.summary.push_str(s("content").unwrap_or_default())
                    }
                    "compaction_end" => {
                        c.status = CompactionStatus::Completed;
                        if let Some(summary) = s("summary") {
                            c.summary = summary.to_string();
                        }
                        if let Some(after) = data.get("context_tokens").and_then(Value::as_u64) {
                            c.tokens_after = Some(after);
                            used = Some(after);
                        }
                    }
                    "compaction_failed" => {
                        let cancelled = data
                            .get("cancelled")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        if cancelled {
                            c.status = CompactionStatus::Cancelled;
                            c.error = None;
                        } else {
                            c.status = CompactionStatus::Failed;
                            c.error = s("reason").map(str::to_owned);
                        }
                    }
                    _ => {}
                }
                if kind != "compaction_start"
                    && kind != "compaction_summary_chunk"
                    && s("compaction_id").is_none()
                {
                    open_legacy = None;
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct Item {
    pub key: String,
    pub rev: u64,
    pub kind: ItemKind,
}

#[derive(Clone, Debug)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

/// A message sent while a turn runs (steer / queue), shown in the activity
/// dock's waiting-messages bubble until the agent applies or starts it.
/// Mirrors desktop's `PendingSessionInput`.
#[derive(Clone, Debug, Default)]
pub struct PendingInput {
    pub input_id: String,
    /// `steer` or `queue` (the agent may turn a steer into a queue).
    pub delivery: String,
    /// `sending`, `accepted`, `queued`, …
    pub state: String,
    pub text: String,
    pub attachments: usize,
    pub discard_pending: bool,
}

#[derive(Default)]
pub struct SessionView {
    pub agent_id: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub items: Vec<Item>,
    pub loading: bool,
    pub error: Option<String>,
    /// Copyable diagnostic report behind `error` (request, raw agent error).
    pub error_details: Option<String>,
    /// Runtime phase reported by the agent (`idle`, `running`, …).
    pub phase: String,
    /// A prompt sent from this client is in flight.
    pub prompting: bool,
    pub plan: Vec<PlanEntry>,
    /// Context window (used, size) in tokens.
    pub usage: Option<(u64, u64)>,
    /// Latest-request / session-total token breakdown (`_meta.querymt.usage`).
    pub breakdown: Option<Breakdown>,
    /// Replacement API-spend accounting (`_meta.querymt.accounting`).
    pub accounting: Option<Accounting>,
    /// Standard ACP `usage_update.cost`, only shown without accounting.
    pub standard_cost_usd: Option<f64>,
    /// Settled time spent actively processing prompts.
    pub active_work_ms: f64,
    /// Epoch ms the current stretch of work started, while running.
    pub active_started_at: Option<f64>,
    pub mode: Option<String>,
    pub model: Option<String>,
    /// Session config options (`session/new` / `session/load` shape), kept
    /// current through `config_option_update`.
    pub options: Vec<Value>,
    /// `availableModes` from `session/load`, for agents without a `mode` config option.
    pub modes: Vec<(String, String)>,
    /// Config option ids with a change in flight.
    pub pending_options: std::collections::HashSet<String>,
    /// Older turns exist beyond the loaded window.
    pub history_more: bool,
    /// `history.before` cursor for `_querymt/session/history` (older turns).
    pub history_before: Option<String>,
    /// An older-history page is being fetched.
    pub history_loading: bool,
    pub history_error: Option<String>,
    /// `active_run_id` of the running turn (`runtimeChanged` / load snapshot);
    /// steering must name it as `expected_run_id`.
    pub run_id: Option<String>,
    /// Messages sent mid-turn that the agent hasn't applied or started yet.
    pub pending_inputs: Vec<PendingInput>,
    /// `inputState` notifications that arrived while their submit was in
    /// flight, keyed by input id: (delivery, state).
    pub input_notes: HashMap<String, (String, String)>,
    /// Workspace undo frames (`querymt/session/undoStack`), oldest first.
    pub undo_stack: Vec<UndoFrame>,
    /// A workspace undo or redo is in flight.
    pub undo_pending: bool,
    /// Notice to append once the transcript has (re)loaded, e.g. after an undo.
    pub after_load_notice: Option<String>,
    tool_index: HashMap<String, usize>,
    counter: u64,
    /// (name, mime) of local-prompt attachments whose agent echo hasn't
    /// arrived yet. The text echo can come first and clear `local`, so the
    /// echoed images must be matched against this instead.
    echo_pending: Vec<(String, String)>,
}

/// One frame of the workspace undo stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoFrame {
    /// User turn the undo went back to.
    pub message_id: String,
    /// The undo that produced this frame failed part-way: it deleted no
    /// history, and redo only recovers the state from before it.
    pub restore_failed: bool,
}

impl UndoFrame {
    /// Frames of an `undo_stack` array in an undo/redo/undoStack response.
    pub fn parse_stack(response: &Value) -> Vec<Self> {
        response
            .get("undo_stack")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some(Self {
                    message_id: f.get("message_id")?.as_str()?.to_string(),
                    restore_failed: f
                        .get("restore_failed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect()
    }
}

/// A `select` config option, flattened for the composer.
#[derive(Clone, Debug)]
pub struct SelectOption {
    pub id: String,
    pub current: String,
    /// (value, label)
    pub choices: Vec<(String, String)>,
}

impl SelectOption {
    pub fn current_label(&self) -> String {
        self.choices
            .iter()
            .find(|(v, _)| *v == self.current)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| self.current.clone())
    }
}

/// ACP kind the agent assigns a tool (mirrors querymt `tool_kind_for_tool`).
fn tool_kind(name: &str) -> &'static str {
    match name {
        "search_text" | "mdq" => "search",
        "write_file" | "edit" | "multiedit" | "accept_delegation_result" => "edit",
        "delete_file" => "delete",
        "shell" => "execute",
        "web_fetch" | "browse" => "fetch",
        _ => "other",
    }
}

/// `session/update` payloads for one raw history event (`{ kind: { type, data } }`),
/// matching the agent's replay projection.
fn replay_updates(ev: &Value) -> Vec<Value> {
    use serde_json::json;
    let kind = ev
        .pointer("/kind/type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = ev.pointer("/kind/data").cloned().unwrap_or(Value::Null);
    let s = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);
    match kind {
        "tool_call_start" => {
            let name = s("tool_name").unwrap_or_default();
            if is_todo_write(&name) {
                return vec![];
            }
            let args = s("arguments")
                .and_then(|a| serde_json::from_str::<Value>(&a).ok())
                .unwrap_or(Value::Null);
            vec![json!({
                "sessionUpdate": "tool_call",
                "toolCallId": s("tool_call_id"),
                "title": format!("Run {name}"),
                "kind": tool_kind(&name),
                "status": "in_progress",
                "rawInput": args,
            })]
        }
        "tool_call_end" => {
            let name = s("tool_name").unwrap_or_default();
            if is_todo_write(&name) {
                return vec![];
            }
            let failed = data
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": s("tool_call_id"),
                "title": format!("Run {name}"),
                "kind": tool_kind(&name),
                "status": if failed { "failed" } else { "completed" },
                "content": [{ "type": "content", "content": { "type": "text", "text": s("result").unwrap_or_default() } }],
            })]
        }
        _ => vec![],
    }
}

fn is_todo_write(name: &str) -> bool {
    matches!(name, "todowrite" | "mcp_todowrite")
}

/// Plan entries of a `todowrite` call's arguments (cancelled entries omitted,
/// as in the agent's ACP projection).
fn todo_plan(arguments: &str) -> Option<Vec<PlanEntry>> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    Some(
        parsed
            .get("todos")?
            .as_array()?
            .iter()
            .filter_map(|t| {
                let status = t.get("status")?.as_str()?;
                matches!(status, "pending" | "in_progress" | "completed").then(|| PlanEntry {
                    content: t
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    status: status.into(),
                })
            })
            .collect(),
    )
}

/// Text and attachments of a user prompt's content blocks.
fn prompt_parts(blocks: &[Value]) -> (String, Vec<UserAttachment>) {
    let mut text = String::new();
    let mut attachments = Vec::new();
    for block in blocks {
        if let Some(a) = UserAttachment::from_block(block) {
            attachments.push(a);
        } else if block.get("type").and_then(Value::as_str) == Some("text") {
            text.push_str(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
        }
    }
    (text, attachments)
}

fn content_text(content: &Value) -> String {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

impl SessionView {
    pub fn new(agent_id: &str, session_id: &str, cwd: &str, title: &str) -> Self {
        Self {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    pub fn running(&self) -> bool {
        self.prompting || (!self.phase.is_empty() && self.phase != "idle")
    }

    /// The compaction currently running, if any.
    pub fn active_compaction(&self) -> Option<&Compaction> {
        self.items.iter().rev().find_map(|i| match &i.kind {
            ItemKind::Compaction(c) if c.status == CompactionStatus::InProgress => Some(&**c),
            _ => None,
        })
    }

    /// Index of the compaction item `id`, created (in progress) when unseen.
    fn ensure_compaction(&mut self, id: &str) -> usize {
        let key = format!("compaction-{id}");
        if let Some(ix) = self.items.iter().position(|i| i.key == key) {
            return ix;
        }
        let compaction = Compaction {
            id: id.to_string(),
            tokens_before: self.usage.map(|u| u.0),
            ..Default::default()
        };
        self.push(key, ItemKind::Compaction(Box::new(compaction)));
        self.items.len() - 1
    }

    /// Live `compaction_update`. `summary` and `error` patch: omitted keeps the
    /// value, null clears it. A settled compaction is never reopened.
    fn apply_compaction_update(&mut self, update: &Value) {
        let Some(id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        let status = CompactionStatus::parse(update.get("status").and_then(Value::as_str));
        let ix = self.ensure_compaction(id);
        let ItemKind::Compaction(c) = &mut self.items[ix].kind else {
            return;
        };
        if c.status != CompactionStatus::InProgress && status == CompactionStatus::InProgress {
            return;
        }
        c.status = status;
        if let Some(summary) = update.get("summary") {
            c.summary = summary
                .as_array()
                .into_iter()
                .flatten()
                .map(content_text)
                .collect();
        }
        if let Some(error) = update.get("error") {
            c.error = error.as_str().map(str::to_owned);
        }
        c.awaiting_usage = status == CompactionStatus::Completed;
        self.touch(ix);
    }

    /// Live `compaction_summary_chunk`: append to a running compaction.
    fn apply_compaction_chunk(&mut self, update: &Value) {
        let Some(id) = update.get("compactionId").and_then(Value::as_str) else {
            return;
        };
        let text = update.get("content").map(content_text).unwrap_or_default();
        let ix = self.ensure_compaction(id);
        if let ItemKind::Compaction(c) = &mut self.items[ix].kind
            && c.status == CompactionStatus::InProgress
        {
            c.summary.push_str(&text);
            self.touch(ix);
        }
    }

    /// The usage update right after a completed compaction carries its
    /// post-compaction context size; any other update ends that window.
    fn settle_compaction_usage(&mut self, used: Option<u64>) {
        for ix in 0..self.items.len() {
            if let ItemKind::Compaction(c) = &mut self.items[ix].kind
                && c.awaiting_usage
            {
                c.awaiting_usage = false;
                if used.is_some() {
                    c.tokens_after = used;
                }
                self.touch(ix);
            }
        }
    }

    /// Place compactions folded from a history audit after the items they
    /// followed. Ones whose anchor isn't loaded belong to older history and
    /// are skipped unless the history is complete (then they lead).
    fn insert_compactions(
        &mut self,
        folded: Vec<(Option<CompactionAnchor>, Compaction)>,
        history_complete: bool,
    ) {
        for (anchor, mut compaction) in folded {
            let key = format!("compaction-{}", compaction.id);
            // A replayed `compaction_update` arrives before the conversation it
            // belongs to: move it into place, keeping a summary only it carries.
            if let Some(ix) = self.items.iter().position(|i| i.key == key) {
                let old = self.items.remove(ix);
                if let ItemKind::Compaction(old) = old.kind {
                    if compaction.summary.is_empty() {
                        compaction.summary = old.summary;
                    }
                    if compaction.error.is_none() {
                        compaction.error = old.error;
                    }
                    if compaction.status == CompactionStatus::InProgress {
                        compaction.status = old.status;
                    }
                }
                self.reindex_tools();
            }
            let anchored = anchor.as_ref().and_then(|a| match a {
                CompactionAnchor::Item(k) => self
                    .items
                    .iter()
                    .position(|i| &i.key == k || i.key == format!("work-{k}")),
                CompactionAnchor::Tool(id) => self.tool_index.get(id).copied(),
            });
            let mut at = match (anchored, anchor.is_some()) {
                (Some(ix), _) => ix + 1,
                (None, false) => 0,
                (None, true) if history_complete => 0,
                (None, true) => continue,
            };
            // Successive compactions after the same item keep their order.
            while self
                .items
                .get(at)
                .is_some_and(|i| matches!(i.kind, ItemKind::Compaction(_)))
            {
                at += 1;
            }
            let rev = self.next_rev();
            self.items.insert(
                at,
                Item {
                    key,
                    rev,
                    kind: ItemKind::Compaction(Box::new(compaction)),
                },
            );
            self.reindex_tools();
        }
    }

    fn reindex_tools(&mut self) {
        self.tool_index.clear();
        for (ix, item) in self.items.iter().enumerate() {
            if let ItemKind::Work(entries) = &item.kind {
                for e in entries {
                    if let WorkEntry::Tool(c) = e {
                        self.tool_index.insert(c.id.clone(), ix);
                    }
                }
            }
        }
    }

    /// Rebuild compactions from the load snapshot's audit (the replayed
    /// updates carry no position), then mark ones still open after the run
    /// ended as interrupted.
    fn rebuild_compactions(&mut self, events: &[Value], history_complete: bool) {
        self.insert_compactions(fold_compactions(events), history_complete);
        if !self.running() {
            for ix in 0..self.items.len() {
                if let ItemKind::Compaction(c) = &mut self.items[ix].kind
                    && c.status == CompactionStatus::InProgress
                {
                    c.status = CompactionStatus::Failed;
                    c.error.get_or_insert_with(|| {
                        "Compaction was interrupted before it finished.".into()
                    });
                    self.touch(ix);
                }
            }
        }
    }

    /// Message id of the user turn a workspace undo would go back to: the
    /// latest turn before the newest completed undo, like the desktop app.
    /// Only turns the agent has identified (not local echoes) qualify.
    pub fn undo_target(&self) -> Option<String> {
        let turns = self.undo_turns();
        let end = self.undo_frontier(&turns).unwrap_or(turns.len());
        turns[..end].last().map(|t| t.to_string())
    }

    /// Agent-identified user turns, in transcript order.
    fn undo_turns(&self) -> Vec<&str> {
        let mut turns: Vec<&str> = Vec::new();
        for item in &self.items {
            if let ItemKind::User { local: false, .. } = item.kind
                && !item.key.starts_with("local-")
                && !item.key.starts_with("msg-")
                && !turns.contains(&item.key.as_str())
            {
                turns.push(&item.key);
            }
        }
        turns
    }

    /// Position (in `turns`) of the newest completed undo's turn.
    fn undo_frontier(&self, turns: &[&str]) -> Option<usize> {
        self.undo_stack
            .iter()
            .rev()
            .find(|f| !f.restore_failed)
            .and_then(|f| turns.iter().position(|t| *t == f.message_id))
    }

    /// Turns a workspace undo to `message_id` rolls back (that turn and every
    /// later live turn), or `None` when that turn is already undone.
    pub fn undo_affected_turns(&self, message_id: &str) -> Option<usize> {
        let turns = self.undo_turns();
        let at = turns.iter().position(|t| *t == message_id)?;
        let end = self.undo_frontier(&turns).unwrap_or(turns.len());
        (at < end).then_some(end - at)
    }

    /// First user turn rolled back by the newest completed undo, if any.
    pub fn reverted_from(&self) -> Option<&str> {
        let turns = self.undo_turns();
        self.undo_frontier(&turns).map(|at| turns[at])
    }

    /// User turn that redo would restore (top of the undo stack).
    pub fn redo_target(&self) -> Option<&str> {
        self.undo_stack.last().map(|f| f.message_id.as_str())
    }

    /// Start or settle the active-work clock to match `running()`.
    pub fn sync_work(&mut self, now: f64) {
        match (self.running(), self.active_started_at) {
            (true, None) => self.active_started_at = Some(now),
            (false, Some(start)) => {
                self.active_work_ms += (now - start).max(0.);
                self.active_started_at = None;
            }
            _ => {}
        }
    }

    /// Usage figures from `querymt/sessionLoadSnapshot.v1`: accounting, the
    /// audit's request usage and active time, and (for a windowed load with
    /// older history) the summary's whole-session totals.
    pub fn apply_load_snapshot(&mut self, snap: &Value) {
        if let Some(a) = Accounting::read(snap.get("accounting")) {
            self.accounting = Some(a);
        }
        // The client advertises snapshot history, so the agent skips the
        // `session/update` replay and the transcript is built from the audit.
        // (An agent that replays anyway already filled it.)
        if self.items.is_empty() {
            let events: &[Value] = snap
                .pointer("/audit/events")
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice);
            let prompts: &[Value] = snap
                .get("userPrompts")
                .and_then(Value::as_array)
                .map_or(&[], Vec::as_slice);
            self.apply_history_events(events, prompts);
            if !self.running() {
                self.fail_unfinished_tools();
            }
        }
        let ts = |ev: &Value| {
            ev.get("timestamp")
                .and_then(Value::as_f64)
                .filter(|t| *t >= 0.)
                .map(|s| s * 1000.)
        };
        let mut started: Option<f64> = None;
        let mut missing = false;
        let settle = |started: &mut Option<f64>, end: Option<f64>, acc: &mut f64| {
            if let (Some(s), Some(e)) = (*started, end)
                && e >= s
            {
                *acc += e - s;
            }
            *started = None;
        };
        let mut active = 0.;
        // Folded from scratch: replayed `usage_update`s may already have set an
        // agent-computed breakdown, which wins over this reconstruction.
        let mut folded: Option<Breakdown> = None;
        let (mut used, mut size) = self
            .usage
            .map_or((None, None), |(u, s)| (Some(u), (s > 0).then_some(s)));
        for ev in snap
            .pointer("/audit/events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let data = ev.pointer("/kind/data");
            match ev.pointer("/kind/type").and_then(Value::as_str) {
                Some("llm_request_start") => started = started.or(ts(ev)),
                Some("provider_changed") => {
                    if let Some(l) = data
                        .and_then(|d| d.get("context_limit"))
                        .and_then(Value::as_u64)
                        .filter(|l| *l > 0)
                    {
                        size = Some(l);
                    }
                }
                Some("llm_request_end") => {
                    let u = data.and_then(|d| d.get("usage")).filter(|u| !u.is_null());
                    if u.is_some() || folded.is_some() {
                        let mut b = Breakdown::append_historical(folded, u);
                        b.partial |= missing;
                        folded = Some(b);
                    } else {
                        missing = true;
                    }
                    if let Some(c) = data
                        .and_then(|d| d.get("context_tokens"))
                        .and_then(Value::as_u64)
                    {
                        used = Some(c);
                    }
                    settle(&mut started, ts(ev), &mut active);
                }
                Some("compaction_end") => {
                    if let Some(c) = data
                        .and_then(|d| d.get("context_tokens"))
                        .and_then(Value::as_u64)
                    {
                        used = Some(c);
                    }
                }
                Some("cancelled" | "error") => settle(&mut started, ts(ev), &mut active),
                _ => {}
            }
        }
        self.active_work_ms += active;
        // A request still open in the audit is the live stretch of a running turn.
        if self.running() && self.active_started_at.is_none() {
            self.active_started_at = started;
        }
        if self.breakdown.is_none() {
            self.breakdown = folded;
        }
        // A windowed load only carries recent turns: its summary has the totals.
        if snap.pointer("/history/complete").and_then(Value::as_bool) == Some(false)
            && let Some(summary) = snap.get("summary")
        {
            {
                let u = summary.get("usage");
                let side = |k: &str| {
                    u.and_then(|u| u.get(k))
                        .and_then(|v| crate::usage::TokenUsage::read(v, false))
                };
                let (latest, totals) = (side("latestRequest"), side("sessionTotals"));
                self.breakdown = (latest.is_some() || totals.is_some()).then(|| Breakdown {
                    latest,
                    totals,
                    partial: u.and_then(|u| u.get("partial")).and_then(Value::as_bool)
                        == Some(true),
                });
                if let Some(c) = u
                    .and_then(|u| u.get("contextTokens"))
                    .and_then(Value::as_u64)
                {
                    used = Some(c);
                }
                if let Some(l) = summary
                    .pointer("/provider/contextLimit")
                    .and_then(Value::as_u64)
                    .filter(|l| *l > 0)
                {
                    size = Some(l);
                }
            }
        }
        if let Some(u) = used {
            self.usage = Some((u, size.unwrap_or(0)));
        }
        let events: &[Value] = snap
            .pointer("/audit/events")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        let complete = snap.pointer("/history/complete").and_then(Value::as_bool) != Some(false);
        self.rebuild_compactions(events, complete);
    }

    /// Copy of everything except the transcript items and plan (cheap; for
    /// rendering chrome while the store is not borrowed).
    pub fn shallow(&self) -> Self {
        Self {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            items: Vec::new(),
            loading: self.loading,
            error: self.error.clone(),
            error_details: self.error_details.clone(),
            phase: self.phase.clone(),
            prompting: self.prompting,
            plan: Vec::new(),
            usage: self.usage,
            breakdown: self.breakdown,
            accounting: self.accounting.clone(),
            standard_cost_usd: self.standard_cost_usd,
            active_work_ms: self.active_work_ms,
            active_started_at: self.active_started_at,
            mode: self.mode.clone(),
            model: self.model.clone(),
            options: self.options.clone(),
            modes: self.modes.clone(),
            pending_options: self.pending_options.clone(),
            history_more: self.history_more,
            history_before: self.history_before.clone(),
            history_loading: self.history_loading,
            history_error: self.history_error.clone(),
            run_id: self.run_id.clone(),
            pending_inputs: self.pending_inputs.clone(),
            input_notes: HashMap::new(),
            undo_stack: self.undo_stack.clone(),
            undo_pending: self.undo_pending,
            after_load_notice: None,
            tool_index: HashMap::new(),
            counter: 0,
            echo_pending: Vec::new(),
        }
    }

    /// Prepend an older-history page (raw agent events, ascending) to the
    /// transcript, projecting them the way the agent's replay does.
    pub fn prepend_events(&mut self, events: &[Value], prompts: &[Value]) {
        let mut older = SessionView {
            counter: self.counter,
            ..Default::default()
        };
        older.apply_history_events(events, prompts);
        older.insert_compactions(fold_compactions(events), true);
        self.counter = older.counter;
        let known: std::collections::HashSet<&str> =
            self.items.iter().map(|i| i.key.as_str()).collect();
        let mut items: Vec<Item> = older
            .items
            .into_iter()
            .filter(|i| !known.contains(i.key.as_str()))
            .collect();
        if items.is_empty() {
            return;
        }
        items.append(&mut self.items);
        self.items = items;
        self.tool_index.clear();
        for (ix, item) in self.items.iter().enumerate() {
            if let ItemKind::Work(entries) = &item.kind {
                for e in entries {
                    if let WorkEntry::Tool(c) = e {
                        self.tool_index.insert(c.id.clone(), ix);
                    }
                }
            }
        }
    }

    /// Build transcript items from raw history events, like the desktop app's
    /// `activeSessionFromLoadResponse`: prompts (with their structured blocks
    /// from `userPrompts`, so attachments survive), assistant messages and
    /// reasoning, tool calls and the todo plan. Structured prompts with no
    /// `prompt_received` event (e.g. steering) are placed by timestamp.
    pub fn apply_history_events(&mut self, events: &[Value], prompts: &[Value]) {
        let mut structured: Vec<(u64, String, i64, &[Value])> = prompts
            .iter()
            .filter_map(|p| {
                let id = p
                    .get("message_id")
                    .or_else(|| p.get("messageId"))?
                    .as_str()?;
                let blocks = p.get("blocks")?.as_array()?;
                let order = p
                    .get("message_order")
                    .or_else(|| p.get("messageOrder"))
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX);
                let ts = p.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
                Some((order, id.to_string(), ts, blocks.as_slice()))
            })
            .collect();
        structured.sort_by_key(|p| (p.0, p.2));
        let mut restored: std::collections::HashSet<String> = Default::default();
        // Event timestamp of each item this build added, for placing prompts.
        let mut stamps: HashMap<String, i64> = HashMap::new();
        let mut last_assistant: Option<String> = None;
        for (n, ev) in events.iter().enumerate() {
            let kind = ev
                .pointer("/kind/type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let data = ev.pointer("/kind/data").unwrap_or(&Value::Null);
            let s = |k: &str| {
                data.get(k)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            };
            let fallback = format!(
                "snapshot-{}",
                ev.get("seq").and_then(Value::as_i64).unwrap_or(n as i64)
            );
            let before = self.items.len();
            match kind {
                "prompt_received" => {
                    let id = s("message_id")
                        .or_else(|| s("messageId"))
                        .unwrap_or(fallback);
                    if self.items.iter().any(|i| i.key == id) {
                        continue;
                    }
                    let (text, attachments) =
                        match structured.iter().find(|p| p.1 == id && !p.3.is_empty()) {
                            Some(p) => {
                                restored.insert(id.clone());
                                prompt_parts(p.3)
                            }
                            None => match data.get("content") {
                                Some(Value::Array(blocks)) => prompt_parts(blocks),
                                Some(Value::String(t)) => (t.clone(), Vec::new()),
                                _ => (String::new(), Vec::new()),
                            },
                        };
                    if text.is_empty() && attachments.is_empty() {
                        continue;
                    }
                    self.push(
                        id,
                        ItemKind::User {
                            text,
                            attachments,
                            local: false,
                        },
                    );
                }
                "assistant_content_delta" => {
                    let id = s("message_id")
                        .or_else(|| last_assistant.clone())
                        .unwrap_or(fallback);
                    last_assistant = Some(id.clone());
                    self.apply_chunk(
                        "agent_message_chunk",
                        Some(id),
                        s("content").unwrap_or_default(),
                    );
                }
                "assistant_thinking_delta" => {
                    let id = s("message_id")
                        .or_else(|| last_assistant.clone())
                        .unwrap_or(fallback);
                    last_assistant = Some(id.clone());
                    if let Some(text) = s("content") {
                        self.apply_thought(Some(id), text);
                    }
                }
                "assistant_message_stored" => {
                    let id = s("message_id")
                        .or_else(|| last_assistant.clone())
                        .unwrap_or(fallback);
                    last_assistant = Some(id.clone());
                    // Streamed reasoning already covers the stored parts.
                    if !self.has_thought(&id) {
                        let parts: Vec<String> = data
                            .get("reasoning_parts")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned)
                            .collect();
                        let parts = if parts.is_empty() {
                            s("thinking").into_iter().collect()
                        } else {
                            parts
                        };
                        for text in parts {
                            self.apply_thought(Some(id.clone()), text);
                        }
                    }
                    if let Some(text) = s("content") {
                        self.set_agent_text(id, text);
                    }
                }
                "tool_call_start" | "tool_call_end" => {
                    if kind == "tool_call_start"
                        && s("tool_name").as_deref().is_some_and(is_todo_write)
                        && let Some(plan) = s("arguments").as_deref().and_then(todo_plan)
                    {
                        self.plan = plan;
                    }
                    for update in replay_updates(ev) {
                        self.apply(&update);
                    }
                }
                _ => {}
            }
            if let Some(ts) = ev.get("timestamp").and_then(Value::as_i64) {
                for item in &self.items[before.min(self.items.len())..] {
                    stamps.entry(item.key.clone()).or_insert(ts);
                }
            }
        }
        // Prompts the events don't mention go before the first later item.
        for (_, id, ts, blocks) in structured {
            if restored.contains(&id) || self.items.iter().any(|i| i.key == id) {
                continue;
            }
            let (text, attachments) = prompt_parts(blocks);
            if text.is_empty() && attachments.is_empty() {
                continue;
            }
            let at = self
                .items
                .iter()
                .position(|i| stamps.get(&i.key).is_some_and(|t| *t > ts))
                .unwrap_or(self.items.len());
            let rev = self.next_rev();
            self.items.insert(
                at,
                Item {
                    key: id,
                    rev,
                    kind: ItemKind::User {
                        text,
                        attachments,
                        local: false,
                    },
                },
            );
            self.reindex_tools();
        }
    }

    /// The run is over: tools still marked running never got a result.
    fn fail_unfinished_tools(&mut self) {
        for ix in 0..self.items.len() {
            let mut changed = false;
            if let ItemKind::Work(entries) = &mut self.items[ix].kind {
                for e in entries.iter_mut() {
                    if let WorkEntry::Tool(call) = e
                        && call.running()
                    {
                        call.status = "failed".into();
                        if call.output.is_empty() {
                            call.output = "Run ended before a tool result was recorded.".into();
                        }
                        changed = true;
                    }
                }
            }
            if changed {
                self.touch(ix);
            }
        }
    }

    fn has_thought(&self, message_id: &str) -> bool {
        self.items.iter().any(|i| match &i.kind {
            ItemKind::Work(entries) => entries
                .iter()
                .any(|e| matches!(e, WorkEntry::Thought { id, .. } if id == message_id)),
            _ => false,
        })
    }

    /// Final text of assistant message `id` (replaces streamed deltas).
    fn set_agent_text(&mut self, id: String, text: String) {
        if let Some(ix) = self.items.iter().position(|i| i.key == id)
            && let ItemKind::Agent { text: t } = &mut self.items[ix].kind
        {
            *t = text;
            self.touch(ix);
            return;
        }
        self.apply_chunk("agent_message_chunk", Some(id), text);
    }

    fn next_rev(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    fn push(&mut self, key: String, kind: ItemKind) {
        let rev = self.next_rev();
        self.items.push(Item { key, rev, kind });
    }

    fn touch(&mut self, ix: usize) {
        let rev = self.next_rev();
        self.items[ix].rev = rev;
    }

    pub fn push_local_user(&mut self, text: &str, attachments: Vec<UserAttachment>) {
        let key = format!("local-{}", self.counter + 1);
        self.echo_pending = attachments
            .iter()
            .map(|a| (a.name.clone(), a.mime.clone()))
            .collect();
        self.push(
            key,
            ItemKind::User {
                text: text.into(),
                attachments,
                local: true,
            },
        );
    }

    /// Apply a `runtimeChanged` / snapshot `runtime` object: phase and active run.
    pub fn apply_runtime(&mut self, runtime: &Value) {
        if let Some(phase) = runtime.get("phase").and_then(Value::as_str) {
            self.phase = phase.into();
        }
        self.run_id = runtime
            .get("active_run_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if !self.running() {
            // Nothing can still be waiting once the session is idle.
            self.pending_inputs.retain(|i| i.state == "sending");
            self.input_notes.clear();
        }
    }

    /// Track a message sent mid-turn until the agent applies or starts it.
    pub fn add_pending_input(&mut self, input: PendingInput) {
        self.pending_inputs.retain(|i| i.input_id != input.input_id);
        self.pending_inputs.push(input);
    }

    /// Update (or drop, once terminal) a pending input from a lifecycle
    /// state: `accepted`, `queued`, `applied`, `started`, `discarded`, `failed`.
    pub fn set_input_state(&mut self, input_id: &str, delivery: &str, state: &str) {
        if matches!(state, "applied" | "started" | "discarded" | "failed") {
            self.pending_inputs.retain(|i| i.input_id != input_id);
            return;
        }
        if let Some(i) = self
            .pending_inputs
            .iter_mut()
            .find(|i| i.input_id == input_id)
        {
            i.state = state.into();
            if !delivery.is_empty() {
                i.delivery = delivery.into();
            }
        }
    }

    /// Re-key a pending input when the agent assigns its own input id.
    pub fn rekey_input(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        if let Some(i) = self.pending_inputs.iter_mut().find(|i| i.input_id == from) {
            i.input_id = to.into();
        }
    }

    pub fn push_notice(&mut self, text: impl Into<String>, error: bool) {
        let key = format!("notice-{}", self.counter + 1);
        self.push(
            key,
            ItemKind::Notice {
                text: text.into(),
                error,
            },
        );
    }

    /// Concatenated agent text of the turn containing item `ix` (for copying).
    pub fn turn_text(items: &[Item], ix: usize) -> String {
        if items.is_empty() {
            return String::new();
        }
        let start = items[..=ix.min(items.len() - 1)]
            .iter()
            .rposition(|i| matches!(i.kind, ItemKind::User { .. }))
            .map_or(0, |u| u + 1);
        let end = items[start..]
            .iter()
            .position(|i| matches!(i.kind, ItemKind::User { .. }))
            .map_or(items.len(), |u| start + u);
        items[start..end]
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Agent { text } => Some(text.trim()),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn select_option(&self, aliases: &[&str]) -> Option<SelectOption> {
        let norm = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .map(|s| s.to_lowercase().replace(['-', ' '], "_"))
        };
        let o = self.options.iter().find(|o| {
            o.get("type")
                .and_then(Value::as_str)
                .is_none_or(|t| t == "select")
                && ["id", "category", "name"]
                    .iter()
                    .filter_map(|k| norm(o.get(*k)))
                    .any(|v| aliases.contains(&v.as_str()))
        })?;
        let mut choices = Vec::new();
        let push = |choices: &mut Vec<(String, String)>, e: &Value| {
            if let Some(v) = e.get("value").and_then(Value::as_str) {
                let label = e.get("name").and_then(Value::as_str).unwrap_or(v);
                choices.push((v.to_string(), label.to_string()));
            }
        };
        for entry in o
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match entry.get("options").and_then(Value::as_array) {
                Some(group) => group.iter().for_each(|e| push(&mut choices, e)),
                None => push(&mut choices, entry),
            }
        }
        Some(SelectOption {
            id: o.get("id")?.as_str()?.to_string(),
            current: o
                .get("currentValue")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            choices,
        })
    }

    /// Mode control: the `mode` config option, else `availableModes`.
    pub fn mode_option(&self) -> Option<SelectOption> {
        if let Some(o) = self.select_option(&["mode"]) {
            return Some(o);
        }
        (!self.modes.is_empty()).then(|| SelectOption {
            id: String::new(),
            current: self.mode.clone().unwrap_or_default(),
            choices: self.modes.clone(),
        })
    }

    pub fn reasoning_option(&self) -> Option<SelectOption> {
        self.select_option(&["thought_level", "reasoning", "reasoning_effort", "thought"])
    }

    /// Short model label (`anthropic/claude-x` -> `claude-x`), from the model
    /// option's choice label when it has one.
    pub fn model_label(&self) -> Option<String> {
        let id = self.model.as_deref()?;
        if let Some(o) = self.select_option(&["model"])
            && let Some((_, label)) = o.choices.iter().find(|(v, _)| v == id)
        {
            return Some(label.clone());
        }
        Some(id.rsplit('/').next().unwrap_or(id).to_string())
    }

    /// Apply the `update` object of a `session/update` notification.
    pub fn apply(&mut self, update: &Value) {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_id = update
            .get("messageId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match kind {
            "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
                let content = update.get("content");
                if kind == "user_message_chunk"
                    && let Some(a) = content.and_then(UserAttachment::from_block)
                {
                    return self.apply_user_attachment(message_id, a);
                }
                let text = content.map(content_text).unwrap_or_default();
                self.apply_chunk(kind, message_id, text);
            }
            "tool_call" => self.apply_tool(update, true),
            "tool_call_update" => self.apply_tool(update, false),
            "plan" => {
                self.plan = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|e| PlanEntry {
                        content: e
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                        status: e
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    })
                    .collect();
            }
            "usage_update" => {
                let n = |k: &str| update.get(k).and_then(Value::as_u64);
                match (n("used"), n("size").filter(|s| *s > 0)) {
                    (Some(used), Some(size)) => self.usage = Some((used, size)),
                    (Some(used), None) => self.usage = Some((used, self.usage.map_or(0, |u| u.1))),
                    _ => {}
                }
                let meta = update.get("_meta");
                if let Some(b) = Breakdown::from_meta(meta) {
                    self.breakdown = Some(b);
                }
                if let Some(c) = crate::usage::read_cost_usd(update.get("cost")) {
                    self.standard_cost_usd = Some(c);
                }
                if let Some(a) = Accounting::from_meta(meta) {
                    self.accounting = Some(a);
                }
                self.settle_compaction_usage(n("used"));
            }
            "compaction_update" => self.apply_compaction_update(update),
            "compaction_summary_chunk" => self.apply_compaction_chunk(update),
            "current_mode_update" => {
                self.mode = update
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(o) = self
                    .options
                    .iter_mut()
                    .find(|o| o.get("id").and_then(Value::as_str) == Some("mode"))
                    && let Some(m) = &self.mode
                {
                    o["currentValue"] = Value::String(m.clone());
                }
            }
            "available_commands_update" => {}
            "session_info_update" => {
                if let Some(title) = update
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    self.title = title.into();
                }
                if let Some(a) = Accounting::from_meta(update.get("_meta")) {
                    self.accounting = Some(a);
                }
            }
            "config_option_update" => self.apply_config(update.get("configOptions")),
            _ => {}
        }
    }

    pub fn apply_config(&mut self, options: Option<&Value>) {
        let Some(list) = options.and_then(Value::as_array) else {
            return;
        };
        self.options = list.clone();
        for o in list {
            let current = o
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match o.get("id").and_then(Value::as_str) {
                Some("model") => self.model = current,
                Some("mode") => self.mode = current,
                _ => {}
            }
        }
    }

    /// `modes` object of `session/load` / `session/new`.
    pub fn apply_modes(&mut self, modes: Option<&Value>) {
        let Some(modes) = modes else { return };
        if let Some(id) = modes.get("currentModeId").and_then(Value::as_str) {
            self.mode = Some(id.into());
        }
        self.modes = modes
            .get("availableModes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                let name = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string();
                Some((id, name))
            })
            .collect();
    }

    /// Total (added, removed, files) of the turn's edits up to item `ix`,
    /// counting from the preceding user message.
    pub fn turn_change_stats(items: &[Item], ix: usize) -> (usize, usize, usize) {
        let start = items[..=ix]
            .iter()
            .rposition(|i| matches!(i.kind, ItemKind::User { .. }))
            .map_or(0, |u| u + 1);
        let (mut adds, mut dels) = (0usize, 0usize);
        let mut files: Vec<&str> = Vec::new();
        for item in &items[start..=ix] {
            if let ItemKind::Work(entries) = &item.kind {
                for e in entries {
                    if let WorkEntry::Tool(call) = e {
                        adds += call.stats.0;
                        dels += call.stats.1;
                        for d in &call.diffs {
                            if !files.contains(&d.path.as_str()) {
                                files.push(&d.path);
                            }
                        }
                    }
                }
            }
        }
        (adds, dels, files.len())
    }

    /// An `image` / `resource` block of a user message: attach it to the
    /// message being built (or the local echo), or start a new user item.
    fn apply_user_attachment(&mut self, message_id: Option<String>, attachment: UserAttachment) {
        if let Some(ix) = self.items.len().checked_sub(1) {
            let same = message_id
                .as_ref()
                .is_none_or(|id| &self.items[ix].key == id);
            let item = &mut self.items[ix];
            if let ItemKind::User {
                attachments, local, ..
            } = &mut item.kind
            {
                // Echo of our own prompt (before or after its text echo): the
                // local copy already lists it.
                let echoed = self
                    .echo_pending
                    .iter()
                    .position(|(n, m)| *n == attachment.name && *m == attachment.mime);
                if *local || (same && echoed.is_some()) {
                    match echoed {
                        Some(at) => {
                            self.echo_pending.remove(at);
                        }
                        None => attachments.push(attachment),
                    }
                    if let Some(id) = &message_id {
                        item.key = id.clone();
                    }
                    self.touch(ix);
                    return;
                }
                if same {
                    attachments.push(attachment);
                    self.touch(ix);
                    return;
                }
            }
        }
        self.echo_pending.clear();
        let key = message_id.unwrap_or_else(|| format!("msg-{}", self.counter + 1));
        self.push(
            key,
            ItemKind::User {
                text: String::new(),
                attachments: vec![attachment],
                local: false,
            },
        );
    }

    fn apply_chunk(&mut self, kind: &str, message_id: Option<String>, text: String) {
        if kind == "agent_thought_chunk" {
            return self.apply_thought(message_id, text);
        }
        if let Some(ix) = self.items.len().checked_sub(1) {
            let same = message_id
                .as_ref()
                .is_none_or(|id| &self.items[ix].key == id);
            let item = &mut self.items[ix];
            let merged = match (kind, &mut item.kind) {
                ("user_message_chunk", ItemKind::User { text: t, local, .. }) if *local => {
                    // The agent echoes the prompt we added locally: adopt its id and text.
                    *local = false;
                    *t = text.clone();
                    if let Some(id) = &message_id {
                        item.key = id.clone();
                    }
                    true
                }
                ("user_message_chunk", ItemKind::User { text: t, .. })
                | ("agent_message_chunk", ItemKind::Agent { text: t })
                    if same =>
                {
                    t.push_str(&text);
                    true
                }
                _ => false,
            };
            if merged {
                self.touch(ix);
                return;
            }
        }
        if text.is_empty() {
            return;
        }
        let key = message_id.unwrap_or_else(|| format!("msg-{}", self.counter + 1));
        let kind = match kind {
            "user_message_chunk" => ItemKind::User {
                text,
                attachments: Vec::new(),
                local: false,
            },
            _ => ItemKind::Agent { text },
        };
        self.push(key, kind);
    }

    fn apply_thought(&mut self, message_id: Option<String>, text: String) {
        if let Some(ix) = self.items.len().checked_sub(1)
            && let ItemKind::Work(entries) = &mut self.items[ix].kind
        {
            if let Some(WorkEntry::Thought { id, text: t }) = entries.last_mut()
                && message_id.as_ref().is_none_or(|m| m == id)
            {
                t.push_str(&text);
                self.touch(ix);
                return;
            }
            if text.is_empty() {
                return;
            }
            let id = message_id.unwrap_or_else(|| format!("thought-{}", self.counter + 1));
            entries.push(WorkEntry::Thought { id, text });
            self.touch(ix);
            return;
        }
        if text.is_empty() {
            return;
        }
        let id = message_id.unwrap_or_else(|| format!("thought-{}", self.counter + 1));
        self.push(
            format!("work-{id}"),
            ItemKind::Work(vec![WorkEntry::Thought { id, text }]),
        );
    }

    fn apply_tool(&mut self, update: &Value, created: bool) {
        let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let ix = match self.tool_index.get(id).copied() {
            Some(ix) => ix,
            None if created || !self.tool_index.contains_key(id) => {
                let call = ToolCall {
                    id: id.into(),
                    title: String::new(),
                    kind: String::new(),
                    status: String::new(),
                    input: None,
                    output: String::new(),
                    diffs: vec![],
                    locations: vec![],
                    stats: (0, 0),
                    synthesized: false,
                };
                let ix = match self.items.last_mut() {
                    Some(Item {
                        kind: ItemKind::Work(entries),
                        ..
                    }) => {
                        entries.push(WorkEntry::Tool(Box::new(call)));
                        self.items.len() - 1
                    }
                    _ => {
                        self.push(
                            format!("work-{id}"),
                            ItemKind::Work(vec![WorkEntry::Tool(Box::new(call))]),
                        );
                        self.items.len() - 1
                    }
                };
                self.tool_index.insert(id.into(), ix);
                ix
            }
            None => return,
        };
        let ItemKind::Work(entries) = &mut self.items[ix].kind else {
            return;
        };
        let Some(call) = entries.iter_mut().find_map(|e| match e {
            WorkEntry::Tool(c) if c.id == id => Some(c),
            _ => None,
        }) else {
            return;
        };
        let s = |k: &str| update.get(k).and_then(Value::as_str).map(str::to_owned);
        if let Some(v) = s("title") {
            call.title = v;
        }
        if let Some(v) = s("kind") {
            call.kind = v;
        }
        if let Some(v) = s("status") {
            call.status = v;
        }
        if let Some(v) = update.get("rawInput").filter(|v| !v.is_null()) {
            call.input = Some(v.clone());
        }
        if let Some(locs) = update.get("locations").and_then(Value::as_array) {
            call.locations = locs
                .iter()
                .filter_map(|l| l.get("path").and_then(Value::as_str).map(str::to_owned))
                .collect();
        }
        if let Some(content) = update.get("content").and_then(Value::as_array) {
            let mut output = String::new();
            let mut diffs = Vec::new();
            for c in content {
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
            call.output = output;
            if !diffs.is_empty() {
                call.synthesized = false;
                call.stats = diffs
                    .iter()
                    .map(diff_stats)
                    .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                call.diffs = diffs;
            }
        } else if call.output.is_empty()
            && let Some(raw) = update.get("rawOutput").filter(|v| !v.is_null())
        {
            call.output = raw
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| raw.to_string());
        }
        // Edit tools report a receipt, not a diff: rebuild one from the arguments.
        if call.diffs.is_empty() || call.synthesized {
            let diffs = synthesized_diffs(call);
            if !diffs.is_empty() || call.synthesized {
                call.synthesized = !diffs.is_empty();
                call.stats = diffs
                    .iter()
                    .map(diff_stats)
                    .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                call.diffs = diffs;
            }
        }
        self.touch(ix);
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn multiedit_receipt_anchors_line_numbers() {
        let call = ToolCall {
            id: "t".into(),
            title: "Run multiedit".into(),
            kind: "edit".into(),
            status: "completed".into(),
            input: Some(json!({
                "file_path": "/x/a.rs",
                "edits": [
                    { "old_string": "a\nb\n", "new_string": "a\nc\nd\n" },
                    { "old_string": "z", "new_string": "" }
                ]
            })),
            output: "OK paths=1 edits=2 added=2 deleted=1\nP /x/a.rs\nH replace old=10,2 new=10,3 +2 -1\nH replace old=40,1 new=41,0 +0 -1".into(),
            diffs: vec![],
            locations: vec![],
            stats: (0, 0),
            synthesized: false,
        };
        let diffs = synthesized_diffs(&call);
        assert_eq!(diffs.len(), 2);
        assert_eq!(
            (diffs[0].old_start, diffs[0].new_start),
            (Some(10), Some(10))
        );
        assert_eq!(
            (diffs[1].old_start, diffs[1].new_start),
            (Some(40), Some(41))
        );
        assert!(diffs.iter().all(|d| d.snippet));
    }

    #[test]
    fn failed_or_running_edits_have_no_diff() {
        let mut call = ToolCall {
            id: "t".into(),
            title: "Run edit".into(),
            kind: "edit".into(),
            status: "completed".into(),
            input: Some(json!({ "file_path": "a", "old_string": "x", "new_string": "y" })),
            output: "old_string not found in content".into(),
            diffs: vec![],
            locations: vec![],
            stats: (0, 0),
            synthesized: false,
        };
        assert!(synthesized_diffs(&call).is_empty());
        call.output = String::new();
        call.status = "in_progress".into();
        assert!(synthesized_diffs(&call).is_empty());
        call.status = "completed".into();
        assert_eq!(synthesized_diffs(&call).len(), 1);
    }
}

#[cfg(test)]
mod history_tests {
    use super::{ItemKind, SessionView, WorkEntry};
    use serde_json::json;

    fn ev(kind: &str, data: serde_json::Value) -> serde_json::Value {
        json!({ "kind": { "type": kind, "data": data } })
    }

    #[test]
    fn echoed_images_are_not_duplicated_after_text_echo() {
        let mut view = SessionView::new("a", "s", "/", "t");
        let img = |n: &str| super::UserAttachment {
            name: n.into(),
            mime: "image/png".into(),
            size: None,
            data: None,
        };
        view.push_local_user("hi", vec![img("a.png"), img("b.png")]);
        // Text echo first (clears `local`), then the image blocks.
        view.apply(&json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1", "content": { "type": "text", "text": "hi" } }));
        for n in ["a.png", "b.png"] {
            view.apply(&json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
                "content": { "type": "image", "mimeType": "image/png", "data": "x", "_meta": { "querymt": { "filename": n } } } }));
        }
        let users: Vec<_> = view
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::User { attachments, .. } => Some(attachments.len()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec![2]);
    }

    #[test]
    fn older_page_is_prepended_in_order() {
        let mut view = SessionView::new("a", "s", "/", "t");
        view.apply(&json!({ "sessionUpdate": "user_message_chunk", "messageId": "u2", "content": { "type": "text", "text": "second" } }));
        view.apply(&json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a2", "content": { "type": "text", "text": "reply 2" } }));

        view.prepend_events(&[
            ev("prompt_received", json!({ "content": "first", "message_id": "u1" })),
            ev("assistant_message_stored", json!({ "content": "", "message_id": "m1", "reasoning_parts": [{ "id": "p", "text": "thinking" }] })),
            ev("tool_call_start", json!({ "tool_call_id": "t1", "tool_name": "shell", "arguments": "{\"command\":\"ls\"}" })),
            ev("tool_call_end", json!({ "tool_call_id": "t1", "tool_name": "shell", "is_error": false, "result": "ok" })),
            ev("assistant_message_stored", json!({ "content": "reply 1", "message_id": "a1" })),
            // Overlap with the loaded window is dropped.
            ev("prompt_received", json!({ "content": "second", "message_id": "u2" })),
        ], &[]);

        let keys: Vec<&str> = view.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys.first(), Some(&"u1"));
        assert_eq!(keys.iter().filter(|k| **k == "u2").count(), 1);
        assert_eq!(&keys[keys.len() - 2..], &["u2", "a2"]);
        let tool = view.items.iter().find_map(|i| match &i.kind {
            ItemKind::Work(e) => e.iter().find_map(|e| match e {
                WorkEntry::Tool(c) => Some(c.clone()),
                _ => None,
            }),
            _ => None,
        });
        let tool = tool.expect("tool call");
        assert_eq!(
            (
                tool.status.as_str(),
                tool.output.as_str(),
                tool.kind.as_str()
            ),
            ("completed", "ok", "execute")
        );

        // Live updates for an older tool still find it after the prepend.
        view.apply(
            &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed" }),
        );
        assert!(view.items.iter().any(|i| matches!(&i.kind, ItemKind::Work(e) if e.iter().any(|e| matches!(e, WorkEntry::Tool(c) if c.status == "failed")))));
    }
}

#[cfg(test)]
mod input_tests {
    use super::{PendingInput, SessionView};
    use serde_json::json;

    fn pending(id: &str) -> PendingInput {
        PendingInput {
            input_id: id.into(),
            delivery: "steer".into(),
            state: "sending".into(),
            text: "hi".into(),
            ..Default::default()
        }
    }

    #[test]
    fn runtime_sets_run_id_and_idle_clears_waiting_inputs() {
        let mut view = SessionView::new("a", "s", "/", "t");
        view.apply_runtime(&json!({ "phase": "tools", "active_run_id": "r1" }));
        assert_eq!(view.run_id.as_deref(), Some("r1"));
        view.add_pending_input(pending("c1"));
        view.set_input_state("c1", "steer", "accepted");
        view.apply_runtime(&json!({ "phase": "idle" }));
        assert!(view.run_id.is_none());
        assert!(view.pending_inputs.is_empty());
    }

    #[test]
    fn input_lifecycle_rekeys_and_drops_terminal_states() {
        let mut view = SessionView::new("a", "s", "/", "t");
        view.add_pending_input(pending("c1"));
        view.rekey_input("c1", "i1");
        view.set_input_state("i1", "queue", "queued");
        assert_eq!(view.pending_inputs[0].delivery, "queue");
        assert_eq!(view.pending_inputs[0].state, "queued");
        view.set_input_state("i1", "queue", "started");
        assert!(view.pending_inputs.is_empty());
    }
}

#[cfg(test)]
mod undo_tests {
    use super::{SessionView, UndoFrame};
    use serde_json::json;

    fn view_with_turns(ids: &[&str]) -> SessionView {
        let mut view = SessionView::new("a", "s", "/w", "t");
        for id in ids {
            view.apply(
                &json!({ "sessionUpdate": "user_message_chunk", "messageId": id,
                "content": { "type": "text", "text": "prompt" } }),
            );
            view.apply(
                &json!({ "sessionUpdate": "agent_message_chunk", "messageId": format!("r-{id}"),
                "content": { "type": "text", "text": "reply" } }),
            );
        }
        view
    }

    #[test]
    fn undo_targets_the_latest_turn_before_the_undo_frontier() {
        let mut view = view_with_turns(&["u1", "u2", "u3"]);
        assert_eq!(view.undo_target().as_deref(), Some("u3"));
        assert_eq!(view.undo_affected_turns("u1"), Some(3));

        view.undo_stack = vec![UndoFrame {
            message_id: "u3".into(),
            restore_failed: false,
        }];
        assert_eq!(view.undo_target().as_deref(), Some("u2"));

        // A failed undo deleted nothing: its turn can be undone again.
        view.undo_stack = vec![UndoFrame {
            message_id: "u2".into(),
            restore_failed: true,
        }];
        assert_eq!(view.undo_target().as_deref(), Some("u3"));

        view.undo_stack = vec![UndoFrame {
            message_id: "u1".into(),
            restore_failed: false,
        }];
        assert_eq!(view.undo_target(), None);
    }

    #[test]
    fn undo_stack_is_parsed_from_responses() {
        let frames = UndoFrame::parse_stack(&json!({ "undo_stack": [
            { "message_id": "u1", "restore_failed": false },
            { "message_id": "u2", "restore_failed": true },
            { "restore_failed": false },
        ] }));
        assert_eq!(frames.len(), 2);
        assert!(frames[1].restore_failed);
    }
}

#[cfg(test)]
mod compaction_tests {
    use super::{
        COMPACTING_LABEL, CompactionStatus, ItemKind, SessionView, format_tokens,
        is_compact_command,
    };
    use serde_json::{Value, json};

    fn compaction(view: &SessionView, id: &str) -> super::Compaction {
        view.items
            .iter()
            .find_map(|i| match &i.kind {
                ItemKind::Compaction(c) if c.id == id => Some((**c).clone()),
                _ => None,
            })
            .expect("compaction item")
    }

    fn ev(kind: &str, data: Value) -> Value {
        json!({ "kind": { "type": kind, "data": data } })
    }

    #[test]
    fn live_compaction_updates_stream_summary_and_settle_usage() {
        let mut view = SessionView::new("a", "s", "/w", "t");
        view.apply(&json!({ "sessionUpdate": "usage_update", "used": 120000, "size": 200000 }));
        view.apply(&json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress" }));
        view.apply(
            &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
            "content": { "type": "text", "text": "Summary " } }),
        );
        view.apply(
            &json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c1",
            "content": { "type": "text", "text": "text" } }),
        );
        let c = compaction(&view, "c1");
        assert_eq!(c.status, CompactionStatus::InProgress);
        assert_eq!(c.summary, "Summary text");
        assert_eq!(c.title(), COMPACTING_LABEL);
        assert_eq!(c.detail().as_deref(), Some("120k tokens in context"));
        assert!(view.active_compaction().is_some());

        view.apply(&json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "completed" }));
        view.apply(&json!({ "sessionUpdate": "usage_update", "used": 18000, "size": 200000 }));
        let c = compaction(&view, "c1");
        assert_eq!(c.status, CompactionStatus::Completed);
        assert_eq!(c.detail().as_deref(), Some("120k → 18k tokens"));
        assert!(view.active_compaction().is_none());

        // A settled compaction is never reopened.
        view.apply(&json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress" }));
        assert_eq!(compaction(&view, "c1").status, CompactionStatus::Completed);
    }

    #[test]
    fn failed_compaction_shows_reason() {
        let mut view = SessionView::new("a", "s", "/w", "t");
        view.apply(
            &json!({ "sessionUpdate": "compaction_update", "compactionId": "c1",
            "status": "failed", "error": "model refused" }),
        );
        let c = compaction(&view, "c1");
        assert_eq!(c.title(), "Compaction failed");
        assert_eq!(c.detail().as_deref(), Some("model refused"));
    }

    #[test]
    fn load_snapshot_places_compactions_after_their_turn() {
        let mut view = SessionView::new("a", "s", "/w", "t");
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": { "type": "text", "text": "hi" } }),
        );
        view.apply(
            &json!({ "sessionUpdate": "agent_message_chunk", "messageId": "a1",
            "content": { "type": "text", "text": "reply" } }),
        );
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u2",
            "content": { "type": "text", "text": "next" } }),
        );
        let snap = json!({ "audit": { "events": [
            ev("prompt_received", json!({ "message_id": "u1", "content": "hi" })),
            ev("llm_request_end", json!({ "context_tokens": 90000 })),
            ev("assistant_message_stored", json!({ "message_id": "a1", "content": "reply" })),
            // Legacy record without an id: start and end pair up.
            ev("compaction_start", json!({})),
            ev("compaction_summary_chunk", json!({ "content": "short" })),
            ev("compaction_end", json!({ "summary": "Final summary", "context_tokens": 9000 })),
            ev("compaction_start", json!({ "compaction_id": "c2" })),
            ev("prompt_received", json!({ "message_id": "u2", "content": "next" })),
        ] } });
        view.apply_load_snapshot(&snap);
        let kinds: Vec<&str> = view
            .items
            .iter()
            .map(|i| match &i.kind {
                ItemKind::Compaction(_) => "compaction",
                ItemKind::User { .. } => "user",
                ItemKind::Agent { .. } => "agent",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["user", "agent", "compaction", "compaction", "user"]);
        let legacy = match &view.items[2].kind {
            ItemKind::Compaction(c) => (**c).clone(),
            _ => unreachable!(),
        };
        assert_eq!(legacy.summary, "Final summary");
        assert_eq!(legacy.detail().as_deref(), Some("90k → 9k tokens"));
        // Still open after the run ended: interrupted.
        let open = compaction(&view, "c2");
        assert_eq!(open.status, CompactionStatus::Failed);
        assert!(open.error.unwrap().contains("interrupted"));
    }

    #[test]
    fn compact_command_and_token_format() {
        assert!(is_compact_command("/compact"));
        assert!(is_compact_command("  /COMPACT keep the plan"));
        assert!(!is_compact_command("/compactly"));
        assert!(!is_compact_command("compact"));
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12500), "12.5k");
        assert_eq!(format_tokens(18000), "18k");
        assert_eq!(format_tokens(120400), "120k");
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::{ItemKind, SessionView, WorkEntry};
    use serde_json::{Value, json};

    fn ev(seq: i64, ts: i64, kind: &str, data: Value) -> Value {
        json!({ "seq": seq, "timestamp": ts, "kind": { "type": kind, "data": data } })
    }

    #[test]
    fn transcript_is_built_from_the_load_snapshot() {
        let mut view = SessionView::new("a", "s", "/w", "t");
        view.phase = "idle".into();
        let snap = json!({
            "audit": { "events": [
                ev(1, 100, "prompt_received", json!({ "message_id": "u1", "content": "hi" })),
                ev(2, 101, "assistant_thinking_delta", json!({ "message_id": "a1", "content": "hmm" })),
                ev(3, 102, "tool_call_start", json!({ "tool_call_id": "t1", "tool_name": "shell", "arguments": "{\"command\":\"ls\"}" })),
                ev(4, 103, "tool_call_end", json!({ "tool_call_id": "t1", "tool_name": "shell", "is_error": false, "result": "ok" })),
                ev(5, 104, "tool_call_start", json!({ "tool_call_id": "td", "tool_name": "todowrite",
                    "arguments": "{\"todos\":[{\"content\":\"a\",\"status\":\"pending\"},{\"content\":\"b\",\"status\":\"cancelled\"}]}" })),
                ev(6, 105, "assistant_content_delta", json!({ "message_id": "a1", "content": "partial" })),
                ev(7, 106, "assistant_message_stored", json!({ "message_id": "a1", "content": "final reply",
                    "reasoning_parts": [{ "id": "p", "text": "hmm" }] })),
                ev(8, 200, "tool_call_start", json!({ "tool_call_id": "t2", "tool_name": "read_tool", "arguments": "{}" })),
                ev(9, 300, "assistant_message_stored", json!({ "message_id": "a2", "content": "later" })),
            ] },
            // u1 has an image block; s1 is a steering prompt with no event.
            "userPrompts": [
                { "message_id": "u1", "message_order": 0, "timestamp": 100, "blocks": [
                    { "type": "text", "text": "hi" },
                    { "type": "image", "mimeType": "image/png", "data": "AAA" } ] },
                { "message_id": "s1", "message_order": 3, "timestamp": 250, "blocks": [
                    { "type": "text", "text": "steer" } ] },
            ],
        });
        view.apply_load_snapshot(&snap);

        let shape: Vec<String> = view
            .items
            .iter()
            .map(|i| match &i.kind {
                ItemKind::User {
                    text, attachments, ..
                } => format!("user:{text}:{}", attachments.len()),
                ItemKind::Agent { text } => format!("agent:{text}"),
                ItemKind::Work(e) => format!("work:{}", e.len()),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "user:hi:1",
                "work:2",
                "agent:final reply",
                "work:1",
                "user:steer:0",
                "agent:later"
            ]
        );
        // Reasoning is not duplicated by the stored parts.
        let thoughts = view
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Work(e) => Some(
                    e.iter()
                        .filter(|e| matches!(e, WorkEntry::Thought { .. }))
                        .count(),
                ),
                _ => None,
            })
            .sum::<usize>();
        assert_eq!(thoughts, 1);
        // The todo plan is restored without cancelled entries.
        assert_eq!(view.plan.len(), 1);
        // The run is over: the tool without a result failed.
        let unfinished = view.items.iter().find_map(|i| match &i.kind {
            ItemKind::Work(e) => e.iter().find_map(|e| match e {
                WorkEntry::Tool(c) if c.id == "t2" => Some(c.status.clone()),
                _ => None,
            }),
            _ => None,
        });
        assert_eq!(unfinished.as_deref(), Some("failed"));
    }

    #[test]
    fn replayed_transcript_is_not_duplicated() {
        let mut view = SessionView::new("a", "s", "/w", "t");
        view.apply(
            &json!({ "sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": { "type": "text", "text": "hi" } }),
        );
        view.apply_load_snapshot(&json!({ "audit": { "events": [
            ev(1, 100, "prompt_received", json!({ "message_id": "u1", "content": "hi" })),
        ] } }));
        assert_eq!(view.items.len(), 1);
    }
}
