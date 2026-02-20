//! Shared test fixtures for remote feature tests.
//!
//! All fixtures follow the project convention:
//!
//! ```rust,ignore
//! let f = SomeFixture::new().await;
//! // use f.field, f.method(), ...
//! ```
//!
//! Mesh-dependent fixtures share a single `OnceCell<MeshHandle>` — kameo
//! allows only one global `ActorSwarm` per process, but one mesh is sufficient
//! because actors registered under distinct DHT names simulate separate nodes.

#[cfg(all(test, feature = "remote"))]
pub(crate) mod fixtures {
    use crate::agent::agent_config::AgentConfig;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::remote::mesh::{MeshConfig, MeshDiscovery, MeshHandle, bootstrap_mesh};
    use crate::agent::remote::node_manager::RemoteNodeManager;
    use crate::agent::remote::provider_host::ProviderHostActor;
    use crate::agent::session_registry::SessionRegistry;
    use crate::session::backend::StorageBackend as _;
    use crate::session::sqlite_storage::SqliteStorage;
    use kameo::actor::{ActorRef, Spawn};
    use querymt::LLMParams;
    use querymt::LLMProvider;
    use querymt::chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk, Tool};
    use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
    use querymt::embedding::EmbeddingProvider;
    use querymt::error::LLMError;
    use querymt::plugin::host::PluginRegistry;
    use std::pin::Pin;
    use std::sync::{Arc, OnceLock};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, OnceCell};

    // ── Persistent runtime that owns the swarm event loop ────────────────────
    //
    // `#[tokio::test]` creates a *per-test* runtime.  `bootstrap_mesh` spawns
    // the libp2p swarm event-loop task with `tokio::spawn`, binding it to
    // whichever runtime calls it first.  When that test finishes its runtime
    // is shut down, killing the swarm task.  Every subsequent test that touches
    // `ActorSwarm::get()` then finds a dead `mpsc::Sender` and panics:
    //
    //   "the swarm should never stop running: SendError { .. }"
    //
    // Fix: bootstrap the mesh on a dedicated `tokio::Runtime` that lives for
    // the entire process lifetime.  All swarm I/O (the event-loop task and the
    // libp2p TCP listener) runs on this runtime; the `MeshHandle` returned is
    // just a cheaply-cloneable capability object whose channel sender is
    // `Send + 'static` and works from any runtime context.

    static MESH_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

    fn mesh_runtime() -> &'static tokio::runtime::Runtime {
        MESH_RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("test-mesh-rt")
                .build()
                .expect("create persistent mesh runtime")
        })
    }

    // ── Single shared mesh ────────────────────────────────────────────────────

    static TEST_MESH: OnceCell<MeshHandle> = OnceCell::const_new();

    /// Return the process-wide test mesh, bootstrapping it on first call.
    ///
    /// All tests share the same `MeshHandle`. Use unique DHT names per test
    /// (e.g. via `uuid::Uuid::now_v7()`) to prevent cross-test interference.
    ///
    /// The mesh swarm event loop is pinned to a dedicated long-lived runtime
    /// (`MESH_RUNTIME`) so it survives the teardown of individual per-test
    /// runtimes created by `#[tokio::test]`.
    pub async fn get_test_mesh() -> &'static MeshHandle {
        TEST_MESH
            .get_or_init(|| async {
                // Run bootstrap_mesh on the persistent runtime so the swarm
                // event-loop task is owned by a runtime that never shuts down.
                mesh_runtime()
                    .spawn(async {
                        let cfg = MeshConfig {
                            listen: Some("/ip4/127.0.0.1/tcp/0".to_string()),
                            discovery: MeshDiscovery::None,
                            bootstrap_peers: vec![],
                        };
                        bootstrap_mesh(&cfg)
                            .await
                            .expect("test mesh bootstrap failed")
                    })
                    .await
                    .expect("mesh bootstrap task panicked")
            })
            .await
    }

    // ── Minimal AgentConfig ───────────────────────────────────────────────────

    /// Fixture for a minimal `AgentConfig` backed by an in-memory SQLite store.
    ///
    /// Suitable for any test that does not need real LLM calls.
    ///
    /// ```rust,ignore
    /// let f = AgentConfigFixture::new().await;
    /// // f.config   — Arc<AgentConfig>
    /// // f._tempdir — kept alive for the duration of the test
    /// ```
    pub struct AgentConfigFixture {
        pub config: Arc<AgentConfig>,
        /// Keep the `TempDir` alive for the duration of the test.
        pub _tempdir: TempDir,
    }

    impl AgentConfigFixture {
        pub async fn new() -> Self {
            let temp_dir = TempDir::new().expect("create temp dir");
            let config_path = temp_dir.path().join("providers.toml");
            std::fs::write(&config_path, "providers = []\n").expect("write providers.toml");
            let registry = PluginRegistry::from_path(&config_path).expect("create plugin registry");
            let plugin_registry = Arc::new(registry);

            let storage = Arc::new(
                SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create sqlite storage"),
            );
            let llm = LLMParams::new().provider("mock").model("mock");

            let builder = AgentConfigBuilder::new(plugin_registry, storage.session_store(), llm);
            let config = Arc::new(builder.build());

            Self {
                config,
                _tempdir: temp_dir,
            }
        }
    }

    // ── MockLLMProvider ───────────────────────────────────────────────────────

    /// A minimal `LLMProvider` that returns a fixed text response.
    ///
    /// Use `MockLLMProvider::new("hello")` to get a provider that always
    /// responds with `"hello"`. Optionally track call counts via the shared
    /// `call_count` mutex.
    pub struct MockLLMProvider {
        pub response_text: String,
        /// Incremented on each `chat_with_tools` call.
        pub call_count: Arc<Mutex<usize>>,
    }

    impl MockLLMProvider {
        pub fn new(response: impl Into<String>) -> Self {
            Self {
                response_text: response.into(),
                call_count: Arc::new(Mutex::new(0)),
            }
        }

        pub fn call_counter(&self) -> Arc<Mutex<usize>> {
            Arc::clone(&self.call_count)
        }
    }

    /// Concrete `ChatResponse` returned by `MockLLMProvider`.
    #[derive(Debug)]
    pub struct MockChatResp {
        pub text: String,
        pub finish: FinishReason,
    }

    impl std::fmt::Display for MockChatResp {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.text)
        }
    }

    impl ChatResponse for MockChatResp {
        fn text(&self) -> Option<String> {
            Some(self.text.clone())
        }
        fn thinking(&self) -> Option<String> {
            None
        }
        fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
            None
        }
        fn finish_reason(&self) -> Option<FinishReason> {
            Some(self.finish.clone())
        }
        fn usage(&self) -> Option<querymt::Usage> {
            None
        }
    }

    #[async_trait::async_trait]
    impl querymt::chat::ChatProvider for MockLLMProvider {
        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            let mut count = self.call_count.lock().await;
            *count += 1;
            Ok(Box::new(MockChatResp {
                text: self.response_text.clone(),
                finish: FinishReason::Stop,
            }))
        }

        async fn chat_stream_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<
            Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
            LLMError,
        > {
            use futures_util::stream;
            let text = self.response_text.clone();
            let chunks = vec![
                Ok(StreamChunk::Text(text)),
                Ok(StreamChunk::Done {
                    stop_reason: "end_turn".to_string(),
                }),
            ];
            Ok(Box::pin(stream::iter(chunks)))
        }
    }

    #[async_trait::async_trait]
    impl CompletionProvider for MockLLMProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("mock".into()))
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for MockLLMProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("mock".into()))
        }
    }

    impl LLMProvider for MockLLMProvider {}

    // ── ProviderHostFixture ───────────────────────────────────────────────────

    /// Fixture for `ProviderHostActor` tests that do **not** require a mesh.
    ///
    /// ```rust,ignore
    /// let f = ProviderHostFixture::new().await;
    /// // f.actor_ref  — ActorRef<ProviderHostActor>
    /// // f.config     — Arc<AgentConfig>
    /// ```
    pub struct ProviderHostFixture {
        pub actor_ref: ActorRef<ProviderHostActor>,
        pub config: Arc<AgentConfig>,
        pub _tempdir: TempDir,
    }

    impl ProviderHostFixture {
        pub async fn new() -> Self {
            let f = AgentConfigFixture::new().await;
            let actor = ProviderHostActor::new(f.config.clone());
            let actor_ref = ProviderHostActor::spawn(actor);
            Self {
                actor_ref,
                config: f.config,
                _tempdir: f._tempdir,
            }
        }
    }

    // ── NodeManagerFixture ────────────────────────────────────────────────────

    /// Fixture for `RemoteNodeManager` tests that do **not** require a mesh.
    ///
    /// ```rust,ignore
    /// let f = NodeManagerFixture::new().await;
    /// // f.actor_ref  — ActorRef<RemoteNodeManager>
    /// // f.config     — Arc<AgentConfig>
    /// ```
    pub struct NodeManagerFixture {
        pub actor_ref: ActorRef<RemoteNodeManager>,
        pub config: Arc<AgentConfig>,
        pub _tempdir: TempDir,
    }

    impl NodeManagerFixture {
        pub async fn new() -> Self {
            let f = AgentConfigFixture::new().await;
            let registry = Arc::new(Mutex::new(SessionRegistry::new(f.config.clone())));
            let nm = RemoteNodeManager::new(f.config.clone(), registry, None);
            let actor_ref = RemoteNodeManager::spawn(nm);
            Self {
                actor_ref,
                config: f.config,
                _tempdir: f._tempdir,
            }
        }
    }

    // ── MeshNodeManagerFixture ────────────────────────────────────────────────

    /// Fixture for a `RemoteNodeManager` registered in the shared test mesh.
    ///
    /// Use `test_id` (e.g. `uuid::Uuid::now_v7().to_string()`) to produce a
    /// unique DHT name and prevent cross-test interference.
    ///
    /// ```rust,ignore
    /// let f = MeshNodeManagerFixture::new("alpha", &test_id).await;
    /// // f.actor_ref  — ActorRef<RemoteNodeManager>
    /// // f.dht_name   — "node_manager::alpha-{test_id}"
    /// // f.mesh       — &'static MeshHandle
    /// ```
    pub struct MeshNodeManagerFixture {
        pub actor_ref: ActorRef<RemoteNodeManager>,
        pub config: Arc<AgentConfig>,
        pub dht_name: String,
        pub mesh: &'static MeshHandle,
        pub _tempdir: TempDir,
    }

    impl MeshNodeManagerFixture {
        pub async fn new(label: &str, test_id: &str) -> Self {
            let mesh = get_test_mesh().await;
            let f = AgentConfigFixture::new().await;
            let registry = Arc::new(Mutex::new(SessionRegistry::new(f.config.clone())));
            let nm = RemoteNodeManager::new(f.config.clone(), registry, Some(mesh.clone()));
            let actor_ref = RemoteNodeManager::spawn(nm);

            let dht_name = format!("node_manager::{}-{}", label, test_id);
            mesh.register_actor(actor_ref.clone(), dht_name.clone())
                .await;

            Self {
                actor_ref,
                config: f.config,
                dht_name,
                mesh,
                _tempdir: f._tempdir,
            }
        }
    }

    // ── TwoNodeFixture ────────────────────────────────────────────────────────

    /// Fixture that simulates two logical nodes ("alpha" and "beta") sharing
    /// the same in-process mesh.
    ///
    /// Alpha acts as the caller, beta as the session host.
    ///
    /// ```rust,ignore
    /// let f = TwoNodeFixture::new(&test_id).await;
    /// // f.alpha / f.beta  — MeshNodeManagerFixture
    /// // f.mesh            — &'static MeshHandle
    /// ```
    pub struct TwoNodeFixture {
        pub alpha: MeshNodeManagerFixture,
        pub beta: MeshNodeManagerFixture,
        pub mesh: &'static MeshHandle,
    }

    impl TwoNodeFixture {
        pub async fn new(test_id: &str) -> Self {
            let mesh = get_test_mesh().await;
            let alpha = MeshNodeManagerFixture::new("alpha", test_id).await;
            let beta = MeshNodeManagerFixture::new("beta", test_id).await;
            Self { alpha, beta, mesh }
        }
    }

    // ── ProviderRoutingFixture ────────────────────────────────────────────────

    /// Fixture for provider-routing integration tests.
    ///
    /// Sets up:
    ///  - Alpha's `ProviderHostActor` (with a `MockLLMProvider`) registered in DHT.
    ///  - Beta's `RemoteNodeManager` registered in DHT.
    ///  - The shared mesh handle.
    ///
    /// ```rust,ignore
    /// let f = ProviderRoutingFixture::new(&test_id).await;
    /// // f.provider_host_ref  — ActorRef<ProviderHostActor> for alpha
    /// // f.provider_host_dht  — "provider_host::alpha-{test_id}"
    /// // f.beta               — MeshNodeManagerFixture
    /// // f.call_count         — Arc<Mutex<usize>> to verify mock was invoked
    /// ```
    pub struct ProviderRoutingFixture {
        pub provider_host_ref: ActorRef<ProviderHostActor>,
        pub provider_host_dht: String,
        pub beta: MeshNodeManagerFixture,
        pub alpha_config: Arc<AgentConfig>,
        pub call_count: Arc<Mutex<usize>>,
        pub mesh: &'static MeshHandle,
        pub _alpha_tempdir: TempDir,
    }

    impl ProviderRoutingFixture {
        pub async fn new(test_id: &str) -> Self {
            let mesh = get_test_mesh().await;

            // Alpha: AgentConfig + ProviderHostActor
            let alpha_f = AgentConfigFixture::new().await;
            let mock = MockLLMProvider::new("provider routing response");
            let call_count = mock.call_counter();
            let _mock_arc: Arc<dyn LLMProvider> = Arc::new(mock);

            // We use the real ProviderHostActor with the alpha AgentConfig.
            // The mock provider won't be invoked via the normal plugin path;
            // provider routing integration tests that need the mock to fire
            // should use direct `ask(ProviderChatRequest)` with a provider
            // name resolvable from the config.
            let actor = ProviderHostActor::new(alpha_f.config.clone());
            let actor_ref = ProviderHostActor::spawn(actor);

            let provider_host_dht = format!("provider_host::alpha-{}", test_id);
            mesh.register_actor(actor_ref.clone(), provider_host_dht.clone())
                .await;

            let beta = MeshNodeManagerFixture::new("beta", test_id).await;

            Self {
                provider_host_ref: actor_ref,
                provider_host_dht,
                beta,
                alpha_config: alpha_f.config,
                call_count,
                mesh,
                _alpha_tempdir: alpha_f._tempdir,
            }
        }
    }

    // ── RemoteAgentStubFixture ────────────────────────────────────────────────

    /// Fixture for `RemoteAgentStub` / `SendAgent` tests.
    ///
    /// Creates a `RemoteNodeManager` (no mesh) and wraps it in a
    /// `RemoteAgentStub` via the same construction path as
    /// `setup_mesh_from_config`.  Tests only require local actor refs —
    /// no DHT lookup is exercised.
    pub struct RemoteAgentStubFixture {
        pub stub: Arc<dyn crate::send_agent::SendAgent>,
        pub node_manager: ActorRef<RemoteNodeManager>,
        pub mesh: &'static MeshHandle,
        pub _tempdir: TempDir,
    }

    impl RemoteAgentStubFixture {
        pub async fn new(test_id: &str) -> Self {
            use crate::agent::remote::remote_setup::RemoteAgentStub;

            let mesh = get_test_mesh().await;
            let nm_fixture = MeshNodeManagerFixture::new("stub", test_id).await;

            let stub = Arc::new(RemoteAgentStub::new_for_test(
                format!("stub-{}", test_id),
                format!("agent-{}", test_id),
                mesh.clone(),
            ));

            Self {
                stub,
                node_manager: nm_fixture.actor_ref,
                mesh,
                _tempdir: nm_fixture._tempdir,
            }
        }
    }
}
