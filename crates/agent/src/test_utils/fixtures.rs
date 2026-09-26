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
