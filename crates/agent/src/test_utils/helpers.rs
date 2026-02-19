//! Helper functions for creating test fixtures

use crate::agent::AgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::SnapshotPolicy;
use crate::middleware::{AgentStats, ConversationContext, ToolCall, ToolFunction};
use crate::model::{AgentMessage, MessagePart};
use crate::session::backend::StorageBackend;
use crate::session::domain::ForkOrigin;
use crate::session::sqlite_storage::SqliteStorage;
use crate::session::store::{LLMConfig, Session};
use crate::snapshot::backend::SnapshotBackend;
use crate::snapshot::git::GitSnapshotBackend;
use crate::test_utils::mocks::{TestPluginLoader, TestProviderFactory};
use anyhow::Result;
use querymt::LLMParams;
use querymt::chat::{ChatMessage, ChatRole};
use querymt::plugin::host::PluginRegistry;
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;
use uuid::Uuid;

/// Creates a test conversation context with the given session ID and step count
pub fn test_context(session_id: &str, steps: usize) -> Arc<ConversationContext> {
    Arc::new(ConversationContext::new(
        session_id.into(),
        Arc::from([]),
        Arc::new(AgentStats {
            steps,
            ..Default::default()
        }),
        "mock".into(),
        "mock-model".into(),
    ))
}

/// Creates a test conversation context with the given number of user messages
/// for testing turn-based middleware
pub fn test_context_with_user_messages(
    session_id: &str,
    user_message_count: usize,
) -> Arc<ConversationContext> {
    let messages: Vec<ChatMessage> = (0..user_message_count)
        .map(|i| ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: format!("User message {}", i),
            thinking: None,
            cache: None,
        })
        .collect();

    Arc::new(ConversationContext::new(
        session_id.into(),
        Arc::from(messages.into_boxed_slice()),
        Arc::new(AgentStats {
            turns: user_message_count,
            ..Default::default()
        }),
        "mock".into(),
        "mock-model".into(),
    ))
}

/// Creates a mock tool call for middleware/state tests
pub fn mock_tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        function: ToolFunction {
            name: name.to_string(),
            arguments: args.to_string(),
        },
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
        params: None,
        created_at: Some(OffsetDateTime::now_utc()),
        updated_at: Some(OffsetDateTime::now_utc()),
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
    let registry = PluginRegistry::from_path(&config_path)?;
    Ok((registry, temp_dir))
}

/// Creates a plugin registry with a mock provider for LLM interaction tests.
///
/// # Returns
/// A tuple of (PluginRegistry, TempDir) - the TempDir must be kept alive
/// for the duration of the test.
pub fn mock_plugin_registry(
    factory: Arc<TestProviderFactory>,
) -> Result<(PluginRegistry, TempDir)> {
    let temp_dir = TempDir::new()?;
    let wasm_path = temp_dir.path().join("mock.wasm");
    std::fs::write(&wasm_path, "")?;
    let config_path = temp_dir.path().join("providers.toml");
    std::fs::write(
        &config_path,
        format!(
            "[[providers]]\nname = \"mock\"\npath = \"{}\"\n",
            wasm_path.display()
        ),
    )?;

    let mut registry = PluginRegistry::from_path(&config_path)?;
    registry.register_loader(Box::new(TestPluginLoader { factory }));
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
}

impl UndoTestFixture {
    /// Creates a new test fixture with snapshot support enabled.
    pub async fn new() -> Result<Self> {
        let worktree = TempDir::new()?;
        let snapshot_base = TempDir::new()?;

        let (registry, config_dir) = empty_plugin_registry()?;
        let registry = Arc::new(registry);

        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await?);
        let llm = LLMParams::new().provider("mock").model("mock");

        let snapshot_base_path = snapshot_base.path().to_path_buf();
        let backend = GitSnapshotBackend::with_snapshot_base(snapshot_base_path.clone());
        let backend_arc = Arc::new(backend);

        let builder = AgentConfigBuilder::new(registry, storage.session_store(), llm)
            .with_snapshot_policy(SnapshotPolicy::Diff)
            .with_snapshot_backend(backend_arc.clone());
        builder.add_observer(storage.event_observer());

        let config = Arc::new(builder.build());
        let handle = AgentHandle::from_config(config);

        Ok(Self {
            worktree,
            snapshot_base,
            config_dir,
            storage,
            handle,
            backend: GitSnapshotBackend::with_snapshot_base(snapshot_base_path),
        })
    }

    /// Create a session and return its public ID
    pub async fn create_session(&self) -> Result<String> {
        let session = self
            .storage
            .session_store()
            .create_session(None, Some(self.worktree.path().to_path_buf()), None, None)
            .await?;

        // Spawn a SessionActor via the kameo registry
        self.register_session_actor(&session.public_id).await;

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
        use crate::agent::session_actor::SessionActor;
        use kameo::actor::Spawn;

        let runtime = crate::agent::core::SessionRuntime::new(
            Some(self.worktree.path().to_path_buf()),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            vec![],
        );

        let actor = SessionActor::new(self.handle.config.clone(), session_id.to_string(), runtime);
        let actor_ref = SessionActor::spawn(actor);

        let mut registry = self.handle.registry.lock().await;
        registry.insert(session_id.to_string(), actor_ref);
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
        };
        self.storage
            .session_store()
            .add_message(session_id, msg)
            .await?;
        Ok(msg_id)
    }

    /// Take a snapshot and record TurnSnapshotStart
    pub async fn take_pre_snapshot(&self, session_id: &str) -> Result<(String, String)> {
        let snapshot_id = self.backend.track(self.worktree.path()).await?;
        let turn_id = Uuid::now_v7().to_string();

        let msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotStart {
                turn_id: turn_id.clone(),
                snapshot_id: snapshot_id.clone(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
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
        turn_id: &str,
        pre_snapshot: &str,
    ) -> Result<Vec<String>> {
        let post_snapshot = self.backend.track(self.worktree.path()).await?;
        let pre_snapshot_string = pre_snapshot.to_string();
        let changed_paths = self
            .backend
            .diff(self.worktree.path(), &pre_snapshot_string, &post_snapshot)
            .await?;

        let changed_strs: Vec<String> = changed_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let msg = AgentMessage {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::TurnSnapshotPatch {
                turn_id: turn_id.to_string(),
                snapshot_id: post_snapshot,
                changed_paths: changed_strs.clone(),
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
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

    /// Convenience: read a file from the worktree
    pub fn read_file(&self, name: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.worktree.path().join(name))?)
    }

    /// Perform undo operation via AgentHandle
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult> {
        self.handle.undo(session_id, message_id).await
    }

    /// Perform redo operation via AgentHandle
    pub async fn redo(&self, session_id: &str) -> Result<crate::agent::undo::RedoResult> {
        self.handle.redo(session_id).await
    }
}
