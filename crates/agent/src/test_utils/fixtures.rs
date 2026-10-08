//! Canonical test fixtures for agent integration tests.
//!
//! Three composable tiers:
//! - [`TestStorage`] — raw storage only (fastest, no agent)
//! - [`TestAgent`] — storage + AgentConfig + AgentHandle

use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::session::backend::StorageBackend;
use crate::session::sqlite_storage::SqliteStorage;
use crate::test_utils::helpers::empty_plugin_registry;
use querymt::LLMParams;
use std::sync::Arc;
use tempfile::TempDir;

// ── Tier 1 ── raw storage ────────────────────────────────────────────────────

/// In-memory SQLite storage with no agent.
pub struct TestStorage {
    pub storage: Arc<SqliteStorage>,
    pub _tempdir: TempDir,
}

impl TestStorage {
    pub async fn new() -> Self {
        let tempdir = TempDir::new().expect("create temp dir");
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("create in-memory sqlite"),
        );
        Self {
            storage,
            _tempdir: tempdir,
        }
    }

    pub fn session_store(&self) -> Arc<dyn crate::session::store::SessionStore> {
        self.storage.session_store()
    }

    pub fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
        self.storage.event_journal()
    }
}

// ── Tier 2 ── storage + AgentConfig + AgentHandle ────────────────────────────

/// Storage + AgentConfig + AgentHandle for integration tests.
pub struct TestAgent {
    pub storage: Arc<SqliteStorage>,
    pub config: Arc<crate::agent::agent_config::AgentConfig>,
    pub handle: Arc<AgentHandle>,
    pub _tempdir: TempDir,
}

impl TestAgent {
    pub(crate) async fn with_mock_provider(provider: crate::test_utils::SharedLlmProvider) -> Self {
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

    pub(crate) async fn execution_context(
        &self,
    ) -> crate::agent::execution_context::ExecutionContext {
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
        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            Default::default(),
            crate::agent::core::McpToolState::empty(),
        );
        crate::agent::execution_context::ExecutionContext::new(
            session_id,
            runtime,
            state,
            session_handle,
            Default::default(),
        )
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
