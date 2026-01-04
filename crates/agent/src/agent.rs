use crate::events::{AgentEvent, AgentEventKind};
use crate::index::merkle::MerkleTree;
use crate::middleware::{
    AgentStats, ConversationContext, MaxStepsMiddleware, MiddlewareAction, MiddlewarePipeline,
    MiddlewareResult, PlanModeMiddleware,
};
use crate::model::{AgentMessage, MessagePart};
use crate::session::provider::{SessionContext, SessionProvider};
use crate::session::store::SessionStore;
use crate::tools::{
    ApplyPatchTool, DeleteFileTool, SearchTextTool, ShellTool, ToolRegistry, WebFetchTool,
    WriteFileTool,
};
use agent_client_protocol::{
    Agent, AuthenticateRequest, AuthenticateResponse, CancelNotification, ContentBlock, Error,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, ProtocolVersion, StopReason,
};
use async_trait::async_trait;
use querymt::{LLMProvider, chat::ChatRole};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, broadcast, watch};

pub struct QueryMTAgent {
    provider: Arc<SessionProvider>,
    active_sessions: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
    max_steps: usize,
    snapshot_root: Option<std::path::PathBuf>,
    snapshot_policy: SnapshotPolicy,
    assume_mutating: bool,
    mutating_tools: HashSet<String>,
    max_prompt_bytes: Option<usize>,
    tool_config: Arc<StdMutex<ToolConfig>>,
    tool_registry: Arc<ToolRegistry>,
    middleware: MiddlewarePipeline,
    plan_mode_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    event_tx: broadcast::Sender<AgentEvent>,
    event_seq: AtomicU64,
    event_observers: Vec<Arc<dyn crate::events::EventObserver>>,
}

impl QueryMTAgent {
    pub fn new(provider: Arc<dyn LLMProvider>, store: Arc<dyn SessionStore>) -> Self {
        let session_provider = Arc::new(SessionProvider::new(provider, store));
        let mut tool_registry = ToolRegistry::new();
        tool_registry.add(Arc::new(SearchTextTool::new()));
        tool_registry.add(Arc::new(WebFetchTool::new()));
        tool_registry.add(Arc::new(ApplyPatchTool::new()));
        tool_registry.add(Arc::new(WriteFileTool::new()));
        tool_registry.add(Arc::new(DeleteFileTool::new()));
        tool_registry.add(Arc::new(ShellTool::new()));
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            provider: session_provider,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            max_steps: 20,
            snapshot_root: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            tool_config: Arc::new(StdMutex::new(ToolConfig::default())),
            tool_registry: Arc::new(tool_registry),
            middleware: MiddlewarePipeline::new(),
            plan_mode_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            event_tx,
            event_seq: AtomicU64::new(1),
            event_observers: Vec::new(),
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn with_middleware<M: crate::middleware::ConversationMiddleware + 'static>(
        mut self,
        middleware: M,
    ) -> Self {
        self.middleware.add(middleware);
        self
    }

    pub fn with_middlewares(
        mut self,
        middlewares: Vec<Arc<dyn crate::middleware::ConversationMiddleware>>,
    ) -> Self {
        self.middleware.extend(middlewares);
        self
    }

    pub fn set_plan_mode(&self, enabled: bool) {
        self.plan_mode_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn plan_mode_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.plan_mode_enabled.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    pub fn with_plan_mode_middleware(mut self, reminder: String) -> Self {
        let middleware = PlanModeMiddleware::new(self.plan_mode_enabled.clone(), reminder);
        self.middleware.add(middleware);
        self
    }

    pub fn with_event_observer<O: crate::events::EventObserver + 'static>(
        mut self,
        observer: O,
    ) -> Self {
        self.event_observers.push(Arc::new(observer));
        self
    }

    pub fn with_snapshot_root<P: Into<std::path::PathBuf>>(mut self, root: P) -> Self {
        self.snapshot_root = Some(root.into());
        self
    }

    pub fn without_snapshots(mut self) -> Self {
        self.snapshot_root = None;
        self.snapshot_policy = SnapshotPolicy::None;
        self
    }

    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    pub fn with_mutating_tools<I, S>(mut self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.mutating_tools = tool_names.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_assume_mutating(mut self, assume_mutating: bool) -> Self {
        self.assume_mutating = assume_mutating;
        self
    }

    pub fn with_max_prompt_bytes(mut self, max_prompt_bytes: usize) -> Self {
        self.max_prompt_bytes = Some(max_prompt_bytes);
        self
    }

    pub fn with_tool_policy(self, policy: ToolPolicy) -> Self {
        if let Ok(mut config) = self.tool_config.lock() {
            config.policy = policy;
        }
        self
    }

    pub fn with_allowed_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allow = tool_names
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<_>>();
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = Some(allow);
        }
        self
    }

    pub fn with_denied_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist = tool_names.into_iter().map(Into::into).collect();
        }
        self
    }

    pub fn set_tool_policy(&self, policy: ToolPolicy) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.policy = policy;
        }
    }

    pub fn set_allowed_tools<I, S>(&self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = Some(tool_names.into_iter().map(Into::into).collect());
        }
    }

    pub fn clear_allowed_tools(&self) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = None;
        }
    }

    pub fn set_denied_tools<I, S>(&self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist = tool_names.into_iter().map(Into::into).collect();
        }
    }

    pub fn clear_denied_tools(&self) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist.clear();
        }
    }

    pub fn with_tool_registry(mut self, registry: ToolRegistry) -> Self {
        self.tool_registry = Arc::new(registry);
        self
    }

    pub fn with_builtin_tool<T: crate::tools::BuiltInTool + 'static>(mut self, tool: T) -> Self {
        let registry = Arc::make_mut(&mut self.tool_registry);
        registry.add(Arc::new(tool));
        self
    }

    async fn execute_cycle(
        &self,
        context: &SessionContext,
        cancel_rx: watch::Receiver<bool>,
    ) -> Result<CycleOutcome, anyhow::Error> {
        let mut steps = 0;
        let session_id = &context.session().id;
        let mut outcome = CycleOutcome::Completed;
        let mut stats = AgentStats::default();
        let mut pipeline = self.middleware.clone();
        pipeline.add(MaxStepsMiddleware::new(self.max_steps));
        let mut llm_messages = context.history().await;

        loop {
            // Check cancellation
            if *cancel_rx.borrow() {
                self.emit_event(session_id, AgentEventKind::Cancelled);
                return Ok(CycleOutcome::Cancelled);
            }

            stats.context_tokens = approximate_token_count(&llm_messages);
            stats.steps = steps;
            let history_len = llm_messages.len();
            let mw_context = ConversationContext {
                session_id: session_id.to_string(),
                history_len,
                stats: stats.clone(),
            };
            let before_result = pipeline.run_before_turn(&mw_context).await;
            let before_effect = self
                .handle_middleware_result(&before_result, context, session_id, &llm_messages)
                .await?;
            if let Some(outcome_override) = before_effect.stop {
                outcome = outcome_override;
                break;
            }
            if before_effect.refresh_history {
                llm_messages = context.history().await;
                stats.context_tokens = approximate_token_count(&llm_messages);
            }
            steps += 1;

            // 2. Chat with Provider via Context
            self.emit_event(
                session_id,
                AgentEventKind::LlmRequestStart {
                    message_count: llm_messages.len(),
                },
            );
            let tools = self.collect_tools(context.provider());
            let response = if tools.is_empty() {
                context.submit_request(&llm_messages).await?
            } else {
                context
                    .provider()
                    .chat_with_tools(&llm_messages, Some(&tools))
                    .await?
            };
            if let Some(usage) = response.usage() {
                stats.total_input_tokens += usage.input_tokens as u64;
                stats.total_output_tokens += usage.output_tokens as u64;
            }

            // 3. Parse Response
            let response_content = response.text().unwrap_or_default();
            let tool_calls = response.tool_calls();
            self.emit_event(
                session_id,
                AgentEventKind::LlmRequestEnd {
                    usage: response.usage(),
                    tool_calls: tool_calls.as_ref().map_or(0, |calls| calls.len()),
                },
            );

            // 4. Create and Store Assistant Message
            let mut parts = Vec::new();
            if !response_content.is_empty() {
                parts.push(MessagePart::Text {
                    content: response_content.clone(),
                });
            }
            if let Some(calls) = &tool_calls {
                for call in calls {
                    parts.push(MessagePart::ToolUse(call.clone()));
                }
            }

            let assistant_msg = AgentMessage {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts,
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
            };
            context.add_message(assistant_msg.clone()).await?;
            llm_messages.push(assistant_msg.to_chat_message());
            self.emit_event(
                session_id,
                AgentEventKind::AssistantMessageStored {
                    content: response_content.clone(),
                },
            );

            // 5. Handle Tool Calls or Break
            if let Some(calls) = tool_calls {
                for call in calls {
                    if *cancel_rx.borrow() {
                        return Ok(CycleOutcome::Cancelled);
                    }

                    // 5a. Pre-execution Snapshot
                    let snapshot = if self.should_snapshot_tool(&call.function.name) {
                        if self.snapshot_policy != SnapshotPolicy::None {
                            self.emit_event(
                                session_id,
                                AgentEventKind::SnapshotStart {
                                    policy: self.snapshot_policy.to_string(),
                                },
                            );
                        }
                        self.prepare_snapshot()
                            .map(|(root, policy)| match policy {
                                SnapshotPolicy::Diff => {
                                    let pre_tree = MerkleTree::scan(root.as_path());
                                    SnapshotState::Diff { pre_tree, root }
                                }
                                SnapshotPolicy::Metadata => SnapshotState::Metadata { root },
                                SnapshotPolicy::None => SnapshotState::None,
                            })
                            .unwrap_or(SnapshotState::None)
                    } else {
                        SnapshotState::None
                    };

                    // 5b. Execute Tool via Context (stateless execution)
                    self.emit_event(
                        session_id,
                        AgentEventKind::ToolCallStart {
                            tool_call_id: call.id.clone(),
                            tool_name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                        },
                    );
                    let (result_json, is_error) = if !self.is_tool_allowed(&call.function.name) {
                        (
                            format!("Error: tool '{}' is not allowed", call.function.name),
                            true,
                        )
                    } else {
                        match serde_json::from_str(&call.function.arguments) {
                            Ok(args) => {
                                if let Some(tool) = self.tool_registry.find(&call.function.name) {
                                    match tool.call(args).await {
                                        Ok(res) => (res, false),
                                        Err(e) => (format!("Error: {}", e), true),
                                    }
                                } else {
                                    match context.call_tool(&call.function.name, args).await {
                                        Ok(res) => (res, false),
                                        Err(e) => (format!("Error: {}", e), true),
                                    }
                                }
                            }
                            Err(e) => (format!("Error: {}", e), true),
                        }
                    };

                    // 5c. Post-execution Snapshot
                    let snapshot_part = match snapshot {
                        SnapshotState::Diff { pre_tree, root } => {
                            let post_tree = MerkleTree::scan(root.as_path());
                            let diff = post_tree.diff_summary(&pre_tree);
                            self.emit_event(
                                session_id,
                                AgentEventKind::SnapshotEnd {
                                    summary: Some(diff.clone()),
                                },
                            );
                            Some(MessagePart::Snapshot {
                                root_hash: post_tree.root_hash,
                                diff_summary: Some(diff),
                            })
                        }
                        SnapshotState::Metadata { root } => {
                            let (part, summary) = snapshot_metadata(root.as_path());
                            self.emit_event(session_id, AgentEventKind::SnapshotEnd { summary });
                            Some(part)
                        }
                        SnapshotState::None => None,
                    };

                    // 5d. Create and Store Tool Result Message
                    let mut parts = vec![MessagePart::ToolResult {
                        call_id: call.id.clone(),
                        content: result_json.clone(),
                        is_error,
                        tool_name: Some(call.function.name.clone()),
                        tool_arguments: Some(call.function.arguments.clone()),
                    }];
                    if let Some(snapshot) = snapshot_part {
                        parts.push(snapshot);
                    }
                    let result_msg = AgentMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.to_string(),
                        role: ChatRole::User,
                        parts,
                        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                        parent_message_id: None,
                    };
                    let result_chat = result_msg.to_chat_message();
                    context.add_message(result_msg).await?;
                    llm_messages.push(result_chat);
                    self.emit_event(
                        session_id,
                        AgentEventKind::ToolCallEnd {
                            tool_call_id: call.id,
                            tool_name: call.function.name,
                            is_error,
                            result: result_json,
                        },
                    );
                }
            } else {
                break;
            }

            stats.steps = steps;
            let after_context = ConversationContext {
                session_id: session_id.to_string(),
                history_len: llm_messages.len(),
                stats: stats.clone(),
            };
            let after_result = pipeline.run_after_turn(&after_context).await;
            let after_effect = self
                .handle_middleware_result(&after_result, context, session_id, &llm_messages)
                .await?;
            if let Some(outcome_override) = after_effect.stop {
                outcome = outcome_override;
                break;
            }
            if after_effect.refresh_history {
                llm_messages = context.history().await;
                stats.context_tokens = approximate_token_count(&llm_messages);
            }
        }

        Ok(outcome)
    }

    async fn handle_middleware_result(
        &self,
        result: &MiddlewareResult,
        context: &SessionContext,
        session_id: &str,
        llm_messages: &[querymt::chat::ChatMessage],
    ) -> Result<MiddlewareEffect, anyhow::Error> {
        match result.action {
            MiddlewareAction::Stop => {
                self.emit_event(
                    session_id,
                    AgentEventKind::MiddlewareStopped {
                        reason: result
                            .reason
                            .clone()
                            .unwrap_or_else(|| "stopped".to_string()),
                    },
                );
                return Ok(MiddlewareEffect {
                    stop: Some(CycleOutcome::Stopped(
                        result.stop_reason.unwrap_or(StopReason::EndTurn),
                    )),
                    refresh_history: false,
                });
            }
            MiddlewareAction::Compact => {
                self.emit_event(
                    session_id,
                    AgentEventKind::CompactionStart {
                        token_estimate: approximate_token_count(llm_messages),
                    },
                );
                self.compact_history(context, session_id, llm_messages)
                    .await?;
                return Ok(MiddlewareEffect {
                    stop: None,
                    refresh_history: true,
                });
            }
            MiddlewareAction::InjectMessage => {
                if let Some(message) = &result.message {
                    let agent_msg = AgentMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        session_id: session_id.to_string(),
                        role: ChatRole::User,
                        parts: vec![MessagePart::Text {
                            content: message.clone(),
                        }],
                        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                        parent_message_id: None,
                    };
                    context.add_message(agent_msg).await?;
                    self.emit_event(
                        session_id,
                        AgentEventKind::MiddlewareInjected {
                            message: message.clone(),
                        },
                    );
                    return Ok(MiddlewareEffect {
                        stop: None,
                        refresh_history: true,
                    });
                }
            }
            MiddlewareAction::Continue => {}
        }
        Ok(MiddlewareEffect {
            stop: None,
            refresh_history: false,
        })
    }

    async fn compact_history(
        &self,
        context: &SessionContext,
        session_id: &str,
        llm_messages: &[querymt::chat::ChatMessage],
    ) -> Result<(), anyhow::Error> {
        let mut transcript = String::new();
        for msg in llm_messages {
            let role = match msg.role {
                ChatRole::User => "User",
                ChatRole::Assistant => "Assistant",
            };
            transcript.push_str(role);
            transcript.push_str(": ");
            transcript.push_str(&msg.content);
            transcript.push_str("\n");
        }

        let summary_prompt = querymt::chat::ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: format!(
                "Summarize the conversation so far for future context. \
Include key facts, decisions, tool results, and open questions. Be concise.\n\nTranscript:\n{}",
                transcript
            ),
        };

        let response = context.submit_request(&[summary_prompt]).await?;
        let summary = response.text().unwrap_or_default();
        if summary.is_empty() {
            self.emit_event(
                session_id,
                AgentEventKind::Error {
                    message: "Compaction failed: empty summary".to_string(),
                },
            );
            return Err(anyhow::anyhow!("Compaction failed: empty summary"));
        }
        let summary_len = summary.len();

        let compaction_msg = AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: summary.clone(),
                original_token_count: approximate_token_count(llm_messages),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };
        context.add_message(compaction_msg).await?;
        self.emit_event(
            session_id,
            AgentEventKind::CompactionEnd {
                summary: summary.clone(),
                summary_len,
            },
        );
        Ok(())
    }

    fn prepare_snapshot(&self) -> Option<(std::path::PathBuf, SnapshotPolicy)> {
        if self.snapshot_policy == SnapshotPolicy::None {
            return None;
        }
        let root = self.snapshot_root.clone()?;
        Some((root, self.snapshot_policy))
    }

    fn should_snapshot_tool(&self, tool_name: &str) -> bool {
        if self.mutating_tools.contains(tool_name) {
            return true;
        }
        self.assume_mutating
    }

    fn collect_tools(&self, provider: Arc<dyn LLMProvider>) -> Vec<querymt::chat::Tool> {
        let mut tools = Vec::new();
        let config = self.tool_config_snapshot();
        match config.policy {
            ToolPolicy::BuiltInOnly => {
                tools.extend(self.tool_registry.definitions());
            }
            ToolPolicy::ProviderOnly => {
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned());
                }
            }
            ToolPolicy::BuiltInAndProvider => {
                tools.extend(self.tool_registry.definitions());
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned());
                }
            }
        }
        tools
            .into_iter()
            .filter(|tool| is_tool_allowed_with(&config, &tool.function.name))
            .collect()
    }

    fn is_tool_allowed(&self, name: &str) -> bool {
        let config = self.tool_config_snapshot();
        is_tool_allowed_with(&config, name)
    }

    fn emit_event(&self, session_id: &str, kind: AgentEventKind) {
        let event = AgentEvent {
            seq: self.event_seq.fetch_add(1, Ordering::Relaxed),
            timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
            session_id: session_id.to_string(),
            kind,
        };
        let _ = self.event_tx.send(event.clone());
        for observer in &self.event_observers {
            let event = event.clone();
            let observer = Arc::clone(observer);
            tokio::spawn(async move {
                let _ = observer.on_event(&event).await;
            });
        }
    }

    fn tool_config_snapshot(&self) -> ToolConfig {
        self.tool_config
            .lock()
            .map(|config| config.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    Completed,
    Cancelled,
    Stopped(StopReason),
}

struct MiddlewareEffect {
    stop: Option<CycleOutcome>,
    refresh_history: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPolicy {
    None,
    Metadata,
    Diff,
}

impl std::fmt::Display for SnapshotPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SnapshotPolicy::None => "none",
            SnapshotPolicy::Metadata => "metadata",
            SnapshotPolicy::Diff => "diff",
        };
        write!(f, "{}", value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    BuiltInOnly,
    ProviderOnly,
    BuiltInAndProvider,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        ToolPolicy::BuiltInAndProvider
    }
}

#[derive(Clone, Default)]
struct ToolConfig {
    policy: ToolPolicy,
    allowlist: Option<HashSet<String>>,
    denylist: HashSet<String>,
}

fn is_tool_allowed_with(config: &ToolConfig, name: &str) -> bool {
    if config.denylist.contains(name) {
        return false;
    }
    match &config.allowlist {
        Some(allowlist) => allowlist.contains(name),
        None => true,
    }
}

enum SnapshotState {
    None,
    Metadata {
        root: std::path::PathBuf,
    },
    Diff {
        pre_tree: MerkleTree,
        root: std::path::PathBuf,
    },
}

#[async_trait(?Send)]
impl Agent for QueryMTAgent {
    async fn initialize(&self, _req: InitializeRequest) -> Result<InitializeResponse, Error> {
        Ok(
            InitializeResponse::new(ProtocolVersion::LATEST).agent_capabilities(
                agent_client_protocol::AgentCapabilities::new()
                    .prompt_capabilities(PromptCapabilities::new().embedded_context(true)),
            ),
        )
    }

    async fn authenticate(&self, _req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, _req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let session_context = self
            .provider
            .with_session(None)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        self.emit_event(
            &session_context.session().id,
            AgentEventKind::SessionCreated,
        );
        Ok(NewSessionResponse::new(
            session_context.session().id.clone(),
        ))
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();

        // 1. Setup Cancellation
        let (tx, rx) = watch::channel(false);
        {
            let mut active = self.active_sessions.lock().await;
            active.insert(session_id.clone(), tx);
        }

        // 2. Get Session Context
        let context = self
            .provider
            .with_session(Some(session_id.clone()))
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // 3. Store User Messages
        let content = format_prompt_blocks(&req.prompt, self.max_prompt_bytes);
        self.emit_event(
            &session_id,
            AgentEventKind::PromptReceived {
                content: content.clone(),
            },
        );
        let agent_msg = AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text { content }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        };
        if let Err(e) = context.add_message(agent_msg).await {
            let mut active = self.active_sessions.lock().await;
            active.remove(&session_id);
            self.emit_event(
                &session_id,
                AgentEventKind::Error {
                    message: e.to_string(),
                },
            );
            return Err(Error::new(-32000, e.to_string()));
        }
        self.emit_event(
            &session_id,
            AgentEventKind::UserMessageStored {
                content: format_prompt_blocks(&req.prompt, self.max_prompt_bytes),
            },
        );

        // 4. Execute Agent Loop using Context
        let result = self.execute_cycle(&context, rx).await;

        // 5. Cleanup
        {
            let mut active = self.active_sessions.lock().await;
            active.remove(&session_id);
        }

        match result {
            Ok(CycleOutcome::Completed) => Ok(PromptResponse::new(StopReason::EndTurn)),
            Ok(CycleOutcome::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
            Ok(CycleOutcome::Stopped(stop_reason)) => Ok(PromptResponse::new(stop_reason)),
            Err(e) => Err(Error::new(-32000, e.to_string())),
        }
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();
        self.emit_event(&session_id, AgentEventKind::Cancelled);
        let active = self.active_sessions.lock().await;
        if let Some(tx) = active.get(&session_id) {
            let _ = tx.send(true);
        }
        Ok(())
    }
}

fn format_prompt_blocks(blocks: &[ContentBlock], max_prompt_bytes: Option<usize>) -> String {
    let mut content = String::new();
    for block in blocks {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        match block {
            ContentBlock::Text(text) => {
                content.push_str(&text.text);
            }
            ContentBlock::ResourceLink(link) => {
                content.push_str(&format!(
                    "[Resource: {}] {}\n{}",
                    link.name,
                    link.uri,
                    link.description.clone().unwrap_or_default()
                ));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                agent_client_protocol::EmbeddedResourceResource::TextResourceContents(text) => {
                    content.push_str(&format!("[Embedded Resource: {}]\n{}", text.uri, text.text));
                }
                agent_client_protocol::EmbeddedResourceResource::BlobResourceContents(blob) => {
                    content.push_str(&format!(
                        "[Embedded Resource: {}] (blob, {} bytes)",
                        blob.uri,
                        blob.blob.len()
                    ));
                }
                _ => {
                    content.push_str("[Embedded Resource: unsupported]");
                }
            },
            ContentBlock::Image(image) => {
                content.push_str(&format!(
                    "[Image] mime={}, bytes={}",
                    image.mime_type,
                    image.data.len()
                ));
            }
            ContentBlock::Audio(audio) => {
                content.push_str(&format!(
                    "[Audio] mime={}, bytes={}",
                    audio.mime_type,
                    audio.data.len()
                ));
            }
            _ => {
                content.push_str("[Unsupported content block]");
            }
        }
    }
    if let Some(max_bytes) = max_prompt_bytes {
        truncate_to_bytes(&content, max_bytes)
    } else {
        content
    }
}

fn snapshot_metadata(root: &std::path::Path) -> (MessagePart, Option<String>) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut latest_mtime = 0i128;

    for result in ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .build()
    {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            if let Ok(metadata) = entry.metadata() {
                files += 1;
                bytes += metadata.len();
                if let Ok(mtime) = metadata.modified() {
                    if let Ok(duration) = mtime.duration_since(std::time::UNIX_EPOCH) {
                        let seconds = duration.as_secs() as i128;
                        latest_mtime = latest_mtime.max(seconds);
                    }
                }
            }
        }
    }

    let meta_string = format!("files={files},bytes={bytes},mtime={latest_mtime}");
    let root_hash = blake3::hash(meta_string.as_bytes()).to_hex().to_string();

    let summary = format!("Files: {files}, Bytes: {bytes}, Latest mtime: {latest_mtime}");
    let part = MessagePart::Snapshot {
        root_hash,
        diff_summary: Some(summary.clone()),
    };
    (part, Some(summary))
}

fn approximate_token_count(messages: &[querymt::chat::ChatMessage]) -> usize {
    let mut chars = 0usize;
    for msg in messages {
        chars += msg.content.len();
    }
    (chars / 4).max(1)
}

fn truncate_to_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let note = "\n[truncated]";
    if max_bytes <= note.len() {
        return note[..max_bytes].to_string();
    }

    let mut end = max_bytes - note.len();
    while !input.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut truncated = input[..end].to_string();
    truncated.push_str(note);
    truncated
}
