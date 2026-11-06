//! In-process MCP server for tests.
//!
//! Spins up an rmcp server over `tokio::io::duplex` and connects the agent's
//! real client path ([`serve_mcp_client`] with [`McpClientHandler`]) to it.
//! The server's tools are selected by name:
//!
//! | tool             | behaviour                                                          |
//! |------------------|--------------------------------------------------------------------|
//! | `echo`           | returns `args.text`                                                |
//! | `error`          | `isError: true` with "quota exceeded"                              |
//! | `structured`     | empty content, `structuredContent: {"temp": 21}`                   |
//! | `slow`           | blocks until the request is cancelled                              |
//! | `progress`       | sends `args.count` progress notifications, then holds `args.hold_ms` |
//! | `task`           | answers with a task whose states come from [`TaskScript`]          |
//! | `input_required` | answers with an MRTR `input_required` result                       |

use crate::agent::core::McpToolState;
use crate::agent::protocol::serve_mcp_client;
use crate::elicitation::McpClientHandler;
use parking_lot::Mutex;
use querymt::mcp::adapter::McpToolAdapter;
use querymt::tool_decorator::CallFunctionTool;
use rmcp::{
    RoleClient, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        CancelledNotificationParam, ClientCapabilities, ContentBlock, CreateTaskResult,
        DetailedTask, ErrorData, GetTaskParams, GetTaskResult, InputRequiredResult,
        ListToolsResult, NumberOrString, PaginatedRequestParams, ProgressNotificationParam,
        ServerCapabilities, ServerConfig, Task, TaskPayload, TaskStatus,
        TaskStatusNotificationParams, Tool,
    },
    service::{Peer, RequestContext, RunningService, serve_server},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

/// What the server observed.
#[derive(Default)]
pub struct ServerLog {
    /// `(tool name, request _meta)` of every `tools/call`.
    pub calls: Mutex<Vec<(String, Value)>>,
    pub cancelled: Mutex<Vec<CancelledNotificationParam>>,
    pub task_cancels: Mutex<Vec<String>>,
    /// Did the client declare the tasks extension during `initialize`?
    pub client_supports_tasks: Mutex<Option<bool>>,
    pub list_tools_calls: AtomicUsize,
    /// Signalled whenever a `slow` / `task` call starts blocking.
    pub started: Notify,
    /// Signalled on every `notifications/cancelled` and `tasks/cancel`.
    pub cancel_seen: Notify,
}

impl ServerLog {
    /// `_meta.progressToken` of the n-th call, if the client sent one.
    pub fn progress_token_of_call(&self, n: usize) -> Option<Value> {
        self.calls.lock().get(n)?.1.get("progressToken").cloned()
    }
}

/// States returned by successive `tasks/get` calls; the last one repeats.
#[derive(Clone)]
pub struct TaskScript {
    pub states: Vec<TaskPayload>,
    pub poll_interval_ms: Option<u64>,
    pub status_message: Option<String>,
}

impl Default for TaskScript {
    fn default() -> Self {
        Self {
            states: vec![TaskPayload::Working],
            poll_interval_ms: Some(10),
            status_message: None,
        }
    }
}

#[derive(Default)]
struct Shared {
    tools: Mutex<Vec<Tool>>,
    task_script: Mutex<TaskScript>,
    task_polls: AtomicUsize,
    ttl_ms: Mutex<u64>,
}

#[derive(Clone)]
struct HarnessServer {
    shared: Arc<Shared>,
    log: Arc<ServerLog>,
}

fn tool(name: &'static str) -> Tool {
    Tool::new(
        name,
        "harness tool",
        Arc::new(rmcp::model::object(json!({"type": "object"}))),
    )
}

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(s)]
}

fn task_seed(script: &TaskScript, status: TaskStatus) -> Task {
    let mut task = Task::new("t1", status, "2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z");
    if let Some(ms) = script.poll_interval_ms {
        task = task.with_poll_interval_ms(ms);
    }
    if let Some(m) = &script.status_message {
        task = task.with_status_message(m.clone());
    }
    task
}

impl ServerHandler for HarnessServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_tasks()
                .build(),
        );
        // MRTR (`input_required`) results need the 2026-07-28 protocol.
        info.protocol_version = rmcp::model::ProtocolVersion::V_2026_07_28;
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.log.list_tools_calls.fetch_add(1, Ordering::SeqCst);
        let tools = self.shared.tools.lock().clone();
        Ok(ListToolsResult::with_all_items(tools).with_ttl_ms(*self.shared.ttl_ms.lock()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let meta = serde_json::to_value(&context.meta).unwrap_or(Value::Null);
        self.log.calls.lock().push((request.name.to_string(), meta));
        let args = request.arguments.clone().unwrap_or_default();
        let arg_u64 = |k: &str| args.get(k).and_then(Value::as_u64).unwrap_or(0);
        match request.name.as_ref() {
            "echo" => {
                let t = args.get("text").and_then(Value::as_str).unwrap_or("");
                Ok(CallToolResult::success(text(t)).into())
            }
            "error" => Ok(CallToolResult::error(text("quota exceeded")).into()),
            "structured" => {
                let mut r = CallToolResult::success(Vec::new());
                r.structured_content = Some(json!({"temp": 21}));
                Ok(r.into())
            }
            "slow" => {
                self.log.started.notify_one();
                context.ct.cancelled().await;
                Err(ErrorData::internal_error("cancelled", None))
            }
            "progress" => {
                let token = context.meta.get_progress_token();
                for i in 1..=arg_u64("count") {
                    if let Some(token) = token.clone() {
                        let note = ProgressNotificationParam::new(token, i as f64)
                            .with_total(arg_u64("total").max(i) as f64)
                            .with_message(format!("step {i}"));
                        let _ = context.peer.notify_progress(note).await;
                    }
                    if arg_u64("gap_ms") > 0 {
                        tokio::time::sleep(Duration::from_millis(arg_u64("gap_ms"))).await;
                    }
                }
                self.log.started.notify_one();
                tokio::time::sleep(Duration::from_millis(arg_u64("hold_ms"))).await;
                Ok(CallToolResult::success(text("done")).into())
            }
            "task" => {
                self.log.started.notify_one();
                let script = self.shared.task_script.lock().clone();
                self.shared.task_polls.store(0, Ordering::SeqCst);
                Ok(CreateTaskResult::new(task_seed(&script, TaskStatus::Working)).into())
            }
            "input_required" => Ok(InputRequiredResult::new(None, Some("state".into())).into()),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }

    async fn get_task(
        &self,
        _request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, ErrorData> {
        let script = self.shared.task_script.lock().clone();
        let n = self.shared.task_polls.fetch_add(1, Ordering::SeqCst);
        let payload = script.states[n.min(script.states.len() - 1)].clone();
        let task = task_seed(&script, payload.status());
        Ok(GetTaskResult::new(DetailedTask::new(task, payload)))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.log.task_cancels.lock().push(request.task_id);
        self.log.cancel_seen.notify_one();
        Ok(())
    }

    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: rmcp::service::NotificationContext<RoleServer>,
    ) {
        self.log.cancelled.lock().push(notification);
        self.log.cancel_seen.notify_one();
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<RoleServer>) {
        let supports = context
            .peer
            .peer_info()
            .map(|info| ClientCapabilities::supports_tasks(&info.capabilities));
        *self.log.client_supports_tasks.lock() = supports;
    }
}

/// Options for [`McpHarness::start`].
#[derive(Clone)]
pub struct HarnessOptions {
    pub tasks: bool,
    pub cache: bool,
    /// `ttlMs` hint on `tools/list` responses.
    pub ttl_ms: u64,
    pub tools: Vec<&'static str>,
    /// Skip the handshake and connect both ends directly at protocol
    /// 2026-07-28, which rmcp requires before a server may answer with an
    /// MRTR `input_required` result.
    pub modern: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            tasks: false,
            cache: true,
            ttl_ms: 0,
            modern: false,
            tools: vec![
                "echo",
                "error",
                "structured",
                "slow",
                "progress",
                "task",
                "input_required",
            ],
        }
    }
}

/// A connected client/server pair plus everything needed to observe it.
pub struct McpHarness {
    pub running: RunningService<RoleClient, McpClientHandler>,
    pub server_peer: Peer<RoleServer>,
    _server: RunningService<RoleServer, HarnessServer>,
    pub log: Arc<ServerLog>,
    pub tool_state: Arc<McpToolState>,
    /// Adapters built exactly like `build_mcp_state` does (with the router).
    pub adapters: HashMap<String, Arc<McpToolAdapter>>,
    shared: Arc<Shared>,
}

impl McpHarness {
    pub async fn start(options: HarnessOptions) -> Self {
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .expect("in-memory storage");
        let event_sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        let shared = Arc::new(Shared {
            tools: Mutex::new(options.tools.iter().copied().map(tool).collect()),
            ttl_ms: Mutex::new(options.ttl_ms),
            ..Shared::default()
        });
        let log = Arc::new(ServerLog::default());
        let server = HarnessServer {
            shared: shared.clone(),
            log: log.clone(),
        };

        let tool_state = McpToolState::empty();
        let handler = McpClientHandler::new(
            Default::default(),
            event_sink,
            "harness".into(),
            "s1".into(),
            crate::agent::mcp::agent_implementation(),
            tool_state.clone(),
        )
        .with_tasks(options.tasks);
        let router = handler.router();

        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let (running, server_running) = if options.modern {
            use rmcp::model::{
                Implementation, InitializeRequestParams, InitializeResult, ProtocolVersion,
            };
            use rmcp::service::serve_directly;
            let mut caps = ClientCapabilities::default();
            if options.tasks {
                caps.extensions = Some(
                    [(
                        rmcp::model::TASKS_EXTENSION_ID.to_string(),
                        Default::default(),
                    )]
                    .into(),
                );
            }
            let client_info =
                InitializeRequestParams::new(caps, Implementation::new("harness-client", "0"))
                    .with_protocol_version(ProtocolVersion::V_2026_07_28);
            let mut server_info =
                InitializeResult::new(ServerCapabilities::builder().enable_tools().build());
            server_info.protocol_version = ProtocolVersion::V_2026_07_28;
            let server_running =
                serve_directly::<RoleServer, _, _, _, _>(server, server_io, Some(client_info));
            let running = serve_directly::<RoleClient, _, _, _, _>(
                handler,
                client_io,
                Some(server_info.into()),
            );
            (running, server_running)
        } else {
            let server_task = tokio::spawn(serve_server(server, server_io));
            let running = serve_mcp_client(handler, client_io, "test", options.cache)
                .await
                .expect("client connects");
            let server_running = server_task
                .await
                .expect("server task")
                .expect("server initializes");
            (running, server_running)
        };

        let peer = running.peer().clone();
        let mut adapters = HashMap::new();
        for t in peer.list_all_tools().await.expect("list tools") {
            let adapter = McpToolAdapter::try_new(t, peer.clone(), "harness".into())
                .expect("adapter")
                .with_router(router.clone());
            adapters.insert(
                adapter.descriptor().function.name.clone(),
                Arc::new(adapter),
            );
        }
        // Publish the adapters like `build_mcp_state` does, so agent-level
        // tests can execute them through the tool pipeline.
        tool_state.store(crate::agent::core::McpToolSnapshot {
            tool_defs: adapters.values().map(|a| a.descriptor()).collect(),
            tools: adapters.clone(),
            tools_hash: None,
        });
        Self {
            running,
            server_peer: server_running.peer().clone(),
            _server: server_running,
            log,
            tool_state,
            adapters,
            shared,
        }
    }

    /// Adapter for `name`.
    pub fn adapter(&self, name: &str) -> Arc<McpToolAdapter> {
        self.adapters
            .get(name)
            .unwrap_or_else(|| panic!("no adapter for {name}"))
            .clone()
    }

    pub fn set_task_script(&self, script: TaskScript) {
        *self.shared.task_script.lock() = script;
    }

    /// Replace the server's tool set (does not notify).
    pub fn set_tools(&self, names: &[&'static str]) {
        *self.shared.tools.lock() = names.iter().copied().map(tool).collect();
    }

    /// Push a `notifications/tasks` status for task `t1`.
    pub async fn push_task_status(&self, payload: TaskPayload) {
        let task = Task::new(
            "t1",
            payload.status(),
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
        );
        let params = TaskStatusNotificationParams::new(DetailedTask::new(task, payload));
        let _ = self
            .server_peer
            .send_notification(rmcp::model::ServerNotification::TaskStatusNotification(
                rmcp::model::Notification::new(params),
            ))
            .await;
    }

    pub fn request_id_of_first_cancel(&self) -> Option<NumberOrString> {
        self.log.cancelled.lock().first()?.request_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ToolResultPart;
    use querymt::mcp::adapter::McpToolError;
    use querymt::mcp::progress::{CURRENT_PROGRESS, ProgressSink, ProgressUpdate};

    #[derive(Default)]
    struct Collect(Mutex<Vec<ProgressUpdate>>);
    impl ProgressSink for Collect {
        fn report(&self, update: ProgressUpdate) {
            self.0.lock().push(update);
        }
    }

    fn texts(parts: &[ToolResultPart]) -> Vec<&str> {
        parts
            .iter()
            .filter_map(|p| match p {
                ToolResultPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn mcp_err(e: &anyhow::Error) -> &McpToolError {
        e.downcast_ref::<McpToolError>().expect("McpToolError")
    }

    /// Run `call` with a collecting progress sink installed.
    async fn with_sink<T>(sink: &Arc<Collect>, fut: impl std::future::Future<Output = T>) -> T {
        CURRENT_PROGRESS
            .scope(sink.clone() as Arc<dyn ProgressSink>, fut)
            .await
    }

    #[tokio::test]
    async fn smoke_lists_and_calls_echo() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        assert!(h.adapters.contains_key("echo"));
        let parts = h
            .adapter("echo")
            .call(json!({"text": "hello"}))
            .await
            .expect("echo succeeds");
        assert_eq!(texts(&parts), ["hello"]);
    }

    #[tokio::test]
    async fn is_error_and_structured_results() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        let err = h.adapter("error").call(json!({})).await.unwrap_err();
        assert!(matches!(mcp_err(&err), McpToolError::Reported { .. }));
        assert!(err.to_string().contains("quota exceeded"));
        let parts = h.adapter("structured").call(json!({})).await.unwrap();
        assert_eq!(texts(&parts), [r#"{"temp":21}"#]);
    }

    #[tokio::test]
    async fn input_required_fails_with_explicit_error() {
        let h = McpHarness::start(HarnessOptions {
            modern: true,
            ..HarnessOptions::default()
        })
        .await;
        let err = h
            .adapter("input_required")
            .call(json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            mcp_err(&err),
            McpToolError::InputRequiredUnsupported
        ));
        assert!(err.to_string().contains("not supported"));
    }

    #[tokio::test]
    async fn progress_is_not_forwarded_without_sink_or_router() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        let args = json!({"count": 2, "hold_ms": 20});
        // Router attached but no sink in scope: the call just completes.
        h.adapter("progress").call(args.clone()).await.unwrap();
        // Sink in scope but the adapter has no router (ConnectedMcpPeer path).
        let tool = h
            .running
            .peer()
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.name == "progress")
            .unwrap();
        let routerless =
            McpToolAdapter::try_new(tool, h.running.peer().clone(), "x".into()).unwrap();
        let sink = Arc::new(Collect::default());
        with_sink(&sink, routerless.call(args)).await.unwrap();
        assert!(sink.0.lock().is_empty());
    }

    #[tokio::test]
    async fn progress_notifications_reach_the_sink_and_late_ones_do_not() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        let sink = Arc::new(Collect::default());
        let parts = with_sink(
            &sink,
            h.adapter("progress")
                .call(json!({"count": 3, "total": 3, "hold_ms": 50})),
        )
        .await
        .unwrap();
        assert_eq!(texts(&parts), ["done"]);
        {
            let got = sink.0.lock();
            assert_eq!(got.len(), 3);
            assert_eq!(got[2].progress, Some(3.0));
            assert_eq!(got[2].message.as_deref(), Some("step 3"));
        }
        // A notification for the finished call's token finds no subscriber.
        let token = h.log.progress_token_of_call(0).expect("rmcp sent a token");
        let token = serde_json::from_value(token).unwrap();
        let _ = h
            .server_peer
            .notify_progress(ProgressNotificationParam::new(token, 99.0))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sink.0.lock().len(), 3);
    }

    // ── tasks ────────────────────────────────────────────────────────────

    fn completed(text: &str, is_error: bool) -> TaskPayload {
        let mut r = CallToolResult::success(vec![ContentBlock::text(text)]);
        r.is_error = Some(is_error);
        let Value::Object(result) = serde_json::to_value(r).unwrap() else {
            unreachable!()
        };
        TaskPayload::Completed { result }
    }

    fn tasks_on() -> HarnessOptions {
        HarnessOptions {
            tasks: true,
            ..HarnessOptions::default()
        }
    }

    #[tokio::test]
    async fn tasks_extension_is_declared_only_when_enabled() {
        for enabled in [false, true] {
            let h = McpHarness::start(HarnessOptions {
                tasks: enabled,
                ..HarnessOptions::default()
            })
            .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(*h.log.client_supports_tasks.lock(), Some(enabled));
        }
    }

    #[tokio::test]
    async fn task_working_then_completed_reports_progress() {
        let h = McpHarness::start(tasks_on()).await;
        h.set_task_script(TaskScript {
            states: vec![
                TaskPayload::Working,
                TaskPayload::Working,
                completed("done", false),
            ],
            status_message: Some("crunching".into()),
            ..TaskScript::default()
        });
        let sink = Arc::new(Collect::default());
        let parts = with_sink(&sink, h.adapter("task").call(json!({})))
            .await
            .unwrap();
        assert_eq!(texts(&parts), ["done"]);
        let got = sink.0.lock();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].message.as_deref(), Some("crunching"));
    }

    #[tokio::test]
    async fn task_completed_with_is_error_is_reported() {
        let h = McpHarness::start(tasks_on()).await;
        h.set_task_script(TaskScript {
            states: vec![completed("boom", true)],
            ..TaskScript::default()
        });
        let err = h.adapter("task").call(json!({})).await.unwrap_err();
        assert!(matches!(mcp_err(&err), McpToolError::Reported { .. }));
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn task_failed_and_cancelled() {
        let h = McpHarness::start(tasks_on()).await;
        let Value::Object(error) = json!({"code": -1, "message": "kaput"}) else {
            unreachable!()
        };
        h.set_task_script(TaskScript {
            states: vec![TaskPayload::Failed { error }],
            ..TaskScript::default()
        });
        let err = h.adapter("task").call(json!({})).await.unwrap_err();
        assert!(
            matches!(mcp_err(&err), McpToolError::TaskFailed { status, message }
            if status == "failed" && message == "kaput")
        );
        h.set_task_script(TaskScript {
            states: vec![TaskPayload::Cancelled],
            ..TaskScript::default()
        });
        let err = h.adapter("task").call(json!({})).await.unwrap_err();
        assert!(
            matches!(mcp_err(&err), McpToolError::TaskFailed { status, .. } if status == "cancelled")
        );
        assert!(
            h.log.task_cancels.lock().is_empty(),
            "terminal tasks are not cancelled"
        );
    }

    #[tokio::test]
    async fn task_input_required_cancels_the_task() {
        let h = McpHarness::start(tasks_on()).await;
        h.set_task_script(TaskScript {
            states: vec![TaskPayload::InputRequired {
                input_requests: Default::default(),
            }],
            ..TaskScript::default()
        });
        let err = h.adapter("task").call(json!({})).await.unwrap_err();
        assert!(
            matches!(mcp_err(&err), McpToolError::TaskFailed { status, .. } if status == "input_required")
        );
        tokio::time::timeout(Duration::from_secs(2), h.log.cancel_seen.notified())
            .await
            .expect("tasks/cancel received");
        assert_eq!(*h.log.task_cancels.lock(), ["t1"]);
    }

    #[tokio::test]
    async fn status_push_wakes_poller_before_the_interval() {
        let h = McpHarness::start(tasks_on()).await;
        h.set_task_script(TaskScript {
            states: vec![completed("done", false)],
            poll_interval_ms: Some(10_000),
            ..TaskScript::default()
        });
        let adapter = h.adapter("task");
        let call = tokio::spawn(async move { adapter.call(json!({})).await });
        tokio::time::timeout(Duration::from_secs(2), h.log.started.notified())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = tokio::time::Instant::now();
        h.push_task_status(TaskPayload::Working).await;
        let parts = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .expect("completes well before the 10s interval")
            .unwrap()
            .unwrap();
        assert_eq!(texts(&parts), ["done"]);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn dropping_a_task_call_cancels_the_task_on_the_server() {
        let h = McpHarness::start(tasks_on()).await;
        h.set_task_script(TaskScript::default()); // working forever
        let adapter = h.adapter("task");
        let call = tokio::spawn(async move { adapter.call(json!({})).await });
        tokio::time::timeout(Duration::from_secs(2), h.log.started.notified())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await; // task id acknowledged
        call.abort();
        let _ = call.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.log.task_cancels.lock().is_empty() {
                h.log.cancel_seen.notified().await;
            }
        })
        .await
        .expect("tasks/cancel received");
        assert_eq!(*h.log.task_cancels.lock(), ["t1"]);
    }

    // ── cache ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn cache_honours_ttl_when_enabled() {
        let h = McpHarness::start(HarnessOptions {
            ttl_ms: 60_000,
            ..HarnessOptions::default()
        })
        .await;
        let before = h.log.list_tools_calls.load(Ordering::SeqCst);
        h.running.peer().list_all_tools().await.unwrap();
        h.running.peer().list_all_tools().await.unwrap();
        assert_eq!(
            h.log.list_tools_calls.load(Ordering::SeqCst),
            before,
            "served from cache"
        );
    }

    #[tokio::test]
    async fn cache_disabled_fetches_every_listing_including_the_first() {
        let h = McpHarness::start(HarnessOptions {
            ttl_ms: 60_000,
            cache: false,
            ..HarnessOptions::default()
        })
        .await;
        assert_eq!(
            h.log.list_tools_calls.load(Ordering::SeqCst),
            1,
            "first listing hit the server"
        );
        h.running.peer().list_all_tools().await.unwrap();
        h.running.peer().list_all_tools().await.unwrap();
        assert_eq!(h.log.list_tools_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn list_changed_bypasses_the_cache_and_updates_tool_state() {
        let h = McpHarness::start(HarnessOptions {
            ttl_ms: 60_000,
            ..HarnessOptions::default()
        })
        .await;
        h.set_tools(&["echo", "structured"]);
        h.server_peer.notify_tool_list_changed().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if h.tool_state.load().tools.len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("tool state refreshed despite the TTL");
        assert!(h.tool_state.load().tools.contains_key("structured"));
    }
}
