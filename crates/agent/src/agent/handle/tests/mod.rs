use super::*;
use crate::agent::SessionActor;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::ToolPolicy;
use crate::api::AgentInfra;
use crate::send_agent::SendAgent;
use crate::session::backend::StorageBackend;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory,
    empty_plugin_registry, mock_llm_config, mock_plugin_registry, mock_session,
};
use agent_client_protocol::schema::{
    CancelNotification, CloseSessionRequest, DeleteSessionRequest, InitializeRequest,
    ListSessionsRequest, ProtocolVersion, SessionId,
};
use querymt::LLMParams;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use kameo::actor::Spawn;

// ── Shared fixture ───────────────────────────────────────────────────────

struct HandleFixture {
    handle: LocalAgentHandle,
    _temp_dir: tempfile::TempDir,
}

impl HandleFixture {
    async fn new() -> Self {
        Self::with_list_sessions(vec![]).await
    }

    async fn with_profiles(self, active_profile_id: &str, profile_dir: &Path) -> Self {
        let catalog: Arc<dyn ProfileCatalog> = Arc::new(
            crate::profiles::LocalProfileCatalog::builder()
                .include_embedded_default(false)
                .local_dir(profile_dir)
                .build(),
        );
        let (plugin_registry, _temp_dir) = empty_plugin_registry().expect("plugin registry");
        let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
            catalog,
            active_profile_id,
            AgentInfra {
                plugin_registry: Arc::new(plugin_registry),
                storage: None,
                session_mcp_attachment_source: None,
            },
        ));
        self.handle.set_profiles(profiles);
        self
    }

    async fn with_list_sessions(listed_sessions: Vec<crate::session::store::Session>) -> Self {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider.clone(),
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory { provider: shared });
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let llm_config = mock_llm_config();
        let session = mock_session("test-session");
        let mut store = MockSessionStore::new();
        let session_clone = session.clone();
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session_clone.clone())))
            .times(0..);
        let llm_for_mock = llm_config.clone();
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(llm_for_mock.clone())))
            .times(0..);
        store
            .expect_get_llm_config()
            .returning(move |_| Ok(Some(llm_config.clone())))
            .times(0..);
        store
            .expect_list_sessions()
            .returning(move || Ok(listed_sessions.clone()))
            .times(0..);
        store
            .expect_create_or_get_llm_config()
            .returning(|_| Ok(mock_llm_config()))
            .times(0..);
        store
            .expect_set_session_llm_config()
            .returning(|_, _| Ok(()))
            .times(0..);
        store
            .expect_delete_session()
            .returning(|_| Ok(()))
            .times(0..);

        let store: Arc<dyn SessionStore> = Arc::new(store);
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let mut builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            store.clone(),
            storage.event_journal(),
            LLMParams::new().provider("mock").model("mock-model"),
        )
        .with_tool_policy(ToolPolicy::ProviderOnly);

        if let Some(repo) = storage.schedule_repository() {
            builder = builder.with_schedule_repository(repo);
        }

        let config = Arc::new(builder.build());

        Self {
            handle: LocalAgentHandle::from_config(config),
            _temp_dir: temp_dir,
        }
    }
}

fn raw_params(value: &str) -> Arc<serde_json::value::RawValue> {
    Arc::from(serde_json::value::RawValue::from_string(value.to_string()).unwrap())
}

const ALPHA_PROFILE_TOML: &str = r#"
[agent]
provider = "test"
model = "test-model"
system = "alpha"
"#;

const BETA_PROFILE_TOML: &str = r#"
[agent]
provider = "test"
model = "test-model"
system = "beta"
"#;

fn write_profile(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).expect("profile should be written");
}

async fn profile_fixture_with_files(files: &[(&str, &str)]) -> (HandleFixture, tempfile::TempDir) {
    let profile_dir = tempfile::tempdir().expect("profile dir");
    for (name, content) in files {
        write_profile(profile_dir.path(), name, content);
    }
    let f = HandleFixture::new()
        .await
        .with_profiles("alpha", profile_dir.path())
        .await;
    (f, profile_dir)
}

async fn bind_test_profile(f: &HandleFixture, session_id: &str, profile_id: &str) {
    f.handle
        .profiles()
        .unwrap()
        .set_session_binding(
            session_id,
            test_profile_metadata(profile_id, profile_id, None).session_binding(),
        )
        .await;
}

async fn register_test_session(f: &HandleFixture, session_id: &str) {
    let actor = SessionActor::new(
        f.handle.config.clone(),
        session_id.to_string(),
        crate::agent::core::SessionRuntime::new(
            None,
            std::collections::HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    );
    let actor_ref = SessionActor::spawn(actor);
    f.handle
        .registry
        .lock()
        .await
        .insert(session_id.to_string(), actor_ref);
}

async fn profile_config_options(
    f: &HandleFixture,
    session_id: Option<&str>,
) -> Vec<SessionConfigOption> {
    f.handle
        .session_config_options(
            session_id,
            AgentMode::Build,
            **f.handle.config.default_reasoning_effort.load(),
        )
        .await
        .expect("config options")
}

fn select_options(
    option: &SessionConfigOption,
) -> &[agent_client_protocol::schema::SessionConfigSelectOption] {
    match &option.kind {
        agent_client_protocol::schema::SessionConfigKind::Select(select) => match &select.options {
            agent_client_protocol::schema::SessionConfigSelectOptions::Ungrouped(options) => {
                options
            }
            _ => panic!("expected ungrouped select config option"),
        },
        _ => panic!("expected select config option"),
    }
}

fn select_option_values(option: &SessionConfigOption) -> Vec<String> {
    select_options(option)
        .iter()
        .map(|option| option.value.0.to_string())
        .collect()
}

fn test_profile_metadata(
    id: &str,
    name: &str,
    description: Option<&str>,
) -> crate::profiles::ProfileMetadata {
    crate::profiles::ProfileMetadata {
        id: id.to_string(),
        name: name.to_string(),
        description: description.map(str::to_string),
        tags: Vec::new(),
        source: crate::profiles::ProfileSource::EmbeddedToml {
            key: id.to_string(),
        },
        config_kind: None,
        fingerprint: None,
    }
}

impl LocalAgentHandle {
    fn should_return_without_force_stop(
        status: crate::agent::messages::SessionRuntimeStatus,
    ) -> bool {
        matches!(status, crate::agent::messages::SessionRuntimeStatus::Idle)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

mod core_a;
mod core_b;
mod remote;
mod scheduler;
