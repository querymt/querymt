//! Canonical test fixtures for agent integration tests.
//!
//! Three composable tiers:
//! - [`TestStorage`] — raw storage only (fastest, no agent)
//! - [`TestAgent`] — storage + AgentConfig + AgentHandle
//! - [`TestServerState`] — agent + ServerState + connection helpers

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
            storage.session_store(),
            storage.event_journal(),
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
            storage.session_store(),
            storage.event_journal(),
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

// ── Tier 3 ── agent + ServerState ────────────────────────────────────────────

/// Agent + ServerState for UI/handler tests.
#[cfg(feature = "dashboard")]
pub struct TestServerState {
    pub agent: TestAgent,
    pub(crate) state: crate::ui::ServerState,
}

#[cfg(feature = "dashboard")]
impl TestServerState {
    /// Default server state backed by `TestAgent::with_observer()`.
    pub async fn new() -> Self {
        let agent = TestAgent::with_observer().await;
        let state = crate::ui::ServerState {
            agent: agent.handle.clone(),
            view_store: agent.storage.view_store().expect("view store"),
            session_store: agent.storage.session_store(),
            default_cwd: None,
            event_sources: vec![],
            connections: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_agents: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_cwds: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            workspace_manager: crate::index::WorkspaceIndexManagerActor::new(
                crate::index::WorkspaceIndexManagerConfig::default(),
            ),
            model_cache: moka::future::Cache::new(100),
            oauth_flows: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            oauth_callback_listener: Arc::new(tokio::sync::Mutex::new(None)),
        };
        Self { agent, state }
    }

    /// Insert a default connection and return (tx, rx) channel pair.
    pub async fn add_connection(
        &self,
        conn_id: &str,
    ) -> (
        tokio::sync::mpsc::Sender<String>,
        tokio::sync::mpsc::Receiver<String>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut connections = self.state.connections.lock().await;
        connections.insert(
            conn_id.to_string(),
            crate::ui::ConnectionState {
                routing_mode: crate::ui::RoutingMode::Single,
                active_agent_id: "primary".to_string(),
                sessions: std::collections::HashMap::new(),
                subscribed_sessions: std::collections::HashSet::new(),
                session_cursors: std::collections::HashMap::new(),
                current_workspace_root: None,
                file_index_forwarder: None,
            },
        );
        (tx, rx)
    }
}
