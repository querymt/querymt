#[cfg(feature = "remote")]
use crate::agent::remote::NodeId;
use crate::model::{AgentMessage, MessagePart};
use crate::model_heuristics::ModelDefaults;
use crate::model_info::get_model_info;
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::{LLMConfig, Session, SessionExecutionConfig, SessionStore};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt::providers::ModelPricing;
use querymt::{
    LLMProvider,
    chat::{ChatMessage, ChatResponse, MessageType},
    error::LLMError,
};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::RwLock;

fn prune_config_by_schema(cfg: &Value, schema: &Value) -> Value {
    match (cfg, schema.get("properties")) {
        (Value::Object(cfg_map), Some(Value::Object(props))) => {
            // Build a new object only with keys in `properties`.
            let mut out = Map::with_capacity(cfg_map.len());
            for (k, v) in cfg_map {
                if let Some(prop_schema) = props.get(k) {
                    // If the subschema has its own nested properties, recurse.
                    let pruned_val = if prop_schema.get("properties").is_some() {
                        prune_config_by_schema(v, prop_schema)
                    } else {
                        v.clone()
                    };
                    out.insert(k.clone(), pruned_val);
                }
            }
            Value::Object(out)
        }
        // Not an object or no properties defined -> return as-is.
        _ => cfg.clone(),
    }
}

fn pruned_top_level_keys(before: &Value, after: &Value) -> Vec<String> {
    let Some(before_obj) = before.as_object() else {
        return Vec::new();
    };
    let Some(after_obj) = after.as_object() else {
        return Vec::new();
    };

    let mut removed: Vec<String> = before_obj
        .keys()
        .filter(|k| !after_obj.contains_key(*k))
        .cloned()
        .collect();
    removed.sort();
    removed
}

/// Type alias for the provider cache: (config_id, provider_node_id, allow_mesh_fallback, provider) tuple.
///
/// `provider_node_id` is stored separately from `llm_configs` (in the `sessions`
/// table) so the same `config_id` can resolve to different providers depending
/// on which mesh node owns the session. `allow_mesh_fallback` is included so
/// toggling policy does not reuse a provider built under different routing rules.
type ProviderCache = Arc<RwLock<Option<(i64, Option<String>, bool, Arc<dyn LLMProvider>)>>>;

/// A wrapper around a `SessionStore` that resolves providers dynamically.
///
/// # Provider Caching
///
/// Provider construction is cached at two levels:
///
/// 1. **Global cache** (`cached_provider`): A single-entry cache keyed on
///    `LLMConfig.id`. Avoids rebuilding identical providers across turns.
///    For multi-model scenarios consider an LRU at the factory level.
///
/// 2. **Per-turn cache** (`SessionHandle::cached_llm_provider`): Each
///    `SessionHandle` lazily caches its resolved provider for the lifetime of
///    the turn. This means the global cache is consulted at most once per turn
///    instead of on every state-machine transition (~8× reduction).
///
/// Model switches via the dashboard write a new `llm_config_id` to the DB but
/// take effect only when the **next** `SessionHandle` is constructed (i.e. the
/// next `run_prompt` call), not mid-turn.
pub struct SessionProvider {
    plugin_registry: Arc<PluginRegistry>,
    history_store: Arc<dyn SessionStore>,
    initial_config: LLMParams,
    /// Cache for the most recently used provider, keyed by LLMConfig.id
    /// Uses a single-entry cache to ensure safe VRAM management for GPU models
    cached_provider: ProviderCache,
    /// Optional mesh handle — present when this node participates in a kameo mesh.
    /// Passed through to `build_provider_from_config` to enable `MeshChatProvider`
    /// routing and mesh-fallback discovery.
    ///
    /// Wrapped in `Arc<StdMutex<...>>` so the mesh can be injected *after* the
    /// `Arc<SessionProvider>` is already shared (e.g. via `AgentHandle::set_mesh`
    /// which is called after `AgentConfigBuilder::build()`).
    #[cfg(feature = "remote")]
    mesh: Arc<StdMutex<Option<crate::agent::remote::MeshHandle>>>,
    /// Whether to scan the mesh when a provider is missing locally and
    /// `provider_node_id` is not explicitly set. Defaults to false.
    #[cfg(feature = "remote")]
    allow_mesh_fallback: Arc<StdMutex<bool>>,
}

impl SessionProvider {
    pub fn new(
        plugin_registry: Arc<PluginRegistry>,
        store: Arc<dyn SessionStore>,
        initial_config: LLMParams,
    ) -> Self {
        Self {
            plugin_registry,
            history_store: store,
            initial_config,
            cached_provider: Arc::new(RwLock::new(None)),
            #[cfg(feature = "remote")]
            mesh: Arc::new(StdMutex::new(None)),
            #[cfg(feature = "remote")]
            allow_mesh_fallback: Arc::new(StdMutex::new(false)),
        }
    }

    /// Returns the initial `LLMParams` this provider was constructed with.
    ///
    /// Used by `ProviderHostActor` to forward the agent's custom parameters
    /// (e.g. `model_path`, `n_ctx`) when building a local provider on behalf
    /// of a remote mesh peer.  Without these params the friendly model name
    /// (e.g. `"qwen3-coder"`) cannot be resolved to a real GGUF path.
    pub fn initial_params(&self) -> &LLMParams {
        &self.initial_config
    }

    /// Attach a mesh handle at construction time (consuming builder).
    ///
    /// Use this when the mesh is available before the `SessionProvider` is
    /// wrapped in an `Arc`. For late injection (mesh bootstrapped after
    /// `Arc<SessionProvider>` is shared), use `set_mesh` instead.
    #[cfg(feature = "remote")]
    pub fn with_mesh(self, mesh: Option<crate::agent::remote::MeshHandle>) -> Self {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = mesh;
        self
    }

    /// Inject or replace the mesh handle after construction.
    ///
    /// Because `mesh` is wrapped in `Arc<StdMutex<...>>`, this works even when
    /// the `SessionProvider` is already shared behind an `Arc`. All clones of
    /// this `SessionProvider` share the same `Arc` and therefore see the update
    /// immediately — no restart or rebuild is required.
    ///
    /// Called by `AgentHandle::set_mesh` so that sessions created by a
    /// `RemoteNodeManager` (which holds `Arc<AgentConfig>` with this provider)
    /// can route LLM calls through the mesh even though the mesh was bootstrapped
    /// after the config was built.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: Option<crate::agent::remote::MeshHandle>) {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = mesh;
    }

    /// Enable/disable automatic mesh fallback when `provider_node_id` is not set.
    ///
    /// When disabled (default), `provider_node_id = None` means local-only lookup.
    /// When enabled, unresolved providers may be discovered on mesh peers.
    #[cfg(feature = "remote")]
    pub fn set_mesh_fallback(&self, enabled: bool) {
        *self
            .allow_mesh_fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
    }

    /// Attach mesh fallback policy at construction time (consuming builder).
    #[cfg(feature = "remote")]
    pub fn with_mesh_fallback(self, enabled: bool) -> Self {
        *self
            .allow_mesh_fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = enabled;
        self
    }

    /// Fetch an existing session by ID
    pub async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>> {
        self.history_store.get_session(session_id).await
    }

    /// Load an existing session by ID
    pub async fn with_session(&self, session_id: &str) -> SessionResult<SessionHandle> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;
        SessionHandle::new(Arc::new(self.clone()), session).await
    }

    /// Create a new session with optional cwd and parent
    pub async fn create_session(
        &self,
        cwd: Option<std::path::PathBuf>,
        parent_session_id: Option<&str>,
        execution_config: &SessionExecutionConfig,
    ) -> SessionResult<SessionHandle> {
        let fork_origin = if parent_session_id.is_some() {
            Some(crate::session::domain::ForkOrigin::Delegation)
        } else {
            None
        };
        let mut session = self
            .history_store
            .create_session(
                None,
                cwd,
                parent_session_id.map(|s| s.to_string()),
                fork_origin,
            )
            .await?;
        let llm_config = self
            .history_store
            .create_or_get_llm_config(&self.initial_config)
            .await?;
        self.history_store
            .set_session_llm_config(&session.public_id, llm_config.id)
            .await?;
        self.history_store
            .set_session_execution_config(&session.public_id, execution_config)
            .await?;
        session.llm_config_id = Some(llm_config.id);
        SessionHandle::new(Arc::new(self.clone()), session).await
    }

    pub fn history_store(&self) -> Arc<dyn SessionStore> {
        self.history_store.clone()
    }

    pub fn plugin_registry(&self) -> Arc<PluginRegistry> {
        self.plugin_registry.clone()
    }

    pub fn initial_config(&self) -> &LLMParams {
        &self.initial_config
    }

    pub async fn build_provider_for_session(
        &self,
        session_id: &str,
    ) -> SessionResult<Arc<dyn LLMProvider>> {
        let config = self
            .history_store
            .get_session_llm_config(session_id)
            .await?
            .ok_or_else(|| {
                SessionError::InvalidOperation("Session has no LLM config".to_string())
            })?;

        // `parse_llm_config_row` always returns `provider_node_id: None` because
        // provider_node_id lives in the `sessions` table, not `llm_configs`.
        // Read it separately so mesh routing (Case 1 in build_provider_from_config)
        // is actually triggered for remote sessions.
        #[cfg(feature = "remote")]
        let provider_node_id: Option<String> = self
            .history_store
            .get_session_provider_node_id(session_id)
            .await
            .unwrap_or(None);
        #[cfg(not(feature = "remote"))]
        let provider_node_id: Option<String> = None;

        #[cfg(feature = "remote")]
        let allow_mesh_fallback = *self
            .allow_mesh_fallback
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        #[cfg(not(feature = "remote"))]
        let allow_mesh_fallback = false;

        // Check cache first - fast path for same config + provider_node_id + fallback policy
        {
            let cache = self.cached_provider.read().await;
            if let Some((
                cached_config_id,
                cached_provider_node_id,
                cached_allow_mesh_fallback,
                cached_provider,
            )) = cache.as_ref()
                && *cached_config_id == config.id
                && cached_provider_node_id.as_deref() == provider_node_id.as_deref()
                && *cached_allow_mesh_fallback == allow_mesh_fallback
            {
                log::trace!(
                    "Provider cache hit for config_id={} ({}:{}) provider_node_id={:?} allow_mesh_fallback={}",
                    config.id,
                    config.provider,
                    config.model,
                    provider_node_id,
                    allow_mesh_fallback,
                );
                return Ok(Arc::clone(cached_provider));
            }
        }

        // Cache miss or config/provider_node_id changed - build new provider
        log::debug!(
            "Provider cache miss for config_id={} ({}:{}) provider_node_id={:?} allow_mesh_fallback={}, building new provider",
            config.id,
            config.provider,
            config.model,
            provider_node_id,
            allow_mesh_fallback,
        );

        // Read the mesh handle outside of any async context — StdMutex must not
        // be held across await points.
        #[cfg(feature = "remote")]
        let mesh_handle: Option<crate::agent::remote::MeshHandle> =
            self.mesh.lock().unwrap_or_else(|e| e.into_inner()).clone();

        #[cfg(feature = "remote")]
        let routing = ProviderRouting {
            provider_node_id: provider_node_id.as_deref(),
            mesh_handle: mesh_handle.as_ref(),
            allow_mesh_fallback,
        };

        let provider = build_provider_from_config(
            &self.plugin_registry,
            &config.provider,
            &config.model,
            config.params.as_ref(),
            None,
            #[cfg(feature = "remote")]
            routing,
        )
        .await?;

        // Update cache - this will drop the old provider if config changed.
        // For GPU models (llama_cpp), this frees VRAM before loading the new model.
        {
            let mut cache = self.cached_provider.write().await;
            *cache = Some((
                config.id,
                provider_node_id,
                allow_mesh_fallback,
                Arc::clone(&provider),
            ));
        }

        Ok(provider)
    }

    /// Clear the global provider cache.
    ///
    /// This forces the next provider resolution to rebuild from current config/credentials.
    pub async fn clear_provider_cache(&self) {
        let mut cache = self.cached_provider.write().await;
        *cache = None;
    }

    /// Get pricing information for a session's model
    ///
    /// Returns `None` if:
    /// - The session doesn't have an LLM config
    /// - Pricing information is not available for the model
    pub async fn get_session_pricing(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<ModelPricing>> {
        let llm_config = self
            .history_store
            .get_session_llm_config(session_id)
            .await?;

        Ok(llm_config
            .and_then(|config| get_model_info(&config.provider, &config.model))
            .map(|info| info.pricing))
    }

    /// Get pricing information for a specific provider and model
    pub fn get_pricing(provider: &str, model: &str) -> Option<ModelPricing> {
        get_model_info(provider, model).map(|info| info.pricing)
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> SessionResult<Option<LLMConfig>> {
        self.history_store.get_llm_config(config_id).await
    }
}

impl Clone for SessionProvider {
    fn clone(&self) -> Self {
        Self {
            plugin_registry: self.plugin_registry.clone(),
            history_store: Arc::clone(&self.history_store),
            initial_config: self.initial_config.clone(),
            cached_provider: Arc::clone(&self.cached_provider),
            // Share the same Arcs so all clones see runtime updates.
            #[cfg(feature = "remote")]
            mesh: Arc::clone(&self.mesh),
            #[cfg(feature = "remote")]
            allow_mesh_fallback: Arc::clone(&self.allow_mesh_fallback),
        }
    }
}

/// A handle to an active session, providing access to the session's LLM provider
/// and message history.
///
/// This handle encapsulates the session state and provides methods for interacting
/// with the LLM and managing conversation history.
///
/// # Turn-pinned semantics
///
/// A `SessionHandle` captures the session's `LLMConfig` at construction time and
/// caches the resolved `LLMProvider` for reuse within a turn. Model switches
/// (e.g. via the dashboard) take effect on the **next** `run_prompt`, not mid-turn.
/// This avoids redundant DB lookups and ensures consistent provider/model identity
/// throughout a single execution cycle.
pub struct SessionHandle {
    provider: Arc<SessionProvider>,
    session: Session,
    /// LLM config resolved once at construction time (turn-pinned).
    llm_config: Option<LLMConfig>,
    /// Session execution config resolved once at construction time (turn-pinned).
    execution_config: Option<SessionExecutionConfig>,
    /// Lazily cached LLM provider for this turn.
    cached_llm_provider: tokio::sync::OnceCell<Arc<dyn LLMProvider>>,
}

impl Clone for SessionHandle {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            session: self.session.clone(),
            llm_config: self.llm_config.clone(),
            execution_config: self.execution_config.clone(),
            // Each clone gets its own OnceCell; the first `.provider()` call
            // will still hit the global cache (cheap Arc::clone on hit) so
            // this is fine — it just won't share the local cell.
            cached_llm_provider: tokio::sync::OnceCell::new(),
        }
    }
}

impl SessionHandle {
    pub async fn new(provider: Arc<SessionProvider>, session: Session) -> SessionResult<Self> {
        // Eagerly resolve the LLM config so callers never need a separate DB fetch.
        let llm_config = if let Some(config_id) = session.llm_config_id {
            provider.get_llm_config(config_id).await?
        } else {
            None
        };
        let execution_config = provider
            .history_store
            .get_session_execution_config(&session.public_id)
            .await?;
        Ok(Self {
            provider,
            session,
            llm_config,
            execution_config,
            cached_llm_provider: tokio::sync::OnceCell::new(),
        })
    }

    /// Get the session information
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Get the LLM config captured at handle creation time (turn-pinned).
    pub fn llm_config(&self) -> Option<&LLMConfig> {
        self.llm_config.as_ref()
    }

    /// Get the session execution config captured at handle creation time (turn-pinned).
    pub fn execution_config(&self) -> Option<&SessionExecutionConfig> {
        self.execution_config.as_ref()
    }

    pub async fn provider(&self) -> SessionResult<Arc<dyn LLMProvider>> {
        self.cached_llm_provider
            .get_or_try_init(|| async {
                self.provider
                    .build_provider_for_session(&self.session.public_id)
                    .await
            })
            .await
            .map(Arc::clone)
    }

    /// Get the session history as rich AgentMessages
    pub async fn get_agent_history(&self) -> SessionResult<Vec<AgentMessage>> {
        self.provider
            .history_store
            .get_history(&self.session.public_id)
            .await
    }

    /// Get the session history converted to standard ChatMessages for the LLM
    pub async fn history(&self) -> Vec<ChatMessage> {
        match self.get_agent_history().await {
            Ok(agent_msgs) => {
                let start_index = agent_msgs
                    .iter()
                    .rposition(|m| {
                        m.parts
                            .iter()
                            .any(|p| matches!(p, MessagePart::CompactionRequest { .. }))
                    })
                    .unwrap_or(0);
                agent_msgs[start_index..]
                    .iter()
                    // Filter out messages that only contain snapshot metadata parts
                    // These are for undo/redo tracking and should not be sent to the LLM
                    // Keeping them creates empty messages that break tool_use -> tool_result sequencing
                    .filter(|m| {
                        m.parts.iter().any(|p| {
                            !matches!(
                                p,
                                MessagePart::TurnSnapshotStart { .. }
                                    | MessagePart::TurnSnapshotPatch { .. }
                            )
                        })
                    })
                    .map(|m| {
                        m.to_chat_message_with_max_prompt_bytes(
                            self.execution_config
                                .as_ref()
                                .and_then(|cfg| cfg.max_prompt_bytes),
                        )
                    })
                    .collect()
            }
            Err(err) => {
                log::warn!("Failed to load session history: {}", err);
                Vec::new()
            }
        }
    }

    /// Persist an AgentMessage to the store
    pub async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.provider
            .history_store
            .add_message(&self.session.public_id, message)
            .await
    }

    /// Execute a raw tool call without side effects
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        let provider = self.provider().await?;
        provider.call_tool(name, args).await
    }

    /// Submit messages to the LLM without auto-saving
    pub async fn submit_request(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let provider = self.provider().await?;
        provider.chat(messages).await
    }

    /// Higher-level chat interface (used by CLI) that handles conversion and storage
    pub async fn chat(&self, messages: &[ChatMessage]) -> SessionResult<Box<dyn ChatResponse>> {
        // 1. Store incoming messages (User or Tool Result)
        for msg in messages {
            let agent_msg = self.convert_chat_to_agent(msg);
            self.add_message(agent_msg).await?;
        }

        // 2. Fetch full history for context
        let llm_messages = self.history().await;

        // 3. Call LLM
        let response = self.submit_request(&llm_messages).await?;

        // 4. Store response
        let response_msg: ChatMessage = response.as_ref().into();
        let agent_response = self.convert_chat_to_agent(&response_msg);
        self.add_message(agent_response).await?;

        Ok(response)
    }

    /// Get pricing information for this session's model.
    ///
    /// Returns `None` for OAuth sessions (no per-token cost) or when
    /// pricing information is unavailable for the model.
    ///
    /// OAuth is detected by checking whether the cached provider has a key
    /// resolver attached — OAuth providers always do, static-key providers
    /// never do. This avoids threading a boolean through the provider stack.
    pub fn get_pricing(&self) -> Option<ModelPricing> {
        if let Some(provider) = self.cached_llm_provider.get()
            && provider.key_resolver().is_some()
        {
            return None;
        }

        self.llm_config
            .as_ref()
            .and_then(|config| get_model_info(&config.provider, &config.model))
            .map(|info| info.pricing)
    }

    pub fn convert_chat_to_agent(&self, msg: &ChatMessage) -> AgentMessage {
        let mut parts = Vec::new();

        match &msg.message_type {
            MessageType::Text => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
            MessageType::ToolUse(calls) => {
                if !msg.content.is_empty() {
                    parts.push(MessagePart::Text {
                        content: msg.content.clone(),
                    });
                }
                for call in calls {
                    parts.push(MessagePart::ToolUse(call.clone()));
                }
            }
            MessageType::ToolResult(calls) => {
                for (i, call) in calls.iter().enumerate() {
                    parts.push(MessagePart::ToolResult {
                        call_id: call.id.clone(),
                        content: if i == 0 {
                            msg.content.clone()
                        } else {
                            "(See previous result)".to_string()
                        },
                        is_error: false,
                        tool_name: Some(call.function.name.clone()),
                        tool_arguments: Some(call.function.arguments.clone()),
                        compacted_at: None,
                    });
                }
            }
            _ => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
        }

        AgentMessage {
            id: uuid::Uuid::now_v7().to_string(),
            session_id: self.session.public_id.clone(),
            role: msg.role.clone(),
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        }
    }
}

/// Build an LLM provider from configuration parameters (reusable helper)
///
/// This function encapsulates the provider construction logic, including:
/// - Routing to a remote `MeshChatProvider` when `provider_node_id` names a mesh peer
/// - Looking up the factory from the plugin registry
/// - Merging model and params into a builder config
/// - Resolving API keys (OAuth first, then env var fallback)
/// - Applying model-specific heuristic defaults
/// - Falling back to the mesh when the provider is unavailable locally (requires `remote` feature)
///
/// Used by both session-based provider construction and standalone providers
/// (e.g., for delegation summarization).
///
/// # Arguments
/// * `plugin_registry`  — local plugin registry.
/// * `provider_name`    — provider name (e.g. `"anthropic"`).
/// * `model`            — model name (e.g. `"claude-sonnet-4-20250514"`).
/// * `params`           — optional extra params JSON blob.
/// * `api_key_override` — override the API key resolved from env/OAuth.
#[cfg(feature = "remote")]
pub struct ProviderRouting<'a> {
    /// When `Some`, route the call to this mesh node's `ProviderHostActor`.
    /// `None` or `"local"` means local provider resolution.
    pub provider_node_id: Option<&'a str>,
    /// Required when `provider_node_id` is `Some` (except `"local"`) or when fallback is enabled.
    pub mesh_handle: Option<&'a crate::agent::remote::MeshHandle>,
    /// When true and `provider_node_id` is `None`, unresolved local providers may be discovered on mesh peers.
    pub allow_mesh_fallback: bool,
}

/// * `routing`          — remote routing/fallback policy (remote feature only).
pub async fn build_provider_from_config(
    plugin_registry: &PluginRegistry,
    provider_name: &str,
    model: &str,
    params: Option<&serde_json::Value>,
    api_key_override: Option<&str>,
    #[cfg(feature = "remote")] routing: ProviderRouting<'_>,
) -> SessionResult<Arc<dyn LLMProvider>> {
    // ── Case 1: Explicit remote node requested ─────────────────────────────────
    #[cfg(feature = "remote")]
    if let Some(node_id) = routing.provider_node_id
        && node_id != "local"
    {
        let mesh = routing.mesh_handle.ok_or_else(|| {
            SessionError::InvalidOperation(format!(
                "provider_node_id='{}' specified but no mesh handle available",
                node_id
            ))
        })?;
        let node_id = NodeId::parse(node_id).map_err(SessionError::InvalidOperation)?;
        log::debug!(
            "build_provider_from_config: routing {}/{} to mesh node '{}'",
            provider_name,
            model,
            node_id
        );
        return Ok(Arc::new(
            crate::agent::remote::mesh_provider::MeshChatProvider::from_node_id(
                mesh,
                &node_id,
                provider_name,
                model,
            ),
        ));
    }

    // ── Case 2: Try local provider (existing logic) ────────────────────────────
    let factory = plugin_registry.get(provider_name).await;

    #[cfg(not(feature = "remote"))]
    let factory = factory.ok_or_else(|| {
        SessionError::InvalidOperation(format!("Unknown provider: {}", provider_name))
    })?;

    // With the remote feature enabled we may optionally fall back to the mesh
    // (Case 3 below), so we don't error-out immediately.
    #[cfg(feature = "remote")]
    let factory = match factory {
        Some(f) => f,
        None => {
            // ── Case 3: Not available locally → optional mesh fallback ─────────
            if routing.allow_mesh_fallback
                && let Some(mesh) = routing.mesh_handle
            {
                log::debug!(
                    "build_provider_from_config: provider '{}' not found locally, searching mesh",
                    provider_name
                );
                if let Some(node_id) =
                    crate::agent::remote::mesh_provider::find_provider_on_mesh(mesh, provider_name)
                        .await
                {
                    log::info!(
                        "build_provider_from_config: found '{}' on mesh peer '{}', using MeshChatProvider",
                        provider_name,
                        node_id
                    );
                    return Ok(Arc::new(
                        crate::agent::remote::mesh_provider::MeshChatProvider::from_node_id(
                            mesh,
                            &node_id,
                            provider_name,
                            model,
                        ),
                    ));
                }
            }
            return Err(SessionError::InvalidOperation(format!(
                "Unknown provider: {}",
                provider_name
            )));
        }
    };

    // Build config JSON, starting with model
    let mut builder_config = serde_json::json!({ "model": model });

    // Merge params if provided
    if let Some(params_value) = params
        && let Some(obj) = params_value.as_object()
    {
        for (key, value) in obj {
            builder_config[key] = value.clone();
        }
    }

    // Apply model/provider heuristic defaults (only fills keys not already present)
    let defaults = ModelDefaults::for_model(provider_name, model);
    defaults.apply_to(&mut builder_config, "standalone");

    // Track whether we should attach an OAuth key resolver after construction.
    // The resolver enables transparent token refresh without rebuilding the provider.
    let mut _use_oauth_resolver = false;

    // Get API key - try override, then OAuth (if feature enabled), then env var
    if let Some(http_factory) = factory.as_http()
        && let Some(env_var_name) = http_factory.api_key_name()
    {
        let api_key = if let Some(key) = api_key_override {
            Some(key.to_string())
        } else {
            #[cfg(feature = "oauth")]
            {
                use crate::auth::get_or_refresh_token;

                log::debug!("Resolving API key for provider: {}", provider_name);

                // Try OAuth tokens first
                match get_or_refresh_token(provider_name).await {
                    Ok(token) => {
                        log::debug!("Using OAuth token for provider: {}", provider_name);
                        _use_oauth_resolver = true;
                        Some(token)
                    }
                    Err(e) => {
                        // OAuth failed - fall back to environment variable
                        log::debug!("OAuth unavailable for {}: {}", provider_name, e);
                        log::debug!("Falling back to env var: {}", env_var_name);
                        std::env::var(&env_var_name).ok()
                    }
                }
            }
            #[cfg(not(feature = "oauth"))]
            {
                std::env::var(&env_var_name).ok()
            }
        };

        if let Some(key) = api_key {
            builder_config["api_key"] = key.into();
        } else {
            // Both OAuth and env var failed
            log::warn!(
                "No API key found for provider '{}'. Set {} or run 'qmt auth login {}'",
                provider_name,
                env_var_name,
                provider_name
            );
        }
    }

    // Prune config by provider schema to avoid providers with
    // `deny_unknown_fields` rejecting unrelated parameters.
    let schema: Value = serde_json::from_str(&factory.config_schema())?;
    let pruned_config = prune_config_by_schema(&builder_config, &schema);

    let pruned_keys = pruned_top_level_keys(&builder_config, &pruned_config);
    if !pruned_keys.is_empty() {
        const MAX_KEYS_TO_LOG: usize = 50;
        let shown = pruned_keys.len().min(MAX_KEYS_TO_LOG);
        let suffix = if pruned_keys.len() > shown {
            format!(" (+{} more)", pruned_keys.len() - shown)
        } else {
            String::new()
        };

        log::warn!(
            "Pruned unsupported config keys for provider '{}': {}{}",
            provider_name,
            pruned_keys[..shown].join(", "),
            suffix
        );
    }

    let pruned_config_str = serde_json::to_string(&pruned_config)?;

    // If OAuth was used, attach a resolver so expired tokens are refreshed
    // transparently on each request.
    //
    // We try the generic LLMProviderFactory path first: Extism providers
    // handle HTTP internally and accept set_key_resolver directly on the
    // LLMProvider. For native HTTP providers the generic path wraps the
    // inner HTTP provider in Arc before we can set the resolver, so we
    // fall back to the HTTPLLMProviderFactory path to set it on the inner
    // provider before wrapping.
    #[cfg(feature = "oauth")]
    if _use_oauth_resolver {
        let initial_key = builder_config
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let resolver = std::sync::Arc::new(crate::auth::OAuthKeyResolver::new(
            provider_name,
            &initial_key,
        ));

        // Generic path — works for Extism providers that implement set_key_resolver.
        let mut provider = factory.from_config(&pruned_config_str)?;
        provider.set_key_resolver(resolver.clone());

        if provider.key_resolver().is_some() {
            log::debug!(
                "Attached OAuthKeyResolver via LLMProvider for '{}' (model: {})",
                provider_name,
                model
            );
            return Ok(Arc::from(provider));
        }

        // Fallback for native HTTP providers: set the resolver on the inner
        // HTTPLLMProvider before it gets wrapped in Arc by the adapter.
        if let Some(http_factory) = factory.as_http() {
            let mut http_provider = http_factory.from_config(&pruned_config_str)?;
            http_provider.set_key_resolver(resolver);

            log::debug!(
                "Attached OAuthKeyResolver via HTTPLLMProvider for '{}' (model: {})",
                provider_name,
                model
            );

            let arc_provider: std::sync::Arc<dyn querymt::HTTPLLMProvider> =
                std::sync::Arc::from(http_provider);
            let adapter = querymt::adapters::LLMProviderFromHTTP::new(arc_provider);
            return Ok(Arc::from(Box::new(adapter) as Box<dyn LLMProvider>));
        }

        // Neither path attached a resolver — return the provider as-is.
        log::warn!(
            "OAuthKeyResolver could not be attached for provider '{}' (model: {})",
            provider_name,
            model
        );
        return Ok(Arc::from(provider));
    }

    let provider = factory.from_config(&pruned_config_str)?;
    Ok(Arc::from(provider))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use querymt::chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk};
    use querymt::completion::{CompletionRequest, CompletionResponse};
    use querymt::error::LLMError;
    use std::pin::Pin;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    /// Mock LLM provider for testing
    pub struct MockProvider {
        response_text: String,
    }

    impl Default for MockProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockProvider {
        pub fn new() -> Self {
            Self {
                response_text: "Mock response".to_string(),
            }
        }

        #[allow(dead_code)]
        pub fn with_response(response: String) -> Self {
            Self {
                response_text: response,
            }
        }
    }

    // ChatResponse implementation
    #[derive(Debug)]
    struct MockChatResponse {
        content: String,
    }

    impl std::fmt::Display for MockChatResponse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.content)
        }
    }

    impl ChatResponse for MockChatResponse {
        fn text(&self) -> Option<String> {
            Some(self.content.clone())
        }

        fn thinking(&self) -> Option<String> {
            None
        }

        fn usage(&self) -> Option<querymt::Usage> {
            None
        }

        fn finish_reason(&self) -> Option<FinishReason> {
            Some(FinishReason::Stop)
        }

        fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
            None
        }
    }

    #[async_trait::async_trait]
    impl querymt::chat::ChatProvider for MockProvider {
        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[querymt::chat::Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            Ok(Box::new(MockChatResponse {
                content: self.response_text.clone(),
            }))
        }

        async fn chat_stream_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[querymt::chat::Tool]>,
        ) -> Result<
            Pin<Box<dyn tokio_stream::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
            LLMError,
        > {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Box::pin(ReceiverStream::new(rx)))
        }
    }

    #[async_trait::async_trait]
    impl querymt::completion::CompletionProvider for MockProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("Not implemented".into()))
        }
    }

    #[async_trait::async_trait]
    impl querymt::embedding::EmbeddingProvider for MockProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("Not implemented".into()))
        }
    }

    impl LLMProvider for MockProvider {}
}
