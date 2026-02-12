use crate::model::{AgentMessage, MessagePart};
use crate::model_heuristics::ModelDefaults;
use crate::model_info::get_model_info;
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::{LLMConfig, Session, SessionStore};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt::providers::ModelPricing;
use querymt::{
    LLMProvider,
    chat::{ChatMessage, ChatResponse, MessageType},
    error::LLMError,
};
use serde_json::{Map, Value};
use std::sync::Arc;
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

/// Type alias for the provider cache: (config_id, provider) pair
type ProviderCache = Arc<RwLock<Option<(i64, Arc<dyn LLMProvider>)>>>;

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
        }
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

        // Check cache first - fast path for same config
        {
            let cache = self.cached_provider.read().await;
            if let Some((cached_config_id, cached_provider)) = cache.as_ref()
                && *cached_config_id == config.id
            {
                log::trace!(
                    "Provider cache hit for config_id={} ({}:{})",
                    config.id,
                    config.provider,
                    config.model
                );
                return Ok(Arc::clone(cached_provider));
            }
        }

        // Cache miss or config changed - build new provider
        log::debug!(
            "Provider cache miss for config_id={} ({}:{}), building new provider",
            config.id,
            config.provider,
            config.model
        );

        let provider = build_provider_from_config(
            &self.plugin_registry,
            &config.provider,
            &config.model,
            config.params.as_ref(),
            None,
        )
        .await?;

        // Update cache - this will drop the old provider if config changed
        // For GPU models (llama_cpp), this frees VRAM before loading the new model
        {
            let mut cache = self.cached_provider.write().await;
            *cache = Some((config.id, Arc::clone(&provider)));
        }

        Ok(provider)
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
    /// Lazily cached LLM provider for this turn.
    cached_llm_provider: tokio::sync::OnceCell<Arc<dyn LLMProvider>>,
}

impl Clone for SessionHandle {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            session: self.session.clone(),
            llm_config: self.llm_config.clone(),
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
        Ok(Self {
            provider,
            session,
            llm_config,
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
                            .any(|p| matches!(p, MessagePart::Compaction { .. }))
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
                    .map(|m| m.to_chat_message())
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
    /// Uses the turn-pinned `LLMConfig` instead of querying the DB.
    pub fn get_pricing(&self) -> Option<ModelPricing> {
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
/// - Looking up the factory from the plugin registry
/// - Merging model and params into a builder config
/// - Resolving API keys (OAuth first, then env var fallback)
/// - Applying model-specific heuristic defaults
///
/// Used by both session-based provider construction and standalone providers
/// (e.g., for delegation summarization).
pub async fn build_provider_from_config(
    plugin_registry: &PluginRegistry,
    provider_name: &str,
    model: &str,
    params: Option<&serde_json::Value>,
    api_key_override: Option<&str>,
) -> SessionResult<Arc<dyn LLMProvider>> {
    let factory = plugin_registry.get(provider_name).await.ok_or_else(|| {
        SessionError::InvalidOperation(format!("Unknown provider: {}", provider_name))
    })?;

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
