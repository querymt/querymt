//! Session transcript model: folds ACP `session/update` notifications (both the
//! replay sent during `session/load` and live updates) into display items.
//!
//! Pure data, no UI. Every mutated item gets a fresh `rev` so the view can
//! re-measure only what changed.

use std::collections::HashMap;

use serde_json::Value;

use crate::usage::{Accounting, Breakdown};

#[cfg(test)]
mod golden;
mod overlay;
mod projection;
mod view;
#[cfg(test)]
mod view_tests;

use projection::Projection;
pub use view::next_client_id;

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

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub input: Option<Value>,
    /// The result text; written through [`ToolCall::set_output`] so the
    /// facts derived from it are computed once, never per frame.
    output: String,
    facts: OutputFacts,
    /// A windowed load shortened the result: its full size in bytes.
    /// `session/toolContent` returns the complete text.
    pub truncated_result: Option<u64>,
    /// A windowed load shortened string arguments: their full size in bytes.
    pub truncated_args: Option<u64>,
    /// Fetching the complete result or arguments (`session/toolContent`).
    pub fetch: Option<ContentFetch>,
    pub diffs: Vec<Diff>,
    pub locations: Vec<String>,
    /// Lines (added, removed) across `diffs`; computed once when the diffs arrive.
    pub stats: (usize, usize),
    /// `diffs` were derived from the arguments rather than sent by the agent.
    pub synthesized: bool,
    /// Session a delegation spawned, once the agent reports it.
    pub child_session: Option<String>,
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

/// Progress of fetching a shortened tool call's complete content.
#[derive(Clone, Debug, PartialEq)]
pub enum ContentFetch {
    /// The part being fetched: `result` or `arguments`.
    Loading(&'static str),
    Failed(String),
}

/// What rendering needs from a tool result, derived once per change: shell
/// results are JSON (`stdout`, `stderr`, `exit_code`).
#[derive(Clone, Debug, Default)]
struct OutputFacts {
    /// stdout + stderr (+ exit code) of a shell JSON result; `None`: the
    /// output shows as it is.
    pretty: Option<String>,
    exit: Option<i64>,
}

impl OutputFacts {
    fn of(output: &str) -> Self {
        let trimmed = output.trim();
        if !trimmed.starts_with('{') {
            return Self::default();
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            return Self::default();
        };
        let exit = v.get("exit_code").and_then(Value::as_i64);
        let out = v.get("stdout").and_then(Value::as_str);
        let err = v.get("stderr").and_then(Value::as_str);
        let pretty = (out.is_some() || err.is_some()).then(|| {
            let mut text = out.unwrap_or_default().trim_end().to_string();
            if let Some(err) = err.filter(|e| !e.trim().is_empty()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(err.trim_end());
            }
            if let Some(code) = exit.filter(|c| *c != 0) {
                text.push_str(&format!("\n[exit code {code}]"));
            }
            text
        });
        Self { pretty, exit }
    }
}

impl ToolCall {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn set_output(&mut self, output: String) {
        self.facts = OutputFacts::of(&output);
        self.output = output;
    }

    #[cfg(test)]
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.set_output(output.into());
        self
    }

    pub fn running(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "in_progress" | "")
    }

    /// An edit tool, whose diff is built from its arguments.
    pub fn is_edit(&self) -> bool {
        EDIT_TOOLS.contains(&self.name().to_lowercase().as_str())
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
        self.facts.exit
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
    pub fn pretty_output(&self) -> &str {
        self.facts.pretty.as_deref().unwrap_or(&self.output)
    }
}

/// One entry of a work group: a reasoning step or a tool call.
#[derive(Clone, Debug)]
pub enum WorkEntry {
    Thought(Thought),
    /// Shared, so copying a work group (every transcript snapshot does)
    /// copies pointers; [`Arc::make_mut`] copies one call when it changes.
    Tool(std::sync::Arc<ToolCall>),
}

impl WorkEntry {
    pub fn id(&self) -> &str {
        match self {
            WorkEntry::Thought(t) => &t.id,
            WorkEntry::Tool(call) => &call.id,
        }
    }
}

/// One reasoning part of an assistant message. The agent tags each provider
/// summary with its own part id (`<item>:summary:<n>`); chunks of the same
/// part append, a new part starts a new thought (desktop `canMergeStreamChunk`).
#[derive(Clone, Debug)]
pub struct Thought {
    /// Unique key: the message id, plus the part id when there is one.
    pub id: String,
    pub part: Option<String>,
    pub text: String,
}

impl Thought {
    fn new(message: String, part: Option<String>, text: String) -> Self {
        let id = match &part {
            Some(part) => format!("{message}/{part}"),
            None => message.clone(),
        };
        Self { id, part, text }
    }

    /// A provider reasoning summary rather than raw reasoning (desktop
    /// `isReasoningSummaryPart`: a `summary` segment in the part id).
    pub fn is_summary(&self) -> bool {
        self.part.as_deref().is_some_and(|part| {
            part.split([':', '/'])
                .any(|seg| seg.eq_ignore_ascii_case("summary"))
        })
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


/// The open session as the UI reads it: a projection of the session core
/// (`Projection`: the reducer, the rows and their caches) plus the header
/// fields mapped from the core's header and the client-only state.
///
/// All conversation state is written through [`SessionView::feed`] and the
/// prompt and input methods in `view.rs`; the public fields below are either
/// copied from the core's header after a change or plain client state the UI
/// and the store set directly. `items` and the turn tables are reached
/// through `Deref` to the projection.
#[derive(Default)]
pub struct SessionView {
    pub agent_id: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    /// Session this one was opened from through a delegation pill; Back returns there.
    pub parent: Option<Box<ParentSession>>,
    pub loading: bool,
    pub error: Option<String>,
    /// Copyable diagnostic report behind `error` (request, raw agent error).
    pub error_details: Option<String>,
    /// Runtime phase (`idle`, `starting`, `model`, `tools`, `waiting`).
    pub phase: String,
    /// A prompt sent from this client is in flight.
    pub prompting: bool,
    pub plan: Vec<PlanEntry>,
    /// Context window (used, size) in tokens.
    pub usage: Option<(u64, u64)>,
    /// Latest-request / session-total token breakdown.
    pub breakdown: Option<Breakdown>,
    /// API-spend accounting.
    pub accounting: Option<Accounting>,
    /// Time of the ended runs, from the core's `active_time`.
    pub active_work_ms: f64,
    /// Host epoch ms the active run started, while one runs (the core's
    /// `active_time.active_since_ms`).
    pub active_started_at: Option<f64>,
    pub mode: Option<String>,
    pub model: Option<String>,
    /// Mesh node serving `model`, from the model option's
    /// `_meta.querymt.modelEntry.node_id`; `None`: the agent's own.
    pub model_node: Option<String>,
    /// Session config options, kept current through the core.
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
    /// `active_run_id` of the running turn; steering must name it as
    /// `expected_run_id`.
    pub run_id: Option<String>,
    /// Messages sent mid-turn that the agent hasn't applied or started yet.
    pub pending_inputs: Vec<PendingInput>,
    /// Workspace undo frames (`querymt/session/undoStack`), oldest first.
    pub undo_stack: Vec<UndoFrame>,
    /// A workspace undo or redo is in flight.
    pub undo_pending: bool,
    /// Notice to append once the transcript has (re)loaded, e.g. after an undo.
    pub after_load_notice: Option<String>,
    /// The agent serves this session read-only (`_meta.querymt.sessionAccess`).
    pub access: Option<SessionAccess>,
    /// Inputs waiting for the next flush (a burst of chunks is one batch), or
    /// for a background load to finish; always applied in arrival order.
    queue: Vec<querymt_agent_session_core::input::SessionInput>,
    /// A large load is being decoded off the UI thread; inputs queue behind it.
    hydrating: bool,
    /// A flush of the queue is already scheduled.
    flush_scheduled: bool,
    proj: Projection,
}

impl std::ops::Deref for SessionView {
    type Target = Projection;
    fn deref(&self) -> &Projection {
        &self.proj
    }
}

impl std::ops::DerefMut for SessionView {
    fn deref_mut(&mut self) -> &mut Projection {
        &mut self.proj
    }
}

/// Where Back goes from a delegated child session.
#[derive(Clone, Debug, Default)]
pub struct ParentSession {
    pub agent_id: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    /// The parent's own parent, so Back walks up a delegation chain.
    pub parent: Option<Box<ParentSession>>,
}

/// One file a turn changed (desktop `SessionTurnDiffFile`).
#[derive(Clone, Debug, Default)]
pub struct TurnDiffFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// One per tool call that edited the file, in order.
    pub changes: Vec<TurnDiffChange>,
}

/// One tool call's edit of a file: its hunks.
#[derive(Clone, Debug, Default)]
pub struct TurnDiffChange {
    /// Tool call id.
    pub id: String,
    pub hunks: Vec<Diff>,
}

/// Why a session can only be read (desktop `SessionAccessInfo`): its
/// worktree was handed off or archived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionAccess {
    pub reason: Option<String>,
    pub owner_session_id: Option<String>,
}

impl SessionAccess {
    /// `_meta.querymt.sessionAccess` of a `session/load` response; only an
    /// explicit read-only grant counts.
    pub fn parse(response: &Value) -> Option<Self> {
        let access = response.pointer("/_meta/querymt/sessionAccess")?;
        (access.get("mode").and_then(Value::as_str) == Some("read_only")).then(|| {
            let s = |k: &str| {
                access
                    .get(k)
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_owned)
            };
            Self {
                reason: s("reason"),
                owner_session_id: s("ownerSessionId"),
            }
        })
    }

    /// Notice shown in place of the composer (desktop `describeSessionAccess`).
    pub fn describe(&self) -> &'static str {
        match self.reason.as_deref() {
            Some("workspace_archived") => {
                "This worktree was archived, so this conversation is read-only. To delete the \
                 checkout, run /worktree remove from another session."
            }
            Some("owner_transferred") => {
                "This worktree was handed off, so this conversation is read-only."
            }
            _ if self.owner_session_id.is_some() => {
                "This worktree was handed off, so this conversation is read-only."
            }
            _ => "This conversation is read-only.",
        }
    }
}

/// Where a fork from one turn branches, and what it keeps (desktop
/// `SessionForkTarget`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkTarget {
    pub message_id: String,
    pub prompt: String,
    pub response: String,
    /// The fork point is the reply itself, so the fork keeps it.
    pub includes_response: bool,
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


#[cfg(test)]
mod diff_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn multiedit_receipt_anchors_line_numbers() {
        let call = ToolCall {
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
            ..ToolCall::new("t")
        }
        .with_output("OK paths=1 edits=2 added=2 deleted=1\nP /x/a.rs\nH replace old=10,2 new=10,3 +2 -1\nH replace old=40,1 new=41,0 +0 -1");
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
            title: "Run edit".into(),
            kind: "edit".into(),
            status: "completed".into(),
            input: Some(json!({ "file_path": "a", "old_string": "x", "new_string": "y" })),
            ..ToolCall::new("t")
        }
        .with_output("old_string not found in content");
        assert!(synthesized_diffs(&call).is_empty());
        call.set_output(String::new());
        call.status = "in_progress".into();
        assert!(synthesized_diffs(&call).is_empty());
        call.status = "completed".into();
        assert_eq!(synthesized_diffs(&call).len(), 1);
    }
}

#[cfg(test)]
pub(crate) mod undo_tests {
    use super::UndoFrame;
    use serde_json::json;

    /// Edit tool call `id` changing `path` from `old` to `new`.
    pub(crate) fn edit(id: &str, path: &str, old: &str, new: &str) -> serde_json::Value {
        json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "edit",
            "kind": "edit", "status": "completed",
            "content": [{ "type": "diff", "path": path, "oldText": old, "newText": new }] })
    }

    #[test]
    fn read_only_access_is_parsed_from_a_load() {
        use super::SessionAccess;
        let load = |access: serde_json::Value| {
            SessionAccess::parse(&json!({ "_meta": { "querymt": { "sessionAccess": access } } }))
        };
        let handed = load(
            json!({ "mode": "read_only", "reason": "owner_transferred", "ownerSessionId": "h1" }),
        )
        .unwrap();
        assert_eq!(handed.owner_session_id.as_deref(), Some("h1"));
        assert!(handed.describe().contains("handed off"));
        let archived =
            load(json!({ "mode": "read_only", "reason": "workspace_archived" })).unwrap();
        assert!(archived.describe().contains("archived"));
        // Only an explicit read-only grant counts.
        assert_eq!(load(json!({ "mode": "read_write" })), None);
        assert_eq!(SessionAccess::parse(&json!({})), None);
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
mod format_tests {
    use super::{format_tokens, is_compact_command};

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
