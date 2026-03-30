//! Execution context bundling all per-run state for agent execution

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::core::{SessionRuntime, ToolConfig};
use crate::knowledge::KnowledgeStore;
use crate::model::AgentMessage;
use crate::session::error::SessionResult;
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::{LLMConfig, SessionExecutionConfig};
use crate::tools::AgentToolContext;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Describes the origin of an execution cycle.
///
/// Used to correlate scheduled executions with the `SchedulerActor` so that
/// terminal events (`ScheduledExecutionCompleted` / `ScheduledExecutionFailed`)
/// carry the correct `schedule_public_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOrigin {
    /// Interactive prompt from a user or delegation.
    Interactive,
    /// Prompt fired by the scheduler for a recurring schedule.
    Scheduled {
        /// Public ID of the schedule that triggered this execution.
        schedule_public_id: String,
    },
}

/// Bundles all execution state needed for a single agent run.
///
/// This struct consolidates what was previously passed as separate parameters:
/// - `session_id: &str`
/// - `runtime: Option<&SessionRuntime>` (infrastructure state)
/// - `runtime_context: &RuntimeContext` (domain state)
/// - `session_handle: SessionHandle` (turn-pinned session + provider)
///
/// The `session_handle` is resolved once at the start of `run_prompt` and reused
/// for the entire turn. This eliminates redundant `with_session()` calls (DB
/// lookups) that previously happened on every state-machine transition.
pub(crate) struct ExecutionContext {
    /// The session ID for this execution
    pub session_id: String,

    /// Per-session runtime infrastructure (cwd, MCP tools, snapshots, etc.)
    pub runtime: Arc<SessionRuntime>,

    /// Per-run domain state (active task, intent, progress recording)
    pub state: RuntimeContext,

    /// Turn-pinned session handle. Created once per `run_prompt` and reused
    /// throughout execution — avoids repeated DB lookups for session/config.
    pub session_handle: SessionHandle,

    /// Cancellation token for this execution. Cancelled when the session receives
    /// a cancel signal. Propagated into tool contexts so individual tools can
    /// abort long-running work cooperatively.
    pub cancellation_token: CancellationToken,

    /// Per-session tool configuration snapshot. Captured from `SessionActor.tool_config`
    /// at the start of each turn so that runtime mutations via `SetAllowedTools`,
    /// `SetDeniedTools`, and `SetToolPolicy` messages are honoured during execution.
    /// Falls back to `AgentConfig.tool_config` when not overridden.
    pub tool_config: ToolConfig,

    /// Origin of this execution cycle — interactive (user/delegation) or scheduled.
    ///
    /// When `Scheduled`, the `SessionActor` emits explicit terminal events
    /// (`ScheduledExecutionCompleted` / `ScheduledExecutionFailed`) so the
    /// `SchedulerActor` can correlate cycle completion without relying on
    /// generic task events.
    pub execution_origin: ExecutionOrigin,

    /// Optional knowledge store, propagated into tool contexts so knowledge
    /// tools (`knowledge_ingest`, `knowledge_query`, etc.) can access it.
    pub knowledge_store: Option<Arc<dyn KnowledgeStore>>,

    /// Optional event sink, propagated into tool contexts so tools can emit
    /// agent events (e.g. `KnowledgeIngested`, `KnowledgeConsolidated`).
    pub event_sink: Option<Arc<crate::event_sink::EventSink>>,

    /// Optional workspace query bridge for VS Code language intelligence.
    /// When set, the `language_query` tool can access diagnostics, references,
    /// definitions, symbols, hover docs, and type definitions through the
    /// editor's language server.
    pub workspace_query_bridge: Option<ClientBridgeSender>,
}

impl ExecutionContext {
    /// Create a new execution context
    pub fn new(
        session_id: String,
        runtime: Arc<SessionRuntime>,
        state: RuntimeContext,
        session_handle: SessionHandle,
        tool_config: ToolConfig,
    ) -> Self {
        Self {
            session_id,
            runtime,
            state,
            session_handle,
            cancellation_token: CancellationToken::new(),
            tool_config,
            execution_origin: ExecutionOrigin::Interactive,
            knowledge_store: None,
            event_sink: None,
            workspace_query_bridge: None,
        }
    }

    /// Attach a cancellation token, replacing the default no-op token.
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
        self
    }

    /// Set the execution origin for this context.
    pub fn with_execution_origin(mut self, origin: ExecutionOrigin) -> Self {
        self.execution_origin = origin;
        self
    }

    /// Set the knowledge store for this context.
    pub fn with_knowledge_store(mut self, store: Option<Arc<dyn KnowledgeStore>>) -> Self {
        self.knowledge_store = store;
        self
    }

    /// Set the event sink for this context.
    pub fn with_event_sink(mut self, sink: Arc<crate::event_sink::EventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Set the workspace query bridge for language intelligence queries.
    ///
    /// When set, the `language_query` tool can access VS Code's language APIs
    /// (diagnostics, references, definitions, etc.) through this bridge.
    pub fn with_workspace_query_bridge(mut self, bridge: Option<ClientBridgeSender>) -> Self {
        self.workspace_query_bridge = bridge;
        self
    }

    /// Get the current working directory, if set
    pub fn cwd(&self) -> Option<&Path> {
        self.runtime.cwd.as_deref()
    }

    /// Persist an `AgentMessage` via the turn-pinned session handle.
    pub async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.session_handle.add_message(message).await
    }

    /// Get the turn-pinned LLM config (provider name, model, params).
    pub fn llm_config(&self) -> Option<&LLMConfig> {
        self.session_handle.llm_config()
    }

    /// Get the turn-pinned execution config snapshot.
    pub fn execution_config(&self) -> Option<&SessionExecutionConfig> {
        self.session_handle.execution_config()
    }

    /// Create a tool execution context from this execution context
    ///
    /// This centralizes the construction of `AgentToolContext` from the execution state,
    /// extracting the necessary fields (session_id, cwd, agent_registry) in one place.
    /// The cancellation token and knowledge store are propagated so tools can abort
    /// long-running work and access the knowledge layer.
    pub fn tool_context(
        &self,
        agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
        elicitation_tx: Option<tokio::sync::mpsc::Sender<crate::tools::ElicitationRequest>>,
    ) -> AgentToolContext {
        let mut ctx = AgentToolContext::new(
            self.session_id.clone(),
            self.cwd().map(|p| p.to_path_buf()),
            Some(agent_registry),
            elicitation_tx,
        )
        .with_cancellation_token(self.cancellation_token.clone());

        if let Some(ref ks) = self.knowledge_store {
            ctx.with_knowledge_store(ks.clone());
        }
        if let Some(ref sink) = self.event_sink {
            ctx.with_event_sink(sink.clone());
        }
        if let Some(ref bridge) = self.workspace_query_bridge {
            ctx = ctx.with_workspace_query_bridge(bridge.clone());
        }

        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::core::SessionRuntime;
    use crate::session::backend::StorageBackend;
    use crate::session::provider::SessionProvider;
    use crate::session::runtime::RuntimeContext;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::helpers::empty_plugin_registry;
    use querymt::LLMParams;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn make_runtime(cwd: Option<PathBuf>) -> Arc<SessionRuntime> {
        SessionRuntime::new(
            cwd,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        )
    }

    async fn make_context_parts() -> (Arc<SessionRuntime>, RuntimeContext, SessionHandle) {
        let (registry, _td) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock");
        let store: Arc<dyn crate::session::store::SessionStore> = storage.session_store();
        let provider = Arc::new(SessionProvider::new(Arc::new(registry), store.clone(), llm));
        let session = store.create_session(None, None, None, None).await.unwrap();
        let session_id = session.public_id.clone();
        let runtime_ctx = RuntimeContext::new(store, session_id).await.unwrap();
        let handle = SessionHandle::new(provider, session).await.unwrap();
        let runtime = make_runtime(None);
        (runtime, runtime_ctx, handle)
    }

    async fn make_context_with_cwd(
        cwd: PathBuf,
    ) -> (Arc<SessionRuntime>, RuntimeContext, SessionHandle) {
        let (registry, _td) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock");
        let store: Arc<dyn crate::session::store::SessionStore> = storage.session_store();
        let provider = Arc::new(SessionProvider::new(Arc::new(registry), store.clone(), llm));
        let session = store
            .create_session(None, Some(cwd.clone()), None, None)
            .await
            .unwrap();
        let session_id = session.public_id.clone();
        let runtime_ctx = RuntimeContext::new(store, session_id).await.unwrap();
        let handle = SessionHandle::new(provider, session).await.unwrap();
        let runtime = make_runtime(Some(cwd));
        (runtime, runtime_ctx, handle)
    }

    #[tokio::test]
    async fn construction_sets_session_id() {
        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let session_id = handle.session().public_id.clone();
        let ctx = ExecutionContext::new(
            session_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );
        assert_eq!(ctx.session_id, session_id);
    }

    #[tokio::test]
    async fn cwd_returns_none_when_not_set() {
        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );
        assert!(ctx.cwd().is_none());
    }

    #[tokio::test]
    async fn cwd_returns_path_when_set() {
        let cwd = PathBuf::from("/tmp/my-workspace");
        let (runtime, runtime_ctx, handle) = make_context_with_cwd(cwd.clone()).await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );
        assert_eq!(ctx.cwd(), Some(cwd.as_path()));
    }

    #[tokio::test]
    async fn with_cancellation_token_replaces_default() {
        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let token = CancellationToken::new();
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        )
        .with_cancellation_token(token.clone());

        // Verify the token is the one we set — cancelling it should reflect
        assert!(!ctx.cancellation_token.is_cancelled());
        token.cancel();
        assert!(ctx.cancellation_token.is_cancelled());
    }

    #[tokio::test]
    async fn llm_config_returns_none_for_fresh_session() {
        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );
        // Fresh session has no LLM config assigned
        assert!(ctx.llm_config().is_none());
    }

    #[tokio::test]
    async fn execution_config_returns_none_for_fresh_session() {
        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );
        // Fresh session has no execution config stored
        assert!(ctx.execution_config().is_none());
    }

    #[tokio::test]
    async fn tool_context_propagates_knowledge_store() {
        use crate::knowledge::KnowledgeStore;
        use crate::knowledge::sqlite::SqliteKnowledgeStore;
        use crate::test_utils::sqlite_conn_with_schema;
        use crate::tools::context::ToolContext;

        let db = sqlite_conn_with_schema();
        let knowledge_store: Arc<dyn KnowledgeStore> = Arc::new(SqliteKnowledgeStore::new(db));

        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        )
        .with_knowledge_store(Some(knowledge_store.clone()));

        let agent_registry = Arc::new(crate::delegation::DefaultAgentRegistry::new());
        let tool_ctx = ctx.tool_context(agent_registry, None);

        // The knowledge_store must be propagated to the tool context
        assert!(
            tool_ctx.knowledge_store().is_some(),
            "knowledge_store should be Some when set on ExecutionContext"
        );
    }

    #[tokio::test]
    async fn tool_context_no_knowledge_store_when_not_set() {
        use crate::tools::context::ToolContext;

        let (runtime, runtime_ctx, handle) = make_context_parts().await;
        let ctx = ExecutionContext::new(
            handle.session().public_id.clone(),
            runtime,
            runtime_ctx,
            handle,
            ToolConfig::default(),
        );

        let agent_registry = Arc::new(crate::delegation::DefaultAgentRegistry::new());
        let tool_ctx = ctx.tool_context(agent_registry, None);

        // Without knowledge_store, tool context should return None
        assert!(
            tool_ctx.knowledge_store().is_none(),
            "knowledge_store should be None when not set"
        );
    }
}
