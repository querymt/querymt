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
//!
//! With [`HarnessOptions::skills`] set, the server also declares the MCP
//! Skills extension (plus resources) and serves `skills/list`, `skills/get`
//! and `resources/read` from a mutable set of [`ScriptedSkill`]s. Switches
//! ([`Tamper`], [`EntryMode`], `fail_list`) misbehave on purpose, and the
//! [`ServerLog`] records every skills and resource request. The protocol
//! version stays 2026-07-28. rmcp validates every entry the server sends, so
//! entries that break the structural rules (for example a manifest without
//! `SKILL.md`) surface as server-side errors rather than as bad data.

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
        DetailedTask, ErrorData, GetSkillRequestParams, GetSkillResult, GetTaskParams,
        GetTaskResult, InputRequiredResult, ListSkillsResult, ListToolsResult, NumberOrString,
        PaginatedRequestParams, ProgressNotificationParam, ReadResourceRequestParams,
        ReadResourceResult, ResourceContents, ServerCapabilities, ServerConfig, Skill,
        SkillFrontmatter, SkillResource, SkillResources, Task, TaskPayload, TaskStatus,
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
    /// `skills/list` requests received (skills mode only).
    pub skills_list_calls: AtomicUsize,
    /// URI of every `skills/get` request.
    pub skills_get: Mutex<Vec<String>>,
    /// URI of every `resources/read` request.
    pub resource_reads: Mutex<Vec<String>>,
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

/// How served `SKILL.md`/file bytes differ from their manifest entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tamper {
    #[default]
    None,
    /// Same length, different bytes: the digest no longer matches.
    Digest,
    /// One byte longer: the size no longer matches.
    Size,
}

/// How `skills/get` differs from `skills/list`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryMode {
    #[default]
    Normal,
    /// The manifest omits the `SKILL.md` entry (rmcp rejects this entry on the
    /// server side, so the client sees a failed `skills/get`).
    DropSkillMd,
    /// The `SKILL.md` digest differs from the listed entry's.
    Changed,
}

/// One skill the harness serves.
#[derive(Clone, Debug)]
pub struct ScriptedSkill {
    pub name: String,
    /// Frontmatter description as listed (may be oversized or multi-line).
    pub description: String,
    /// Full `SKILL.md` text.
    pub skill_md: String,
    /// Supporting files: path relative to the skill root, and bytes.
    pub files: Vec<(String, Vec<u8>)>,
    /// Declare the files as `"dynamic"` (no manifest or digests).
    pub dynamic: bool,
    /// Declared `SKILL.md` size in the manifest, when it should lie.
    pub declared_size: Option<u64>,
}

impl ScriptedSkill {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            skill_md: format!("---\nname: {name}\ndescription: scripted\n---\nBody of {name}\n"),
            files: Vec::new(),
            dynamic: false,
            declared_size: None,
        }
    }

    pub fn with_file(mut self, path: &str, bytes: &[u8]) -> Self {
        self.files.push((path.into(), bytes.to_vec()));
        self
    }

    pub fn with_skill_md(mut self, text: &str) -> Self {
        self.skill_md = text.into();
        self
    }

    pub fn dynamic(mut self) -> Self {
        self.dynamic = true;
        self
    }

    pub fn with_declared_size(mut self, size: u64) -> Self {
        self.declared_size = Some(size);
        self
    }

    /// URI of the skill's `SKILL.md`.
    pub fn uri(&self) -> String {
        format!("skill://{}/SKILL.md", self.name)
    }

    fn root(&self) -> String {
        format!("skill://{}/", self.name)
    }

    fn entry(&self, entry: EntryMode) -> Skill {
        let frontmatter = SkillFrontmatter::new(self.name.clone(), self.description.clone());
        let resources = if self.dynamic {
            SkillResources::Dynamic
        } else {
            let mut md_bytes = self.skill_md.clone().into_bytes();
            if entry == EntryMode::Changed {
                md_bytes.extend_from_slice(b"\n<changed>");
            }
            let mut files = Vec::new();
            if entry != EntryMode::DropSkillMd {
                let mut md = SkillResource::from_bytes(self.uri(), &md_bytes);
                if let Some(size) = self.declared_size {
                    md.size = size;
                }
                files.push(md);
            }
            for (path, bytes) in &self.files {
                files.push(SkillResource::from_bytes(
                    format!("{}{path}", self.root()),
                    bytes,
                ));
            }
            SkillResources::Files(files)
        };
        Skill::new(self.uri(), frontmatter, resources)
    }

    fn bytes_of(&self, uri: &str) -> Option<Vec<u8>> {
        if uri == self.uri() {
            return Some(self.skill_md.clone().into_bytes());
        }
        let rel = uri.strip_prefix(&self.root())?;
        self.files
            .iter()
            .find(|(path, _)| path == rel)
            .map(|(_, bytes)| bytes.clone())
    }
}

/// Mutable state of the skills mode.
#[derive(Clone, Default)]
struct SkillsScript {
    skills: Vec<ScriptedSkill>,
    tamper: Tamper,
    entry: EntryMode,
    fail_list: bool,
    /// `skills/list` never answers.
    stall_list: bool,
}

#[derive(Default)]
struct Shared {
    /// `Some` enables the skills mode.
    skills: Mutex<Option<SkillsScript>>,
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
        let caps = if self.shared.skills.lock().is_some() {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_tasks()
                .enable_skills()
                .build()
        } else {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_tasks()
                .build()
        };
        let mut info = ServerConfig::new(caps);
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

    async fn list_skills(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListSkillsResult, ErrorData> {
        self.log.skills_list_calls.fetch_add(1, Ordering::SeqCst);
        let script = self.shared.skills.lock().clone().unwrap_or_default();
        if script.fail_list {
            return Err(ErrorData::internal_error("skills listing failed", None));
        }
        if script.stall_list {
            std::future::pending::<()>().await;
        }
        let skills = script
            .skills
            .iter()
            .map(|skill| skill.entry(EntryMode::Normal))
            .collect();
        Ok(ListSkillsResult::with_all_items(skills).with_ttl_ms(0))
    }

    async fn get_skill(
        &self,
        request: GetSkillRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetSkillResult, ErrorData> {
        self.log.skills_get.lock().push(request.uri.clone());
        let script = self.shared.skills.lock().clone().unwrap_or_default();
        let skill = script
            .skills
            .iter()
            .find(|skill| skill.uri() == request.uri)
            .ok_or_else(|| ErrorData::invalid_params("unknown skill", None))?;
        Ok(GetSkillResult::new(skill.entry(script.entry)).with_ttl_ms(0))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        self.log.resource_reads.lock().push(request.uri.clone());
        let script = self.shared.skills.lock().clone().unwrap_or_default();
        let mut bytes = script
            .skills
            .iter()
            .find_map(|skill| skill.bytes_of(&request.uri))
            .ok_or_else(|| ErrorData::invalid_params("unknown resource", None))?;
        match script.tamper {
            Tamper::None => {}
            Tamper::Digest => {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 1;
                }
            }
            Tamper::Size => bytes.push(b'!'),
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(
            ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)])
                .with_ttl_ms(0)
                .into(),
        )
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
    /// Enable the skills mode with these initial skills (`Some(vec![])` =
    /// extension declared, nothing published).
    pub skills: Option<Vec<ScriptedSkill>>,
    /// Server name the client side uses (default `harness`).
    pub server_name: &'static str,
    /// Start with `skills/list` failing (skills mode only).
    pub fail_list: bool,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            skills: None,
            server_name: "harness",
            fail_list: false,
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
            skills: Mutex::new(options.skills.clone().map(|skills| SkillsScript {
                skills,
                fail_list: options.fail_list,
                ..SkillsScript::default()
            })),
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
            options.server_name.into(),
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
            let mut server_info = InitializeResult::new(server.get_info().capabilities);
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
            let adapter = McpToolAdapter::try_new(t, peer.clone(), options.server_name.into())
                .expect("adapter")
                .with_router(router.clone());
            adapters.insert(
                adapter.descriptor().function.name.clone(),
                Arc::new(adapter),
            );
        }
        let skills = crate::skills::mcp::discover_server_skills(options.server_name, &peer).await;
        // Publish the adapters like `build_mcp_state` does, so agent-level
        // tests can execute them through the tool pipeline.
        let mut skill_servers = HashMap::new();
        if skills.is_some() {
            skill_servers.insert(options.server_name.to_string(), peer.clone());
        }
        tool_state.store(crate::agent::core::McpToolSnapshot {
            tool_defs: adapters.values().map(|a| a.descriptor()).collect(),
            tools: adapters.clone(),
            tools_hash: None,
            skills: skills.unwrap_or_default(),
            skill_servers,
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

    fn with_script(&self, f: impl FnOnce(&mut SkillsScript)) {
        f(self
            .shared
            .skills
            .lock()
            .as_mut()
            .expect("harness started without the skills mode"));
    }

    /// Replace the served skills (does not notify).
    pub fn set_skills(&self, skills: Vec<ScriptedSkill>) {
        self.with_script(|script| script.skills = skills);
    }

    pub fn set_tamper(&self, tamper: Tamper) {
        self.with_script(|script| script.tamper = tamper);
    }

    pub fn set_entry_mode(&self, entry: EntryMode) {
        self.with_script(|script| script.entry = entry);
    }

    /// Make `skills/list` fail.
    pub fn set_fail_list(&self, fail: bool) {
        self.with_script(|script| script.fail_list = fail);
    }

    /// Make `skills/list` never answer.
    pub fn set_stall_list(&self, stall: bool) {
        self.with_script(|script| script.stall_list = stall);
    }

    /// Send `notifications/resources/list_changed`.
    pub async fn notify_resources_changed(&self) {
        let _ = self.server_peer.notify_resource_list_changed().await;
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

/// A skills-mode harness server reachable over streamable HTTP, for tests
/// that attach it to a real session through `mcp.json` or an ACP request.
pub struct HttpHarness {
    pub url: String,
    pub log: Arc<ServerLog>,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl HttpHarness {
    /// Serve `skills` on an ephemeral localhost port.
    pub async fn start(skills: Vec<ScriptedSkill>) -> Self {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };
        let shared = Arc::new(Shared {
            skills: Mutex::new(Some(SkillsScript {
                skills,
                ..SkillsScript::default()
            })),
            tools: Mutex::new(Vec::new()),
            ..Shared::default()
        });
        let log = Arc::new(ServerLog::default());
        let (factory_shared, factory_log) = (shared.clone(), log.clone());
        let service = StreamableHttpService::new(
            move || {
                Ok(HarnessServer {
                    shared: factory_shared.clone(),
                    log: factory_log.clone(),
                })
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            url,
            log,
            shared,
            task,
        }
    }

    /// Replace the served skills (does not notify).
    pub fn set_skills(&self, skills: Vec<ScriptedSkill>) {
        self.shared
            .skills
            .lock()
            .as_mut()
            .expect("skills mode")
            .skills = skills;
    }
}

impl Drop for HttpHarness {
    fn drop(&mut self) {
        self.task.abort();
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

    #[tokio::test]
    async fn skills_mode_lists_scripted_skills_and_logs_requests() {
        let h = McpHarness::start(HarnessOptions {
            skills: Some(vec![
                ScriptedSkill::new("release-notes", "Write notes").with_file("a/b.md", b"B"),
                ScriptedSkill::new("triage", "Triage").dynamic(),
            ]),
            ..HarnessOptions::default()
        })
        .await;
        let peer = h.running.peer();
        assert!(
            peer.peer_info()
                .is_some_and(|info| info.capabilities.supports_skills())
        );
        let skills = peer.list_all_skills().await.expect("list skills");
        let names: Vec<_> = skills.iter().map(|s| s.name().to_string()).collect();
        assert_eq!(names, ["release-notes", "triage"]);
        // One listing on connect, one just now.
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 2);

        // get + read are served and logged with their URIs.
        let uri = skills[0].uri.clone();
        let got = peer
            .get_skill(GetSkillRequestParams::new(&uri))
            .await
            .unwrap();
        got.skill.validate().unwrap();
        let read = peer
            .read_resource(ReadResourceRequestParams::new(&uri))
            .await
            .unwrap();
        assert_eq!(read.contents.len(), 1);
        assert_eq!(*h.log.skills_get.lock(), std::slice::from_ref(&uri));
        assert_eq!(*h.log.resource_reads.lock(), [uri]);
    }

    #[tokio::test]
    async fn skills_mode_switches_and_notification() {
        let h = McpHarness::start(HarnessOptions {
            skills: Some(vec![ScriptedSkill::new("a", "A")]),
            ..HarnessOptions::default()
        })
        .await;
        let peer = h.running.peer();
        let uri = "skill://a/SKILL.md";
        let SkillResources::Files(files) = peer
            .get_skill(GetSkillRequestParams::new(uri))
            .await
            .unwrap()
            .skill
            .resources
        else {
            panic!("expected a manifest");
        };
        let bytes_of = || async {
            match &peer
                .read_resource(ReadResourceRequestParams::new(uri))
                .await
                .unwrap()
                .contents[0]
            {
                ResourceContents::TextResourceContents { text, .. } => text.clone().into_bytes(),
                other => panic!("unexpected contents {other:?}"),
            }
        };
        files[0]
            .verify(&bytes_of().await)
            .expect("untampered bytes verify");
        h.set_tamper(Tamper::Digest);
        assert!(files[0].verify(&bytes_of().await).is_err());
        h.set_tamper(Tamper::Size);
        assert!(files[0].verify(&bytes_of().await).is_err());

        h.set_fail_list(true);
        assert!(peer.list_all_skills().await.is_err());
        h.set_fail_list(false);
        h.set_entry_mode(EntryMode::DropSkillMd);
        assert!(
            peer.get_skill(GetSkillRequestParams::new(uri))
                .await
                .is_err()
        );

        // `resources/list_changed` reaches the client handler.
        h.set_entry_mode(EntryMode::Normal);
        h.notify_resources_changed().await;
    }

    #[tokio::test]
    async fn server_without_skills_mode_records_no_skills_requests() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        let peer = h.running.peer();
        assert!(
            !peer
                .peer_info()
                .is_some_and(|info| info.capabilities.supports_skills())
        );
        assert!(peer.list_all_skills().await.is_err());
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 0);
        assert!(h.log.skills_get.lock().is_empty());
        assert!(h.log.resource_reads.lock().is_empty());
    }

    // ── Skills discovery and refresh (tasks 3.2 / 3.3) ───────────────────

    fn skills_on(skills: Vec<ScriptedSkill>) -> HarnessOptions {
        HarnessOptions {
            skills: Some(skills),
            ..HarnessOptions::default()
        }
    }

    fn skill_names(state: &McpToolState) -> Vec<String> {
        let mut names: Vec<_> = state
            .load()
            .skills
            .iter()
            .map(|s| s.callable_id())
            .collect();
        names.sort();
        names
    }

    async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    #[tokio::test]
    async fn skills_are_listed_on_connect() {
        let h = McpHarness::start(skills_on(vec![
            ScriptedSkill::new("release-notes", "Write notes"),
            ScriptedSkill::new("triage", "Triage"),
        ]))
        .await;
        assert_eq!(
            skill_names(&h.tool_state),
            ["harness.release-notes", "harness.triage"]
        );
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn server_without_the_extension_gets_no_skills_request() {
        let h = McpHarness::start(HarnessOptions::default()).await;
        assert!(h.tool_state.load().skills.is_empty());
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dotted_server_name_gets_no_skills_request_but_keeps_tools() {
        let h = McpHarness::start(HarnessOptions {
            server_name: "team.docs",
            ..skills_on(vec![ScriptedSkill::new("a", "A")])
        })
        .await;
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 0);
        assert!(h.tool_state.load().skills.is_empty());
        assert!(h.tool_state.load().tools.contains_key("echo"), "tools stay");
    }

    #[tokio::test]
    async fn failing_skills_listing_keeps_the_servers_tools() {
        let h = McpHarness::start(HarnessOptions {
            fail_list: true,
            ..skills_on(vec![ScriptedSkill::new("a", "A")])
        })
        .await;
        assert_eq!(h.log.skills_list_calls.load(Ordering::SeqCst), 1);
        assert!(h.tool_state.load().skills.is_empty());
        assert!(h.tool_state.load().tools.contains_key("echo"));
    }

    #[tokio::test]
    async fn dotted_skill_name_is_excluded_and_siblings_remain() {
        let h = McpHarness::start(skills_on(vec![
            ScriptedSkill::new("v1.2", "dotted"),
            ScriptedSkill::new("good", "fine"),
        ]))
        .await;
        assert_eq!(skill_names(&h.tool_state), ["harness.good"]);
    }

    /// rmcp validates entries on the server side, so a structurally invalid
    /// entry cannot be served by the harness; the client-side filter is
    /// exercised directly (it is the defence against non-rmcp servers).
    #[test]
    fn invalid_entry_is_excluded_and_valid_siblings_remain() {
        use crate::skills::mcp::filter_listed_skills;
        let valid = Skill::new(
            "skill://ok/SKILL.md",
            SkillFrontmatter::new("ok", "fine"),
            SkillResources::Dynamic,
        );
        let bad_uri = Skill::new(
            "skill://bad/README.md",
            SkillFrontmatter::new("bad", "x"),
            SkillResources::Dynamic,
        );
        let name_mismatch = Skill::new(
            "skill://one/SKILL.md",
            SkillFrontmatter::new("two", "x"),
            SkillResources::Dynamic,
        );
        let kept = filter_listed_skills("srv", vec![bad_uri, valid, name_mismatch]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name(), "ok");
    }

    #[tokio::test]
    async fn oversized_multiline_description_is_listed_as_one_bounded_line() {
        let long = "line one\nline two\n".repeat(150);
        assert!(long.chars().count() > 2000 - 100);
        let h = McpHarness::start(skills_on(vec![ScriptedSkill::new("long", &long)])).await;
        let snap = h.tool_state.load();
        let description = &snap.skills[0].skill.frontmatter.description;
        assert!(description.chars().count() <= 1024, "{}", description.len());
        assert!(!description.contains('\n'));
        assert!(description.starts_with("line one line two"));
    }

    /// A client-supplied (attached) server contributes skills to the session
    /// it is merged into, and a second session does not see them.
    #[tokio::test]
    async fn attached_server_skills_are_session_scoped() {
        let h = McpHarness::start(skills_on(vec![ScriptedSkill::new("triage", "Triage")])).await;
        let peer = crate::agent::session_mcp::ConnectedMcpPeer {
            server_name: "client-docs".into(),
            peer: h.running.peer().clone(),
        };
        let session_a = McpToolState::empty();
        let session_b = McpToolState::empty();
        crate::agent::protocol::merge_connected_mcp_peers(session_a.clone(), &[peer])
            .await
            .unwrap();
        assert_eq!(skill_names(&session_a), ["client-docs.triage"]);
        assert!(skill_names(&session_b).is_empty());
    }

    #[tokio::test]
    async fn resource_list_changed_adds_and_removes_skills_over_2026_07_28() {
        // `modern` connects both ends directly at 2026-07-28.
        let h = McpHarness::start(HarnessOptions {
            modern: true,
            ..skills_on(vec![ScriptedSkill::new("a", "A")])
        })
        .await;
        assert_eq!(
            h.running.peer().peer_info().unwrap().protocol_version,
            rmcp::model::ProtocolVersion::V_2026_07_28
        );
        let tools_before: Vec<_> = {
            let mut t: Vec<_> = h.tool_state.load().tools.keys().cloned().collect();
            t.sort();
            t
        };

        h.set_skills(vec![
            ScriptedSkill::new("a", "A"),
            ScriptedSkill::new("b", "B"),
        ]);
        h.notify_resources_changed().await;
        wait_for("skill added", || skill_names(&h.tool_state).len() == 2).await;
        assert_eq!(skill_names(&h.tool_state), ["harness.a", "harness.b"]);

        h.set_skills(vec![ScriptedSkill::new("b", "B")]);
        h.notify_resources_changed().await;
        wait_for("skill removed", || {
            skill_names(&h.tool_state) == ["harness.b"]
        })
        .await;

        let mut tools_after: Vec<_> = h.tool_state.load().tools.keys().cloned().collect();
        tools_after.sort();
        assert_eq!(
            tools_before, tools_after,
            "tools untouched by a skills refresh"
        );
        assert!(h.tool_state.load().tools_hash.is_none());
    }

    #[tokio::test]
    async fn failed_skills_refresh_keeps_previous_skills() {
        let h = McpHarness::start(skills_on(vec![ScriptedSkill::new("a", "A")])).await;
        h.set_fail_list(true);
        let before = h.log.skills_list_calls.load(Ordering::SeqCst);
        h.notify_resources_changed().await;
        wait_for("refresh attempt", || {
            h.log.skills_list_calls.load(Ordering::SeqCst) > before
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(skill_names(&h.tool_state), ["harness.a"]);
    }

    #[tokio::test]
    async fn replace_server_leaves_other_servers_untouched() {
        let h = McpHarness::start(skills_on(vec![ScriptedSkill::new("a", "A")])).await;
        let state = McpToolState::empty();
        let peer = h.running.peer().clone();
        for name in ["one", "two"] {
            crate::agent::protocol::merge_connected_mcp_peers(
                state.clone(),
                &[crate::agent::session_mcp::ConnectedMcpPeer {
                    server_name: name.into(),
                    peer: peer.clone(),
                }],
            )
            .await
            .unwrap();
        }
        assert_eq!(skill_names(&state), ["one.a", "two.a"]);
        let tools_two = state
            .load()
            .tools
            .values()
            .filter(|a| a.server_name() == "two")
            .count();

        state.replace_server("one", None, Some(vec![]));
        assert_eq!(skill_names(&state), ["two.a"]);
        state.replace_server("one", Some(vec![]), None);
        assert_eq!(
            skill_names(&state),
            ["two.a"],
            "skills kept by a tools refresh"
        );
        let still_two = state
            .load()
            .tools
            .values()
            .filter(|a| a.server_name() == "two")
            .count();
        assert_eq!(tools_two, still_two);
    }

    /// A tools refresh and a skills refresh of the same server racing must
    /// both survive (the old load/store handlers lost one of them).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_tool_and_skill_refreshes_both_survive() {
        let h = McpHarness::start(skills_on(vec![ScriptedSkill::new("a", "A")])).await;
        let state = h.tool_state.clone();
        let peer = h.running.peer().clone();
        let skills = crate::skills::mcp::discover_server_skills("harness", &peer)
            .await
            .unwrap();
        let tools: Vec<_> = h.adapters.values().cloned().collect();
        for _ in 0..300 {
            state.replace_server("harness", Some(vec![]), Some(vec![]));
            let (s1, s2) = (state.clone(), state.clone());
            let (t, k) = (tools.clone(), skills.clone());
            let a = tokio::spawn(async move { s1.replace_server("harness", Some(t), None) });
            let b = tokio::spawn(async move { s2.replace_server("harness", None, Some(k)) });
            a.await.unwrap();
            b.await.unwrap();
            let snap = state.load();
            assert_eq!(snap.skills.len(), 1, "skills refresh lost");
            assert_eq!(snap.tools.len(), tools.len(), "tools refresh lost");
            assert_eq!(snap.tool_defs.len(), tools.len());
        }
    }
}
