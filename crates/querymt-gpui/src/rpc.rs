//! Minimal JSON-RPC 2.0 client on top of a [`crate::transport::Connection`].
//! The owner runs the read loop and feeds every inbound line to [`RpcClient::handle`].

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use async_channel::Sender;
use futures::channel::oneshot;
use serde_json::{Value, json};

use crate::rpc_log::WireLog;

pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// A failed request: the short text callers show, plus the raw JSON-RPC
/// `error` object (code, message, data) for diagnostics.
#[derive(Clone, Debug)]
pub struct RpcError {
    message: String,
    raw: Option<Value>,
}

impl RpcError {
    fn local(message: &str) -> Self {
        Self {
            message: message.to_string(),
            raw: None,
        }
    }

    /// The `error` object of the response; `None` for transport failures.
    pub fn raw(&self) -> Option<&Value> {
        self.raw.as_ref()
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A request awaiting its response, and the `rpc.request` span timing it.
type Pending = (oneshot::Sender<Result<Value, RpcError>>, tracing::Span);

pub struct RpcClient {
    outgoing: Sender<String>,
    next_id: Cell<u64>,
    pending: RefCell<HashMap<u64, Pending>>,
    /// Records the traffic for the debug events viewer (Developer mode).
    wire: RefCell<Option<Rc<WireLog>>>,
}

impl RpcClient {
    pub fn new(outgoing: Sender<String>) -> Rc<Self> {
        Rc::new(Self {
            outgoing,
            next_id: Cell::new(1),
            pending: RefCell::new(HashMap::new()),
            wire: RefCell::new(None),
        })
    }

    /// Record every message of this connection in `wire`.
    pub fn set_wire_log(&self, wire: Rc<WireLog>) {
        *self.wire.borrow_mut() = Some(wire);
    }

    /// The recorder, when one is set and recording.
    fn recording(&self) -> Option<Rc<WireLog>> {
        self.wire.borrow().clone().filter(|w| w.enabled())
    }

    pub fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, String>> + 'static {
        let request = self.request_detailed(method, params);
        async move { request.await.map_err(|e| e.to_string()) }
    }

    /// [`Self::request`], keeping the raw JSON-RPC error for diagnostics.
    pub fn request_detailed(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value, RpcError>> + 'static {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = oneshot::channel();
        // Round trip from send to the response being parsed on the UI thread.
        let span = tracing::info_span!(
            "rpc.request",
            rpc.method = method,
            rpc.id = id,
            rpc.request_bytes = tracing::field::Empty,
            rpc.response_bytes = tracing::field::Empty,
            rpc.parse_ms = tracing::field::Empty,
            rpc.error = tracing::field::Empty,
        );
        self.pending.borrow_mut().insert(id, (tx, span.clone()));
        let text =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": &params }).to_string();
        span.record("rpc.request_bytes", text.len());
        if let Some(wire) = self.recording() {
            wire.sent_request(id, method, &params, &text);
        }
        let outgoing = self.outgoing.clone();
        let request = async move {
            outgoing
                .send(text)
                .await
                .map_err(|_| RpcError::local("Connection closed."))?;
            rx.await
                .map_err(|_| RpcError::local("Connection closed."))?
        };
        tracing::Instrument::instrument(request, span)
    }

    /// Fire-and-forget JSON-RPC notification.
    pub fn notify(&self, method: &str, params: Value) {
        let text = json!({ "jsonrpc": "2.0", "method": method, "params": &params }).to_string();
        if let Some(wire) = self.recording() {
            wire.sent_notification(method, &params, &text);
        }
        let _ = self.outgoing.try_send(text);
    }

    pub fn respond(&self, id: Value, result: Result<Value, (i64, String)>) {
        let error = result.as_ref().err().map(|(_, m)| m.clone());
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": &id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": &id, "error": { "code": code, "message": message } })
            }
        };
        let text = message.to_string();
        if let Some(wire) = self.recording() {
            wire.sent_response(&id, error.as_deref(), &text);
        }
        let _ = self.outgoing.try_send(text);
    }

    /// Route one inbound line: resolves responses, returns notifications and requests.
    pub fn handle(&self, text: &str) -> Option<Incoming> {
        #[cfg(not(target_family = "wasm"))]
        if std::env::var_os("QMT_DEBUG").is_some() {
            eprintln!("[rpc <-] {}", text.chars().take(200).collect::<String>());
        }
        let parse_started = crate::util::now_ms();
        let message: Value = serde_json::from_str(text).ok()?;
        let parse_ms = crate::util::now_ms() - parse_started;
        if let Some(wire) = self.recording() {
            wire.received(&message, text);
        }
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            return Some(match message.get("id") {
                Some(id) if !id.is_null() => Incoming::Request {
                    id: id.clone(),
                    method: method.into(),
                    params,
                },
                _ => Incoming::Notification {
                    method: method.into(),
                    params,
                },
            });
        }
        let id = message.get("id").and_then(Value::as_u64)?;
        let (pending, span) = self.pending.borrow_mut().remove(&id)?;
        span.record("rpc.response_bytes", text.len());
        span.record("rpc.parse_ms", parse_ms);
        let result = match message.get("error") {
            Some(error) => {
                let mut text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Request failed")
                    .to_string();
                if let Some(detail) = error.pointer("/data/error").and_then(Value::as_str) {
                    text = format!("{text}: {detail}");
                }
                // Machine-readable codes (e.g. `history_changed`) so callers can branch on them.
                if let Some(code) = error.pointer("/data/code").and_then(Value::as_str) {
                    text = format!("{text} [{code}]");
                }
                Err(RpcError {
                    message: text,
                    raw: Some(error.clone()),
                })
            }
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        if let Err(err) = &result {
            span.record("rpc.error", err.message.as_str());
        }
        let _ = pending.send(result);
        None
    }

    pub fn fail_all(&self, reason: &str) {
        for (_, (pending, _)) in self.pending.borrow_mut().drain() {
            let _ = pending.send(Err(RpcError::local(reason)));
        }
    }

    /// Tear down the transport.
    pub fn close(&self) {
        self.outgoing.close();
        self.fail_all("Disconnected.");
    }
}
