//! Recorder of one agent's JSON-RPC traffic, for the debug events viewer.
//! Records only while Developer mode is on. Each request is paired with its
//! response (status, duration); the session a message belongs to is read
//! from its params, and responses inherit their request's. Runs of streamed
//! `*_chunk` session updates merge into one event, so a long reply does not
//! push everything else out of the buffer.

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// Events kept per agent.
pub const CAP_EVENTS: usize = 4000;
/// Bytes of one message kept; larger ones are cut (loads, big tool output).
pub const CAP_BYTES: usize = 256 * 1024;
/// Text kept from a run of merged chunks.
const CAP_JOINED: usize = 64 * 1024;
const PREVIEW_CHARS: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// This app asked the agent (has a response).
    Request,
    /// This app told the agent (no response).
    Notify,
    /// The agent told this app.
    Notification,
    /// The agent asked this app (permission, question, file).
    AgentRequest,
}

impl EventKind {
    /// Sent by this app.
    pub fn outgoing(self) -> bool {
        matches!(self, Self::Request | Self::Notify)
    }

    /// Waits for a response.
    pub fn is_call(self) -> bool {
        matches!(self, Self::Request | Self::AgentRequest)
    }
}

/// A JSON-RPC message as sent, cut at [`CAP_BYTES`].
#[derive(Clone, Debug)]
pub struct Body {
    pub text: String,
    /// Length of the whole message.
    pub full_len: usize,
}

impl Body {
    fn new(text: &str) -> Self {
        let mut end = text.len().min(CAP_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].to_string(),
            full_len: text.len(),
        }
    }

    pub fn cut(&self) -> bool {
        self.full_len > self.text.len()
    }
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub at_ms: f64,
    /// The `error.message` of a failed call.
    pub error: Option<String>,
    pub body: Body,
}

#[derive(Clone, Debug)]
pub struct WireEvent {
    pub seq: u64,
    pub at_ms: f64,
    pub kind: EventKind,
    pub method: String,
    /// What kind of `session/update` (`tool_call`, `agent_message_chunk`, …).
    pub update: Option<String>,
    /// Short text: a chunk's text, a tool's title, compact params.
    pub preview: String,
    pub session: Option<String>,
    pub body: Body,
    pub reply: Option<Reply>,
    /// Streamed chunks merged into this event (1: just this one).
    pub merged: u32,
}

impl WireEvent {
    /// Milliseconds from the call to its response.
    pub fn duration_ms(&self) -> Option<f64> {
        self.reply.as_ref().map(|r| (r.at_ms - self.at_ms).max(0.))
    }

    pub fn failed(&self) -> bool {
        self.reply.as_ref().is_some_and(|r| r.error.is_some())
    }

    /// A call still waiting for its response.
    pub fn pending(&self) -> bool {
        self.kind.is_call() && self.reply.is_none()
    }
}

#[derive(Default)]
struct Inner {
    events: VecDeque<WireEvent>,
    next_seq: u64,
    /// Our request id → event seq.
    requests: HashMap<u64, u64>,
    /// The agent's request id (as JSON) → event seq.
    agent_requests: HashMap<String, u64>,
}

impl Inner {
    fn get_mut(&mut self, seq: u64) -> Option<&mut WireEvent> {
        let first = self.events.front()?.seq;
        self.events.get_mut(seq.checked_sub(first)? as usize)
    }
}

/// Traffic of one agent across reconnects.
#[derive(Default)]
pub struct WireLog {
    enabled: Cell<bool>,
    /// Bumped on every change, including replies to recorded calls.
    version: Cell<u64>,
    inner: RefCell<Inner>,
}

impl WireLog {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    pub fn version(&self) -> u64 {
        self.version.get()
    }

    pub fn events(&self) -> Ref<'_, VecDeque<WireEvent>> {
        Ref::map(self.inner.borrow(), |i| &i.events)
    }

    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.events.clear();
        inner.requests.clear();
        inner.agent_requests.clear();
        self.bump();
    }

    fn bump(&self) {
        self.version.set(self.version.get() + 1);
    }

    /// A request this app sent.
    pub fn sent_request(&self, id: u64, method: &str, params: &Value, text: &str) {
        if !self.enabled() {
            return;
        }
        let seq = self.push(EventKind::Request, method, params, text);
        self.inner.borrow_mut().requests.insert(id, seq);
    }

    /// A notification this app sent.
    pub fn sent_notification(&self, method: &str, params: &Value, text: &str) {
        if self.enabled() {
            self.push(EventKind::Notify, method, params, text);
        }
    }

    /// This app's response to the agent's request `id`.
    pub fn sent_response(&self, id: &Value, error: Option<&str>, text: &str) {
        if !self.enabled() {
            return;
        }
        let mut inner = self.inner.borrow_mut();
        let Some(seq) = inner.agent_requests.remove(&id.to_string()) else {
            return;
        };
        if let Some(event) = inner.get_mut(seq) {
            event.reply = Some(Reply {
                at_ms: crate::util::now_ms(),
                error: error.map(str::to_owned),
                body: Body::new(text),
            });
        }
        drop(inner);
        self.bump();
    }

    /// Any message from the agent.
    pub fn received(&self, message: &Value, text: &str) {
        if !self.enabled() {
            return;
        }
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").unwrap_or(&Value::Null);
            match message.get("id").filter(|id| !id.is_null()) {
                Some(id) => {
                    let seq = self.push(EventKind::AgentRequest, method, params, text);
                    self.inner
                        .borrow_mut()
                        .agent_requests
                        .insert(id.to_string(), seq);
                }
                None => {
                    if !self.merge_chunk(method, params) {
                        self.push(EventKind::Notification, method, params, text);
                    }
                }
            }
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            return;
        };
        let mut inner = self.inner.borrow_mut();
        let Some(seq) = inner.requests.remove(&id) else {
            return;
        };
        if let Some(event) = inner.get_mut(seq) {
            let error = message.get("error").map(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Request failed")
                    .to_string()
            });
            if event.session.is_none() {
                event.session = message.get("result").and_then(session_of);
            }
            event.reply = Some(Reply {
                at_ms: crate::util::now_ms(),
                error,
                body: Body::new(text),
            });
        }
        drop(inner);
        self.bump();
    }

    fn push(&self, kind: EventKind, method: &str, params: &Value, text: &str) -> u64 {
        let update = (method == "session/update")
            .then(|| {
                params
                    .pointer("/update/sessionUpdate")
                    .and_then(Value::as_str)
            })
            .flatten()
            .map(str::to_owned);
        let mut inner = self.inner.borrow_mut();
        let seq = inner.next_seq + 1;
        inner.next_seq = seq;
        inner.events.push_back(WireEvent {
            seq,
            at_ms: crate::util::now_ms(),
            kind,
            method: method.to_string(),
            preview: preview(method, params),
            update,
            session: session_of(params),
            body: Body::new(text),
            reply: None,
            merged: 1,
        });
        while inner.events.len() > CAP_EVENTS {
            if let Some(old) = inner.events.pop_front() {
                inner.requests.retain(|_, s| *s != old.seq);
                inner.agent_requests.retain(|_, s| *s != old.seq);
            }
        }
        drop(inner);
        self.bump();
        seq
    }

    /// Fold a streamed chunk into the previous event when that is a chunk of
    /// the same kind for the same session.
    fn merge_chunk(&self, method: &str, params: &Value) -> bool {
        let Some(kind) = params
            .pointer("/update/sessionUpdate")
            .and_then(Value::as_str)
            .filter(|k| method == "session/update" && k.ends_with("_chunk"))
        else {
            return false;
        };
        let session = session_of(params);
        let mut inner = self.inner.borrow_mut();
        let Some(last) = inner.events.back_mut().filter(|e| {
            e.kind == EventKind::Notification
                && e.update.as_deref() == Some(kind)
                && e.session == session
        }) else {
            return false;
        };
        if last.preview.len() < CAP_JOINED {
            last.preview.push_str(&chunk_text(params));
        }
        last.merged += 1;
        drop(inner);
        self.bump();
        true
    }
}

fn session_of(v: &Value) -> Option<String> {
    ["sessionId", "session_id", "sessionID"]
        .iter()
        .find_map(|k| v.get(*k))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn chunk_text(params: &Value) -> String {
    params
        .pointer("/update/content/text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// One line saying what a message carries.
fn preview(method: &str, params: &Value) -> String {
    if method == "session/update"
        && let Some(update) = params.get("update")
    {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind.ends_with("_chunk") {
            return chunk_text(params);
        }
        let text = |k: &str| update.get(k).and_then(Value::as_str);
        if let Some(title) = text("title") {
            return match text("status") {
                Some(status) => format!("{title} · {status}"),
                None => title.to_string(),
            };
        }
        if let (Some(id), Some(status)) = (text("toolCallId"), text("status")) {
            return format!("{id} · {status}");
        }
        let mut rest = update.clone();
        if let Some(obj) = rest.as_object_mut() {
            obj.remove("sessionUpdate");
        }
        return compact(&rest);
    }
    let mut rest = params.clone();
    if let Some(obj) = rest.as_object_mut() {
        for k in ["sessionId", "session_id"] {
            obj.remove(k);
        }
    }
    compact(&rest)
}

/// JSON on one line, cut to [`PREVIEW_CHARS`]; empty objects say nothing.
fn compact(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(o) if o.is_empty() => String::new(),
        _ => {
            let text = v.to_string();
            match text.char_indices().nth(PREVIEW_CHARS) {
                Some((ix, _)) => format!("{}…", &text[..ix]),
                None => text,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn recording() -> WireLog {
        let log = WireLog::default();
        log.set_enabled(true);
        log
    }

    #[test]
    fn pairs_calls_with_responses() {
        let log = recording();
        let params = json!({ "sessionId": "s1", "prompt": [{ "type": "text", "text": "hi" }] });
        log.sent_request(7, "session/prompt", &params, "{\"id\":7}");
        assert!(log.events()[0].pending());
        assert_eq!(log.events()[0].session.as_deref(), Some("s1"));
        assert!(!log.events()[0].preview.contains("sessionId"));
        let reply = json!({ "id": 7, "error": { "code": -32000, "message": "boom" } });
        log.received(&reply, &reply.to_string());
        {
            let events = log.events();
            let e = &events[0];
            assert!(e.failed() && !e.pending());
            assert_eq!(e.reply.as_ref().unwrap().error.as_deref(), Some("boom"));
        }

        // `session/new` names its session in the result.
        log.sent_request(8, "session/new", &json!({ "cwd": "/w" }), "{}");
        let reply = json!({ "id": 8, "result": { "sessionId": "s2" } });
        log.received(&reply, &reply.to_string());
        assert_eq!(log.events()[1].session.as_deref(), Some("s2"));

        // The agent asks, we answer.
        let ask = json!({ "id": "p1", "method": "session/request_permission",
                          "params": { "sessionId": "s2" } });
        log.received(&ask, &ask.to_string());
        assert_eq!(log.events()[2].kind, EventKind::AgentRequest);
        log.sent_response(&json!("p1"), None, "{}");
        assert!(log.events()[2].reply.is_some());
    }

    #[test]
    fn merges_streamed_chunks() {
        let log = recording();
        let chunk = |text: &str| {
            json!({ "method": "session/update", "params": { "sessionId": "s",
                "update": { "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": text } } } })
        };
        for t in ["Hel", "lo", "!"] {
            let m = chunk(t);
            log.received(&m, &m.to_string());
        }
        let tool = json!({ "method": "session/update", "params": { "sessionId": "s",
            "update": { "sessionUpdate": "tool_call", "title": "Read a.rs", "status": "pending" } } });
        log.received(&tool, &tool.to_string());
        let m = chunk("next");
        log.received(&m, &m.to_string());
        let events = log.events();
        assert_eq!(events.len(), 3);
        assert_eq!(
            (events[0].merged, events[0].preview.as_str()),
            (3, "Hello!")
        );
        assert_eq!(events[1].preview, "Read a.rs · pending");
        assert_eq!(events[2].merged, 1);
    }

    #[test]
    fn records_only_when_enabled_and_caps_bodies() {
        let log = WireLog::default();
        log.sent_notification("session/cancel", &json!({}), "{}");
        assert!(log.events().is_empty());
        log.set_enabled(true);
        let big = "x".repeat(CAP_BYTES + 10);
        log.sent_notification("session/cancel", &json!({}), &big);
        {
            let events = log.events();
            let body = &events[0].body;
            assert!(body.cut() && body.text.len() == CAP_BYTES);
        }
        log.clear();
        assert!(log.events().is_empty());
    }
}
