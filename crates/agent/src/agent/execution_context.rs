//! Execution context bundling all per-run state for agent execution

use crate::agent::core::{SessionRuntime, ToolConfig};
use crate::model::AgentMessage;
use crate::session::error::SessionResult;
use crate::session::provider::SessionHandle;
use crate::session::runtime::RuntimeContext;
use crate::session::store::{LLMConfig, SessionExecutionConfig};
use crate::tools::AgentToolContext;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
        }
    }

    /// Attach a cancellation token, replacing the default no-op token.
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = token;
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
    /// The cancellation token is propagated so tools can abort long-running work.
    pub fn tool_context(
        &self,
        agent_registry: Arc<dyn crate::delegation::AgentRegistry>,
        elicitation_tx: Option<tokio::sync::mpsc::Sender<crate::tools::ElicitationRequest>>,
    ) -> AgentToolContext {
        AgentToolContext::new(
            self.session_id.clone(),
            self.cwd().map(|p| p.to_path_buf()),
            Some(agent_registry),
            elicitation_tx,
        )
        .with_cancellation_token(self.cancellation_token.clone())
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
        SessionRuntime::new(cwd, HashMap::new(), HashMap::new(), vec![])
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
}
