//! `SessionView` behavior: feeding the session core, prompts and inputs,
//! client-only state and the turn tables (undo, fork, diff, copy) that work
//! on the projection's turn slices (design D1, D4, D7).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use querymt_agent_session_core::SessionCore;
use querymt_agent_session_core::entry::{EntryKind, PromptStatus};
use querymt_agent_session_core::input::SessionInput;
use querymt_agent_session_core::view::ChangeReport;
use serde_json::{Value, json};

use super::projection::Projection;
use super::{
    Compaction, CompactionStatus, ContentFetch, ForkTarget, ItemKind, PendingInput, SelectOption,
    SessionAccess, SessionView, ToolCall, TurnDiffChange, TurnDiffFile, WorkEntry, diff_stats,
};

/// An input of the session core from its adjacently tagged JSON form, so the
/// store hands over the values it already has.
fn input(kind: &str, data: Value) -> Option<SessionInput> {
    match serde_json::from_value(json!({ "type": kind, "data": data })) {
        Ok(input) => Some(input),
        Err(err) => {
            tracing::warn!(%err, kind, "session input did not decode");
            None
        }
    }
}

/// A fresh id for a prompt or steer/queue input this client sends:
/// `gpui-<ms>-<n>`, so two sent in the same millisecond differ.
pub fn next_client_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "gpui-{}-{}",
        crate::util::now_ms() as u64,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The parts of a load response the view handles itself.
struct LoadParts {
    options: Option<Value>,
}

/// A `session/load` response whose snapshot is decoded off the UI thread.
///
/// The job owns the view's core while it decodes (design D9): what the core
/// already holds, such as the slash commands the agent advertised before the
/// response, goes through the load exactly as it does for an inline `load`.
pub struct LoadJob {
    core: SessionCore,
    response: Value,
    parts: LoadParts,
}

impl LoadJob {
    /// Apply the snapshot to the core the job carries. Heavy; run it on a
    /// background executor.
    pub fn hydrate(self) -> Hydrated {
        let mut core = self.core;
        core.apply(&[SessionInput::SnapshotLoaded {
            response: self.response,
        }]);
        Hydrated {
            core,
            parts: self.parts,
        }
    }
}

/// A decoded load, ready for `SessionView::finish_load`.
pub struct Hydrated {
    core: SessionCore,
    parts: LoadParts,
}

/// A turn the agent can undo to: its first confirmed prompt, every confirmed
/// prompt it holds, and its rows.
struct UndoTurn {
    id: String,
    ids: Vec<String>,
    rows: Range<usize>,
}

impl SessionView {
    pub fn new(agent_id: &str, session_id: &str, cwd: &str, title: &str) -> Self {
        Self {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            cwd: cwd.into(),
            title: title.into(),
            proj: Projection::new(session_id),
            ..Default::default()
        }
    }

    /// One-line summary (command or path) of a raw ACP tool call payload, for
    /// permission prompts: the transcript's own tool parsing, on a scratch
    /// session.
    pub fn tool_call_summary(tool: &Value) -> String {
        let mut view = Self::new("", "scratch", "", "");
        let mut update = tool.clone();
        if let Some(obj) = update.as_object_mut() {
            obj.insert("sessionUpdate".into(), Value::from("tool_call"));
        }
        view.session_update(&json!({ "sessionId": "scratch", "update": update }));
        match view.items.first().map(|i| &i.kind) {
            Some(ItemKind::Work(entries)) => match entries.first() {
                Some(WorkEntry::Tool(c)) => c.summary(),
                _ => String::new(),
            },
            _ => String::new(),
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

    // ---- feeding the core ---------------------------------------------------

    /// Apply inputs to the core now, after everything queued before them,
    /// re-project what changed and refresh the header fields. The work clock
    /// follows `running()`.
    pub fn feed(&mut self, inputs: &[SessionInput]) -> ChangeReport {
        self.queue.extend(inputs.iter().cloned());
        self.flush()
    }

    /// Hold an input for the next flush: a burst of notifications becomes one
    /// batch, one change report and one repaint (design D6).
    pub fn queue_input(&mut self, input: Option<SessionInput>) {
        self.queue.extend(input);
    }

    /// Whether inputs wait to be applied.
    #[cfg(test)]
    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Claim the right to schedule a flush: `true` when none was scheduled.
    pub fn claim_flush(&mut self) -> bool {
        !std::mem::replace(&mut self.flush_scheduled, true)
    }

    /// The scheduled flush ran.
    pub fn flush_done(&mut self) {
        self.flush_scheduled = false;
    }

    /// A large load is still being decoded off the UI thread.
    #[cfg(test)]
    pub fn hydrating(&self) -> bool {
        self.hydrating
    }

    /// Apply what is queued, in order, as one batch. While a background load
    /// runs the queue stays put: its inputs follow the load.
    pub fn flush(&mut self) -> ChangeReport {
        if self.hydrating || self.queue.is_empty() {
            return ChangeReport::default();
        }
        let batch = std::mem::take(&mut self.queue);
        let report = self.apply_inputs(&batch);
        self.sync_inputs();
        report
    }

    /// `feed` without touching the work clock.
    fn apply_inputs(&mut self, inputs: &[SessionInput]) -> ChangeReport {
        let report = self.proj.feed(inputs);
        if report.header_changed {
            self.refresh_header();
        }
        report
    }

    /// Copy the core's header into the fields the UI reads.
    fn refresh_header(&mut self) {
        let f = self.proj.header_fields();
        self.phase = f.phase;
        self.run_id = f.run_id;
        self.plan = f.plan;
        self.usage = f.usage;
        self.breakdown = f.breakdown;
        self.accounting = f.accounting;
        self.active_work_ms = f.active_ms;
        self.active_started_at = f.active_since_ms;
        if let Some(title) = f.title {
            self.title = title;
        }
        if f.mode.is_some() {
            self.mode = f.mode;
        }
        if !f.options.is_empty() {
            self.select_from(&f.options);
            self.options = f.options;
        }
        self.slash_commands = f.slash_commands;
        self.history_more = f.history_more;
        self.history_before = f.history_before;
        self.undo_stack = f.undo_stack;
        self.pending_inputs = f.pending_inputs;
    }

    /// Only the merged list of pending inputs.
    fn refresh_inputs(&mut self) {
        self.pending_inputs = self.proj.header_fields().pending_inputs;
    }

    /// Settle the unconfirmed inputs to match `running()`.
    pub fn sync_inputs(&mut self) {
        let running = self.running();
        self.proj.overlay.sync_inputs(running);
        self.refresh_inputs();
    }

    /// The `session/load` response of the open session.
    pub fn load(&mut self, response: Value) {
        let parts = self.load_parts(&response);
        // Everything that arrived before the response goes first.
        self.flush();
        self.apply_inputs(&[SessionInput::SnapshotLoaded { response }]);
        self.after_load(parts);
    }

    /// What a load needs besides the core's decode: access, modes and the
    /// response's config options.
    fn load_parts(&mut self, response: &Value) -> LoadParts {
        self.access = SessionAccess::parse(response);
        self.apply_modes(response.get("modes"));
        LoadParts {
            options: response.get("configOptions").cloned(),
        }
    }

    fn after_load(&mut self, parts: LoadParts) {
        self.apply_config(parts.options.as_ref());
        self.sync_inputs();
        if let Some(notice) = self.after_load_notice.take() {
            self.push_notice(notice, false);
        }
    }

    /// Start a load whose decode runs off the UI thread (design D6): the
    /// returned job is hydrated anywhere, then handed to `finish_load`. Until
    /// then every input queues behind it, in arrival order. Only a view with
    /// no turns yet can hydrate this way (the load replaces nothing a client
    /// still needs); any other falls back to `load` and returns the response.
    ///
    /// The queued inputs are applied first and the core, with everything they
    /// put in its header, moves into the job; an empty core stands in until
    /// `finish_load` (nothing is applied to it meanwhile: the queue holds).
    pub fn begin_load(&mut self, response: Value) -> Result<LoadJob, Value> {
        if !self.proj.slices().is_empty() {
            return Err(response);
        }
        self.flush();
        let parts = self.load_parts(&response);
        self.hydrating = true;
        let core = std::mem::replace(
            &mut self.proj.core,
            SessionCore::new(self.session_id.as_str()),
        );
        Ok(LoadJob {
            core,
            response,
            parts,
        })
    }

    /// The hydrated load arrives: project it, then apply the inputs that
    /// queued up behind it in the order they came.
    pub fn finish_load(&mut self, hydrated: Hydrated) {
        self.hydrating = false;
        self.proj.replace_core(hydrated.core);
        self.refresh_header();
        self.after_load(hydrated.parts);
        self.flush();
    }

    /// One `session/update` notification's params (`sessionId`, `update`).
    pub fn session_update(&mut self, params: &Value) {
        let input = Self::update_input(params);
        self.feed(input.as_slice());
    }

    /// `session_update`, held for the next flush.
    pub fn queue_session_update(&mut self, params: &Value) {
        self.queue_input(Self::update_input(params));
    }

    fn update_input(params: &Value) -> Option<SessionInput> {
        let replayed = params
            .pointer("/_meta/querymtReplay")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let data = json!({
            "notification": params,
            "replayed": replayed,
            "at_ms": crate::util::now_ms() as i64,
        });
        input("acp_notification", data)
    }

    /// `querymt/session/stateUpdate` params.
    #[cfg(test)]
    pub fn state_update(&mut self, params: &Value) {
        let input = Self::state_input(params);
        self.feed(input.as_slice());
    }

    /// `state_update`, held for the next flush.
    pub fn queue_state_update(&mut self, params: &Value) {
        self.queue_input(Self::state_input(params));
    }

    fn state_input(params: &Value) -> Option<SessionInput> {
        let data = json!({ "notification": params, "at_ms": crate::util::now_ms() as i64 });
        input("state_update", data)
    }

    /// The `runtime` of `runtimeChanged` or of a load snapshot.
    pub fn apply_runtime(&mut self, runtime: &Value) {
        let input = input("runtime_observed", json!({ "status": runtime }));
        self.feed(input.as_slice());
    }

    /// `apply_runtime`, held for the next flush.
    pub fn queue_runtime(&mut self, runtime: &Value) {
        self.queue_input(input("runtime_observed", json!({ "status": runtime })));
    }

    /// `querymt/session/inputState` params: the core's pending inputs, held
    /// for the next flush.
    pub fn queue_input_state(&mut self, params: &Value) {
        self.queue_input(input("input_state", json!({ "notification": params })));
    }

    /// An `_querymt/session/history` page, raw.
    pub fn history_page(&mut self, page: &Value) {
        if let Some(input) = input("history_page", json!({ "response": page })) {
            self.feed(&[input]);
        }
    }

    /// The agent's undo stack (from an undo stack, undo or redo response).
    pub fn set_undo_stack(&mut self, frames: &[super::UndoFrame]) {
        let frames: Vec<Value> = frames
            .iter()
            .map(|f| json!({ "message_id": f.message_id, "restore_failed": f.restore_failed }))
            .collect();
        if let Some(input) = input("undo_stack", json!({ "frames": frames })) {
            self.feed(&[input]);
        }
    }

    // ---- prompts ------------------------------------------------------------

    /// Show a prompt at once, as pending after the last turn. `blocks` go out
    /// on `session/prompt` with `client_prompt_id` in `_meta.querymt`.
    pub fn prompt_optimistic(&mut self, client_prompt_id: &str, blocks: &[Value]) {
        let data = json!({
            "client_prompt_id": client_prompt_id,
            "blocks": blocks,
            "at_ms": crate::util::now_ms() as i64,
        });
        if let Some(input) = input("prompt_optimistic", data) {
            self.feed(&[input]);
        }
    }

    /// The prompt request returned: its echo is matched by id.
    pub fn prompt_completed(
        &mut self,
        client_prompt_id: Option<&str>,
        message_id: Option<&str>,
        stop_reason: &str,
    ) {
        let data = json!({
            "client_prompt_id": client_prompt_id,
            "message_id": message_id,
            "stop_reason": stop_reason,
            "at_ms": crate::util::now_ms() as i64,
        });
        if let Some(input) = input("prompt_completed", data) {
            self.feed(&[input]);
        }
    }

    /// The prompt request failed: the prompt stays, marked failed.
    pub fn prompt_failed(&mut self, client_prompt_id: &str, message: &str) {
        let data = json!({ "client_prompt_id": client_prompt_id, "message": message });
        if let Some(input) = input("prompt_failed", data) {
            self.feed(&[input]);
        }
    }

    // ---- steer and queue inputs the agent has not confirmed ------------------

    /// Track a message sent mid-run until the agent applies or starts it.
    pub fn add_pending_input(&mut self, input: PendingInput) {
        self.proj.overlay.add_input(input);
        self.refresh_inputs();
    }

    /// Re-key an input when the agent assigns its own input id.
    pub fn rekey_input(&mut self, from: &str, to: &str) {
        self.proj.overlay.rekey_input(from, to);
        self.refresh_inputs();
    }

    /// Mark a queued input's removal as in flight (or failed).
    pub fn set_discard_pending(&mut self, input_id: &str, pending: bool) {
        self.proj.overlay.set_discarding(input_id, pending);
        self.refresh_inputs();
    }

    /// Update (or drop, once terminal) an unconfirmed input from a state.
    pub fn set_input_state(&mut self, input_id: &str, delivery: &str, state: &str) {
        self.proj.overlay.set_input_state(input_id, delivery, state);
        self.refresh_inputs();
    }

    /// A lifecycle notification: applied at once when the input is known,
    /// otherwise kept until the submit response gives it its agent id.
    pub fn note_input_state(&mut self, input_id: &str, delivery: &str, state: &str) {
        self.proj
            .overlay
            .note_input_state(input_id, delivery, state);
        self.refresh_inputs();
    }

    /// Apply what arrived for `input_id` (or its former id) before its submit
    /// response.
    pub fn apply_input_notes(&mut self, input_id: &str, former_id: &str) {
        self.proj.overlay.apply_input_notes(input_id, former_id);
        self.refresh_inputs();
    }

    // ---- tool content -------------------------------------------------------

    pub fn set_tool_fetch(&mut self, id: &str, fetch: Option<ContentFetch>) {
        self.proj.set_fetch(id, fetch);
    }

    /// The current state of tool call `id`.
    pub fn truncated_tool(&self, id: &str) -> Option<&ToolCall> {
        self.proj.tool(id)
    }

    /// The complete `part` (`result` or `arguments`) of tool call `id`
    /// replaces the shortened one in place.
    pub fn set_full_tool_content(&mut self, id: &str, part: &str, content: String) {
        self.proj.overlay.fetches.remove(id);
        let part = if part == "result" {
            "result"
        } else {
            "arguments"
        };
        let data = json!({ "tool_call_id": id, "part": part, "content": content });
        match input("tool_content", data) {
            Some(input) => {
                self.feed(&[input]);
            }
            // Nothing was applied: still drop the loading state.
            None => self.proj.set_fetch(id, None),
        }
    }

    // ---- client notices and options -----------------------------------------

    /// Add a client note after the last turn.
    pub fn push_notice(&mut self, text: impl Into<String>, error: bool) {
        self.proj.push_notice(text, error);
    }

    /// Config options from a `session/load` or `session/set_config_option`
    /// response, merged by id; the model and mode follow.
    pub fn apply_config(&mut self, options: Option<&Value>) {
        let Some(list) = options.and_then(Value::as_array) else {
            return;
        };
        if let Some(input) = input("config_options", json!({ "options": list })) {
            self.apply_inputs(&[input]);
        }
        self.select_from(list);
    }

    /// Model, node and mode as the options list says.
    fn select_from(&mut self, list: &[Value]) {
        for o in list {
            let current = o
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_owned);
            match o.get("id").and_then(Value::as_str) {
                Some("model") => {
                    match o.pointer("/_meta/querymt/modelEntry") {
                        Some(entry) => {
                            self.model_node = entry
                                .get("node_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        }
                        // Without the entry the machine is unknown: keep it
                        // for the same model, never carry it to another one.
                        None if self.model != current => self.model_node = None,
                        None => {}
                    }
                    self.model = current;
                }
                Some("mode") => self.mode = current,
                _ => {}
            }
        }
    }

    /// `availableModes` from `session/load` or `session/new`.
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

    /// The agent mode selector: the `mode` config option, else the modes
    /// `session/load` listed.
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

    /// The model option carries its full catalog entry (mesh-aware agents).
    pub fn model_has_entry(&self) -> bool {
        self.options.iter().any(|o| {
            o.get("id").and_then(Value::as_str) == Some("model")
                && o.pointer("/_meta/querymt/modelEntry").is_some()
        })
    }

    /// Display label of the current model.
    pub fn model_label(&self) -> Option<String> {
        let id = self.model.as_deref()?;
        if let Some(o) = self.select_option(&["model"])
            && let Some((_, label)) = o.choices.iter().find(|(v, _)| v == id)
        {
            return Some(label.clone());
        }
        Some(id.rsplit('/').next().unwrap_or(id).to_string())
    }

    /// Copy of everything except the transcript (cheap; for rendering chrome
    /// while the store is not borrowed).
    pub fn shallow(&self) -> Self {
        Self {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            parent: self.parent.clone(),
            loading: self.loading,
            error: self.error.clone(),
            error_details: self.error_details.clone(),
            phase: self.phase.clone(),
            prompting: self.prompting,
            plan: Vec::new(),
            usage: self.usage,
            breakdown: self.breakdown,
            accounting: self.accounting.clone(),
            active_work_ms: self.active_work_ms,
            active_started_at: self.active_started_at,
            mode: self.mode.clone(),
            model: self.model.clone(),
            model_node: self.model_node.clone(),
            options: self.options.clone(),
            slash_commands: self.slash_commands.clone(),
            modes: self.modes.clone(),
            pending_options: self.pending_options.clone(),
            history_more: self.history_more,
            history_before: self.history_before.clone(),
            history_loading: self.history_loading,
            history_error: self.history_error.clone(),
            run_id: self.run_id.clone(),
            pending_inputs: self.pending_inputs.clone(),
            undo_stack: self.undo_stack.clone(),
            undo_pending: self.undo_pending,
            after_load_notice: None,
            access: self.access.clone(),
            queue: Vec::new(),
            hydrating: false,
            flush_scheduled: false,
            proj: Projection::default(),
        }
    }

    // ---- turns (design D7) --------------------------------------------------

    /// Turns the agent can undo to, in transcript order: runs holding a
    /// prompt the agent confirmed. A pending prompt or a run without a prompt
    /// is not one.
    fn undo_turns(&self) -> Vec<UndoTurn> {
        let mut turns = Vec::new();
        for (id, rows) in self.proj.turn_bounds() {
            let Some(turn) = self.proj.core.turn_ref(id) else {
                continue;
            };
            let ids: Vec<String> = turn
                .entries()
                .filter_map(|e| match &e.kind {
                    EntryKind::UserMessage(m) if m.status == PromptStatus::Sent => {
                        m.message_id.clone()
                    }
                    _ => None,
                })
                .collect();
            if let Some(first) = ids.first().cloned() {
                turns.push(UndoTurn {
                    id: first,
                    ids,
                    rows,
                });
            }
        }
        turns
    }

    /// Position (in `turns`) of the newest completed undo's turn.
    fn undo_frontier(&self, turns: &[UndoTurn]) -> Option<usize> {
        self.undo_stack
            .iter()
            .rev()
            .find(|f| !f.restore_failed)
            .and_then(|f| turns.iter().position(|t| t.ids.contains(&f.message_id)))
    }

    /// Message id of the turn a workspace undo would go back to: the latest
    /// turn before the newest completed undo. The target is the turn's first
    /// prompt, so the whole run, steering included, rolls back.
    pub fn undo_target(&self) -> Option<String> {
        let turns = self.undo_turns();
        let end = self.undo_frontier(&turns).unwrap_or(turns.len());
        turns[..end].last().map(|t| t.id.clone())
    }

    /// Turns a workspace undo to `message_id` rolls back (that turn and every
    /// later live turn), or `None` when that turn is already undone.
    pub fn undo_affected_turns(&self, message_id: &str) -> Option<usize> {
        let turns = self.undo_turns();
        let at = turns.iter().position(|t| t.id == message_id)?;
        let end = self.undo_frontier(&turns).unwrap_or(turns.len());
        (at < end).then_some(end - at)
    }

    /// First row of the first turn rolled back by the newest completed undo.
    pub fn reverted_from(&self) -> Option<usize> {
        let turns = self.undo_turns();
        self.undo_frontier(&turns).map(|at| turns[at].rows.start)
    }

    /// User turn that redo would restore (top of the undo stack).
    pub fn redo_target(&self) -> Option<&str> {
        self.undo_stack.last().map(|f| f.message_id.as_str())
    }

    /// Message ids a workspace undo can still target.
    pub fn undoable(&self) -> HashSet<String> {
        let turns = self.undo_turns();
        let end = self.undo_frontier(&turns).unwrap_or(turns.len());
        turns.into_iter().take(end).map(|t| t.id).collect()
    }

    /// The closing row (last agent or work row) of each undoable turn, with
    /// the turn's undo target.
    pub fn turn_users(&self) -> HashMap<usize, String> {
        self.undo_turns()
            .into_iter()
            .filter_map(|t| Some((self.closing_row(&t.rows)?, t.id)))
            .collect()
    }

    /// For each turn's closing row, the turn's (added, removed, files).
    pub fn turn_ends(&self) -> HashMap<usize, (usize, usize, usize)> {
        self.proj
            .turn_bounds()
            .into_iter()
            .filter_map(|(_, rows)| {
                let ix = self.closing_row(&rows)?;
                Some((ix, self.turn_change_stats(ix)))
            })
            .collect()
    }

    /// Last agent or work row of a turn's rows.
    fn closing_row(&self, rows: &Range<usize>) -> Option<usize> {
        self.items[rows.clone()]
            .iter()
            .rposition(|i| matches!(i.kind, ItemKind::Agent { .. } | ItemKind::Work(_)))
            .map(|at| rows.start + at)
    }

    /// Message a fork of the turn holding row `end` branches from: the turn's
    /// last agent-identified reply or reasoning message, else its first
    /// prompt. `None` while nothing in the turn has an agent id yet.
    pub fn fork_point(&self, end: usize) -> Option<&str> {
        let (id, _) = self.proj.turn_containing(end)?;
        let turn = self.proj.core.turn_ref(id)?;
        let mut point: Option<&str> = None;
        let mut prompt: Option<&str> = None;
        for entry in turn.entries() {
            match &entry.kind {
                EntryKind::AssistantMessage(m) => point = m.message_id.as_deref().or(point),
                EntryKind::Reasoning(r) => point = r.message_id.as_deref().or(point),
                EntryKind::UserMessage(m) if m.status == PromptStatus::Sent => {
                    prompt = prompt.or(m.message_id.as_deref());
                }
                _ => {}
            }
        }
        point.or(prompt)
    }

    /// What a fork of the turn holding row `end` keeps: its first prompt and,
    /// when the fork point is a reply, the reply text.
    pub fn fork_target(&self, end: usize) -> Option<ForkTarget> {
        let message_id = self.fork_point(end)?.to_string();
        let (id, rows) = self.proj.turn_containing(end)?;
        let turn = &self.items[rows];
        let prompt = turn
            .iter()
            .find_map(|i| match &i.kind {
                ItemKind::User { text, local, .. } if !local => Some(text.trim().to_string()),
                _ => None,
            })
            .unwrap_or_default();
        let response = turn
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Agent { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_string();
        let includes_response = self.proj.core.turn_ref(id).is_some_and(|t| {
            t.entries().any(|e| {
                matches!(&e.kind, EntryKind::AssistantMessage(m)
                    if m.message_id.as_deref() == Some(message_id.as_str()))
            })
        });
        Some(ForkTarget {
            message_id,
            prompt,
            response,
            includes_response,
        })
    }

    /// Id of the turn holding row `end`: identifies it while rows stream in.
    pub fn turn_key(&self, end: usize) -> Option<&str> {
        self.proj.turn_containing(end).map(|(id, _)| id)
    }

    /// Rows of turn `key`.
    pub fn turn_range(&self, key: &str) -> Option<Range<usize>> {
        self.proj
            .turn_bounds()
            .into_iter()
            .find(|(id, _)| *id == key)
            .map(|(_, rows)| rows)
    }

    /// Files the turn in `range` changed: in first-touched order, each tool's
    /// change kept apart because successive edits don't share one baseline.
    pub fn turn_diffs(&self, range: Range<usize>) -> Vec<TurnDiffFile> {
        let mut files: Vec<TurnDiffFile> = Vec::new();
        for item in &self.items[range] {
            let ItemKind::Work(entries) = &item.kind else {
                continue;
            };
            for call in entries.iter().filter_map(|e| match e {
                WorkEntry::Tool(call) => Some(call),
                _ => None,
            }) {
                for d in &call.diffs {
                    let path = d.path.trim_start_matches("./");
                    let ix = match files.iter().position(|f| f.path == path) {
                        Some(ix) => ix,
                        None => {
                            files.push(TurnDiffFile {
                                path: path.to_string(),
                                ..Default::default()
                            });
                            files.len() - 1
                        }
                    };
                    let file = &mut files[ix];
                    let (a, r) = diff_stats(d);
                    file.added += a;
                    file.removed += r;
                    match file.changes.last_mut().filter(|c| c.id == call.id) {
                        Some(change) => change.hunks.push(d.clone()),
                        None => file.changes.push(TurnDiffChange {
                            id: call.id.clone(),
                            hunks: vec![d.clone()],
                        }),
                    }
                }
            }
        }
        files
    }

    /// All assistant text of the turn holding row `ix`, in order.
    pub fn turn_text(&self, ix: usize) -> String {
        let Some((_, rows)) = self.proj.turn_containing(ix) else {
            return String::new();
        };
        self.items[rows]
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Agent { text } => Some(text.trim()),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// (added, removed, files) of the turn holding row `ix`, up to that row.
    pub fn turn_change_stats(&self, ix: usize) -> (usize, usize, usize) {
        let Some((_, rows)) = self.proj.turn_containing(ix) else {
            return (0, 0, 0);
        };
        let (mut adds, mut dels) = (0usize, 0usize);
        let mut files: Vec<&str> = Vec::new();
        for item in &self.items[rows.start..=ix] {
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

    /// Test helper: apply one live `session/update` payload of this session.
    #[cfg(test)]
    pub(crate) fn apply(&mut self, update: &Value) {
        let params = json!({ "sessionId": self.session_id, "update": update });
        self.session_update(&params);
    }

    /// Test helper: the agent reports the run idle, as after a finished
    /// conversation (live chunks alone leave the run streaming).
    #[cfg(test)]
    pub(crate) fn settle(&mut self) {
        let params = json!({
            "sessionId": self.session_id, "state": "idle", "runId": "test-run",
        });
        self.state_update(&params);
    }
}
