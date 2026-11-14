//! Minimal JSON-RPC 2.0 client on top of a [`crate::transport::Connection`].
//! The owner runs the read loop and feeds every inbound line to [`RpcClient::handle`].

use std::{cell::{Cell, RefCell}, collections::HashMap, rc::Rc};

use async_channel::Sender;
use futures::channel::oneshot;
use serde_json::{Value, json};

pub enum Incoming {
    Notification { method: String, params: Value },
    Request { id: Value, method: String, params: Value },
}

type Pending = oneshot::Sender<Result<Value, String>>;

pub struct RpcClient {
    outgoing: Sender<String>,
    next_id: Cell<u64>,
    pending: RefCell<HashMap<u64, Pending>>,
}

impl RpcClient {
    pub fn new(outgoing: Sender<String>) -> Rc<Self> {
        Rc::new(Self { outgoing, next_id: Cell::new(1), pending: RefCell::new(HashMap::new()) })
    }

    pub fn request(&self, method: &str, params: Value) -> impl Future<Output = Result<Value, String>> + 'static {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.borrow_mut().insert(id, tx);
        let text = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let outgoing = self.outgoing.clone();
        async move {
            outgoing.send(text).await.map_err(|_| "Connection closed.".to_string())?;
            rx.await.map_err(|_| "Connection closed.".to_string())?
        }
    }

    /// Fire-and-forget JSON-RPC notification.
    pub fn notify(&self, method: &str, params: Value) {
        let text = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        let _ = self.outgoing.try_send(text);
    }

    pub fn respond(&self, id: Value, result: Result<Value, (i64, String)>) {
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        };
        let _ = self.outgoing.try_send(message.to_string());
    }

    /// Route one inbound line: resolves responses, returns notifications and requests.
    pub fn handle(&self, text: &str) -> Option<Incoming> {
        #[cfg(not(target_family = "wasm"))]
        if std::env::var_os("QMT_DEBUG").is_some() {
            eprintln!("[rpc <-] {}", text.chars().take(200).collect::<String>());
        }
        let message: Value = serde_json::from_str(text).ok()?;
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            return Some(match message.get("id") {
                Some(id) if !id.is_null() => Incoming::Request { id: id.clone(), method: method.into(), params },
                _ => Incoming::Notification { method: method.into(), params },
            });
        }
        let id = message.get("id").and_then(Value::as_u64)?;
        let pending = self.pending.borrow_mut().remove(&id)?;
        let result = match message.get("error") {
            Some(error) => {
                let mut text = error.get("message").and_then(Value::as_str).unwrap_or("Request failed").to_string();
                if let Some(detail) = error.pointer("/data/error").and_then(Value::as_str) {
                    text = format!("{text}: {detail}");
                }
                // Machine-readable codes (e.g. `history_changed`) so callers can branch on them.
                if let Some(code) = error.pointer("/data/code").and_then(Value::as_str) {
                    text = format!("{text} [{code}]");
                }
                Err(text)
            }
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = pending.send(result);
        None
    }

    pub fn fail_all(&self, reason: &str) {
        for (_, pending) in self.pending.borrow_mut().drain() {
            let _ = pending.send(Err(reason.to_string()));
        }
    }

    /// Tear down the transport.
    pub fn close(&self) {
        self.outgoing.close();
        self.fail_all("Disconnected.");
    }
}
