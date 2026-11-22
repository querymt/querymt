//! Helper functions for creating test fixtures

/// In-memory metric capture for tests. Installs one process-wide meter provider
/// on first use (run tests with `cargo nextest run`, one process per test);
/// assertions should compare counter deltas so they stay independent of other
/// calls recorded in the same process.
pub(crate) mod genai_metrics {
    use opentelemetry::Value;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use std::sync::OnceLock;

    static STATE: OnceLock<(InMemoryMetricExporter, SdkMeterProvider)> = OnceLock::new();

    fn state() -> &'static (InMemoryMetricExporter, SdkMeterProvider) {
        STATE.get_or_init(|| {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            opentelemetry::global::set_meter_provider(provider.clone());
            (exporter, provider)
        })
    }

    /// Install the capture provider. Call before the code under test records.
    pub(crate) fn install() {
        let _ = state();
    }

    /// Number of recorded samples of f64 histogram `name` for `model`. With
    /// `error = None` only points without an `error.type` attribute count;
    /// otherwise only points carrying exactly that `error.type`.
    pub(crate) fn histogram_count(name: &str, model: &str, error: Option<&str>) -> u64 {
        let (exporter, provider) = state();
        exporter.reset();
        provider.force_flush().expect("flush metrics");
        let mut total = 0;
        for resource in exporter.get_finished_metrics().expect("finished metrics") {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics().filter(|metric| metric.name() == name) {
                    if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data()
                    {
                        for point in histogram.data_points() {
                            let attrs: Vec<_> = point.attributes().collect();
                            let find = |key: &str| {
                                attrs
                                    .iter()
                                    .find(|kv| kv.key.as_str() == key)
                                    .map(|kv| &kv.value)
                            };
                            let model_matches = find("gen_ai.request.model")
                                == Some(&Value::from(model.to_owned()));
                            let error_matches = match error {
                                None => find("error.type").is_none(),
                                Some(kind) => {
                                    find("error.type") == Some(&Value::from(kind.to_owned()))
                                }
                            };
                            if model_matches && error_matches {
                                total += point.count();
                            }
                        }
                    }
                }
            }
        }
        total
    }

    /// Current cumulative value of counter `name` summed over every data point
    /// whose attributes include all of `want`.
    pub(crate) fn sum(name: &str, want: &[(&str, Value)]) -> u64 {
        let (exporter, provider) = state();
        exporter.reset();
        provider.force_flush().expect("flush metrics");
        let mut total = 0;
        for resource in exporter.get_finished_metrics().expect("finished metrics") {
            for scope in resource.scope_metrics() {
                for metric in scope.metrics().filter(|metric| metric.name() == name) {
                    if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                        for point in sum.data_points() {
                            let attrs: Vec<_> = point.attributes().collect();
                            if want.iter().all(|(key, value)| {
                                attrs
                                    .iter()
                                    .any(|kv| kv.key.as_str() == *key && &kv.value == value)
                            }) {
                                total += point.value();
                            }
                        }
                    }
                }
            }
        }
        total
    }
}

/// Deterministic incompressible bytes (xorshift), so test files do not deflate.
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A ledger over a fresh in-memory journal for tests of components that
/// require explicit recording but do not assert on it.
pub async fn test_ledger() -> querymt_agent_services::accounting::DispatchLedger {
    use crate::session::backend::StorageBackend;
    let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
        .await
        .unwrap();
    let journal = storage.event_journal();
    let accounting =
        querymt_agent_services::accounting::SessionAccounting::hydrate("session", journal.clone())
            .await
            .unwrap();
    let stream = querymt_agent_services::session_event_stream::SessionEventStream::new(
        "session",
        journal,
        std::sync::Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
    );
    let recorder = std::sync::Arc::new(
        querymt_agent_services::accounting::AttemptRecorder::with_projection(
            stream,
            accounting.clone(),
        ),
    );
    let cycle = querymt_agent_services::accounting::CycleAccounting::with_id(accounting, "cycle-1");
    querymt_agent_services::accounting::DispatchLedger {
        operation: std::sync::Arc::new(querymt_agent_services::accounting::AttemptOperation::new(
            recorder,
            &cycle,
            querymt_agent_services::accounting::AttemptPurpose::MainResponse,
            "p",
            "m",
        )),
        basis: querymt_agent_services::accounting::BillingBasis {
            billing: querymt::providers::BillingClass::Unknown,
            pricing: None,
        },
    }
}

use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::delegation::AgentInfo;
use crate::middleware::{ToolCall, ToolFunction};
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::{LLMConfig, Session};
use crate::test_utils::mocks::{TestPluginLoader, TestProviderFactory};
use anyhow::Result;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use querymt::plugin::host::PluginRegistry;
use querymt_agent_core::model::{AgentMessage, MessagePart};
use querymt_agent_services::snapshot::backend::SnapshotBackend;
use querymt_agent_services::snapshot::git::GitSnapshotBackend;
pub use querymt_agent_sqlite::test_support::sqlite_conn_with_schema;
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;
use uuid::Uuid;

/// Creates a mock tool call for middleware/state tests
pub fn mock_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        function: ToolFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
        invalid_input: false,
    }
}

/// Creates a mock querymt tool call for execution tests
pub fn mock_querymt_tool_call(id: &str, name: &str, args: &str) -> querymt::ToolCall {
    querymt::ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: querymt::FunctionCall {
            name: name.to_string(),
            arguments: args.to_string(),
        },
    }
}

/// Creates a mock session for testing
pub fn mock_session(session_id: &str) -> Session {
    Session {
        id: 1,
        public_id: session_id.to_string(),
        name: None,
        cwd: None,
        created_at: Some(OffsetDateTime::now_utc()),
        updated_at: Some(OffsetDateTime::now_utc()),
        current_intent_snapshot_id: None,
        active_task_id: None,
        llm_config_id: Some(1),
        parent_session_id: None,
        fork_origin: None,
        session_kind: None,
        fork_point_type: None,
        fork_point_ref: None,
        fork_instructions: None,
    }
}

/// Creates a mock LLM configuration for testing
pub fn mock_llm_config() -> LLMConfig {
    LLMConfig {
        id: 1,
        name: Some("test-config".to_string()),
        provider: "mock".to_string(),
        model: "mock-model".to_string(),
        protocol: String::new(),
        endpoint: String::new(),
        params: None,
        created_at: Some(OffsetDateTime::now_utc()),
        updated_at: Some(OffsetDateTime::now_utc()),
        provider_node_id: None,
    }
}

/// Creates an empty plugin registry for tests that don't need LLM calls.
/// Useful for undo/snapshot tests, middleware tests, etc.
///
/// # Returns
/// A tuple of (PluginRegistry, TempDir) - the TempDir must be kept alive
/// for the duration of the test.
pub fn empty_plugin_registry() -> Result<(PluginRegistry, TempDir)> {
    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("providers.toml");
    std::fs::write(&config_path, "providers = []\n")?;
    let registry = plugin_registry_from_path(&config_path)?;
    Ok((registry, temp_dir))
}

/// Load a fixture's providers and keep the plugin cache beside its config.
pub fn plugin_registry_from_path(path: &std::path::Path) -> Result<PluginRegistry> {
    querymt_utils::secret_store::SecretStore::install_test_store();
    let config = querymt::plugin::host::PluginConfig::from_path(path)?;
    Ok(PluginRegistry::from_config_with_cache_path(
        config,
        path.with_extension("cache"),
    )?)
}

#[cfg(test)]
#[test]
fn plugin_fixture_does_not_fall_back_when_local_paths_fail() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("providers.toml");
    let cache = config.with_extension("cache");
    assert!(plugin_registry_from_path(&config).is_err());
    assert!(!cache.exists());

    std::fs::write(&config, "providers = []\n").unwrap();
    std::fs::write(&cache, "not a directory").unwrap();
    assert!(plugin_registry_from_path(&config).is_err());
    assert!(cache.is_file());
}

/// Creates a plugin registry with a mock provider for LLM interaction tests.
///
/// # Returns
/// A tuple of (PluginRegistry, TempDir) - the TempDir must be kept alive
/// for the duration of the test.
pub fn register_mock_provider(registry: &PluginRegistry) {
    let provider = Arc::new(tokio::sync::Mutex::new(
        crate::test_utils::MockLlmProvider::new(),
    ));
    registry.register_static(Arc::new(TestProviderFactory::new(
        crate::test_utils::SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        },
    )));
}

/// Renders a filesystem path as a quoted TOML string value (including the
/// surrounding quotes), escaping it correctly for the TOML grammar.
///
/// Interpolating `path.display()` into `"..."` is wrong on Windows: TOML basic
/// strings treat `\` as an escape, so `C:\Users\...` fails to parse (`\U`).
pub fn toml_path_value(path: &std::path::Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}

/// Builds a [`PluginRegistry`] with a single `mock` provider backed by `factory`,
/// writing its config into `dir`.
pub fn mock_plugin_registry_in(
    dir: &std::path::Path,
    factory: Arc<TestProviderFactory>,
) -> Result<PluginRegistry> {
    mock_plugin_registry_for(dir, factory)
}

/// Like [`mock_plugin_registry_in`] for any provider factory that names itself
/// `mock`.
pub fn mock_plugin_registry_for(
    dir: &std::path::Path,
    factory: Arc<dyn querymt::plugin::LLMProviderFactory>,
) -> Result<PluginRegistry> {
    let wasm_path = dir.join("mock.wasm");
    std::fs::write(&wasm_path, "")?;
    let config_path = dir.join("providers.toml");
    std::fs::write(
        &config_path,
        format!(
            "[[providers]]\nname = \"mock\"\npath = {}\n",
            toml_path_value(&wasm_path)
        ),
    )?;

    let mut registry = plugin_registry_from_path(&config_path)?;
    registry.register_loader(Box::new(TestPluginLoader { factory }));
    Ok(registry)
}

pub fn mock_plugin_registry(
    factory: Arc<TestProviderFactory>,
) -> Result<(PluginRegistry, TempDir)> {
    let temp_dir = TempDir::new()?;
    let registry = mock_plugin_registry_in(temp_dir.path(), factory)?;
    Ok((registry, temp_dir))
}

/// Test fixture for undo/snapshot integration tests.
/// Provides a complete agent setup with snapshot support.
pub struct UndoTestFixture {
    pub worktree: TempDir,
    pub snapshot_base: TempDir,
    pub config_dir: TempDir,
    pub storage: Arc<SqliteStorage>,
    pub(crate) handle: AgentHandle,
    pub backend: GitSnapshotBackend,
    /// Workspace index handed to every session actor registered afterwards.
    workspace: Option<querymt_agent_index::WorkspaceHandle>,
}

impl UndoTestFixture {
    /// Creates a new test fixture with snapshot support enabled.
    pub async fn new() -> Result<Self> {
        let (registry, config_dir) = empty_plugin_registry()?;
        Self::with_registry(registry, config_dir, None, Vec::new()).await
    }

    /// Creates a fixture whose LLM is `provider`, so prompts run end to end
    /// (real turn snapshots, real tool calls) instead of hand-written records.
    pub(crate) async fn with_provider(
        provider: crate::test_utils::SharedLlmProvider,
    ) -> Result<Self> {
        let (registry, config_dir) =
            mock_plugin_registry(Arc::new(TestProviderFactory::new(provider)))?;
        Self::with_registry(registry, config_dir, None, Vec::new()).await
    }

    /// Like [`Self::with_provider`], with `middleware` installed on the agent,
    /// in the given order.
    pub(crate) async fn with_provider_and_middleware(
        provider: crate::test_utils::SharedLlmProvider,
        middleware: Vec<Arc<dyn crate::middleware::Middleware>>,
    ) -> Result<Self> {
        let (registry, config_dir) =
            mock_plugin_registry(Arc::new(TestProviderFactory::new(provider)))?;
        Self::with_registry(registry, config_dir, None, middleware).await
    }

    /// Index the worktree as it is now.
    pub(crate) async fn build_workspace_index(
        &self,
    ) -> Result<querymt_agent_index::WorkspaceHandle> {
        use querymt_agent_index::{FileIndexConfig, FunctionIndexConfig, WorkspaceIndexActor};
        Ok(WorkspaceIndexActor::create(
            self.worktree.path().to_path_buf(),
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await?)
    }

    /// Index the worktree as it is now and give the index to every session
    /// created afterwards, as a materialized session gets its own.
    pub(crate) async fn index_workspace(&mut self) -> Result<()> {
        self.workspace = Some(self.build_workspace_index().await?);
        Ok(())
    }

    /// Create a session in the worktree whose runtime has no workspace index
    /// yet. Setting the returned cell later makes the index ready, as the
    /// background build of a materialized session does.
    pub(crate) async fn create_session_with_pending_index(
        &self,
    ) -> Result<(
        String,
        Arc<tokio::sync::OnceCell<querymt_agent_index::WorkspaceHandle>>,
    )> {
        let cwd = self.worktree.path();
        let session = self
            .storage
            .session_store()
            .create_session(None, Some(cwd.to_path_buf()), None, None)
            .await?;
        let cell = self
            .register_session_actor_with_cwd(&session.public_id, cwd)
            .await;
        Ok((session.public_id, cell))
    }

    /// A fixture whose LLM is built by `factory` (a provider that is not a
    /// [`SharedLlmProvider`]), with `hooks` and `middleware` installed.
    pub(crate) async fn with_factory(
        factory: Arc<dyn querymt::plugin::LLMProviderFactory>,
        hooks: Option<crate::hooks::Hooks>,
        middleware: Vec<Arc<dyn crate::middleware::Middleware>>,
    ) -> Result<Self> {
        let config_dir = TempDir::new()?;
        let registry = mock_plugin_registry_for(config_dir.path(), factory)?;
        Self::with_registry(registry, config_dir, hooks, middleware).await
    }

    /// Like [`Self::with_provider`], with `hooks` installed on the agent.
    pub(crate) async fn with_provider_and_hooks(
        provider: crate::test_utils::SharedLlmProvider,
        hooks: crate::hooks::Hooks,
    ) -> Result<Self> {
        let (registry, config_dir) =
            mock_plugin_registry(Arc::new(TestProviderFactory::new(provider)))?;
        Self::with_registry(registry, config_dir, Some(hooks), Vec::new()).await
    }

    async fn with_registry(
        registry: PluginRegistry,
        config_dir: TempDir,
        hooks: Option<crate::hooks::Hooks>,
        middleware: Vec<Arc<dyn crate::middleware::Middleware>>,
    ) -> Result<Self> {
        let worktree = TempDir::new()?;
        let snapshot_base = TempDir::new()?;

        let registry = Arc::new(registry);

        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);
        let llm = LLMParams::new().provider("mock").model("mock");

        let snapshot_base_path = snapshot_base.path().to_path_buf();
        let backend = GitSnapshotBackend::new().with_snapshot_base(snapshot_base_path.clone());
        let backend_arc = Arc::new(backend);

        let mut builder = AgentConfigBuilder::new(registry, storage.clone(), llm)
            .with_snapshot_policy(SnapshotPolicy::Diff)
            .with_snapshot_backend(backend_arc.clone());
        if let Some(hooks) = hooks {
            builder = builder.with_hooks(hooks);
        }
        if !middleware.is_empty() {
            builder = builder.with_middlewares(middleware);
        }

        let config = Arc::new(builder.build());
        let handle = AgentHandle::from_config(config);

        Ok(Self {
            worktree,
            snapshot_base,
            config_dir,
            storage,
            handle,
            backend: GitSnapshotBackend::new().with_snapshot_base(snapshot_base_path),
            workspace: None,
        })
    }

    /// Create a session and return its public ID
    pub async fn create_session(&self) -> Result<String> {
        self.create_session_with_cwd(self.worktree.path()).await
    }

    /// Create a session bound to an explicit workspace cwd and return its
    /// public ID. The session actor is registered against the same cwd so
    /// undo/redo operate on that workspace.
    pub async fn create_session_with_cwd(&self, cwd: &std::path::Path) -> Result<String> {
        let session = self
            .storage
            .session_store()
            .create_session(None, Some(cwd.to_path_buf()), None, None)
            .await?;

        // Spawn a SessionActor via the kameo registry
        self.register_session_actor_with_cwd(&session.public_id, cwd)
            .await;

        Ok(session.public_id)
    }

    /// Create a child session (for delegation tests)
    pub async fn create_child_session(&self, parent_id: &str) -> Result<String> {
        let session = self
            .storage
            .session_store()
            .create_session(
                None,
                Some(self.worktree.path().to_path_buf()),
                Some(parent_id.to_string()),
                Some(ForkOrigin::Delegation),
            )
            .await?;

        // Spawn a SessionActor via the kameo registry
        self.register_session_actor(&session.public_id).await;

        Ok(session.public_id)
    }

    /// Helper to spawn a SessionActor for a session (needed for undo/redo via kameo)
    async fn register_session_actor(&self, session_id: &str) {
        self.register_session_actor_with_cwd(session_id, self.worktree.path())
            .await;
    }

    /// Spawn a SessionActor whose runtime cwd binds to an explicit workspace
    pub(crate) async fn register_session_actor_with_cwd(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> Arc<tokio::sync::OnceCell<querymt_agent_index::WorkspaceHandle>> {
        self.register_session_actor_with_mcp(
            session_id,
            cwd,
            crate::agent::core::McpToolState::empty(),
        )
        .await
    }

    /// Like [`Self::register_session_actor_with_cwd`], with the session's MCP
    /// tool state supplied by the caller, who can change it while a run is going.
    pub(crate) async fn register_session_actor_with_mcp(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        mcp: Arc<crate::agent::core::McpToolState>,
    ) -> Arc<tokio::sync::OnceCell<querymt_agent_index::WorkspaceHandle>> {
        use crate::agent::session_actor::SessionActor;
        use kameo::actor::Spawn;

        let runtime = crate::agent::core::SessionRuntime::new(
            Some(cwd.to_path_buf()),
            std::collections::HashMap::new(),
            mcp,
        );
        if let Some(workspace) = &self.workspace {
            assert!(
                runtime.workspace_handle.set(workspace.clone()).is_ok(),
                "a new runtime has no workspace index"
            );
        }

        let workspace_cell = runtime.workspace_handle.clone();

        let actor = SessionActor::new(self.handle.config.clone(), session_id.to_string(), runtime);
        let actor_ref = SessionActor::spawn(actor);

        let mut registry = self.handle.registry.lock().await;
        registry.insert(session_id.to_string(), actor_ref);
        workspace_cell
    }

    /// Simulate a user message and return its ID
    pub async fn add_user_message(&self, session_id: &str, content: &str) -> Result<String> {
        let msg_id = Uuid::new_v4().to_string();
        let msg = AgentMessage {
            id: msg_id.clone(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: content.to_string(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;
        Ok(msg_id)
    }

    /// Simulate a user message with explicit timestamp and return its ID
    pub async fn add_user_message_at(
        &self,
        session_id: &str,
        content: &str,
        created_at: i64,
    ) -> Result<String> {
        let msg_id = Uuid::new_v4().to_string();
        let msg = AgentMessage {
            id: msg_id.clone(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: content.to_string(),
            }],
            created_at,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;
        Ok(msg_id)
    }

    /// Simulate an assistant message with explicit timestamp and return its ID
    pub async fn add_assistant_message_at(
        &self,
        session_id: &str,
        content: &str,
        created_at: i64,
    ) -> Result<String> {
        let msg_id = Uuid::new_v4().to_string();
        let msg = AgentMessage {
            id: msg_id.clone(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Text {
                content: content.to_string(),
            }],
            created_at,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;
        Ok(msg_id)
    }

    /// Take a snapshot and record TurnSnapshotStart
    pub async fn take_pre_snapshot(
        &self,
        session_id: &str,
    ) -> Result<(
        querymt_agent_core::model::TurnId,
        querymt_agent_services::snapshot::SnapshotId,
    )> {
        self.take_pre_snapshot_with_cwd(session_id, self.worktree.path())
            .await
    }

    /// Take a snapshot in an explicit workspace and record TurnSnapshotStart
    pub async fn take_pre_snapshot_with_cwd(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> Result<(
        querymt_agent_core::model::TurnId,
        querymt_agent_services::snapshot::SnapshotId,
    )> {
        let snapshot_id = self.backend.track(cwd).await?;
        let turn_id = querymt_agent_core::model::TurnId::new();

        let msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotStart {
                turn_id,
                snapshot_id: snapshot_id.clone(),
                workspace_root: Some(cwd.to_path_buf()),
                inherited: false,
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;

        Ok((turn_id, snapshot_id))
    }

    /// Take post-snapshot and record TurnSnapshotPatch
    pub async fn take_post_snapshot(
        &self,
        session_id: &str,
        turn_id: &querymt_agent_core::model::TurnId,
        pre_snapshot: &querymt_agent_services::snapshot::SnapshotId,
    ) -> Result<Vec<String>> {
        self.take_post_snapshot_with_cwd(session_id, turn_id, pre_snapshot, self.worktree.path())
            .await
    }

    /// Take a post-snapshot in an explicit workspace and record TurnSnapshotPatch
    pub async fn take_post_snapshot_with_cwd(
        &self,
        session_id: &str,
        turn_id: &querymt_agent_core::model::TurnId,
        pre_snapshot: &querymt_agent_services::snapshot::SnapshotId,
        cwd: &std::path::Path,
    ) -> Result<Vec<String>> {
        let post_snapshot = self.backend.track(cwd).await?;
        let changed_paths = self.backend.diff(cwd, pre_snapshot, &post_snapshot).await?;

        let changed_strs: Vec<String> = changed_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotPatch {
                turn_id: *turn_id,
                snapshot_id: post_snapshot,
                base_snapshot_id: Some(pre_snapshot.clone()),
                changed_paths: changed_strs.clone(),
                workspace_root: Some(cwd.to_path_buf()),
                inherited: false,
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;

        Ok(changed_strs)
    }

    /// Convenience: write a file to the worktree
    pub fn write_file(&self, name: &str, content: &str) -> Result<()> {
        std::fs::write(self.worktree.path().join(name), content)?;
        Ok(())
    }

    /// Convenience: write a file under an explicit workspace
    pub fn write_file_in(&self, cwd: &std::path::Path, name: &str, content: &str) -> Result<()> {
        std::fs::write(cwd.join(name), content)?;
        Ok(())
    }

    /// Convenience: read a file from the worktree
    pub fn read_file(&self, name: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.worktree.path().join(name))?)
    }

    /// Convenience: read a file under an explicit workspace
    pub fn read_file_in(&self, cwd: &std::path::Path, name: &str) -> Result<String> {
        Ok(std::fs::read_to_string(cwd.join(name))?)
    }

    /// Send `text` as a real ACP prompt to `session_id` and wait for the turn
    /// (including its post-turn snapshot) to finish.
    pub(crate) async fn prompt(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<crate::acp::protocol::PromptResponse> {
        use crate::send_agent::SendAgent;
        let request = crate::acp::protocol::PromptRequest::new(
            session_id.to_owned(),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(text),
            )],
        );
        SendAgent::prompt(&self.handle, request)
            .await
            .map_err(|e| anyhow::anyhow!("prompt failed: {e:?}"))
    }

    /// Cancel the active run of `session_id`.
    pub(crate) async fn cancel(&self, session_id: &str) -> Result<()> {
        use crate::send_agent::SendAgent;
        let notification = crate::acp::protocol::CancelNotification::new(session_id.to_owned());
        SendAgent::cancel(&self.handle, notification)
            .await
            .map_err(|e| anyhow::anyhow!("cancel failed: {e:?}"))
    }

    /// Like [`Self::prompt`], but also waits until the run this prompt started
    /// has its `RunCompleted` persisted and published.
    ///
    /// The prompt reply is sent before the actor records the run's completion,
    /// so a caller that reads the journal right after `prompt` can miss it.
    /// A durable event is published only after it is persisted, so seeing
    /// `RunCompleted` on the fanout means it is in the journal. The wait is
    /// bound to this prompt's run id, so an earlier run cannot satisfy it.
    pub(crate) async fn prompt_to_completion(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<crate::acp::protocol::PromptResponse> {
        use querymt_agent_core::events::AgentEventKind;
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};

        // Subscribe before prompting so neither event can be missed.
        let mut events = self.handle.config.subscribe_events();
        let response = self.prompt(session_id, text).await?;

        let mut run_id: Option<String> = None;
        let wait = async {
            loop {
                let envelope = match events.try_recv() {
                    Ok(envelope) => envelope,
                    Err(TryRecvError::Empty) => events.recv().await.map_err(|e| match e {
                        RecvError::Lagged(n) => {
                            anyhow::anyhow!("event stream lagged by {n}; cannot confirm the run")
                        }
                        RecvError::Closed => anyhow::anyhow!("event stream closed"),
                    })?,
                    Err(TryRecvError::Lagged(n)) => {
                        anyhow::bail!("event stream lagged by {n}; cannot confirm the run")
                    }
                    Err(TryRecvError::Closed) => anyhow::bail!("event stream closed"),
                };
                if envelope.session_id() != session_id {
                    continue;
                }
                match envelope.kind() {
                    AgentEventKind::RunStarted {
                        run_id: started, ..
                    } if run_id.is_none() => {
                        run_id = Some(started.clone());
                    }
                    AgentEventKind::RunCompleted { run_id: done, .. }
                        if run_id.as_deref() == Some(done.as_str()) =>
                    {
                        return Ok::<(), anyhow::Error>(());
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for the prompt's run to complete"))??;
        Ok(response)
    }

    /// Perform undo operation via AgentHandle
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult, crate::agent::undo::UndoError> {
        self.handle.undo(session_id, message_id).await
    }

    /// Perform redo operation via AgentHandle
    pub async fn redo(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::undo::RedoResult, crate::agent::undo::UndoError> {
        self.handle.redo(session_id).await
    }
}

pub struct DelegateTestFixture {
    pub planner: Arc<AgentHandle>,
    pub planner_storage: Arc<dyn StorageBackend>,
    pub delegate: Arc<AgentHandle>,
    pub delegate_storage: Arc<dyn StorageBackend>,
}

impl DelegateTestFixture {
    /// Creates a new test fixture with snapshot support enabled.
    pub async fn new() -> Result<Self> {
        let (delegate_registry, _delegate_cfg_dir) = empty_plugin_registry()?;
        let delegate_storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);
        let delegate_builder = AgentConfigBuilder::new(
            Arc::new(delegate_registry),
            delegate_storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        );
        let delegate_config = Arc::new(delegate_builder.build());
        let delegate = Arc::new(crate::agent::LocalAgentHandle::from_config(delegate_config));

        let (planner_registry, _config_dir) = empty_plugin_registry()?;
        let planner_registry = Arc::new(planner_registry);
        let planner_storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);

        let roster = crate::delegation::roster::test_roster_on(
            planner_registry.clone(),
            planner_storage.clone(),
        );
        roster
            .add(crate::delegation::roster::Member::built(
                AgentInfo {
                    isolation: Default::default(),
                    id: crate::delegation::roster::AgentId::try_from("coder")?,
                    name: "Coder".to_string(),
                    description: "Delegate coder".to_string(),
                    capabilities: vec![],
                    required_capabilities: vec![],
                    meta: None,
                },
                crate::delegation::roster::MemberOrigin::Config,
                crate::delegation::roster::MemberAgent::Local(delegate.clone()),
            ))
            .map_err(anyhow::Error::from)?;

        let builder = AgentConfigBuilder::new(
            planner_registry,
            planner_storage.clone(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_roster(roster);
        let planner = Arc::new(crate::agent::LocalAgentHandle::from_config(
            builder.build().into(),
        ));

        Ok(Self {
            planner,
            planner_storage,
            delegate,
            delegate_storage,
        })
    }
}

#[cfg(test)]
mod toml_path_value_tests {
    use super::toml_path_value;
    use std::path::Path;

    #[test]
    fn toml_path_value_round_trips_windows_style_paths() {
        let raw = r#"C:\Users\RUNNER~1\AppData\Local\Temp\.tmpfnisBA\it's "x"\mock.wasm"#;
        let doc = format!("path = {}\n", toml_path_value(Path::new(raw)));
        let parsed: toml::Table = toml::from_str(&doc).expect("valid TOML");
        assert_eq!(parsed["path"].as_str(), Some(raw));
    }
}
