//! Canonical test fixtures for agent integration tests.
//!
//! Three composable tiers:
//! - [`TestStorage`] — raw storage only (fastest, no agent)
//! - [`TestAgent`] — storage + AgentConfig + AgentHandle

use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::execution::turn::{Turn, TurnEnv};
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::helpers::empty_plugin_registry;
use querymt::LLMParams;
pub use querymt_agent_sqlite::test_support::TestStorage;
use std::sync::Arc;
use tempfile::TempDir;

// ── Tier 2 ── storage + AgentConfig + AgentHandle ────────────────────────────

/// Storage + AgentConfig + AgentHandle for integration tests.
pub struct TestAgent {
    pub storage: Arc<SqliteStorage>,
    pub config: Arc<crate::agent::agent_config::AgentConfig>,
    pub handle: Arc<AgentHandle>,
    pub _tempdir: TempDir,
}

impl TestAgent {
    #[doc(hidden)]
    pub async fn with_mock_provider(provider: crate::test_utils::SharedLlmProvider) -> Self {
        let (registry, tempdir) = crate::test_utils::mock_plugin_registry(Arc::new(
            crate::test_utils::TestProviderFactory::new(provider),
        ))
        .unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let mut config = AgentConfigBuilder::new(
            Arc::new(registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .build();
        config.execution_policy.rate_limit.default_wait_secs = 1;
        config.execution_policy.rate_limit.transport_wait_secs = 1;
        config.execution_policy.rate_limit.jitter_ratio = 0.0;
        let config = Arc::new(config);
        let handle = Arc::new(AgentHandle::from_config(config.clone()));
        Self {
            storage,
            config,
            handle,
            _tempdir: tempdir,
        }
    }

    /// A run over a new session: the read-only [`TurnEnv`] and the mutable
    /// [`Turn`], built as `execute_prompt_inner` builds them.
    pub(crate) async fn turn(&self) -> (TurnEnv, Turn) {
        self.turn_with_mcp(crate::agent::core::McpToolState::empty())
            .await
    }

    /// Like [`Self::turn`], with the given live MCP tool state.
    pub(crate) async fn turn_with_mcp(
        &self,
        mcp_tool_state: Arc<crate::agent::core::McpToolState>,
    ) -> (TurnEnv, Turn) {
        self.turn_with(None, mcp_tool_state).await
    }

    /// Like [`Self::turn`], rooted at `cwd`.
    pub(crate) async fn turn_in(&self, cwd: std::path::PathBuf) -> (TurnEnv, Turn) {
        self.turn_with(Some(cwd), crate::agent::core::McpToolState::empty())
            .await
    }

    async fn turn_with(
        &self,
        cwd: Option<std::path::PathBuf>,
        mcp_tool_state: Arc<crate::agent::core::McpToolState>,
    ) -> (TurnEnv, Turn) {
        let session_handle = self
            .config
            .provider
            .create_session(None, None, &Default::default())
            .await
            .unwrap();
        let session_id = session_handle.session().public_id.clone();
        let state = crate::session::runtime::RuntimeContext::new(
            self.storage.session_store(),
            session_id.clone(),
        )
        .await
        .unwrap();
        let runtime =
            crate::agent::core::SessionRuntime::new(cwd, Default::default(), mcp_tool_state);
        turn_parts(&self.config, session_handle, runtime, state).await
    }

    /// Minimal agent with in-memory SQLite, no event observer.
    pub async fn new() -> Self {
        let (registry, tempdir) = empty_plugin_registry().expect("empty plugin registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create in-memory sqlite"),
        );
        let builder = AgentConfigBuilder::new(
            Arc::new(registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock"),
        );
        let config = Arc::new(builder.build());
        let handle = Arc::new(AgentHandle::from_config(config.clone()));
        Self {
            storage,
            config,
            handle,
            _tempdir: tempdir,
        }
    }

    pub async fn with_slash_command_registry(
        registry: crate::slash_commands::SlashCommandRegistry,
    ) -> Self {
        let (plugin_registry, tempdir) = empty_plugin_registry().expect("empty plugin registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create in-memory sqlite"),
        );
        let builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock"),
        )
        .with_slash_command_registry(registry);
        let config = Arc::new(builder.build());
        let handle = Arc::new(AgentHandle::from_config(config.clone()));
        Self {
            storage,
            config,
            handle,
            _tempdir: tempdir,
        }
    }

    /// Like `new()` but with event journal wired (previously had event observer).
    ///
    /// Observer was a no-op; now this is identical to `new()` with event journal.
    pub async fn with_observer() -> Self {
        let (registry, tempdir) = empty_plugin_registry().expect("empty plugin registry");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create in-memory sqlite"),
        );
        let builder = AgentConfigBuilder::new(
            Arc::new(registry),
            storage.clone(),
            LLMParams::new().provider("mock").model("mock"),
        );
        let config = Arc::new(builder.build());
        let handle = Arc::new(AgentHandle::from_config(config.clone()));
        Self {
            storage,
            config,
            handle,
            _tempdir: tempdir,
        }
    }

    /// Create a session and return its public ID.
    pub async fn create_session(&self) -> String {
        self.storage
            .session_store()
            .create_session(None, None, None, None)
            .await
            .expect("create session")
            .public_id
    }
}

/// The read-only [`TurnEnv`] and mutable [`Turn`] of a run over `session_handle`,
/// with the defaults of an interactive build-mode run: no steering, no phase
/// reporter, no knowledge store, a fresh cancellation token and cycle accounting.
pub(crate) async fn turn_parts(
    config: &crate::agent::agent_config::AgentConfig,
    session_handle: crate::session::provider::SessionHandle,
    runtime: Arc<crate::agent::core::SessionRuntime>,
    state: crate::session::runtime::RuntimeContext,
) -> (TurnEnv, Turn) {
    let session_id = session_handle.session().public_id.clone();
    let cycle_accounting = querymt_agent_services::accounting::CycleAccounting::begin(
        runtime
            .accounting(&session_id, &config.event_sink)
            .await
            .unwrap(),
    );
    let tool_config = crate::agent::core::ToolConfig::default();
    let tool_set = TurnEnv::build_tool_set(config, &runtime, &tool_config);
    // A run resolves its model before its environment exists, as the session
    // actor does. A fixture without a registered provider has none to
    // resolve: it gets an inert one that declares nothing and answers no
    // request (mockall panics on an unexpected call), for the tests that use
    // the environment for something else.
    let model = match session_handle.resolve_model().await {
        Ok(model) => model,
        Err(_) => crate::session::provider::ResolvedModel {
            provider: Arc::new(crate::test_utils::MockLlmProvider::new()),
            profile: Arc::new(crate::middleware::ModelProfile::default()),
            provenance: None,
        },
    };
    let env = TurnEnv {
        session_id,
        turn_id: querymt_agent_core::model::TurnId::new(),
        runtime,
        session_handle,
        model,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tool_config,
        knowledge_store: None,
        repository_knowledge: None,
        event_sink: None,
        workspace_query_bridge: None,
        turn_mode: crate::agent::core::AgentMode::Build,
        steering: None,
        phase_reporter: None,
        cycle_accounting,
        tool_set,
    };
    (env, Turn::new(state))
}

impl TurnEnv {
    /// This env with its tool set built from `config`, for tests that run a
    /// step under a config other than the one the fixture was created with.
    pub(crate) fn tools_from(mut self, config: &crate::agent::agent_config::AgentConfig) -> Self {
        self.tool_set = Self::build_tool_set(config, &self.runtime, &self.tool_config);
        self
    }
}
