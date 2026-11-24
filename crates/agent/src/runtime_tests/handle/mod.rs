#![allow(unused_imports)]
use crate::acp::client_bridge::ClientBridgeSender;
use crate::acp::protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption, SetSessionModelRequest,
    SetSessionModelResponse,
};
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, ClientState};
use crate::agent::handle::*;
use crate::agent::remote::SessionActorRef;
use crate::agent::session_materializer::{PreparedSessionResult, SessionMaterializer};
use crate::agent::session_registry::SessionRegistry;
use crate::event_fanout::EventFanout;
use crate::events::{AgentEventKind, EventEnvelope};
use crate::index::WorkspaceIndexManagerActor;
use crate::middleware::CompositeDriver;
use crate::profile_host::ProfileRuntimeHost;
use crate::profiles::{ProfileCatalog, ProfileRuntimeManager};
use crate::send_agent::SendAgent;
use crate::session::store::LLMConfig;
use crate::tools::ToolRegistry;
use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use parking_lot::Mutex as ParkingMutex;
use querymt::LLMParams;
use querymt::chat::ReasoningEffort;
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
#[cfg(feature = "remote")]
use tokio::sync::OnceCell;
#[cfg(feature = "remote")]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, broadcast};

use crate::acp::protocol::{ProtocolVersion, SessionId};
use crate::agent::SessionActor;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::ToolPolicy;
use crate::api::AgentInfra;
use crate::session::backend::StorageBackend;
use crate::session::store::SessionStore;
use crate::test_utils::{
    MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory,
    empty_plugin_registry, mock_llm_config, mock_plugin_registry, mock_session,
};
use std::collections::HashSet;
use std::path::Path;

use kameo::actor::Spawn;

// ── Shared fixture ───────────────────────────────────────────────────────

struct HandleFixture {
    handle: LocalAgentHandle,
    _temp_dir: tempfile::TempDir,
}

struct RealStorageHandleFixture {
    handle: LocalAgentHandle,
    storage: Arc<crate::session::sqlite_storage::SqliteStorage>,
    _temp_dir: tempfile::TempDir,
}

struct TestStorageBackend {
    session_store: Arc<dyn SessionStore>,
    event_storage: Arc<crate::session::sqlite_storage::SqliteStorage>,
}

impl StorageBackend for TestStorageBackend {
    fn session_store(&self) -> Arc<dyn SessionStore> {
        self.session_store.clone()
    }

    fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
        self.event_storage.clone()
    }

    fn view_store(&self) -> Option<Arc<dyn crate::session::projection::ViewStore>> {
        Some(self.event_storage.clone())
    }
}

impl HandleFixture {
    async fn new() -> Self {
        Self::with_list_sessions(vec![]).await
    }

    /// Build a fixture whose mock provider advertises an `api_key_name` so
    /// API-token ACP methods can resolve a secret-store key.
    async fn with_api_key_name(api_key_name: &str) -> Self {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider.clone(),
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared).with_api_key_name(api_key_name));
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let mut builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.clone(),
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

    async fn with_profiles(self, active_profile_id: &str, profile_dir: &Path) -> Self {
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("profile test storage"),
        );
        self.with_profile_storage(active_profile_id, profile_dir, storage)
            .await
    }

    async fn with_profile_storage(
        self,
        active_profile_id: &str,
        profile_dir: &Path,
        storage: Arc<dyn crate::session::backend::StorageBackend>,
    ) -> Self {
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
                // The manager and its runtimes share explicit test storage, never the user DB.
                storage: Some(storage),
                session_mcp_attachment_source: None,
                event_fanout: None,
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
        let factory = Arc::new(TestProviderFactory::new(shared));
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
        let listed_sessions_for_mock = listed_sessions.clone();
        store
            .expect_list_sessions()
            .returning(move || Ok(listed_sessions_for_mock.clone()))
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
        store
            .expect_create_task()
            .returning(|mut task| {
                task.id = 1;
                if task.public_id.is_empty() {
                    task.public_id = "task-test-1".to_string();
                }
                Ok(task)
            })
            .times(0..);

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );
        for listed_session in &listed_sessions {
            storage
                .create_session(
                    listed_session.name.clone(),
                    listed_session.cwd.clone(),
                    None,
                    listed_session.fork_origin.clone(),
                )
                .await
                .expect("seed listed session");
        }

        let mut builder = AgentConfigBuilder::new(
            Arc::new(plugin_registry),
            storage.clone(),
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

async fn ext_method_json(
    handle: &LocalAgentHandle,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = handle
        .ext_method(crate::acp::protocol::ExtRequest::new(
            method,
            raw_params(&params.to_string()),
        ))
        .await
        .expect("ext_method");
    serde_json::from_str(response.0.get()).expect("valid JSON")
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

const QUORUM_PROFILE_TOML: &str = r#"
[quorum]
delegation = true

[planner]
provider = "test"
model = "test-model"

[[delegates]]
id = "reviewer"
provider = "test"
model = "test-model"
description = "Reviews changes"
capabilities = ["review"]

[[delegates]]
id = "coder"
provider = "test"
model = "test-model"
description = "Writes code"
capabilities = ["coding"]
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
        .await
        .expect("bind test profile");
}

async fn register_bound_test_session(f: &HandleFixture, session_id: &str, profile_id: &str) {
    bind_test_profile(f, session_id, profile_id).await;

    let runtime = f
        .handle
        .profiles()
        .unwrap()
        .runtime_for_profile(profile_id)
        .await
        .expect("bound profile runtime should load");
    let profile_handle = runtime.handle();
    let actor = SessionActor::new(
        profile_handle.config.clone(),
        session_id.to_string(),
        crate::agent::core::SessionRuntime::new(
            None,
            std::collections::HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        ),
    );
    let actor_ref = SessionActor::spawn(actor);
    profile_handle
        .registry
        .lock()
        .await
        .insert(session_id.to_string(), actor_ref);
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
        .session_profile_config_options(
            session_id,
            AgentMode::Build,
            **f.handle.config.default_reasoning_effort.load(),
        )
        .await
        .expect("config options")
}

fn select_options(
    option: &SessionConfigOption,
) -> &[crate::acp::protocol::SessionConfigSelectOption] {
    match &option.kind {
        crate::acp::protocol::SessionConfigKind::Select(select) => match &select.options {
            crate::acp::protocol::SessionConfigSelectOptions::Ungrouped(options) => options,
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

fn config_option_value(options: &[SessionConfigOption], id: &str) -> Option<String> {
    options
        .iter()
        .find(|option| option.id.0.as_ref() == id)
        .and_then(|option| match &option.kind {
            crate::acp::protocol::SessionConfigKind::Select(select) => {
                Some(select.current_value.0.to_string())
            }
            _ => None,
        })
}

async fn seed_session_with_reasoning(
    fixture: &RealStorageHandleFixture,
    effort: Option<ReasoningEffort>,
) -> String {
    let store = fixture.storage.session_store();
    let session = store
        .create_session(None, None, None, None)
        .await
        .expect("create session");
    let mut params = LLMParams::new().provider("mock").model("mock-model");
    params.reasoning_effort = effort;
    let config = store
        .create_or_get_llm_config(&params)
        .await
        .expect("create LLM config");
    store
        .set_session_llm_config(&session.public_id, config.id)
        .await
        .expect("set session LLM config");
    session.public_id
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

impl RealStorageHandleFixture {
    async fn new() -> Self {
        Self::new_with_initial_reasoning(None).await
    }

    async fn new_with_initial_reasoning(reasoning_effort: Option<ReasoningEffort>) -> Self {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create storage")
                .with_run_lease_root(temp_dir.path().join("run-leases")),
        );

        let mut initial_config = LLMParams::new().provider("mock").model("mock-model");
        initial_config.reasoning_effort = reasoning_effort;
        let mut builder =
            AgentConfigBuilder::new(Arc::new(plugin_registry), storage.clone(), initial_config)
                .with_tool_policy(ToolPolicy::ProviderOnly);

        if let Some(repo) = storage.schedule_repository() {
            builder = builder.with_schedule_repository(repo);
        }

        let config = Arc::new(builder.build());
        Self {
            handle: LocalAgentHandle::from_config(config),
            storage,
            _temp_dir: temp_dir,
        }
    }
}

fn should_return_without_force_stop(status: crate::agent::messages::SessionRuntimeStatus) -> bool {
    status.phase == crate::agent::messages::SessionRuntimePhase::Idle
}

#[cfg(feature = "remote")]
struct RemoteScheduleFixture {
    client: RealStorageHandleFixture,
    node_id: String,
    session_id: String,
    host: tokio::process::Child,
    _dir: tempfile::TempDir,
}

#[cfg(feature = "remote")]
impl RemoteScheduleFixture {
    async fn new(missing_session: bool) -> Self {
        use querymt_remote::peer_access::SharingProtocol;
        use std::process::Stdio;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("mesh fixture directory");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("allocate a loopback port")
            .local_addr()
            .unwrap()
            .port();
        let listen = format!("/ip4/127.0.0.1/tcp/{port}");
        let mut config =
            schedule_mesh_config(dir.path().join("client.key"), "/ip4/127.0.0.1/tcp/0".into());
        let client_peer = schedule_mesh_peer_id(config.identity_file.as_deref().unwrap());
        let host_peer = schedule_mesh_peer_id(&dir.path().join("host.key"));
        assert_ne!(client_peer, host_peer);

        // Kameo allows only one mesh identity per process. Start the owner in a
        // child so the ACP client crosses real authenticated mesh ingress.
        let mut host = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_tests::handle::remote_schedule_host_helper",
                "--ignored",
            ])
            .env("QMT_TEST_SCHEDULE_HOST_DIR", dir.path())
            .env("QMT_TEST_SCHEDULE_LISTEN", &listen)
            .env(
                "QMT_TEST_SCHEDULE_DELETE_SESSION",
                if missing_session { "1" } else { "0" },
            )
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start guarded schedule host");
        let ready = dir.path().join("ready");
        let session_id = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(id) = std::fs::read_to_string(&ready) {
                    break id;
                }
                assert!(
                    host.try_wait().unwrap().is_none(),
                    "schedule host exited before readiness"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("schedule host did not become ready");

        config.peers.push(format!("{listen}/p2p/{host_peer}"));
        let mesh = crate::mesh_access::bootstrap_mesh_runtime(&config)
            .await
            .expect("start distinct schedule client");
        let mesh = mesh.as_mesh_handle();
        let name = querymt_remote::scoped_node_manager_for_peer(
            &querymt_remote::MeshScopeId::lan_default(),
            &host_peer,
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if mesh.is_peer_alive(&host_peer)
                    && matches!(mesh.lookup_actor_no_retry_with_timeout::<crate::agent::remote::RemoteNodeManager>(&name, Duration::from_secs(1)).await, Ok(Some(actor)) if actor.id().peer_id() == Some(&host_peer))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("client did not discover the guarded node manager");
        assert_eq!(
            mesh.select_sharing_protocol(host_peer, config.request_timeout)
                .await
                .unwrap(),
            SharingProtocol::V1
        );
        let client = RealStorageHandleFixture::new().await;
        client.handle.set_mesh(mesh.clone());
        Self {
            client,
            node_id: host_peer.to_string(),
            session_id,
            host,
            _dir: dir,
        }
    }

    async fn finish(mut self) {
        drop(self.host.stdin.take());
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), self.host.wait())
            .await
            .expect("schedule host shutdown timed out")
            .expect("wait for schedule host");
        assert!(status.success(), "schedule host failed: {status}");
    }
}

#[cfg(feature = "remote")]
fn schedule_mesh_config(
    identity: std::path::PathBuf,
    listen: String,
) -> querymt_remote::MeshRuntimeConfig {
    use querymt_remote::{LanDiscovery, LanMeshConfig, MeshRuntimeConfig};
    use std::time::Duration;

    MeshRuntimeConfig {
        enabled: true,
        lan: Some(LanMeshConfig {
            listen: Some(listen),
            discovery: LanDiscovery::None,
            directory: Default::default(),
        }),
        identity_file: Some(identity),
        iroh_enabled: false,
        iroh_gso: true,
        iroh_dns: None,
        iroh_scopes: vec![],
        request_timeout: Duration::from_secs(3),
        stream_reconnect_grace: Duration::from_secs(3),
        node_name: None,
        peers: vec![],
        auto_fallback: false,
        access: Default::default(),
    }
}

#[cfg(feature = "remote")]
fn schedule_mesh_peer_id(identity: &Path) -> libp2p::PeerId {
    querymt_remote::identity::load_or_generate_keypair(Some(identity))
        .expect("test mesh keypair")
        .public()
        .to_peer_id()
}

#[cfg(feature = "remote")]
#[tokio::test]
#[ignore = "subprocess for ACP remote schedule tests"]
async fn remote_schedule_host_helper() {
    use crate::agent::remote::{
        CreateRemoteSession, remote_setup::spawn_and_register_local_mesh_actors_with_name,
    };
    use querymt_remote::{
        NodeId,
        peer_access::{PeerGrant, PeerGrantKey},
    };
    use tokio::io::AsyncReadExt;

    let dir = std::path::PathBuf::from(
        std::env::var_os("QMT_TEST_SCHEDULE_HOST_DIR").expect("fixture directory"),
    );
    let listen = std::env::var("QMT_TEST_SCHEDULE_LISTEN").expect("fixture listen address");
    let missing_session = std::env::var("QMT_TEST_SCHEDULE_DELETE_SESSION").unwrap() == "1";
    let host = RealStorageHandleFixture::new().await;
    let runtime = crate::mesh_access::bootstrap_mesh_runtime(&schedule_mesh_config(
        dir.join("host.key"),
        listen,
    ))
    .await
    .expect("start guarded schedule host");
    let mesh = runtime.as_mesh_handle();
    host.handle.set_mesh(mesh.clone());
    let store = mesh.quota_store().expect("guarded host policy store");
    let key = PeerGrantKey {
        host: NodeId::from_peer_id(*mesh.peer_id()),
        peer: NodeId::from_peer_id(schedule_mesh_peer_id(&dir.join("client.key"))),
    };
    let revision = store.grant(&key).await.unwrap().revision;
    // Schedule RPCs require a full grant, including over sharing V1.
    store
        .replace_grant(&key, revision, PeerGrant::Full)
        .await
        .expect("grant schedule access to the distinct client");
    let actors = spawn_and_register_local_mesh_actors_with_name(
        &host.handle,
        mesh,
        Some("peer-alpha".into()),
    )
    .await;
    let session = actors
        .node_manager
        .ask(CreateRemoteSession {
            cwd: (!missing_session).then(|| "/tmp/remote-schedule".into()),
        })
        .await
        .expect("create remote session");
    if missing_session {
        host.storage
            .session_store()
            .delete_session(&session.session_id)
            .await
            .expect("delete persisted remote session row");
    }

    std::fs::write(dir.join("session"), session.session_id).unwrap();
    std::fs::rename(dir.join("session"), dir.join("ready")).unwrap();
    // The parent holds stdin open for the duration of its ACP requests.
    assert_eq!(tokio::io::stdin().read(&mut [0]).await.unwrap(), 0);
    actors.node_manager.stop_gracefully().await.unwrap();
    runtime.shutdown().await;
}

// ── Tests ─────────────────────────────────────────────────────────────────

mod capabilities;
mod core_a;
mod core_b;
mod ext_auth;
mod ext_session_history;
mod mesh;
mod remote;
mod remote_ext;
mod roster_ext;
mod schedule_get_remote;
mod schedule_notifications;
mod scheduler;
mod session_ops_ext;
mod worktree_ops_ext;
mod worktree_slash_commands;
