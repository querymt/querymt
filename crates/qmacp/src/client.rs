use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use agent_client_protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, UntypedMessage,
    schema::ProtocolVersion, schema::v1 as acp,
};
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{
    Connector, connect_async_tls_with_config,
    tungstenite::{Message, client::IntoClientRequest},
};

use crate::events;
use crate::policy::{self, PermissionPolicy};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A JSON-RPC error returned by the server, distinct from a lost connection.
#[derive(Debug)]
pub struct RpcResponseError(Value);

impl RpcResponseError {
    /// Whether the protocol error establishes that the request was rejected.
    pub fn is_rejection(&self) -> bool {
        matches!(
            self.0.get("code").and_then(Value::as_i64),
            Some(-32700 | -32600 | -32601 | -32602)
        )
    }
}

impl std::fmt::Display for RpcResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for RpcResponseError {}

#[derive(Debug)]
enum RpcFailure {
    Response(Value),
    Transport(String),
}

type RpcResult = Result<Value, RpcFailure>;
type PendingRequests = Arc<Mutex<HashMap<i64, oneshot::Sender<RpcResult>>>>;
type Outbound = (Message, Option<oneshot::Sender<Result<(), String>>>);

#[derive(Default)]
struct AssistantText {
    text: String,
    truncated: bool,
    limit: Option<usize>,
}

impl AssistantText {
    fn observe(&mut self, event: &Value) {
        if self.truncated {
            return;
        }
        events::observe_assistant_text(&mut self.text, event);
        if let Some(limit) = self.limit
            && self.text.len() > limit
        {
            let mut end = limit;
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.truncate(end);
            self.truncated = true;
        }
    }
}

#[derive(Clone)]
enum EventSink {
    Cli(bool),
    Channel(mpsc::Sender<Value>),
}

impl EventSink {
    fn enabled(&self) -> bool {
        !matches!(self, Self::Cli(false))
    }

    async fn emit(&self, event: Value) {
        match self {
            Self::Cli(true) => {
                let _ = crate::output::write_event(&event);
            }
            Self::Cli(false) => {}
            Self::Channel(sender) => {
                let _ = sender.send(event).await;
            }
        }
    }
}

/// Native ACP WebSocket client. Dropping it closes only its connection, not the remote session.
pub struct AcpClient {
    tx: mpsc::UnboundedSender<Outbound>,
    pending: PendingRequests,
    next_id: AtomicI64,
    initialized: Mutex<Option<acp::InitializeResponse>>,
    assistant_text: Arc<Mutex<AssistantText>>,
    event_sink: EventSink,
    tasks: [tokio::task::JoinHandle<()>; 2],
}

impl AcpClient {
    /// Connect using the CLI event sink. With stream_events=true, events go to stdout.
    pub async fn connect(
        url: &str,
        endpoint: &str,
        permission: PermissionPolicy,
        stream_events: bool,
    ) -> Result<Self> {
        Self::connect_inner(
            url,
            endpoint,
            permission,
            EventSink::Cli(stream_events),
            None,
        )
        .await
    }

    /// Connect without writing events to stdout. The caller must drain the bounded receiver.
    /// Assistant text is capped at 1 MiB; inspect text_truncated() after an operation.
    pub async fn connect_with_events(
        url: &str,
        endpoint: &str,
        permission: PermissionPolicy,
        event_tx: mpsc::Sender<Value>,
    ) -> Result<Self> {
        Self::connect_inner(
            url,
            endpoint,
            permission,
            EventSink::Channel(event_tx),
            Some(1024 * 1024),
        )
        .await
    }

    async fn connect_inner(
        url: &str,
        endpoint: &str,
        permission: PermissionPolicy,
        event_sink: EventSink,
        text_limit: Option<usize>,
    ) -> Result<Self> {
        let connect = async {
            let request = url.into_client_request()?;
            let connector = match request.uri().scheme_str() {
                Some("wss") => Some(native_tls_connector()?),
                _ => None,
            };
            Ok::<_, anyhow::Error>(
                connect_async_tls_with_config(request, None, false, connector).await?,
            )
        };
        let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "connect {endpoint} timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("connect {endpoint}"))?;
        let (mut write, mut read) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));

        let pending_write = pending.clone();
        let writer = tokio::spawn(async move {
            while let Some((message, ack)) = rx.recv().await {
                let result = write
                    .send(message)
                    .await
                    .map_err(|error| format!("WebSocket write failed: {error}"));
                if let Some(ack) = ack {
                    let _ = ack.send(result.clone());
                }
                if let Err(error) = result {
                    fail_pending(&pending_write, error).await;
                    return;
                }
            }
            fail_pending(&pending_write, "WebSocket writer stopped").await;
        });

        let pending_read = pending.clone();
        let tx_read = tx.clone();
        let assistant_text = Arc::new(Mutex::new(AssistantText {
            limit: text_limit,
            ..Default::default()
        }));
        let assistant_read = assistant_text.clone();
        let events_read = event_sink.clone();
        let reader = tokio::spawn(async move {
            while let Some(message) = read.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if let Err(err) = handle_inbound(
                            &pending_read,
                            &tx_read,
                            &assistant_read,
                            text.as_ref(),
                            permission,
                            &events_read,
                        )
                        .await
                        {
                            eprintln!("inbound error: {err:#}");
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Ping(data)) => {
                        if tx_read.send((Message::Pong(data), None)).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        eprintln!("WebSocket read failed: {err}");
                        break;
                    }
                }
            }
            fail_pending(&pending_read, "WebSocket connection closed").await;
        });

        Ok(Self {
            tx,
            pending,
            next_id: AtomicI64::new(1),
            initialized: Mutex::new(None),
            assistant_text,
            event_sink,
            tasks: [writer, reader],
        })
    }

    pub async fn emit(&self, event: Value) {
        self.event_sink.emit(event).await;
    }

    pub async fn clear_assistant_text(&self) {
        let mut buffer = self.assistant_text.lock().await;
        buffer.text.clear();
        buffer.truncated = false;
    }

    pub async fn snapshot_assistant_text(&self) -> String {
        self.assistant_text.lock().await.text.clone()
    }

    pub async fn text_truncated(&self) -> bool {
        self.assistant_text.lock().await.truncated
    }

    pub async fn take_assistant_text(&self) -> String {
        let mut text = self.assistant_text.lock().await;
        std::mem::take(&mut text.text)
    }

    pub async fn initialize(&self) -> Result<acp::InitializeResponse> {
        if let Some(cached) = self.initialized.lock().await.clone() {
            return Ok(cached);
        }
        let response = self
            .request(
                acp::InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(client_capabilities())
                    .client_info(acp::Implementation::new("qmacp", env!("CARGO_PKG_VERSION"))),
            )
            .await?;
        *self.initialized.lock().await = Some(response.clone());
        Ok(response)
    }

    pub async fn extension(&self, method: &str, params: Value) -> Result<Value> {
        let wire = if method.starts_with('_') {
            method.to_string()
        } else {
            format!("_{method}")
        };
        self.request(UntypedMessage::new(&wire, params)?).await
    }

    pub async fn new_session(
        &self,
        cwd: impl Into<std::path::PathBuf>,
        profile: Option<&str>,
    ) -> Result<(acp::NewSessionResponse, Value)> {
        let mut request = acp::NewSessionRequest::new(cwd.into());
        if let Some(profile_id) = profile {
            request = request.meta(crate::session::profile_meta(profile_id));
        }
        self.request_json(request).await
    }

    pub async fn list_sessions(
        &self,
        cwd: Option<std::path::PathBuf>,
        cursor: Option<String>,
        session_filters: Option<Map<String, Value>>,
    ) -> Result<acp::ListSessionsResponse> {
        self.request(build_list_sessions_request(cwd, cursor, session_filters))
            .await
    }

    pub async fn load_session(
        &self,
        session_id: String,
        cwd: std::path::PathBuf,
    ) -> Result<(acp::LoadSessionResponse, Value)> {
        self.request_json(acp::LoadSessionRequest::new(session_id, cwd))
            .await
    }

    pub async fn resume_session(
        &self,
        session_id: String,
        cwd: std::path::PathBuf,
    ) -> Result<(acp::ResumeSessionResponse, Value)> {
        self.request_json(acp::ResumeSessionRequest::new(session_id, cwd))
            .await
    }

    pub async fn close_session(&self, session_id: String) -> Result<acp::CloseSessionResponse> {
        self.request(acp::CloseSessionRequest::new(session_id))
            .await
    }

    pub async fn set_mode(
        &self,
        session_id: String,
        mode: String,
    ) -> Result<acp::SetSessionModeResponse> {
        self.request(acp::SetSessionModeRequest::new(session_id, mode))
            .await
    }

    pub async fn set_config(
        &self,
        session_id: String,
        config_id: &str,
        value: &str,
    ) -> Result<(acp::SetSessionConfigOptionResponse, Value)> {
        self.request_json(acp::SetSessionConfigOptionRequest::new(
            session_id,
            config_id.to_string(),
            value,
        ))
        .await
    }

    pub async fn prompt(
        &self,
        session_id: String,
        text: String,
        timeout: Option<Duration>,
    ) -> Result<acp::PromptResponse> {
        self.request_with_timeout(
            acp::PromptRequest::new(
                session_id,
                vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
            ),
            timeout.unwrap_or(DEFAULT_PROMPT_TIMEOUT),
        )
        .await
    }

    pub async fn cancel(&self, session_id: String) -> Result<()> {
        self.notify(acp::CancelNotification::new(session_id)).await
    }

    async fn request<R>(&self, request: R) -> Result<R::Response>
    where
        R: JsonRpcRequest + Send + Sync + 'static,
        R::Response: Send + 'static,
    {
        Ok(self.request_with_json(request, REQUEST_TIMEOUT).await?.0)
    }

    pub async fn request_json<R>(&self, request: R) -> Result<(R::Response, Value)>
    where
        R: JsonRpcRequest + Send + Sync + 'static,
        R::Response: Send + 'static,
    {
        self.request_with_json(request, REQUEST_TIMEOUT).await
    }

    async fn request_with_timeout<R>(&self, request: R, timeout: Duration) -> Result<R::Response>
    where
        R: JsonRpcRequest + Send + Sync + 'static,
        R::Response: Send + 'static,
    {
        Ok(self.request_with_json(request, timeout).await?.0)
    }

    async fn request_with_json<R>(
        &self,
        request: R,
        timeout: Duration,
    ) -> Result<(R::Response, Value)>
    where
        R: JsonRpcRequest + Send + Sync + 'static,
        R::Response: Send + 'static,
    {
        let message = request.to_untyped_message()?;
        let method = message.method.clone();
        let result = self.request_raw(&method, message.params, timeout).await?;
        let typed = R::Response::from_value(&method, result.clone())?;
        Ok((typed, result))
    }

    async fn notify<N>(&self, notification: N) -> Result<()>
    where
        N: JsonRpcNotification + Send + Sync + 'static,
    {
        let message = notification.to_untyped_message()?;
        let envelope = json!({
            "jsonrpc": "2.0",
            "method": message.method,
            "params": message.params,
        });
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send((Message::Text(envelope.to_string().into()), Some(ack_tx)))
            .context("WebSocket writer is closed")?;
        // Flush notifications before this per-call client is dropped.
        tokio::time::timeout(REQUEST_TIMEOUT, ack_rx)
            .await
            .context("WebSocket notification write timed out")?
            .context("WebSocket writer stopped before flushing notification")?
            .map_err(anyhow::Error::msg)
    }

    async fn request_raw(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if self
            .tx
            .send((Message::Text(envelope.to_string().into()), None))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            bail!("WebSocket writer is closed");
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result.map_err(|error| match error {
                RpcFailure::Response(value) => anyhow::Error::new(RpcResponseError(value)),
                RpcFailure::Transport(message) => anyhow::Error::msg(message),
            }),
            Ok(Err(_)) => bail!("request {method} was dropped"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("request {method} timed out after {}s", timeout.as_secs());
            }
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // A tool call owns its connection; cancellation must not leave reader/writer tasks alive.
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn native_tls_connector() -> Result<Connector> {
    let certificates = rustls_native_certs::load_native_certs();
    if certificates.certs.is_empty() {
        bail!(
            "no native root CA certificates found (errors: {:?})",
            certificates.errors
        );
    }
    let mut roots = rustls::RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(certificates.certs);
    if added == 0 {
        bail!("no usable native root CA certificates found");
    }
    // Cargo features are additive. Select ring for this connection explicitly,
    // without relying on or changing the embedding application's global provider.
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("configure TLS protocol versions")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}

fn build_list_sessions_request(
    cwd: Option<std::path::PathBuf>,
    cursor: Option<String>,
    session_filters: Option<Map<String, Value>>,
) -> acp::ListSessionsRequest {
    let mut request = acp::ListSessionsRequest::new();
    if let Some(cwd) = cwd {
        request = request.cwd(cwd);
    }
    if let Some(cursor) = cursor {
        request = request.cursor(cursor);
    }
    if let Some(meta) = session_filters {
        request = request.meta(meta);
    }
    request
}

async fn fail_pending(pending: &PendingRequests, message: impl Into<String>) {
    let message = message.into();
    for (_, waiter) in pending.lock().await.drain() {
        let _ = waiter.send(Err(RpcFailure::Transport(message.clone())));
    }
}

async fn handle_inbound(
    pending: &PendingRequests,
    tx: &mpsc::UnboundedSender<Outbound>,
    assistant_text: &Mutex<AssistantText>,
    text: &str,
    permission: PermissionPolicy,
    event_sink: &EventSink,
) -> Result<()> {
    let value: Value = serde_json::from_str(text).context("inbound JSON")?;
    if value.get("method").is_none() {
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            return Ok(());
        };
        let result = if let Some(error) = value.get("error") {
            Err(RpcFailure::Response(error.clone()))
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        if let Some(waiter) = pending.lock().await.remove(&id) {
            let _ = waiter.send(result);
        }
        return Ok(());
    }

    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    if event_sink.enabled() && method == "session/update" {
        let session_id = params.get("sessionId").cloned().unwrap_or(Value::Null);
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        if let Some(event) = events::compact_session_update(&session_id, &update) {
            assistant_text.lock().await.observe(&event);
            event_sink.emit(event).await;
        }
    } else if matches!(
        method.as_str(),
        "querymt/session/inputState" | "_querymt/session/inputState"
    ) && let Some(event) = events::compact_input_state(&params)
    {
        event_sink.emit(event).await;
    }

    if matches!(
        method.as_str(),
        "querymt/session/delegationUpdate" | "_querymt/session/delegationUpdate"
    ) && let Some(event) = events::compact_delegation_update(&params)
    {
        event_sink.emit(event).await;
    }

    if let Some(id) = value.get("id").cloned() {
        let (reply, event) = inbound_request_reply(id, &method, &params, permission)?;
        if let Some(event) = event {
            event_sink.emit(event).await;
        }
        tx.send((Message::Text(reply.to_string().into()), None))
            .context("reply to inbound request")?;
    }
    Ok(())
}

fn inbound_request_reply(
    id: Value,
    method: &str,
    params: &Value,
    permission: PermissionPolicy,
) -> Result<(Value, Option<Value>)> {
    if acp::RequestPermissionRequest::matches_method(method) {
        let request = acp::RequestPermissionRequest::parse_message(method, params)?;
        let (response, event) = policy::permission_response(&request, permission);
        let result = response.into_json(method)?;
        return Ok((
            json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Some(event),
        ));
    }
    if acp::CreateElicitationRequest::matches_method(method) {
        let request = acp::CreateElicitationRequest::parse_message(method, params)?;
        let result = policy::elicitation_cancel_response().into_json(method)?;
        return Ok((
            json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Some(policy::elicitation_event(&id, &request)),
        ));
    }

    Ok((
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("qmacp does not support {method}") },
        }),
        None,
    ))
}

fn client_capabilities() -> acp::ClientCapabilities {
    acp::ClientCapabilities::new().terminal(false).elicitation(
        acp::ElicitationCapabilities::new().form(acp::ElicitationFormCapabilities::new()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn cancel_write_deadline_is_classified_as_timeout() {
        use crate::error::{CliError, ExitCode};

        let (tx, mut rx) = mpsc::unbounded_channel::<Outbound>();
        let writer = tokio::spawn(async move {
            let (message, ack) = rx.recv().await.unwrap();
            let message: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(message["method"], "session/cancel");
            let ack = ack.unwrap();
            // Keep the acknowledgment alive, simulating a writer stalled in send().
            std::future::pending::<()>().await;
            drop(ack);
        });
        let client = AcpClient {
            tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicI64::new(1),
            initialized: Mutex::new(None),
            assistant_text: Arc::new(Mutex::new(AssistantText::default())),
            event_sink: EventSink::Cli(false),
            tasks: [writer, tokio::spawn(std::future::pending())],
        };
        let started = tokio::time::Instant::now();
        let error = client.cancel("fixture-session".into()).await.unwrap_err();
        assert_eq!(started.elapsed(), REQUEST_TIMEOUT);
        assert!(error.is::<tokio::time::error::Elapsed>());
        let error = CliError::from_notification(error);
        assert_eq!(error.exit_code(), ExitCode::Timeout);
        assert_eq!(error.exit_code().as_i32(), 4);
        assert!(
            error
                .to_string()
                .contains("WebSocket notification write timed out")
        );
    }

    #[tokio::test]
    async fn secure_websocket_starts_a_tls_handshake_without_panicking() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("wss://{}/acp/ws", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut first_byte = [0];
            tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut first_byte))
                .await
                .unwrap()
                .unwrap();
            first_byte[0]
        });
        let (tx, _rx) = mpsc::channel(1);
        let result =
            AcpClient::connect_with_events(&url, "fixture", PermissionPolicy::Cancel, tx).await;
        assert!(result.is_err(), "the fixture closes before completing TLS");
        assert_eq!(
            peer.await.unwrap(),
            22,
            "the first TLS record must be a handshake"
        );
    }

    #[test]
    fn bounded_assistant_text_preserves_a_utf8_prefix_and_reports_truncation() {
        let mut buffer = AssistantText {
            limit: Some(5),
            ..Default::default()
        };
        buffer.observe(&json!({"type": "text", "role": "assistant", "text": "\u{e9}\u{e9}\u{e9}"}));
        assert_eq!(buffer.text, "\u{e9}\u{e9}");
        assert!(buffer.truncated);
        buffer.observe(&json!({"type": "text", "role": "assistant", "text": "later"}));
        assert_eq!(buffer.text, "\u{e9}\u{e9}");
    }

    #[test]
    fn list_sessions_request_repeats_filters_on_each_cursor_and_omits_empty_metadata() {
        let filters = serde_json::from_value::<Map<String, Value>>(json!({
            "querymt": {
                "sessionFilters": {
                    "resolution": "open",
                    "archive": "excluded",
                    "pinned": false
                }
            }
        }))
        .unwrap();

        for cursor in [None, Some("next-page".to_string())] {
            let request = build_list_sessions_request(None, cursor.clone(), Some(filters.clone()));
            let params = request.to_untyped_message().unwrap().params;
            assert_eq!(
                params["_meta"]["querymt"]["sessionFilters"]["resolution"],
                "open"
            );
            assert_eq!(
                params["_meta"]["querymt"]["sessionFilters"]["archive"],
                "excluded"
            );
            assert_eq!(
                params["_meta"]["querymt"]["sessionFilters"]["pinned"],
                false
            );
            assert_eq!(
                params.get("cursor").and_then(Value::as_str),
                cursor.as_deref()
            );
        }

        let request = build_list_sessions_request(None, None, None);
        let params = request.to_untyped_message().unwrap().params;
        assert!(params.get("_meta").is_none());
    }

    #[test]
    fn permission_reply_uses_allow_once_by_default() {
        let request = acp::RequestPermissionRequest::new(
            "session-1",
            acp::ToolCallUpdate::new("tool-1", acp::ToolCallUpdateFields::new()),
            vec![
                acp::PermissionOption::new(
                    "reject",
                    "Reject",
                    acp::PermissionOptionKind::RejectOnce,
                ),
                acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
            ],
        );
        let params = request.to_untyped_message().unwrap().params;
        let (reply, event) = inbound_request_reply(
            json!("querymt:conn:1"),
            "session/request_permission",
            &params,
            PermissionPolicy::AllowOnce,
        )
        .unwrap();
        assert_eq!(reply["id"], "querymt:conn:1");
        assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
        assert_eq!(event.unwrap()["optionId"], "allow");
    }

    #[test]
    fn elicitation_reply_cancels_and_keeps_string_id() {
        let request = acp::CreateElicitationRequest::new(
            acp::ElicitationFormMode::new(
                acp::ElicitationSessionScope::new("session-1"),
                acp::ElicitationSchema::new().string("selection", true),
            ),
            "choose",
        );
        let params = request.to_untyped_message().unwrap().params;
        let (reply, event) = inbound_request_reply(
            json!("elic-1"),
            "elicitation/create",
            &params,
            PermissionPolicy::AllowOnce,
        )
        .unwrap();
        assert_eq!(reply["id"], "elic-1");
        assert_eq!(reply["result"]["action"], "cancel");
        assert_eq!(event.unwrap()["cancelled"], true);
    }
}
