//! Identity of an open session: which agent it lives on and its id there.
//!
//! The newtypes exist at the open-session boundary only; the rest of the store
//! keeps plain `&str` and converts at the call (design D3).
//!
//! [`OpenSession`] is the entity one open session lives in. It talks to its
//! agent through a [`Link`] it was given and never reads the store: whatever
//! the store must do afterwards leaves as an [`OpenSessionEvent`] (design D2).
// `SessionRef` and parts of the API are used by the surfaces, which land in
// later groups of this change.
#![allow(dead_code)]

use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{Context, EventEmitter};
use serde_json::{Value, json};
use tracing::Instrument as _;

use super::{ContentFetch, PendingInput, SessionView, UndoFrame};
use crate::rpc::RpcClient;
use crate::store::mcp_notice::{self, SkippedMcp};
use crate::store::{
    Attachment, HYDRATE_BYTES, ImageSendMode, Model, SESSION_FLUSH_MS, json_size,
    load_error_report, prompt_content,
};
use crate::util::now_ms;

/// Id of a configured agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentId(String);

/// Id of a session on an agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionId(String);

macro_rules! string_id {
    ($name:ident) => {
        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(AgentId);
string_id!(SessionId);

/// One session on one agent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) agent: AgentId,
    pub(crate) session: SessionId,
}

impl SessionKey {
    pub(crate) fn new(agent: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            agent: AgentId::new(agent),
            session: SessionId::new(session),
        }
    }

    /// The key a notification's params address, read from `sessionId` (ACP)
    /// or `session_id` (QueryMT runtime notifications). `None` when the params
    /// carry no non-empty session id.
    pub(crate) fn from_update(agent: &str, params: &Value) -> Option<Self> {
        let session = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        Some(Self::new(agent, session))
    }
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.agent, self.session)
    }
}

/// What a surface asks to open, and what a back stack stores.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionRef {
    pub(crate) key: SessionKey,
    pub(crate) cwd: String,
    pub(crate) title: String,
}

/// How a message reaches a session: as a new turn, steering the running one,
/// or queued behind it (design D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Delivery {
    /// A new turn on an idle session.
    Prompt,
    /// Into the running turn.
    Steer,
    /// After the running turn.
    Queue,
}

impl Delivery {
    /// The JSON-RPC method that carries this delivery.
    pub(crate) fn wire_method(self) -> &'static str {
        match self {
            Self::Prompt => "session/prompt",
            Self::Steer => "_querymt/session/steer",
            Self::Queue => "_querymt/session/queue",
        }
    }

    /// The `delivery` of the pending input this creates, as the agent reports
    /// it in `session/inputState`. A prompt starts a turn and is no pending input.
    pub(crate) fn input_state_name(self) -> Option<&'static str> {
        match self {
            Self::Prompt => None,
            Self::Steer => Some("steer"),
            Self::Queue => Some("queue"),
        }
    }

    /// Reads the `delivery` of a reported pending input.
    pub(crate) fn from_input_state_name(name: &str) -> Option<Self> {
        match name {
            "steer" => Some(Self::Steer),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }
}

/// What the agent of a session can do, copied from its runtime when the
/// session is linked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCaps {
    /// Steering and queueing input into a running turn.
    pub(crate) can_control_run: bool,
    /// `querymt/session/toolContent`: full tool content a load shortened.
    pub(crate) tool_content: bool,
    /// Workspace undo and redo (`undoStack`, `undo`, `redo`).
    pub(crate) undo: bool,
    /// `querymt/session/discardQueuedInput`.
    pub(crate) discard_queued: bool,
    /// `session/load` accepts a history window.
    pub(crate) history_window: bool,
}

/// The connection an open session talks through. The store hands it over
/// when the session opens and again whenever the connection changes.
#[derive(Clone, Default)]
pub(crate) struct Link {
    pub(crate) rpc: Option<Rc<RpcClient>>,
    /// The agent finished its handshake on this connection.
    pub(crate) connected: bool,
    pub(crate) generation: u64,
    pub(crate) caps: SessionCaps,
    /// Agent description for the diagnostic report of a failed load.
    pub(crate) agent_line: String,
}

/// What an [`OpenSession`] tells the store, delivered after the emitting
/// update ends.
#[derive(Clone, Debug)]
pub(crate) enum OpenSessionEvent {
    /// A prompt turn ended; `error` is the failure to log, if any.
    TurnFinished { error: Option<String> },
    /// A load ended. `skipped_mcp` is the load response's authoritative list
    /// (`None` when the load failed).
    Loaded {
        skipped_mcp: Option<Vec<SkippedMcp>>,
    },
    /// The user stopped the run: its pending requests are moot.
    Cancelled,
}

/// One open session: its transcript state and the link to its agent. Shared
/// by every surface that shows it, and closed when the last handle drops.
pub(crate) struct OpenSession {
    pub(crate) key: SessionKey,
    pub(crate) view: SessionView,
    link: Link,
}

impl EventEmitter<OpenSessionEvent> for OpenSession {}

impl OpenSession {
    pub(crate) fn new(view: SessionView, link: Link) -> Self {
        Self {
            key: SessionKey::new(&view.agent_id, &view.session_id),
            view,
            link,
        }
    }

    pub(crate) fn link(&self) -> &Link {
        &self.link
    }

    /// Use `link` from now on (the connection was replaced or dropped).
    pub(crate) fn rebind(&mut self, link: Link, cx: &mut Context<Self>) {
        self.link = link;
        cx.notify();
    }

    // ---- what the agent can do -------------------------------------------

    /// Steering and queueing into a running turn.
    pub(crate) fn can_control_run(&self) -> bool {
        self.link.caps.can_control_run
    }

    /// Full tool content can be fetched after a windowed load.
    pub(crate) fn supports_tool_content(&self) -> bool {
        self.link.caps.tool_content
    }

    /// Queued messages can be removed before they start.
    pub(crate) fn can_discard_queued(&self) -> bool {
        self.link.caps.discard_queued
    }

    /// Workspace undo and redo: a writable session on an agent that has them.
    pub(crate) fn supports_undo(&self) -> bool {
        self.link.caps.undo && self.view.access.is_none()
    }

    // ---- feeding -----------------------------------------------------------

    /// Apply the queued inputs once, a frame from now: a burst of chunks is
    /// one batch, one change report and one repaint (design D6). Prompt,
    /// history and load actions feed at once, which flushes the queue first,
    /// so order is kept.
    pub(crate) fn schedule_flush(&mut self, cx: &mut Context<Self>) {
        if !self.view.claim_flush() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SESSION_FLUSH_MS))
                .await;
            let _ = this.update(cx, |o, cx| {
                o.view.flush_done();
                o.view.flush();
                cx.notify();
            });
        })
        .detach();
    }

    // ---- load --------------------------------------------------------------

    /// `session/load`: build the transcript from the snapshot. A load that
    /// ends after the last handle dropped is ignored.
    pub(crate) fn load(&mut self, cx: &mut Context<Self>) {
        let Some(rpc) = self.link.rpc.clone().filter(|_| self.link.connected) else {
            self.view.error = Some("The agent of this session is not connected.".into());
            cx.notify();
            return;
        };
        let windowed = self.link.caps.history_window;
        self.view.loading = true;
        cx.notify();

        let mut params = json!({
            "sessionId": self.key.session.as_str(),
            "cwd": self.view.cwd,
            "mcpServers": [],
        });
        if windowed {
            params["_meta"] = json!({ "querymt": { "loadWindow": { "maxTurns": 20, "maxBytes": 1048576, "truncateBytes": 32768 } } });
        }
        let request_params = params.clone();
        let agent_line = self.link.agent_line.clone();
        let load_span = tracing::info_span!(
            "session.open",
            session.windowed = windowed,
            session.items = tracing::field::Empty,
            session.apply_ms = tracing::field::Empty,
            session.error = tracing::field::Empty,
        );
        cx.spawn(async move |this, cx| {
            let started = now_ms();
            let result = rpc
                .request_detailed("session/load", params)
                .instrument(load_span.clone())
                .await;
            let elapsed_ms = now_ms() - started;
            let apply_started = now_ms();
            let job = this
                .update(cx, |o, cx| {
                    let mut job = None;
                    let mut skipped = None;
                    match result {
                        Ok(v) => {
                            skipped = Some(mcp_notice::from_response(&v));
                            // The core takes the whole response (its snapshot, run
                            // state, history window and options). A large one is
                            // decoded off the UI thread: the view keeps loading and
                            // queues inputs behind it (design D6; native only).
                            let big =
                                !cfg!(target_arch = "wasm32") && json_size(&v) > HYDRATE_BYTES;
                            let v = if big {
                                match o.view.begin_load(v) {
                                    Ok(started) => {
                                        job = Some(started);
                                        None
                                    }
                                    Err(v) => Some(v),
                                }
                            } else {
                                Some(v)
                            };
                            if let Some(v) = v {
                                o.view.loading = false;
                                o.view.load(v);
                            }
                        }
                        Err(err) => {
                            o.view.loading = false;
                            o.view.error = Some(format!("Could not load the session: {err}"));
                            o.view.error_details = Some(load_error_report(
                                &agent_line,
                                &request_params,
                                &err,
                                elapsed_ms,
                            ));
                        }
                    }
                    if job.is_none() {
                        o.finish_loaded(&load_span, skipped.take(), cx);
                    }
                    job.map(|job| (job, skipped))
                })
                .ok()
                .flatten();
            if let Some((job, skipped)) = job {
                let hydrated = cx
                    .background_executor()
                    .spawn(async move { job.hydrate() })
                    .await;
                let _ = this.update(cx, |o, cx| {
                    o.view.loading = false;
                    o.view.finish_load(hydrated);
                    o.finish_loaded(&load_span, skipped, cx);
                });
            }
            load_span.record("session.apply_ms", now_ms() - apply_started);
        })
        .detach();
    }

    /// What follows a finished load, inline or hydrated.
    fn finish_loaded(
        &mut self,
        span: &tracing::Span,
        skipped_mcp: Option<Vec<SkippedMcp>>,
        cx: &mut Context<Self>,
    ) {
        span.record("session.items", self.view.items.len());
        if let Some(err) = &self.view.error {
            span.record("session.error", err.as_str());
        }
        self.refresh_undo_stack(cx);
        cx.emit(OpenSessionEvent::Loaded { skipped_mcp });
        cx.notify();
    }

    /// Close and load again: a fresh transcript for the same session. The
    /// phase carries over.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        let old = &self.view;
        let mut view = SessionView::new(&old.agent_id, &old.session_id, &old.cwd, &old.title);
        view.phase = old.phase.clone();
        self.view = view;
        self.load(cx);
    }

    // ---- older history -----------------------------------------------------

    /// Fetch the turns preceding the loaded transcript (`_querymt/session/history`)
    /// and prepend them. One page in flight at a time; triggered when the user
    /// scrolls near the top.
    pub(crate) fn load_older(&mut self, cx: &mut Context<Self>) {
        let view = &mut self.view;
        if view.history_loading || view.history_error.is_some() {
            return;
        }
        let Some(before) = view.history_before.clone() else {
            return;
        };
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        view.history_loading = true;
        cx.notify();
        let req = rpc.request(
            "_querymt/session/history",
            json!({
                "sessionId": self.key.session.as_str(),
                "before": before,
                "loadWindow": { "maxTurns": 20, "maxBytes": 1048576, "truncateBytes": 32768 },
            }),
        );
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |o, cx| {
                o.view.history_loading = false;
                match result {
                    Ok(page) => {
                        let events = page
                            .get("events")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        // The core inserts the page above and moves the paging
                        // cursor (`history_before`, `history_more`).
                        o.view.history_page(&page);
                        crate::telemetry::record_history_load("ok");
                        tracing::info!(events, "older history page loaded");
                    }
                    // An undo rewrote history: the cursor is void, reload from scratch.
                    Err(err) if err.contains("history_changed") => {
                        crate::telemetry::record_history_load("history_changed");
                        cx.notify();
                        o.reload(cx);
                        return;
                    }
                    Err(err) => {
                        crate::telemetry::record_history_load("error");
                        tracing::warn!(error = %err, "older history page failed");
                        o.view.history_error = Some(err);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Clear a failed older-history fetch and try again.
    pub(crate) fn retry_older(&mut self, cx: &mut Context<Self>) {
        self.view.history_error = None;
        self.load_older(cx);
    }

    // ---- tool content ------------------------------------------------------

    /// Fetch the complete `part` (`result` or `arguments`) of tool call `id`,
    /// which a windowed load shortened.
    pub(crate) fn load_tool_content(
        &mut self,
        id: &str,
        part: &'static str,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_tool_content() {
            return;
        }
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        let Some(call) = self.view.truncated_tool(id) else {
            return;
        };
        let shortened = match part {
            "result" => call.truncated_result.is_some(),
            _ => call.truncated_args.is_some(),
        };
        if !shortened || matches!(call.fetch, Some(ContentFetch::Loading(_))) {
            return;
        }
        let id = id.to_owned();
        self.view
            .set_tool_fetch(&id, Some(ContentFetch::Loading(part)));
        cx.notify();
        let request = rpc.request(
            "_querymt/session/toolContent",
            json!({ "sessionId": self.key.session.as_str(), "toolCallId": id, "part": part }),
        );
        cx.spawn(async move |this, cx| {
            let result = request.await.and_then(|v| {
                v.get("content")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "The agent returned no content.".to_string())
            });
            let _ = this.update(cx, |o, cx| {
                match result {
                    Ok(content) => o.view.set_full_tool_content(&id, part, content),
                    Err(error) => {
                        tracing::warn!(%error, tool = %id, part, "tool content fetch failed");
                        o.view.set_tool_fetch(
                            &id,
                            Some(ContentFetch::Failed(format!(
                                "Could not load the full {}: {error}",
                                if part == "result" {
                                    "output"
                                } else {
                                    "parameters"
                                }
                            ))),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- undo and redo -----------------------------------------------------

    /// Fetch the workspace undo stack (`querymt/session/undoStack`).
    pub(crate) fn refresh_undo_stack(&mut self, cx: &mut Context<Self>) {
        if !self.supports_undo() {
            return;
        }
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        let req = rpc.request(
            "_querymt/session/undoStack",
            json!({ "session_id": self.key.session.as_str() }),
        );
        cx.spawn(async move |this, cx| {
            let Ok(resp) = req.await else { return };
            let _ = this.update(cx, |o, cx| {
                o.view.set_undo_stack(&UndoFrame::parse_stack(&resp));
                cx.notify();
            });
        })
        .detach();
    }

    /// Revert the workspace to before the user turn `message_id` (and every
    /// later turn), then reload the transcript. `blocked`: the store has a
    /// reason not to rewrite history now (a fork is being created).
    pub(crate) fn undo_to(&mut self, message_id: String, blocked: bool, cx: &mut Context<Self>) {
        self.start_undo_op(Some(message_id), blocked, cx);
    }

    /// Re-apply the most recently undone turn (`querymt/session/redo`).
    pub(crate) fn redo(&mut self, blocked: bool, cx: &mut Context<Self>) {
        self.start_undo_op(None, blocked, cx);
    }

    /// `target`: undo to that turn; `None`: redo.
    fn start_undo_op(&mut self, target: Option<String>, blocked: bool, cx: &mut Context<Self>) {
        let undo = target.is_some();
        let supported = self.supports_undo();
        let view = &mut self.view;
        if view.loading || view.running() || view.undo_pending || blocked {
            return;
        }
        if !supported {
            view.push_notice("This agent does not support workspace undo and redo.", true);
            cx.notify();
            return;
        }
        let (method, params) = if let Some(target) = target {
            if view.undo_affected_turns(&target).is_none() {
                return;
            }
            (
                "_querymt/session/undo",
                json!({ "session_id": view.session_id, "message_id": target }),
            )
        } else {
            if view.undo_stack.is_empty() {
                return;
            }
            (
                "_querymt/session/redo",
                json!({ "session_id": view.session_id }),
            )
        };
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        view.undo_pending = true;
        cx.notify();
        let req = rpc.request(method, params);
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |o, cx| o.finish_undo_op(undo, result, cx));
        })
        .detach();
    }

    fn finish_undo_op(
        &mut self,
        undo: bool,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let view = &mut self.view;
        view.undo_pending = false;
        let what = if undo { "Undo" } else { "Redo" };
        let resp = match result {
            Ok(resp) => resp,
            Err(err) => {
                view.push_notice(format!("{what} failed: {err}"), true);
                cx.notify();
                return;
            }
        };
        view.set_undo_stack(&UndoFrame::parse_stack(&resp));
        if resp.get("success").and_then(Value::as_bool) != Some(true) {
            let msg = resp
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| format!("{what} failed."), str::to_owned);
            view.push_notice(msg, true);
            cx.notify();
            return;
        }
        let notice = if undo {
            let n = resp
                .get("reverted_files")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            match n {
                0 => "Turn undone.".to_string(),
                1 => "Undid 1 changed file.".to_string(),
                n => format!("Undid {n} changed files."),
            }
        } else {
            "Turn restored.".to_string()
        };
        // History changed: reload the transcript, then show the result.
        self.reload(cx);
        self.view.after_load_notice = Some(notice);
    }

    // ---- prompts -----------------------------------------------------------

    /// Send a prompt. While a turn runs on an agent with turn control,
    /// `delivery` (steer or queue) picks the QueryMT extension.
    pub(crate) fn send(
        &mut self,
        text: String,
        attachments: Vec<Attachment>,
        delivery: Delivery,
        image_mode: ImageSendMode,
        cx: &mut Context<Self>,
    ) {
        let control = self.can_control_run();
        let Some(rpc) = self.link.rpc.clone() else {
            self.view.push_notice("The agent is not connected.", true);
            cx.notify();
            return;
        };
        let content = prompt_content(&text, &attachments, image_mode);
        if !(self.view.running() && control) {
            self.start_prompt(&content, cx);
            cx.notify();
            return;
        }
        // Like desktop: slash commands can't steer, they always queue.
        let wanted = if text.trim_start().starts_with('/') || delivery == Delivery::Queue {
            Delivery::Queue
        } else {
            Delivery::Steer
        };
        let input_id = super::next_client_id();
        // The message waits in the activity dock; the agent echoes it into
        // the transcript once it is applied (steer) or started (queue).
        self.view.add_pending_input(PendingInput {
            input_id: input_id.clone(),
            delivery: wanted.input_state_name().unwrap_or_default().into(),
            state: "sending".into(),
            text,
            attachments: attachments.len(),
            discard_pending: false,
        });
        let known_run = self.view.run_id.clone();
        let session_id = self.key.session.to_string();
        cx.notify();
        cx.spawn(async move |this, cx| {
            // Steering must name the active run (`expected_run_id`); ask the
            // agent for it, falling back to the last one we saw.
            let mut delivery = wanted;
            let mut run_id = known_run;
            if delivery == Delivery::Steer {
                let state = rpc
                    .request(
                        "_querymt/session/runtimeState",
                        json!({ "session_id": session_id }),
                    )
                    .await;
                if let Ok(rt) = &state {
                    run_id = rt
                        .get("active_run_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(run_id);
                    let phase = rt.get("phase").and_then(Value::as_str).unwrap_or_default();
                    let steerable = rt.get("steerable").and_then(Value::as_bool).unwrap_or(true);
                    if !steerable || matches!(phase, "closing" | "cancel_requested") {
                        run_id = None;
                    }
                    let _ = this.update(cx, |o, cx| {
                        o.view.apply_runtime(rt);
                        o.view.sync_inputs();
                        cx.notify();
                    });
                }
                // No steerable run: queue instead, as desktop does.
                if run_id.is_none() {
                    delivery = Delivery::Queue;
                }
            }
            let mut params =
                json!({ "session_id": session_id, "prompt": content, "client_input_id": input_id });
            if let (Delivery::Steer, Some(run)) = (delivery, &run_id) {
                params["expected_run_id"] = json!(run);
            }
            let result = rpc.request(delivery.wire_method(), params).await;
            let _ = this.update(cx, |o, cx| {
                let view = &mut o.view;
                match &result {
                    Err(err) => {
                        view.set_input_state(
                            &input_id,
                            delivery.input_state_name().unwrap_or_default(),
                            "failed",
                        );
                        view.push_notice(format!("Could not deliver the message: {err}"), true);
                    }
                    Ok(v) => {
                        let status = v.get("status").and_then(Value::as_str).unwrap_or_default();
                        let id = v
                            .pointer("/data/input_id")
                            .and_then(Value::as_str)
                            .unwrap_or(&input_id)
                            .to_string();
                        view.rekey_input(&input_id, &id);
                        let state = match status {
                            "steered" => "accepted",
                            "queued" => "queued",
                            // The session went idle meanwhile: it runs as a new turn.
                            _ => "started",
                        };
                        let delivery = if status == "queued" {
                            Delivery::Queue
                        } else {
                            delivery
                        };
                        view.set_input_state(
                            &id,
                            delivery.input_state_name().unwrap_or_default(),
                            state,
                        );
                        // A notification may have overtaken this response.
                        view.apply_input_notes(&id, &input_id);
                        if status == "started" {
                            view.phase = "starting".into();
                            view.sync_inputs();
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Start a turn: show the prompt at once, mark the session prompting and
    /// send `session/prompt` with a `client_prompt_id`, so the agent's echo
    /// replaces the optimistic row. The turn completes in a background task
    /// that ends in [`Self::finish_prompt`]. The one place a turn is started,
    /// for a first prompt (launch) and later ones (send).
    pub(crate) fn start_prompt(&mut self, content: &[Value], cx: &mut Context<Self>) {
        let Some(rpc) = self.link.rpc.clone() else {
            self.view.push_notice("The agent is not connected.", true);
            return;
        };
        let prompt_id = super::next_client_id();
        self.view.prompt_optimistic(&prompt_id, content);
        self.view.prompting = true;
        self.view.sync_inputs();
        let turn = rpc.request(
            Delivery::Prompt.wire_method(),
            json!({
                "sessionId": self.key.session.as_str(),
                "prompt": content,
                "_meta": { "querymt": { "client_prompt_id": prompt_id } },
            }),
        );
        cx.spawn(async move |this, cx| {
            let result = turn.await;
            let _ = this.update(cx, |o, cx| o.finish_prompt(&prompt_id, result, cx));
        })
        .detach();
    }

    /// The turn of prompt `client_prompt_id` ended.
    fn finish_prompt(
        &mut self,
        client_prompt_id: &str,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let view = &mut self.view;
        view.prompting = false;
        // The outcome goes to the core in order with everything else: the
        // prompt is linked to its echo by id, or marked failed.
        match &result {
            Ok(v) => view.prompt_completed(
                Some(client_prompt_id),
                v.pointer("/_meta/querymt/messageId")
                    .and_then(Value::as_str),
                v.get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn"),
            ),
            Err(err) => view.prompt_failed(client_prompt_id, err),
        }
        view.sync_inputs();
        match &result {
            Err(err) => view.push_notice(format!("Prompt failed: {err}"), true),
            Ok(v) => match v.get("stopReason").and_then(Value::as_str) {
                Some("cancelled") => view.push_notice("Stopped.", false),
                Some("max_tokens") => {
                    view.push_notice("Stopped: the model hit its token limit.", true)
                }
                Some("refusal") => view.push_notice("The model refused to continue.", true),
                _ => {}
            },
        }
        // A finished turn adds an undo target and drops stale redo frames.
        self.refresh_undo_stack(cx);
        cx.emit(OpenSessionEvent::TurnFinished {
            error: result.err(),
        });
        cx.notify();
    }

    /// `session/cancel` for the running turn.
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(rpc) = &self.link.rpc {
            rpc.notify(
                "session/cancel",
                json!({ "sessionId": self.key.session.as_str() }),
            );
        }
        cx.emit(OpenSessionEvent::Cancelled);
    }

    /// Remove a queued message before it starts (`querymt/session/discardQueuedInput`).
    pub(crate) fn discard_queued(&mut self, input_id: &str, cx: &mut Context<Self>) {
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        if !self
            .view
            .pending_inputs
            .iter()
            .any(|i| i.input_id == input_id && i.state == "queued" && !i.discard_pending)
        {
            return;
        }
        self.view.set_discard_pending(input_id, true);
        let input_id = input_id.to_string();
        let req = rpc.request(
            "_querymt/session/discardQueuedInput",
            json!({ "session_id": self.key.session.as_str(), "input_id": input_id }),
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ = this.update(cx, |o, cx| {
                match result {
                    Ok(_) => o.view.set_input_state(
                        &input_id,
                        Delivery::Queue.input_state_name().unwrap_or_default(),
                        "discarded",
                    ),
                    Err(err) => {
                        o.view.set_discard_pending(&input_id, false);
                        o.view.push_notice(
                            format!("Could not remove the queued message: {err}"),
                            true,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- options -----------------------------------------------------------

    /// Change a session config option (`session/set_config_option`); an empty
    /// `config_id` means the mode of an agent that only has `session/set_mode`.
    /// For `model`, `value` is a selection key and `model` its catalog entry,
    /// so the model on the chosen machine is sent.
    pub(crate) fn set_option(
        &mut self,
        config_id: &str,
        value: &str,
        model: Option<Model>,
        cx: &mut Context<Self>,
    ) {
        let Some(rpc) = self.link.rpc.clone() else {
            return;
        };
        let session_id = self.key.session.to_string();
        let key = if config_id.is_empty() {
            "mode".to_string()
        } else {
            config_id.to_string()
        };
        // The model by selection key: its id goes on the wire, its catalog
        // entry (with the node) in `_meta`.
        let wire = model.as_ref().map_or(value, |m| m.id.as_str()).to_string();
        self.view.pending_options.insert(key.clone());
        let req = if config_id.is_empty() {
            rpc.request(
                "session/set_mode",
                json!({ "sessionId": session_id, "modeId": wire }),
            )
        } else {
            let mut params =
                json!({ "sessionId": session_id, "configId": config_id, "value": wire });
            if let Some(model) = &model {
                let mut entry = model.raw.clone();
                if let Some(obj) = entry.as_object_mut() {
                    obj.remove("availability");
                }
                params["_meta"] = json!({ "querymt": { "modelEntry": entry } });
            }
            rpc.request("session/set_config_option", params)
        };
        let config_id = config_id.to_string();
        let node = model.and_then(|m| m.node_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = req.await;
            let _ =
                this.update(cx, |o, cx| {
                    let view = &mut o.view;
                    view.pending_options.remove(&key);
                    match result {
                        Ok(resp) => {
                            if config_id.is_empty() {
                                view.mode = Some(wire.clone());
                            } else if resp.get("configOptions").is_some() {
                                view.apply_config(resp.get("configOptions"));
                                // A confirmation without the model entry names
                                // the model but not its machine: keep the choice.
                                if config_id == "model"
                                    && !view.model_has_entry()
                                    && view.model.as_deref() == Some(wire.as_str())
                                {
                                    view.model_node = node.clone();
                                }
                            } else {
                                if let Some(o) = view.options.iter_mut().find(|o| {
                                    o.get("id").and_then(Value::as_str) == Some(&config_id)
                                }) {
                                    o["currentValue"] = Value::String(wire.clone());
                                }
                                match config_id.as_str() {
                                    "model" => {
                                        view.model = Some(wire.clone());
                                        view.model_node = node.clone();
                                    }
                                    "mode" => view.mode = Some(wire.clone()),
                                    _ => {}
                                }
                            }
                        }
                        Err(err) => view.push_notice(
                            format!(
                                "Could not change {}: {err}",
                                if config_id.is_empty() {
                                    "mode"
                                } else {
                                    &config_id
                                }
                            ),
                            true,
                        ),
                    }
                    cx.notify();
                });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn delivery_maps_to_wire_method() {
        assert_eq!(Delivery::Prompt.wire_method(), "session/prompt");
        assert_eq!(Delivery::Steer.wire_method(), "_querymt/session/steer");
        assert_eq!(Delivery::Queue.wire_method(), "_querymt/session/queue");
        assert_eq!(Delivery::Prompt.input_state_name(), None);
        for delivery in [Delivery::Steer, Delivery::Queue] {
            let name = delivery.input_state_name().expect("pending input name");
            assert_eq!(Delivery::from_input_state_name(name), Some(delivery));
        }
        assert_eq!(Delivery::from_input_state_name("other"), None);
    }

    #[test]
    fn key_from_session_update() {
        let key = SessionKey::from_update("a1", &json!({ "sessionId": "s1", "update": {} }));
        assert_eq!(key, Some(SessionKey::new("a1", "s1")));
        let key = key.expect("key");
        assert_eq!(key.agent.as_str(), "a1");
        assert_eq!(key.session.as_str(), "s1");
        assert_eq!(key.to_string(), "a1/s1");
    }

    #[test]
    fn key_from_snake_case_runtime_params() {
        let key = SessionKey::from_update("a1", &json!({ "session_id": "s2" }));
        assert_eq!(key, Some(SessionKey::new("a1", "s2")));
    }

    #[test]
    fn key_missing_session_id_is_none() {
        assert_eq!(SessionKey::from_update("a1", &json!({})), None);
        assert_eq!(
            SessionKey::from_update("a1", &json!({ "sessionId": "" })),
            None
        );
        assert_eq!(
            SessionKey::from_update("a1", &json!({ "sessionId": 3 })),
            None
        );
    }
}
