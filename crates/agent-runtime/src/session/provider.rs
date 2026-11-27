#[cfg(feature = "remote")]
use crate::agent::remote::NodeId;
use crate::middleware::ModelProfile;
use crate::session::error::{SessionError, SessionResult};
use crate::session::provider_config::{ProviderConfigMode, resolve_provider_config};
use crate::session::store::{LLMConfig, Session, SessionExecutionConfig, SessionStore};
use arc_swap::ArcSwap;
use querymt::LLMParams;
use querymt::plugin::host::{PluginRegistry, ProviderResolver};
use querymt::providers::{BillingClass, ModelPricing};
use querymt::{
    LLMProvider,
    chat::{GenerationOptions, PromptCacheCapabilities},
    error::LLMError,
    plugin::LLMProviderFactory,
};
use querymt_agent_core::model::AgentMessage;
use querymt_agent_services::accounting::{
    BillingBasis, is_non_billable_oauth_provider, pricing_for, resolve_billing_basis,
};
use querymt_agent_services::model_heuristics::{OutputCaps, apply_system_cache_defaults};
use querymt_agent_services::model_info::get_model_info;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
#[cfg(feature = "remote")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// Keep accepting the documented node selector on existing requests and rows.
#[cfg(feature = "remote")]
const LOCAL_PROVIDER_NODE: &str = "local";

/// Cached provider entry: `(config_id, provider_node_id, allow_mesh_fallback, provider)`.
///
/// `provider_node_id` is stored separately from `llm_configs` (in the `sessions`
/// table) so the same `config_id` can resolve to different providers depending
/// on which mesh node owns the session. `allow_mesh_fallback` is included so
/// toggling policy does not reuse a provider built under different routing rules.
type ProviderCacheEntry = (i64, Option<String>, bool, Arc<dyn LLMProvider>);

/// Lock-free single-entry provider cache.  Reads (every turn) use `ArcSwap::load`,
/// writes (model switch, mesh fallback toggle) use `ArcSwap::store`.
type ProviderCache = Arc<ArcSwap<Option<ProviderCacheEntry>>>;

/// Prompt-cache declarations read from built providers, keyed by a hash of the
/// provider configuration (the hash is never persisted, so it does not expose
/// the API key). Lets a rebuild of a known configuration skip the probe.
type DeclarationCache = Arc<Mutex<HashMap<u64, PromptCacheCapabilities>>>;

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
    provider_resolver: Arc<dyn ProviderResolver>,
    history_store: Arc<dyn SessionStore>,
    initial_config: LLMParams,
    /// Lock-free single-entry provider cache keyed on `LLMConfig.id`.
    /// Uses a single-entry cache to ensure safe VRAM management for GPU models.
    cached_provider: ProviderCache,
    /// Prompt-cache declarations per provider configuration.
    declarations: DeclarationCache,
    /// `[agent.execution.prompt_cache] ttl_secs`: the lifetime default system
    /// cache hints request, capped at what the provider declares. 0 means the
    /// provider's default lifetime. Shared by clones, like the other runtime
    /// settings, because the agent builder sets it after the provider is shared.
    cache_ttl_secs: Arc<AtomicU32>,
    /// Agent identifier used for `{{ agent_id }}` in system prompt templates.
    /// Set at build time via `with_agent_id` so that session creation can
    /// resolve per-agent template variables.
    pub(crate) agent_id: Option<String>,
    /// Optional mesh handle — present when this node participates in a kameo mesh.
    /// Passed through to `build_provider` to enable `MeshChatProvider`
    /// routing and mesh-fallback discovery.
    ///
    /// A watch lets deferred peer work wake when policy becomes available.
    #[cfg(feature = "remote")]
    pub(crate) mesh: tokio::sync::watch::Sender<Option<Arc<crate::agent::remote::MeshHandle>>>,
    /// Whether to scan the mesh when a provider is missing locally and
    /// `provider_node_id` is not explicitly set. Defaults to false.
    #[cfg(feature = "remote")]
    allow_mesh_fallback: Arc<AtomicBool>,
}

impl SessionProvider {
    pub fn new(
        plugin_registry: Arc<PluginRegistry>,
        store: Arc<dyn SessionStore>,
        initial_config: LLMParams,
    ) -> Self {
        let provider_resolver: Arc<dyn ProviderResolver> = plugin_registry.clone();
        Self {
            plugin_registry,
            provider_resolver,
            history_store: store,
            initial_config,
            cached_provider: Arc::new(ArcSwap::from_pointee(None)),
            declarations: Arc::default(),
            cache_ttl_secs: Arc::default(),
            agent_id: None,
            #[cfg(feature = "remote")]
            mesh: tokio::sync::watch::Sender::new(None),
            #[cfg(feature = "remote")]
            allow_mesh_fallback: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sets the lifetime default system cache hints request (seconds, 0 for the
    /// provider's default). Applies to providers built after the call.
    pub fn set_cache_ttl_secs(&self, ttl_secs: u32) {
        self.cache_ttl_secs.store(ttl_secs, Ordering::Relaxed);
    }

    /// The lifetime default system cache hints request (0: provider default).
    pub fn cache_ttl_secs(&self) -> u32 {
        self.cache_ttl_secs.load(Ordering::Relaxed)
    }

    pub fn with_provider_resolver(mut self, resolver: Arc<dyn ProviderResolver>) -> Self {
        self.provider_resolver = resolver;
        self
    }

    pub(crate) fn provider_resolver(&self) -> Arc<dyn ProviderResolver> {
        self.provider_resolver.clone()
    }

    /// Set the agent identifier used for `{{ agent_id }}` in system prompt
    /// templates.  Call at build time before wrapping in `Arc`.
    pub fn with_agent_id(mut self, id: Option<String>) -> Self {
        self.agent_id = id;
        self
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
        self.set_mesh(mesh);
        self
    }

    /// Inject or replace the mesh handle after construction.
    ///
    /// Called by `AgentHandle::set_mesh` so that sessions created by a
    /// `RemoteNodeManager` (which holds `Arc<AgentConfig>` with this provider)
    /// can route LLM calls through the mesh even though the mesh was bootstrapped
    /// after the config was built.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: Option<crate::agent::remote::MeshHandle>) {
        self.mesh.send_replace(mesh.map(Arc::new));
    }

    #[cfg(feature = "remote")]
    pub(crate) fn mesh_handle(&self) -> Option<Arc<crate::agent::remote::MeshHandle>> {
        self.mesh.borrow().clone()
    }

    /// Enable/disable automatic mesh fallback when `provider_node_id` is not set.
    ///
    /// When disabled (default), `provider_node_id = None` means local-only lookup.
    /// When enabled, unresolved providers may be discovered on mesh peers.
    #[cfg(feature = "remote")]
    pub fn set_mesh_fallback(&self, enabled: bool) {
        self.allow_mesh_fallback.store(enabled, Ordering::Relaxed);
    }

    /// Attach mesh fallback policy at construction time (consuming builder).
    #[cfg(feature = "remote")]
    pub fn with_mesh_fallback(self, enabled: bool) -> Self {
        self.allow_mesh_fallback.store(enabled, Ordering::Relaxed);
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
        // A parent given here always means delegation; other lineage goes
        // through `create_session_with_isolation_intent` explicitly.
        let fork_origin = parent_session_id.map(|_| crate::session::domain::ForkOrigin::Delegation);
        self.create_session_with_isolation_intent(
            cwd,
            parent_session_id,
            fork_origin,
            execution_config,
            None,
        )
        .await
    }

    /// Create a session whose non-runnable isolation barrier (when `intent`
    /// is present) is persisted atomically with the session row.
    pub async fn create_session_with_isolation_intent(
        &self,
        cwd: Option<std::path::PathBuf>,
        parent_session_id: Option<&str>,
        fork_origin: Option<crate::session::domain::ForkOrigin>,
        execution_config: &SessionExecutionConfig,
        intent: Option<crate::session::store::PendingForkIsolation>,
    ) -> SessionResult<SessionHandle> {
        let session = self
            .history_store
            .create_session_with_isolation_intent(
                None,
                cwd.clone(),
                parent_session_id.map(|s| s.to_string()),
                fork_origin,
                intent,
            )
            .await?;
        self.configure_new_session(session, cwd.as_deref(), execution_config)
            .await
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn create_peer_session(
        &self,
        origin: crate::session::domain_schedule::ScheduleOrigin,
        execution_config: &SessionExecutionConfig,
    ) -> SessionResult<SessionHandle> {
        let session = self.history_store.create_peer_session(origin).await?;
        let id = session.public_id.clone();
        let result = self
            .configure_new_session(session, None, execution_config)
            .await;
        if result.is_err()
            && let Err(error) = self.history_store.delete_session(&id).await
        {
            tracing::warn!(session_id = %id, %error, "failed to remove unconfigured peer session");
        }
        result
    }

    async fn configure_new_session(
        &self,
        mut session: crate::session::store::Session,
        cwd: Option<&std::path::Path>,
        execution_config: &SessionExecutionConfig,
    ) -> SessionResult<SessionHandle> {
        // Resolve session-specific template variables (cwd, datetime, …) in
        // system prompt strings before writing the config to the database.
        let resolved_config = {
            let mut builder = crate::template::SessionTemplateContext::builder();
            if let Some(p) = cwd {
                builder = builder.cwd(p);
            }
            if let Some(ref p) = self.initial_config.provider {
                builder = builder.provider(p.as_str());
            }
            if let Some(ref m) = self.initial_config.model {
                builder = builder.model(m.as_str());
            }
            if let Some(ref id) = self.agent_id {
                builder = builder.agent_id(id.as_str());
            }
            #[cfg(feature = "remote")]
            {
                if self.mesh.borrow().is_some() {
                    builder = builder.has_mesh(true);
                }
            }
            let ctx = builder.build();
            ctx.resolve_params(&self.initial_config).map_err(|e| {
                SessionError::InvalidOperation(format!(
                    "Failed to resolve system prompt templates: {e}"
                ))
            })?
        };

        let llm_config = self
            .history_store
            .create_or_get_llm_config(&resolved_config)
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

    /// Base parameters for a model selection.
    ///
    /// Selecting exactly the profile's provider and model keeps the profile's
    /// parameters; any other selection starts from an empty configuration so
    /// model-specific settings (for example `n_ctx`) do not leak across models.
    /// Callers layer the session's system prompt and reasoning effort on top.
    pub fn selection_params(&self, provider: &str, model: &str) -> LLMParams {
        let is_profile_model = self.initial_config.provider.as_deref() == Some(provider)
            && self.initial_config.model.as_deref() == Some(model);
        let base = if is_profile_model {
            self.initial_config.clone()
        } else {
            LLMParams::new()
        };
        base.provider(provider).model(model)
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
        // Read it separately so mesh routing (Case 1) is actually triggered
        // for remote sessions.
        #[cfg(feature = "remote")]
        let provider_node_id: Option<String> = self
            .history_store
            .get_session_provider_node_id(session_id)
            .await
            .unwrap_or(None);
        #[cfg(not(feature = "remote"))]
        let provider_node_id: Option<String> = None;

        #[cfg(feature = "remote")]
        let allow_mesh_fallback = self.allow_mesh_fallback.load(Ordering::Relaxed);
        #[cfg(not(feature = "remote"))]
        let allow_mesh_fallback = false;

        // Check cache first - lock-free fast path for same config + provider_node_id + fallback policy
        {
            let cache = self.cached_provider.load();
            if let Some(ref entry) = **cache {
                let (
                    ref cached_config_id,
                    ref cached_provider_node_id,
                    cached_allow_mesh_fallback,
                    ref cached_provider,
                ) = *entry;
                if *cached_config_id == config.id
                    && cached_provider_node_id.as_deref() == provider_node_id.as_deref()
                    && cached_allow_mesh_fallback == allow_mesh_fallback
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

        let req = ProviderRequest::new(&config.provider, &config.model)
            .with_params(config.params.as_ref())
            .with_session_id(session_id);
        #[cfg(feature = "remote")]
        let req = req.with_provider_node_id(provider_node_id.as_deref());

        let provider = self.build_provider(req).await?;

        // Update cache - this will drop the old provider if config changed.
        // For GPU models (llama_cpp), this frees VRAM before loading the new model.
        self.cached_provider.store(Arc::new(Some((
            config.id,
            provider_node_id,
            allow_mesh_fallback,
            Arc::clone(&provider),
        ))));

        Ok(provider)
    }

    /// Clear the global provider cache.
    ///
    /// This forces the next provider resolution to rebuild from current config/credentials.
    pub async fn clear_provider_cache(&self) {
        self.cached_provider.store(Arc::new(None));
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

        Ok(llm_config.and_then(|config| {
            if is_non_billable_oauth_provider(&config.provider) {
                return None;
            }
            get_model_info(&config.provider, &config.model).map(|info| info.pricing)
        }))
    }

    /// Get pricing information for a specific provider and model
    pub fn get_pricing(provider: &str, model: &str) -> Option<ModelPricing> {
        pricing_for(provider, model)
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> SessionResult<Option<LLMConfig>> {
        self.history_store.get_llm_config(config_id).await
    }

    /// Build an LLM provider from a [`ProviderRequest`].
    ///
    /// Infrastructure (plugin registry, mesh handle, mesh-fallback policy) is
    /// read from `&self`. The [`ProviderRequest`] carries per-call values:
    /// provider name, model, params, optional API-key override, session ID
    /// (for prompt-cache key substitution), and optional mesh node routing.
    ///
    /// Provider construction includes:
    /// - Routing to a remote `MeshChatProvider` when `provider_node_id` names a mesh peer
    /// - Looking up the factory from the plugin registry
    /// - Merging model and params into a builder config
    /// - Resolving API keys (respecting user's preferred auth method from dashboard)
    /// - Applying model-specific heuristic defaults
    /// - Falling back to the mesh when the provider is unavailable locally (requires `remote` feature)
    pub async fn build_provider(
        &self,
        req: ProviderRequest<'_>,
    ) -> SessionResult<Arc<dyn LLMProvider>> {
        self.build_provider_snapshot(req)
            .await
            .map(|built| built.provider)
    }

    pub(crate) async fn build_provider_snapshot(
        &self,
        req: ProviderRequest<'_>,
    ) -> SessionResult<ProviderSnapshot> {
        self.build_provider_snapshot_with_route(req, false).await
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn build_local_provider_snapshot(
        &self,
        req: ProviderRequest<'_>,
    ) -> SessionResult<ProviderSnapshot> {
        self.build_provider_snapshot_with_route(req, true).await
    }

    async fn build_provider_snapshot_with_route(
        &self,
        req: ProviderRequest<'_>,
        _local_only: bool,
    ) -> SessionResult<ProviderSnapshot> {
        let provider_name = req.provider_name;
        let model = req.model;
        let params = req.params;
        let api_key_override = req.api_key_override;
        let session_id = req.session_id;

        #[cfg(feature = "remote")]
        let explicitly_local = _local_only
            || req.provider_node_id.is_some_and(|node| {
                node == LOCAL_PROVIDER_NODE
                    || self
                        .mesh
                        .borrow()
                        .as_deref()
                        .is_some_and(|mesh| mesh.peer_id().to_string() == node)
            });
        // An owner-local selection can arrive with an explicit node ID from a viewer.
        // Resolve it locally, without falling back to another node on failure.
        #[cfg(feature = "remote")]
        if let Some(node_id) = req.provider_node_id
            && !explicitly_local
        {
            if !self.provider_resolver.allows_remote_fallback() {
                return Err(SessionError::InvalidOperation(format!(
                    "Provider '{}' is profile-locked and cannot be routed to mesh node '{}'",
                    provider_name, node_id
                )));
            }
            let mesh_guard = self.mesh_handle();
            let mesh = mesh_guard.as_deref().ok_or_else(|| {
                SessionError::InvalidOperation(format!(
                    "provider_node_id='{}' specified but no mesh handle available",
                    node_id
                ))
            })?;
            let node_id = NodeId::parse(node_id).map_err(SessionError::InvalidOperation)?;
            log::debug!(
                "build_provider: routing {}/{} to mesh node '{}'",
                provider_name,
                model,
                node_id
            );
            let remote_params = params
                .cloned()
                .map(|mut p| {
                    if let Some(obj) = p.as_object_mut() {
                        obj.insert(
                            "_remote_session_id".to_string(),
                            serde_json::Value::String(session_id.to_string()),
                        );
                    }
                    p
                })
                .or_else(|| Some(serde_json::json!({"_remote_session_id": session_id})));
            return ProviderSnapshot::remote(
                querymt_remote::MeshChatProvider::from_node_id(
                    mesh,
                    &node_id,
                    provider_name,
                    model,
                )
                .with_params(remote_params)
                .with_pass(req.model_pass.clone()),
            )
            .await;
        }

        // ── Case 2: Try local provider ─────────────────────────────────────────
        let binding = self.provider_resolver.resolve(provider_name).await;

        #[cfg(not(feature = "remote"))]
        let binding = binding.map_err(SessionError::ProviderError)?;

        // With the remote feature enabled we may optionally fall back to the mesh
        // (Case 3 below), so we don't error-out immediately.
        #[cfg(feature = "remote")]
        let binding = match binding {
            Ok(binding) => binding,
            Err(local_error) => {
                // ── Case 3: Not available locally → optional mesh fallback ─────
                let allow_mesh_fallback = self.allow_mesh_fallback.load(Ordering::Relaxed);
                let mesh_guard = self.mesh_handle();
                let mesh_handle = mesh_guard.as_deref();

                if !explicitly_local
                    && self.provider_resolver.allows_remote_fallback()
                    && allow_mesh_fallback
                    && let Some(mesh) = mesh_handle
                {
                    log::debug!(
                        "build_provider: provider '{}' not found locally, searching mesh",
                        provider_name
                    );
                    if let Some(node_id) =
                        querymt_remote::find_provider_on_mesh(mesh, provider_name, model).await
                    {
                        log::info!(
                            "build_provider: found '{}' on mesh peer '{}', using MeshChatProvider",
                            provider_name,
                            node_id
                        );
                        let remote_params = params
                            .cloned()
                            .map(|mut p| {
                                if let Some(obj) = p.as_object_mut() {
                                    obj.insert(
                                        "_remote_session_id".to_string(),
                                        serde_json::Value::String(session_id.to_string()),
                                    );
                                }
                                p
                            })
                            .or_else(|| {
                                Some(serde_json::json!({"_remote_session_id": session_id}))
                            });
                        return ProviderSnapshot::remote(
                            querymt_remote::MeshChatProvider::from_node_id(
                                mesh,
                                &node_id,
                                provider_name,
                                model,
                            )
                            .with_params(remote_params),
                        )
                        .await;
                    }
                }
                return Err(SessionError::ProviderError(local_error));
            }
        };

        let factory = binding.factory.clone();
        let resolved_cfg = resolve_provider_config(
            &binding,
            &self.initial_config,
            ProviderConfigMode::Runtime {
                model,
                params,
                api_key_override,
                session_id,
            },
        )
        .await?;

        let settings = resolved_cfg.generation_settings;
        let output_caps = resolved_cfg.output_caps;
        let reasoning_effort = resolved_cfg.reasoning_effort;

        #[cfg(feature = "oauth")]
        let builder_config = resolved_cfg.builder_config;
        let HintedConfig {
            config: pruned_config_str,
            provider: prebuilt,
        } = self.with_default_cache_hints(
            factory.as_ref(),
            resolved_cfg.pruned_config_str,
            self.cache_ttl_secs.load(Ordering::Relaxed),
        )?;
        #[cfg(feature = "remote")]
        let llama_cpp_n_ctx = llama_cpp_context_limit(factory.name(), &pruned_config_str);
        #[cfg(feature = "remote")]
        let system_tokens = querymt_remote::provider_backend::configured_system_tokens(
            serde_json::from_str(&pruned_config_str).ok().as_ref(),
        );
        let pruned_keys = resolved_cfg.pruned_keys;
        let _use_oauth_resolver = resolved_cfg.use_oauth_resolver;

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
            let mut prebuilt = prebuilt;
            let mut provider = match prebuilt.take() {
                Some(provider) => provider,
                None => factory.from_config(&pruned_config_str)?,
            };
            provider.set_key_resolver(resolver.clone());

            if provider.key_resolver().is_some() {
                log::debug!(
                    "Attached OAuthKeyResolver via LLMProvider for '{}' (model: {})",
                    provider_name,
                    model
                );
                return Ok(ProviderSnapshot {
                    provider: Arc::from(provider),
                    settings,
                    output_caps,
                    reasoning_effort,
                    #[cfg(feature = "remote")]
                    llama_cpp_n_ctx,
                    #[cfg(feature = "remote")]
                    system_tokens,
                });
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

                let adapter = querymt::adapters::LLMProviderFromHTTP::new(http_provider)
                    .with_name(provider_name);
                return Ok(ProviderSnapshot {
                    provider: Arc::from(Box::new(adapter) as Box<dyn LLMProvider>),
                    settings,
                    output_caps,
                    reasoning_effort,
                    #[cfg(feature = "remote")]
                    llama_cpp_n_ctx,
                    #[cfg(feature = "remote")]
                    system_tokens,
                });
            }

            // Neither path attached a resolver — return the provider as-is.
            log::warn!(
                "OAuthKeyResolver could not be attached for provider '{}' (model: {})",
                provider_name,
                model
            );
            return Ok(ProviderSnapshot {
                provider: Arc::from(provider),
                settings,
                output_caps,
                reasoning_effort,
                #[cfg(feature = "remote")]
                llama_cpp_n_ctx,
                #[cfg(feature = "remote")]
                system_tokens,
            });
        }

        let provider = match prebuilt {
            Some(provider) => provider,
            None => factory.from_config(&pruned_config_str)?,
        };
        Ok(ProviderSnapshot {
            provider: Arc::from(provider),
            settings,
            output_caps,
            reasoning_effort,
            #[cfg(feature = "remote")]
            llama_cpp_n_ctx,
            #[cfg(feature = "remote")]
            system_tokens,
        })
    }
}

/// A provider configuration after default cache hints were considered, with
/// the provider built from it when one is already at hand.
struct HintedConfig {
    config: String,
    provider: Option<Box<dyn LLMProvider>>,
}

impl SessionProvider {
    /// Adds default cache hints to the system prompt when the provider
    /// declares explicit breakpoints.
    ///
    /// The declaration comes from a provider built without default hints. That
    /// build is returned for reuse unless the hints change the configuration,
    /// so a provider that declares nothing (every local-model provider) is
    /// built once. A known configuration skips the probe and is built once
    /// with its hints.
    fn with_default_cache_hints(
        &self,
        factory: &dyn LLMProviderFactory,
        config: String,
        ttl_secs: u32,
    ) -> SessionResult<HintedConfig> {
        let unchanged = |config, provider| Ok(HintedConfig { config, provider });
        let mut value: serde_json::Value = serde_json::from_str(&config)
            .map_err(|e| SessionError::ProviderError(LLMError::from(e)))?;
        // Without a system prompt no default hint is due.
        if value.get("system").is_none() {
            return unchanged(config, None);
        }

        // The pruned configuration keeps only the keys the provider's schema
        // lists, so two providers can end up with the same text. What a
        // configuration declares depends on the provider that reads it.
        let key = {
            let mut hasher = DefaultHasher::new();
            factory.name().hash(&mut hasher);
            config.hash(&mut hasher);
            hasher.finish()
        };
        let known = self
            .declarations
            .lock()
            .ok()
            .and_then(|known| known.get(&key).copied());
        let (declaration, probe) = match known {
            Some(declaration) => (declaration, None),
            None => {
                let probe = factory.from_config(&config)?;
                let declaration = probe.capabilities().prompt_cache;
                if let Ok(mut known) = self.declarations.lock() {
                    known.insert(key, declaration);
                }
                (declaration, Some(probe))
            }
        };

        if !declaration.explicit_breakpoints {
            return unchanged(config, probe);
        }
        let before = value.clone();
        apply_system_cache_defaults(&mut value, &declaration, ttl_secs);
        if value == before {
            return unchanged(config, probe);
        }
        let hinted = serde_json::to_string(&value)
            .map_err(|e| SessionError::ProviderError(LLMError::from(e)))?;
        // The probe was built without the hints, so it cannot be reused.
        unchanged(hinted, None)
    }
}

impl Clone for SessionProvider {
    fn clone(&self) -> Self {
        Self {
            plugin_registry: self.plugin_registry.clone(),
            provider_resolver: self.provider_resolver.clone(),
            history_store: Arc::clone(&self.history_store),
            initial_config: self.initial_config.clone(),
            cached_provider: Arc::clone(&self.cached_provider),
            declarations: Arc::clone(&self.declarations),
            cache_ttl_secs: Arc::clone(&self.cache_ttl_secs),
            agent_id: self.agent_id.clone(),
            // Share the same Arcs/atomics so all clones see runtime updates.
            #[cfg(feature = "remote")]
            mesh: self.mesh.clone(),
            #[cfg(feature = "remote")]
            allow_mesh_fallback: Arc::clone(&self.allow_mesh_fallback),
        }
    }
}

/// The provider a run talks to and the facts about its model. Resolved once
/// per run by [`SessionHandle::resolve_model`] and fixed for the run; what the
/// run offers the model (its tools) is not part of it.
#[derive(Clone)]
pub(crate) struct ResolvedModel {
    pub(crate) provider: Arc<dyn LLMProvider>,
    /// Capabilities, output caps and configured effort.
    pub(crate) profile: Arc<ModelProfile>,
    /// The provenance the provider stamps on its output, read once when the
    /// model is resolved; `None` when it does not say (see
    /// [`SessionHandle::output_target`]).
    pub(crate) provenance: Option<querymt::chat::ChatOutputProvenance>,
}

impl ResolvedModel {
    /// A stand-in for a session whose provider could not be built, for the
    /// one run that can still make progress without it: native `/compact`
    /// (see `compact_history_with`). It has no capabilities and no
    /// provenance, and its provider fails every call with `reason`; nothing
    /// that sends a model request may be given one.
    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            provider: Arc::new(UnavailableProvider {
                reason: reason.into(),
            }),
            profile: Arc::new(ModelProfile::default()),
            provenance: None,
        }
    }
}

/// The provider of [`ResolvedModel::unavailable`]: every call fails.
struct UnavailableProvider {
    reason: String,
}

impl UnavailableProvider {
    fn error(&self) -> LLMError {
        LLMError::ProviderError(format!("provider unavailable: {}", self.reason))
    }
}

#[async_trait::async_trait]
impl querymt::chat::ChatProvider for UnavailableProvider {
    async fn chat(
        &self,
        _call: &querymt::chat::ChatCall<'_>,
    ) -> Result<querymt::chat::ChatOutput, LLMError> {
        Err(self.error())
    }
}

#[async_trait::async_trait]
impl querymt::completion::CompletionProvider for UnavailableProvider {
    async fn complete(
        &self,
        _req: &querymt::completion::CompletionRequest,
    ) -> Result<querymt::completion::CompletionResponse, LLMError> {
        Err(self.error())
    }
}

#[async_trait::async_trait]
impl querymt::embedding::EmbeddingProvider for UnavailableProvider {
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(self.error())
    }
}

#[async_trait::async_trait]
impl LLMProvider for UnavailableProvider {}

/// The provider and model a compaction request is sent to, by name; see
/// [`SessionHandle::compaction_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionTarget {
    pub(crate) provider: String,
    pub(crate) model: String,
    /// The target's provider is the session's.
    is_session_provider: bool,
    /// The target is the session's own provider and model.
    is_session_model: bool,
}

impl CompactionTarget {
    /// The request goes to the provider the session itself talks to, so
    /// building it is the session's own provider build (and fails when that
    /// does).
    pub(crate) fn is_session_model(&self) -> bool {
        self.is_session_model
    }
}

/// Where one compaction request is sent and how it is billed; see
/// [`SessionHandle::build_compaction_model`].
pub(crate) struct CompactionModel {
    pub(crate) provider: Arc<dyn LLMProvider>,
    /// The provider's name, as the accounting attempts record it.
    pub(crate) provider_name: String,
    pub(crate) model: String,
    pub(crate) basis: BillingBasis,
}

pub(crate) struct ProviderSnapshot {
    pub(crate) provider: Arc<dyn LLMProvider>,
    #[cfg(feature = "remote")]
    llama_cpp_n_ctx: Option<u64>,
    #[cfg(feature = "remote")]
    system_tokens: usize,
    settings: crate::agent::utils::genai::GenerationSettings,
    /// Output caps resolved with the provider; `None` when unknown.
    output_caps: Option<OutputCaps>,
    /// Reasoning effort the provider was configured with, when any.
    reasoning_effort: Option<querymt::chat::ReasoningEffort>,
}

#[cfg(feature = "remote")]
fn llama_cpp_context_limit(factory_name: &str, resolved_config: &str) -> Option<u64> {
    // The logical provider ID can be an alias. Only the native factory's
    // resolved n_ctx is relevant; a context middleware override is not.
    if !matches!(factory_name, "llama_cpp" | "llama.cpp") {
        return None;
    }
    let config: serde_json::Value = serde_json::from_str(resolved_config).ok()?;
    config.get("n_ctx")?.as_u64().filter(|&limit| limit > 0)
}

#[cfg(feature = "remote")]
#[derive(Clone, Copy)]
pub(crate) struct ProviderQuotaInputs {
    initial_max_tokens: Option<u32>,
    options: querymt::chat::GenerationOptionSupport,
    llama_cpp_n_ctx: Option<u64>,
    context_fallback: Option<usize>,
    system_tokens: usize,
}

#[cfg(feature = "remote")]
impl ProviderQuotaInputs {
    pub(crate) fn estimate(
        self,
        provider: &str,
        model: &str,
        call: &querymt::chat::ChatCall<'_>,
    ) -> querymt_remote::peer_access::quota::QuotaEstimate {
        let output_cap = self
            .options
            .retain_supported(call.options)
            .max_tokens
            .or(self.initial_max_tokens);
        querymt_remote::peer_access::quota::QuotaEstimate::for_chat(
            provider,
            model,
            call,
            output_cap,
            self.llama_cpp_n_ctx,
            self.context_fallback,
            self.system_tokens,
        )
    }
}

impl ProviderSnapshot {
    #[cfg(feature = "remote")]
    pub(crate) fn quota_inputs(&self, context_fallback: Option<usize>) -> ProviderQuotaInputs {
        ProviderQuotaInputs {
            initial_max_tokens: self.output_caps.map(|caps| caps.initial),
            options: self.provider.capabilities().options,
            llama_cpp_n_ctx: self.llama_cpp_n_ctx,
            context_fallback,
            system_tokens: self.system_tokens,
        }
    }

    #[cfg(feature = "remote")]
    async fn remote(provider: querymt_remote::MeshChatProvider) -> SessionResult<Self> {
        let info = provider.negotiate_contract().await?;
        Ok(Self::from_remote_contract(Arc::new(provider), &info))
    }

    #[cfg(feature = "remote")]
    fn from_remote_contract(
        provider: Arc<dyn LLMProvider>,
        info: &querymt_remote::ProviderContractInfo,
    ) -> Self {
        Self {
            provider,
            settings: Default::default(),
            output_caps: info
                .generation
                .initial_max_tokens
                .zip(info.generation.output_ceiling)
                .map(|(initial, ceiling)| OutputCaps { initial, ceiling }),
            reasoning_effort: info.generation.reasoning_effort,
            llama_cpp_n_ctx: None,
            system_tokens: 0,
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) fn contract_info(&self) -> querymt_remote::ProviderContractInfo {
        querymt_remote::ProviderContractInfo {
            item_aware_chat_version: Some(querymt_remote::ITEM_AWARE_CHAT_CONTRACT_VERSION),
            protocol_version: Some(querymt_remote::provider_protocol::MESH_PROTOCOL_VERSION),
            capabilities: self.provider.capabilities(),
            generation: querymt_remote::provider_protocol::ProviderGenerationProfile {
                initial_max_tokens: self.output_caps.map(|caps| caps.initial),
                output_ceiling: self.output_caps.map(|caps| caps.ceiling),
                reasoning_effort: self.reasoning_effort,
            },
            charged_peer: None,
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
    /// Provider route resolved once at construction time (turn-pinned).
    provider_node_id: Option<String>,
    /// Session execution config resolved once at construction time (turn-pinned).
    execution_config: Option<SessionExecutionConfig>,
    /// Lazily cached LLM provider for this turn, shared by every clone of the
    /// handle. The handle is turn-pinned (config and route are immutable), so
    /// all clones resolve the same provider; sharing means it is built once per
    /// turn instead of once per clone (the state machine clones the handle on
    /// every step). A failed init is not cached, so retries still rebuild.
    cached_llm_provider: Arc<tokio::sync::OnceCell<ProviderSnapshot>>,
    #[cfg(feature = "remote")]
    mesh_turn: Option<(Arc<crate::agent::session_actor::PeerTurn>, Option<usize>)>,
}

impl Clone for SessionHandle {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            session: self.session.clone(),
            llm_config: self.llm_config.clone(),
            provider_node_id: self.provider_node_id.clone(),
            execution_config: self.execution_config.clone(),
            cached_llm_provider: Arc::clone(&self.cached_llm_provider),
            #[cfg(feature = "remote")]
            mesh_turn: self.mesh_turn.clone(),
        }
    }
}

impl SessionHandle {
    pub async fn new(provider: Arc<SessionProvider>, session: Session) -> SessionResult<Self> {
        // Initialized sessions expose the effective config and route in one atomic snapshot.
        let control_state = provider
            .history_store
            .get_session_control(&session.public_id)
            .await?;
        let (config_id, provider_node_id) = match control_state {
            Some(state) => (
                Some(state.effective_model.llm_config_id),
                state.effective_model.provider_node_id,
            ),
            None => (
                session.llm_config_id,
                provider
                    .history_store
                    .get_session_provider_node_id(&session.public_id)
                    .await?,
            ),
        };
        // Eagerly resolve the LLM config so callers never need a separate DB fetch.
        let llm_config = if let Some(config_id) = config_id {
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
            provider_node_id,
            execution_config,
            cached_llm_provider: Arc::new(tokio::sync::OnceCell::new()),
            #[cfg(feature = "remote")]
            mesh_turn: None,
        })
    }

    #[cfg(feature = "remote")]
    pub(crate) fn for_mesh_turn(
        mut self,
        peer: &Arc<crate::agent::session_actor::PeerTurn>,
        grant: &querymt_remote::peer_access::PeerGrant,
        context_fallback: Option<usize>,
    ) -> Result<Self, crate::error::AgentError> {
        let Some(config) = self.llm_config.as_ref() else {
            return Err(self.mesh_turn_denied(peer, "the session has no model"));
        };
        let node = self.provider_node_id.as_deref();
        let host = peer.key.host.to_string();
        let third_node = node.is_some_and(|node| {
            node != LOCAL_PROVIDER_NODE && node != host && !peer.is_peer_node(Some(node))
        });
        // A pass lends the submitter's own access on a third node for this one
        // session and binding. It must name exactly what this turn runs, so a
        // pass that matches anything else refuses the turn: it never falls
        // back to this host's access.
        if let Some(pass) = peer.pass() {
            let expect = querymt_remote::ModelPassExpectation {
                issuer: Some(&peer.key.peer.to_string()),
                audience: Some(&host),
                model_node: node,
                provider: Some(&config.provider),
                model: Some(&config.model),
                session_id: Some(&self.session.public_id),
            };
            if !third_node
                || peer.protocol != querymt_remote::peer_access::SharingProtocol::V1
                || pass.verify_now(&expect).is_err()
            {
                return Err(self.mesh_turn_denied(peer, "the model pass does not match this turn"));
            }
        }
        // A model on the prompting peer's own node runs under the grant that
        // node gives this host, not under this host's model grant.
        if !peer.is_peer_node(node) {
            if third_node {
                // Below Full access, a third node's model needs the submitter's
                // pass. Full access may use this host's own access there.
                if peer.pass().is_none() && *grant != querymt_remote::peer_access::PeerGrant::Full {
                    return Err(self.mesh_turn_denied(
                        peer,
                        "a third node's model needs a model pass or Full access",
                    ));
                }
            } else if !grant.allows_model(peer.protocol, &config.provider, &config.model) {
                return Err(self.mesh_turn_denied(peer, "the grant does not include the model"));
            }
        }
        // Never reuse a provider built outside this turn's route and metering.
        self.cached_llm_provider = Arc::new(tokio::sync::OnceCell::new());
        self.mesh_turn = Some((peer.clone(), context_fallback));
        Ok(self)
    }

    #[cfg(feature = "remote")]
    fn mesh_turn_denied(
        &self,
        peer: &crate::agent::session_actor::PeerTurn,
        reason: &'static str,
    ) -> crate::error::AgentError {
        let (provider, model) = self.llm_config.as_ref().map_or(("", ""), |config| {
            (config.provider.as_str(), config.model.as_str())
        });
        tracing::warn!(
            peer = %peer.key.peer,
            session_id = %self.session.public_id,
            provider,
            model,
            provider_node = self.provider_node_id.as_deref().unwrap_or(""),
            reason,
            "mesh turn denied"
        );
        crate::error::AgentError::MeshAccessDenied
    }

    /// The mesh node the session's model is pinned to, if any.
    #[cfg(feature = "remote")]
    pub(crate) fn provider_node_id(&self) -> Option<&str> {
        self.provider_node_id.as_deref()
    }

    /// Whether this peer turn's model runs on another node: the prompting
    /// peer's own node, or a third node under the peer's pass or Full access.
    /// That node authorizes and meters the calls, so the host does neither.
    #[cfg(feature = "remote")]
    fn routes_to_peer_node(&self) -> bool {
        // `for_mesh_turn` admits only the combinations that may leave this host:
        // the peer's own node, or a third node under a pass or Full access.
        self.mesh_turn.as_ref().is_some_and(|(peer, _)| {
            self.provider_node_id.as_deref().is_some_and(|node| {
                node != LOCAL_PROVIDER_NODE && node != peer.key.host.to_string()
            })
        })
    }

    /// Get the session information
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn uses_explicit_mesh_provider(&self) -> bool {
        #[cfg(feature = "remote")]
        {
            if self.mesh_turn.is_some() {
                return self.routes_to_peer_node();
            }
            self.provider_node_id.as_deref().is_some_and(|node| {
                node != LOCAL_PROVIDER_NODE
                    && self
                        .provider
                        .mesh_handle()
                        .as_deref()
                        .is_none_or(|mesh| mesh.peer_id().to_string() != node)
            })
        }
        #[cfg(not(feature = "remote"))]
        {
            false
        }
    }

    /// A handle for the same session pinned to `model`, for tests that
    /// change the model between two requests of one run.
    #[cfg(test)]
    pub(crate) fn with_model_for_test(&self, model: &str) -> Self {
        let mut next = self.clone();
        if let Some(config) = next.llm_config.as_mut() {
            config.model = model.to_string();
        }
        next
    }

    /// Get the LLM config captured at handle creation time (turn-pinned).
    pub fn llm_config(&self) -> Option<&LLMConfig> {
        self.llm_config.as_ref()
    }

    /// The identity native replay is checked against for this session's model
    /// (`None` when no model is pinned: nothing native replays).
    ///
    /// A provider stamps its output with a protocol and an endpoint, which the
    /// session store does not record. The identity comes from the resolved
    /// model: the provenance the provider states for the configuration it was
    /// built with, so a reply stamped at another endpoint is sent in portable
    /// form.
    ///
    /// A provider that states nothing (an Extism plugin, a mesh provider, an
    /// OpenAI provider in Chat Completions mode) falls back to completing the
    /// identity from the latest stored output of the same provider and model in
    /// `history`. That fallback does not prove the endpoint is the one
    /// configured now; the provider that is sent the messages re-checks every
    /// output's provenance against its own configuration where it can.
    pub(crate) fn output_target(
        &self,
        model: &ResolvedModel,
        history: &[AgentMessage],
    ) -> Option<querymt_agent_core::model::OutputTarget> {
        self.llm_config
            .as_ref()
            .map(|llm| complete_output_target(llm, model.provenance.as_ref(), history))
    }

    /// Get the session execution config captured at handle creation time (turn-pinned).
    pub fn execution_config(&self) -> Option<&SessionExecutionConfig> {
        self.execution_config.as_ref()
    }

    /// The provider this turn talks to, built on first use.
    pub async fn provider(&self) -> SessionResult<Arc<dyn LLMProvider>> {
        self.snapshot()
            .await
            .map(|built| Arc::clone(&built.provider))
    }

    /// The provider with what was resolved alongside it, built on first use
    /// and shared by every clone of the handle. A failed build is not cached.
    async fn snapshot(&self) -> SessionResult<&ProviderSnapshot> {
        self.cached_llm_provider
            .get_or_try_init(|| async {
                let config = self.llm_config.as_ref().ok_or_else(|| {
                    SessionError::InvalidOperation("Session has no LLM config".to_string())
                })?;
                let request = ProviderRequest::new(&config.provider, &config.model)
                    .with_params(config.params.as_ref())
                    .with_session_id(&self.session.public_id);
                #[cfg(feature = "remote")]
                let request = request
                    .with_provider_node_id(self.provider_node_id.as_deref())
                    // Only this peer's own turn carries its pass, and only to the node it names.
                    .with_model_pass(
                        self.mesh_turn
                            .as_ref()
                            .and_then(|(peer, _)| peer.pass().cloned()),
                    );
                // A hosted turn runs on this node's providers under the peer's
                // grant and quotas. A turn on the peer's own model is routed
                // back to that node, which authorizes and meters it there.
                #[cfg(feature = "remote")]
                let hosted = self
                    .mesh_turn
                    .as_ref()
                    .filter(|_| !self.routes_to_peer_node());
                // Only the cold build is traced: the cached path is a pointer
                // read and must not add a span to every step.
                let span = tracing::info_span!(
                    "agent.provider.build",
                    provider = %config.provider,
                    model = %config.model,
                    // Filled in by `record_auth_source` during credential resolution.
                    auth.source = tracing::field::Empty,
                );
                let built = tracing::Instrument::instrument(
                    async {
                        #[cfg(feature = "remote")]
                        if hosted.is_some() {
                            return self.provider.build_local_provider_snapshot(request).await;
                        }
                        self.provider.build_provider_snapshot(request).await
                    },
                    span,
                )
                .await?;
                #[cfg(feature = "remote")]
                if let Some((peer, context_fallback)) = hosted {
                    let mesh = self.provider.mesh_handle().ok_or_else(|| {
                        SessionError::InvalidOperation("mesh access store unavailable".into())
                    })?;
                    let store = mesh
                        .quota_store()
                        .filter(|_| mesh.peer_id() == peer.key.host.as_peer_id())
                        .cloned()
                        .ok_or_else(|| {
                            SessionError::InvalidOperation("mesh access store unavailable".into())
                        })?;
                    let mut built = built;
                    let quota_inputs = built.quota_inputs(*context_fallback);
                    built.provider = Arc::new(
                        crate::agent::remote::provider_host_backend::hosted_provider(
                            built.provider,
                            peer,
                            store,
                            (config.provider.clone(), config.model.clone()),
                            quota_inputs,
                        ),
                    );
                    return Ok(built);
                }
                Ok(built)
            })
            .await
    }

    /// The provider this turn talks to and the facts about its model (output
    /// caps, what the provider honours per request, configured effort), both
    /// read from the same build.
    ///
    /// Builds (and caches) the provider if needed and returns the error when
    /// that fails: there is no empty profile to fall back on. A model switch
    /// builds a new handle and so a new model.
    pub(crate) async fn resolve_model(&self) -> SessionResult<ResolvedModel> {
        let built = self.snapshot().await?;
        Ok(ResolvedModel {
            provider: Arc::clone(&built.provider),
            provenance: built.provider.output_provenance(),
            profile: Arc::new(ModelProfile {
                caps: built.output_caps,
                capabilities: built.provider.capabilities(),
                reasoning_effort: built.reasoning_effort,
            }),
        })
    }

    /// The provider and model one compaction request is sent to, by name.
    ///
    /// `[agent.execution.compaction]` `provider` and `model` select them; what
    /// is left unset is the session's. Nothing is built, so this is what the
    /// compaction hooks are told before any provider exists.
    ///
    /// - Neither differs from the session's: the session's own pair.
    /// - Only the model differs: that model on the session's provider.
    /// - The provider is set: it needs a `model`, because the session's model
    ///   belongs to another provider. Loading a configuration file already
    ///   refuses that; this covers a configuration built in code.
    pub(crate) fn compaction_target(
        &self,
        policy: &crate::config::CompactionConfig,
    ) -> SessionResult<CompactionTarget> {
        let policy = Some(policy);
        // Hosted turns compact through their pinned, metered binding, not
        // through the owner's alternate compaction route.
        #[cfg(feature = "remote")]
        let policy = policy.filter(|_| self.mesh_turn.is_none());
        if let Some(policy) = policy {
            policy.validate().map_err(SessionError::InvalidOperation)?;
        }
        let session = self.llm_config.as_ref().ok_or_else(|| {
            SessionError::InvalidOperation("Session has no LLM config".to_string())
        })?;
        let provider = policy
            .and_then(|policy| policy.provider.as_deref())
            .unwrap_or(&session.provider);
        let provider_changed = provider != session.provider;
        let model = policy
            .and_then(|policy| policy.model.as_deref())
            .unwrap_or(&session.model);
        Ok(CompactionTarget {
            provider: provider.to_string(),
            model: model.to_string(),
            is_session_provider: !provider_changed,
            is_session_model: !provider_changed && model == session.model,
        })
    }

    /// Builds the provider for a [`CompactionTarget`], with what it is priced
    /// for. The same pair is what the request runs on and what its accounting
    /// attempts and span name, so the three cannot disagree.
    ///
    /// The session's own provider is reused when the target is the session's
    /// own pair. Another model is built with
    /// [`SessionProvider::selection_params`], so the session's own settings
    /// (system prompt, effort, a local model's context size) do not leak into
    /// a request for it. A model on the session's provider keeps the session's
    /// mesh node. Another provider drops the session's pinned node and is
    /// resolved like any provider: locally first, then on the mesh when mesh
    /// fallback is enabled and the provider is not available locally.
    pub(crate) async fn build_compaction_model(
        &self,
        target: &CompactionTarget,
    ) -> SessionResult<CompactionModel> {
        let (provider_name, model) = (target.provider.as_str(), target.model.as_str());
        if target.is_session_model {
            return Ok(CompactionModel {
                provider: self.provider().await?,
                provider_name: target.provider.clone(),
                model: target.model.clone(),
                basis: self.billing_basis_for_model(model),
            });
        }

        let (_, _, params) = crate::session::store::extract_llm_config_values(
            &self.provider.selection_params(provider_name, model),
        )?;
        let request = ProviderRequest::new(provider_name, model)
            .with_params(params.as_ref())
            .with_session_id(&self.session.public_id);
        #[cfg(feature = "remote")]
        let request = request.with_provider_node_id(if target.is_session_provider {
            self.provider_node_id.as_deref()
        } else {
            None
        });
        let span = tracing::info_span!(
            "agent.provider.build",
            provider = %provider_name,
            model = %model,
            compaction = true,
            auth.source = tracing::field::Empty,
        );
        let built =
            tracing::Instrument::instrument(self.provider.build_provider_snapshot(request), span)
                .await?;
        let oauth_backed = built.provider.key_resolver().is_some();
        Ok(CompactionModel {
            basis: resolve_billing_basis(provider_name, model, oauth_backed),
            provider: built.provider,
            provider_name: target.provider.clone(),
            model: target.model.clone(),
        })
    }

    /// [`Self::compaction_target`] and [`Self::build_compaction_model`] in one
    /// step, for a caller that runs no hook between them.
    pub(crate) async fn resolve_compaction_model(
        &self,
        policy: &crate::config::CompactionConfig,
    ) -> SessionResult<CompactionModel> {
        self.build_compaction_model(&self.compaction_target(policy)?)
            .await
    }

    /// Record the request settings on the chat span: the configured values
    /// with this request's applied overrides on top.
    pub(crate) fn record_generation_settings(
        &self,
        span: &tracing::Span,
        applied: GenerationOptions,
    ) {
        if let Some(built) = self.cached_llm_provider.get() {
            built.settings.with_overrides(applied).record(span);
        }
    }

    /// Get the session history as rich AgentMessages
    pub async fn get_agent_history(&self) -> SessionResult<Vec<AgentMessage>> {
        self.provider
            .history_store
            .get_history(&self.session.public_id)
            .await
    }

    /// Get the effective session history (post-compaction, no snapshot-only messages)
    pub async fn get_effective_agent_history(&self) -> SessionResult<Vec<AgentMessage>> {
        let mut messages = self
            .provider
            .history_store
            .get_effective_history(&self.session.public_id)
            .await?;
        let repaired = querymt_agent_core::model::repair_unmatched_tool_calls(&mut messages);
        if repaired > 0 {
            log::warn!(
                "Repaired {} unmatched tool call(s) while loading session {} history",
                repaired,
                self.session.public_id
            );
        }
        Ok(messages)
    }

    /// Persist an AgentMessage to the store
    pub(crate) async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.provider
            .history_store
            .add_message(&self.session.public_id, message)
            .await
    }

    /// Execute a raw tool call without side effects
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<Vec<querymt::chat::ToolResultPart>, LLMError> {
        let provider = self.provider().await?;
        provider.call_tool(name, args).await
    }

    /// Get pricing information for this session's model.
    ///
    /// Returns `None` for non-billable OAuth providers or OAuth sessions
    /// (no per-token cost), or when pricing information is unavailable for the model.
    ///
    /// OAuth is detected by checking whether the cached provider has a key
    /// resolver attached — OAuth providers always do, static-key providers
    /// never do. This avoids threading a boolean through the provider stack.
    pub fn get_pricing(&self) -> Option<ModelPricing> {
        self.billing_basis().pricing
    }

    /// Billing class and pricing snapshot that apply to this session's
    /// provider/model right now. Subscription/OAuth-backed providers are
    /// `Excluded` and never carry catalog pricing; a model with no catalog
    /// entry is `Unknown`.
    pub fn billing_basis(&self) -> BillingBasis {
        self.billing_basis_inner(None)
    }

    /// Like [`Self::billing_basis`], but priced for `model` served by this
    /// session's provider (for example a dedicated compaction model).
    pub fn billing_basis_for_model(&self, model: &str) -> BillingBasis {
        self.billing_basis_inner(Some(model))
    }

    fn billing_basis_inner(&self, model: Option<&str>) -> BillingBasis {
        let oauth_backed = self
            .cached_llm_provider
            .get()
            .is_some_and(|provider| provider.provider.key_resolver().is_some());
        match self.llm_config.as_ref() {
            Some(config) => resolve_billing_basis(
                &config.provider,
                model.unwrap_or(&config.model),
                oauth_backed,
            ),
            None => BillingBasis {
                billing: if oauth_backed {
                    BillingClass::Excluded
                } else {
                    BillingClass::Unknown
                },
                pricing: None,
            },
        }
    }
}

/// Preferred authentication method for a provider.
///
/// Stored in the system keyring under `auth_method_{provider}` and used by
/// the credential resolution logic to determine the order in which OAuth,
/// stored API key, and environment variable sources are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[typeshare::typeshare]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[serde(rename = "oauth")]
    OAuth,
    ApiKey,
    EnvVar,
}

impl std::fmt::Display for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMethod::OAuth => write!(f, "oauth"),
            AuthMethod::ApiKey => write!(f, "api_key"),
            AuthMethod::EnvVar => write!(f, "env_var"),
        }
    }
}

impl std::str::FromStr for AuthMethod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "oauth" => Ok(AuthMethod::OAuth),
            "api_key" => Ok(AuthMethod::ApiKey),
            "env_var" => Ok(AuthMethod::EnvVar),
            other => Err(format!("unknown auth method: {}", other)),
        }
    }
}

/// Read the preferred auth method for a provider from SecretStore.
pub(crate) async fn preferred_auth_method(
    provider_name: &str,
) -> SessionResult<Option<AuthMethod>> {
    let key = format!("auth_method_{provider_name}");
    let store = querymt_utils::secret_store::SecretStore::new_async()
        .await
        .map_err(|error| {
            SessionError::InvalidOperation(format!("Failed to open secret store: {error}"))
        })?;
    let value = store
        .run(move |store| store.try_get(&key))
        .await
        .map_err(|error| {
            SessionError::InvalidOperation(format!("Failed to read auth preference: {error}"))
        })?;
    Ok(value.and_then(|value| value.parse::<AuthMethod>().ok()))
}

/// Resolve an API key for a provider, respecting the user's preferred auth method.
///
/// The three credential sources are:
/// - **OAuth**: token from the system keyring (requires `oauth` feature)
/// - **Stored key**: API key set through ACP auth controls (stored in SecretStore under `env_var_name`)
/// - **Env var**: environment variable (e.g. `OPENAI_API_KEY`)
///
/// `env_var_name` may be `None` for OAuth-only providers (e.g. Codex). In that case
/// only the OAuth path is attempted; stored-key and env-var are skipped.
///
/// An absent or unreadable preference uses the default order: OAuth, stored key, env var.
/// Otherwise the preferred source is tried first, then the remaining sources in order.
/// Preference and stored-key errors are returned only if no credential source succeeds.
pub(crate) async fn resolve_api_key_with_preference(
    provider_name: &str,
    env_var_name: Option<&str>,
    preferred: SessionResult<Option<AuthMethod>>,
    use_oauth_resolver: &mut bool,
) -> SessionResult<Option<String>> {
    resolve_credential_chain(preferred, use_oauth_resolver, |method| async move {
        match method {
            AuthMethod::OAuth => {
                #[cfg(feature = "oauth")]
                {
                    use crate::auth::get_or_refresh_token;
                    match get_or_refresh_token(provider_name).await {
                        Ok(token) => {
                            log::debug!("Using OAuth token for provider: {}", provider_name);
                            return Ok(Some(token));
                        }
                        Err(error) => {
                            // Tokens are stored but unusable: the user is logged in
                            // and the login is broken. Falling back to another
                            // credential would hide that (and may bill a different
                            // account), so fail with the real reason. The failure
                            // is already logged once, at warn, by querymt-utils.
                            if let Some(refresh) =
                                querymt_utils::oauth::token_refresh_error(&error)
                            {
                                log::debug!(
                                    "OAuth refresh failed for {}: {}",
                                    provider_name,
                                    refresh
                                );
                                return Err(SessionError::CredentialRefresh {
                                    message: refresh.to_string(),
                                    relogin_required: refresh.relogin_required(),
                                });
                            }
                            log::debug!("OAuth unavailable for {}: {}", provider_name, error);
                        }
                    }
                }
            }
            AuthMethod::ApiKey => {
                if let Some(name) = env_var_name {
                    let store =
                        querymt_utils::secret_store::SecretStore::new_async().await.map_err(|error| {
                            SessionError::InvalidOperation(format!(
                                "Failed to open secret store: {error}"
                            ))
                        })?;
                    let stored_name = name.to_owned();
                    if let Some(key) = store.run(move |store| store.try_get(&stored_name)).await.map_err(|error| {
                        SessionError::InvalidOperation(format!(
                            "Failed to read stored API key for provider '{provider_name}': {error}"
                        ))
                    })? {
                        log::debug!(
                            "Using stored API key for provider: {} ({})",
                            provider_name,
                            name
                        );
                        return Ok(Some(key));
                    }
                    log::debug!("No stored API key for {} ({})", provider_name, name);
                }
            }
            AuthMethod::EnvVar => {
                if let Some(name) = env_var_name {
                    if let Ok(key) = std::env::var(name) {
                        log::debug!("Using env var for provider: {} ({})", provider_name, name);
                        return Ok(Some(key));
                    }
                    log::debug!("Env var {} not set", name);
                }
            }
        }
        Ok(None)
    })
    .await
}

/// Which credential a provider build used, as recorded in `auth.source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthSource {
    OAuth,
    ApiKey,
    EnvVar,
    /// An explicit `api_key_override` on the request.
    Override,
    /// An inline `api_key` in the provider config.
    Config,
    /// Stored OAuth tokens exist but could not be refreshed.
    OAuthRefreshFailed,
}

impl AuthSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OAuth => "oauth",
            Self::ApiKey => "api_key",
            Self::EnvVar => "env_var",
            Self::Override => "override",
            Self::Config => "config",
            Self::OAuthRefreshFailed => "oauth_refresh_failed",
        }
    }
}

impl From<AuthMethod> for AuthSource {
    fn from(method: AuthMethod) -> Self {
        match method {
            AuthMethod::OAuth => Self::OAuth,
            AuthMethod::ApiKey => Self::ApiKey,
            AuthMethod::EnvVar => Self::EnvVar,
        }
    }
}

/// Record which credential source a provider build used on the current span.
///
/// Spans that want the attribute declare an empty `auth.source` field (tracing
/// field names must be literals, so each declaration repeats the name):
/// `agent.provider.build`, `remote.provider_host.build_provider_for_request`
/// and `remote.provider_host.contract_info`. Elsewhere (for example catalog
/// listing, which has no provider-build span) this is a no-op. Only the source
/// kind is recorded, never the credential.
pub(crate) fn record_auth_source(source: AuthSource) {
    tracing::Span::current().record("auth.source", source.as_str());
}

/// The replay identity of `llm`.
///
/// When the provider states its provenance (`provenance`), that is the
/// identity, all four fields of it: provider, protocol, model and endpoint are
/// exactly what it stamps on its own output, whatever name the session
/// configuration gives the provider (a logical alias) or the model. Without a
/// stated identity, the session's provider and model are used and the
/// protocol and endpoint, which the store does not record, are taken from the
/// latest output of the same provider and model in `history` (see
/// [`SessionHandle::output_target`]).
fn complete_output_target(
    llm: &LLMConfig,
    provenance: Option<&querymt::chat::ChatOutputProvenance>,
    history: &[AgentMessage],
) -> querymt_agent_core::model::OutputTarget {
    use querymt_agent_core::model::{MessagePart, OutputTarget};
    if let Some(stated) = provenance {
        let mut target = OutputTarget::new(stated.provider.clone(), stated.model.clone())
            .with_protocol(stated.protocol.clone())
            .with_endpoint(stated.endpoint.clone());
        // `fallback_provider_model` lets stored output that carries no
        // provenance replay natively when only its provider and model match
        // the target. That is a guess about an origin nobody recorded. A
        // provider that states its full identity is the case where the guess
        // is not needed and not safe: output without provenance may come from
        // any endpoint, so it is replayed in portable form (no encrypted
        // reasoning or signatures), and only output stamped with exactly this
        // identity replays natively.
        target.fallback_provider_model = false;
        return target;
    }
    let mut target = OutputTarget::new(llm.provider.clone(), llm.model.clone());
    target.protocol.clone_from(&llm.protocol);
    target.endpoint.clone_from(&llm.endpoint);
    if target.endpoint.is_empty() {
        let own = history
            .iter()
            .rev()
            .flat_map(|message| message.parts.iter().rev())
            .find_map(|part| match part {
                MessagePart::Output { output } => output.provenance.as_ref().filter(|origin| {
                    !origin.endpoint.is_empty()
                        && origin.provider == llm.provider
                        && origin.model == llm.model
                }),
                _ => None,
            });
        if let Some(origin) = own {
            target.protocol.clone_from(&origin.protocol);
            target.endpoint.clone_from(&origin.endpoint);
        }
    }
    target
}

/// True when `error` means stored OAuth tokens could not be refreshed.
fn is_oauth_refresh_failure(error: &SessionError) -> bool {
    matches!(error, SessionError::CredentialRefresh { .. })
}

async fn resolve_credential_chain<Fut>(
    preferred: SessionResult<Option<AuthMethod>>,
    use_oauth_resolver: &mut bool,
    mut lookup: impl FnMut(AuthMethod) -> Fut,
) -> SessionResult<Option<String>>
where
    Fut: std::future::Future<Output = SessionResult<Option<String>>>,
{
    let (preferred, mut first_error) = match preferred {
        Ok(method) => (method, None),
        Err(error) => (None, Some(error)),
    };
    let order = match preferred {
        Some(AuthMethod::OAuth) | None => {
            vec![AuthMethod::OAuth, AuthMethod::ApiKey, AuthMethod::EnvVar]
        }
        Some(AuthMethod::ApiKey) => {
            vec![AuthMethod::ApiKey, AuthMethod::EnvVar, AuthMethod::OAuth]
        }
        Some(AuthMethod::EnvVar) => {
            vec![AuthMethod::EnvVar, AuthMethod::ApiKey, AuthMethod::OAuth]
        }
    };

    for method in order {
        let is_oauth = method == AuthMethod::OAuth;
        match lookup(method).await {
            Ok(Some(key)) => {
                if is_oauth {
                    *use_oauth_resolver = true;
                }
                record_auth_source(method.into());
                return Ok(Some(key));
            }
            Ok(None) => {}
            // A broken OAuth login must not be masked by another credential.
            Err(error) if is_oauth && is_oauth_refresh_failure(&error) => {
                record_auth_source(AuthSource::OAuthRefreshFailed);
                return Err(error);
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(None), Err)
}

/// Per-call parameters for building an LLM provider via
/// [`SessionProvider::build_provider`].
///
/// Infrastructure concerns (plugin registry, mesh handle, mesh-fallback policy)
/// live on [`SessionProvider`] and are accessed through `&self`. This struct
/// carries only the values that vary per request.
///
/// Use [`ProviderRequest::new`] for the common case — optional fields default
/// to sensible values (`params: None`, `api_key_override: None`,
/// `session_id: "standalone"`, `provider_node_id: None`).
pub struct ProviderRequest<'a> {
    /// Provider name (e.g. `"anthropic"`, `"openai"`).
    pub provider_name: &'a str,
    /// Model name (e.g. `"claude-sonnet-4-20250514"`, `"gpt-5"`).
    pub model: &'a str,
    /// Optional extra params JSON blob merged into the provider config.
    pub params: Option<&'a serde_json::Value>,
    /// Override the API key resolved from env/OAuth/keyring.
    pub api_key_override: Option<&'a str>,
    /// Session identifier for prompt-cache key substitution.
    /// Defaults to `"standalone"` for session-less contexts.
    pub session_id: &'a str,
    /// Route the call to a specific mesh node's `ProviderHostActor`.
    /// `None` (default) or `"local"` means local provider resolution.
    #[cfg(feature = "remote")]
    pub provider_node_id: Option<&'a str>,
    /// A model pass for the node `provider_node_id` names, sent with every
    /// request to it. The node must confirm that it charges the pass issuer.
    #[cfg(feature = "remote")]
    pub model_pass: Option<querymt_remote::ModelPass>,
}

impl<'a> ProviderRequest<'a> {
    /// Create a request with only the required fields; optional fields use defaults.
    pub fn new(provider_name: &'a str, model: &'a str) -> Self {
        Self {
            provider_name,
            model,
            params: None,
            api_key_override: None,
            session_id: "standalone",
            #[cfg(feature = "remote")]
            provider_node_id: None,
            #[cfg(feature = "remote")]
            model_pass: None,
        }
    }

    pub fn with_params(mut self, params: Option<&'a serde_json::Value>) -> Self {
        self.params = params;
        self
    }

    pub fn with_api_key_override(mut self, api_key_override: Option<&'a str>) -> Self {
        self.api_key_override = api_key_override;
        self
    }

    pub fn with_session_id(mut self, session_id: &'a str) -> Self {
        self.session_id = session_id;
        self
    }

    #[cfg(feature = "remote")]
    pub fn with_provider_node_id(mut self, provider_node_id: Option<&'a str>) -> Self {
        self.provider_node_id = provider_node_id;
        self
    }

    #[cfg(feature = "remote")]
    pub fn with_model_pass(mut self, pass: Option<querymt_remote::ModelPass>) -> Self {
        self.model_pass = pass;
        self
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use querymt::chat::{ChatCall, ChatOutput, ChatRole, FinishReason};
    use querymt::completion::{CompletionRequest, CompletionResponse};
    use querymt::error::LLMError;
    use querymt::plugin::LLMProviderFactory;
    use std::collections::HashMap;

    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    use crate::agent::core::AgentMode;
    use crate::agent::session_control::{SessionControlState, SessionModelBinding};
    use crate::test_utils::{MockSessionStore, empty_plugin_registry, mock_session};

    #[test]
    fn output_target_is_completed_from_the_latest_output_of_the_same_model() {
        use querymt::chat::ChatOutputProvenance;
        use querymt_agent_core::model::MessagePart;
        let llm = LLMConfig {
            id: 1,
            name: None,
            provider: "p".into(),
            model: "m".into(),
            protocol: String::new(),
            endpoint: String::new(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let reply = |provider: &str, model: &str, endpoint: &str| {
            let mut message = AgentMessage::new("s".into(), ChatRole::Assistant);
            message.parts.push(MessagePart::Output {
                output: ChatOutput {
                    provenance: Some(ChatOutputProvenance {
                        provider: provider.into(),
                        protocol: "responses".into(),
                        model: model.into(),
                        endpoint: endpoint.into(),
                    }),
                    ..ChatOutput::default()
                },
            });
            message
        };

        // The latest reply of another model, and of another provider, is not
        // the identity of this one.
        let history = vec![
            reply("p", "m", "https://a"),
            reply("p", "other", "https://b"),
            reply("q", "m", "https://c"),
        ];
        let target = complete_output_target(&llm, None, &history);
        assert_eq!(target.endpoint, "https://a");
        assert_eq!(target.protocol, "responses");

        // Without a reply of its own the identity stays incomplete, which
        // never authorizes native replay.
        assert_eq!(complete_output_target(&llm, None, &[]).endpoint, "");
        assert_eq!(
            complete_output_target(&llm, None, &history[1..]).endpoint,
            "",
            "replies of other models do not complete it"
        );

        // What the provider states is the identity; the stored replies, which
        // may come from another endpoint, do not complete it.
        let stated = ChatOutputProvenance {
            provider: "p".into(),
            protocol: "chat".into(),
            model: "m".into(),
            endpoint: "https://now".into(),
        };
        let target = complete_output_target(&llm, Some(&stated), &history);
        assert_eq!(target.endpoint, "https://now");
        assert_eq!(target.protocol, "chat");
        assert_eq!(target.provider, "p");
        assert_eq!(target.model, "m");
        assert_eq!(
            complete_output_target(&llm, Some(&stated), &[]).endpoint,
            "https://now",
            "a stated identity needs no history"
        );
        assert!(
            !target.fallback_provider_model,
            "a stated identity never replays output of unknown origin natively"
        );
        assert!(
            complete_output_target(&llm, None, &history).fallback_provider_model,
            "without a stated identity the legacy comparison stays available"
        );

        // The session may name the provider by an alias; the stated identity,
        // which is what the output is stamped with, is used as it is.
        let aliased = LLMConfig {
            provider: "work-openai".into(),
            model: "alias-of-m".into(),
            ..llm
        };
        let target = complete_output_target(&aliased, Some(&stated), &[]);
        assert_eq!(target.provider, "p");
        assert_eq!(target.model, "m");

        // Output stamped with exactly that identity replays natively (its
        // reasoning signature is kept); the same output after the endpoint
        // moved, and output without provenance even from the same provider
        // and model, are portable.
        let replays = |provenance: Option<ChatOutputProvenance>| {
            use querymt::chat::{ChatOutputItem, ChatReasoningItem, ChatReasoningPart};
            let mut message = AgentMessage::new("s".into(), ChatRole::Assistant);
            message.source_provider = Some("p".into());
            message.source_model = Some("m".into());
            message.parts.push(MessagePart::Output {
                output: ChatOutput {
                    provenance,
                    items: vec![ChatOutputItem::Reasoning(ChatReasoningItem {
                        id: None,
                        summary: vec![ChatReasoningPart::text("visible")],
                        content: Vec::new(),
                        encrypted_content: None,
                        signature: Some("sig".into()),
                        status: None,
                        extensions: Default::default(),
                    })],
                    ..ChatOutput::default()
                },
            });
            let chat = message
                .to_chat_message_with_output_target(Some(&target), None)
                .unwrap();
            chat.output().and_then(|output| output.signature()) == Some("sig".to_string())
        };
        let at = |endpoint: &str| {
            Some(ChatOutputProvenance {
                endpoint: endpoint.into(),
                ..stated.clone()
            })
        };
        assert!(replays(at("https://now")));
        assert!(!replays(at("https://before")));
        assert!(!replays(None));
    }

    #[cfg(feature = "remote")]
    #[test]
    fn quota_context_limit_reads_only_the_llama_cpp_factory_config() {
        let config = r#"{"n_ctx":4096,"speculative":{}}"#;
        assert_eq!(llama_cpp_context_limit("llama_cpp", config), Some(4096));
        assert_eq!(llama_cpp_context_limit("fixture", config), None);
        assert_eq!(llama_cpp_context_limit("llama_cpp", "{}"), None);
    }

    #[cfg(feature = "remote")]
    #[test]
    fn remote_snapshot_preserves_host_resolved_caps_and_effort() {
        for caps in [
            OutputCaps {
                initial: 32_000,
                ceiling: 128_000,
            },
            OutputCaps {
                initial: 20_000,
                ceiling: 20_000,
            },
        ] {
            let host = ProviderSnapshot {
                provider: Arc::new(MockProvider::new()),
                settings: Default::default(),
                output_caps: Some(caps),
                reasoning_effort: Some(querymt::chat::ReasoningEffort::High),
                llama_cpp_n_ctx: None,
                system_tokens: 0,
            };
            assert_eq!(
                host.quota_inputs(None).initial_max_tokens,
                Some(caps.initial)
            );
            let contract = host.contract_info();
            let remote = ProviderSnapshot::from_remote_contract(host.provider.clone(), &contract);
            assert_eq!(remote.output_caps, Some(caps));
            assert_eq!(remote.reasoning_effort, host.reasoning_effort);
            assert_eq!(contract.capabilities, host.provider.capabilities());
        }
    }

    #[tokio::test]
    async fn handle_construction_uses_atomic_control_snapshot_during_model_transition() {
        let mut session = mock_session("transitioning-session");
        session.llm_config_id = Some(1);
        let config_b = LLMConfig {
            id: 2,
            name: None,
            provider: "remote-provider".to_string(),
            model: "model-b".to_string(),
            protocol: String::new(),
            endpoint: String::new(),
            params: None,
            created_at: None,
            updated_at: None,
            provider_node_id: None,
        };
        let binding_b = SessionModelBinding {
            model_id: "remote-provider/model-b".to_string(),
            provider: config_b.provider.clone(),
            model: config_b.model.clone(),
            llm_config_id: config_b.id,
            provider_node_id: Some("node-b".to_string()),
        };
        let control = SessionControlState {
            revision: 2,
            active_mode: AgentMode::Build,
            reasoning_effort: None,
            effective_model: binding_b.clone(),
            mode_models: HashMap::from([("build".to_string(), binding_b)]),
        };
        let mut store = MockSessionStore::new();
        store
            .expect_get_session_control()
            .withf(|id| id == "transitioning-session")
            .return_once(move |_| Ok(Some(control)))
            .times(1);
        store
            .expect_get_llm_config()
            .withf(|id| *id == 2)
            .return_once(move |_| Ok(Some(config_b)))
            .times(1);
        store.expect_get_session_provider_node_id().times(0);
        store
            .expect_get_session_execution_config()
            .returning(|_| Ok(None))
            .times(1);
        let (registry, _temp_dir) = empty_plugin_registry().expect("plugin registry");
        let provider = Arc::new(SessionProvider::new(
            Arc::new(registry),
            Arc::new(store),
            LLMParams::new().provider("mock").model("model-a"),
        ));

        let handle = SessionHandle::new(provider, session)
            .await
            .expect("construct handle");

        assert_eq!(handle.llm_config().unwrap().id, 2);
        assert_eq!(handle.provider_node_id.as_deref(), Some("node-b"));
    }

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

    // ChatOutput projection source for tests
    #[derive(Debug)]
    struct MockChatResponse {
        content: String,
    }

    impl std::fmt::Display for MockChatResponse {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.content)
        }
    }

    impl From<MockChatResponse> for ChatOutput {
        fn from(response: MockChatResponse) -> Self {
            ChatOutput::from_projections(
                None,
                Some(response.content),
                None,
                None,
                Some(FinishReason::Stop),
            )
        }
    }

    #[async_trait::async_trait]
    impl querymt::chat::ChatProvider for MockProvider {
        async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            Ok(MockChatResponse {
                content: self.response_text.clone(),
            }
            .into())
        }

        async fn chat_stream(
            &self,
            _call: &ChatCall<'_>,
        ) -> Result<querymt::chat::ChatStream, LLMError> {
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

    struct InvalidConfigFactory;

    impl LLMProviderFactory for InvalidConfigFactory {
        fn name(&self) -> &str {
            "invalid-config"
        }

        fn config_schema(&self) -> String {
            "{}".into()
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
            Err(LLMError::InvalidRequest("bad provider config".into()))
        }

        fn list_models<'a>(
            &'a self,
            _cfg: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Provider that only reports a prompt-cache declaration.
    struct DeclaringProvider(PromptCacheCapabilities);

    #[async_trait::async_trait]
    impl querymt::chat::ChatProvider for DeclaringProvider {
        async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
            Err(LLMError::NotImplemented("not used".into()))
        }

        fn capabilities(&self) -> querymt::chat::ChatCapabilities {
            querymt::chat::ChatCapabilities::NONE.with_prompt_cache(self.0)
        }
    }

    #[async_trait::async_trait]
    impl querymt::completion::CompletionProvider for DeclaringProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("not used".into()))
        }
    }

    #[async_trait::async_trait]
    impl querymt::embedding::EmbeddingProvider for DeclaringProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("not used".into()))
        }
    }

    impl LLMProvider for DeclaringProvider {}

    /// Factory for a provider with a fixed declaration; counts its builds.
    struct DeclaringFactory {
        name: &'static str,
        declaration: PromptCacheCapabilities,
        builds: std::sync::atomic::AtomicUsize,
    }

    impl DeclaringFactory {
        fn new(declaration: PromptCacheCapabilities) -> Self {
            Self {
                // A name no provider list has ever contained.
                name: "acme",
                declaration,
                builds: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn named(mut self, name: &'static str) -> Self {
            self.name = name;
            self
        }

        fn builds(&self) -> usize {
            self.builds.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl LLMProviderFactory for DeclaringFactory {
        fn name(&self) -> &str {
            self.name
        }

        fn config_schema(&self) -> String {
            "{}".into()
        }

        fn from_config(&self, _cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
            self.builds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(DeclaringProvider(self.declaration)))
        }

        fn list_models<'a>(
            &'a self,
            _cfg: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    async fn session_provider() -> SessionProvider {
        provider_with_profile(LLMParams::new()).await
    }

    fn hinted_sections(config: &str) -> Vec<bool> {
        let value: serde_json::Value = serde_json::from_str(config).unwrap();
        let sections: Vec<querymt::params::SystemPrompt> =
            serde_json::from_value(value["system"].clone()).unwrap();
        sections.iter().map(|s| s.cache().is_some()).collect()
    }

    #[tokio::test]
    async fn declaring_provider_gets_hints_on_first_and_last_section_only() {
        let factory =
            DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS.with_max_ttl_secs(3600));
        let config = r#"{"model":"m","system":["one","two","three","four"]}"#.to_string();

        let hinted = session_provider()
            .await
            .with_default_cache_hints(&factory, config, 0)
            .unwrap();

        assert_eq!(hinted_sections(&hinted.config), [true, false, false, true]);
        // The probe was built without hints, so a second build carries them.
        assert!(hinted.provider.is_none());
        assert_eq!(factory.builds(), 1);
    }

    #[tokio::test]
    async fn single_section_gets_one_hint() {
        let factory = DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS);
        let config = r#"{"model":"m","system":"only"}"#.to_string();

        let hinted = session_provider()
            .await
            .with_default_cache_hints(&factory, config, 0)
            .unwrap();

        assert_eq!(hinted_sections(&hinted.config), [true]);
    }

    #[tokio::test]
    async fn provider_without_a_declaration_is_untouched_and_built_once() {
        // A plugin built before the declaration existed, or a local-model
        // provider: capabilities carry no `prompt_cache`.
        let factory = DeclaringFactory::new(PromptCacheCapabilities::NONE);
        let config = r#"{"model":"m","system":["one","two","three"]}"#.to_string();

        let hinted = session_provider()
            .await
            .with_default_cache_hints(&factory, config.clone(), 3600)
            .unwrap();

        assert_eq!(hinted.config, config);
        // The probe is handed back for reuse: no second build.
        assert!(hinted.provider.is_some());
        assert_eq!(factory.builds(), 1);
    }

    #[tokio::test]
    async fn configuration_without_a_system_prompt_is_not_probed() {
        let factory = DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS);
        let config = r#"{"model":"m"}"#.to_string();

        let hinted = session_provider()
            .await
            .with_default_cache_hints(&factory, config.clone(), 0)
            .unwrap();

        assert_eq!(hinted.config, config);
        assert!(hinted.provider.is_none());
        assert_eq!(factory.builds(), 0);
    }

    #[tokio::test]
    async fn configured_lifetime_is_requested_up_to_the_declared_maximum() {
        let factory =
            DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS.with_max_ttl_secs(1800));
        let config = r#"{"model":"m","system":["one","two"]}"#.to_string();

        let hinted = session_provider()
            .await
            .with_default_cache_hints(&factory, config, 3600)
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&hinted.config).unwrap();
        for index in 0..2 {
            assert_eq!(
                value["system"][index]["cache"],
                serde_json::json!({"ephemeral": {"ttl_seconds": 1800}})
            );
        }
    }

    #[tokio::test]
    async fn known_configuration_skips_the_probe() {
        let factory = DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS);
        let provider = session_provider().await;
        let config = r#"{"model":"m","system":["one","two"]}"#.to_string();

        let first = provider
            .with_default_cache_hints(&factory, config.clone(), 0)
            .unwrap();
        let second = provider
            .with_default_cache_hints(&factory, config, 0)
            .unwrap();

        // Only the first call probed; both yield the same hinted configuration.
        assert_eq!(factory.builds(), 1);
        assert_eq!(first.config, second.config);
        assert_eq!(hinted_sections(&second.config), [true, true]);
    }

    #[tokio::test]
    async fn providers_with_the_same_pruned_configuration_keep_their_own_declaration() {
        // Pruning leaves only the keys a schema lists, so two providers can
        // read the same text. The declaration of the first must not decide
        // what the second gets.
        let declaring = DeclaringFactory::new(PromptCacheCapabilities::BREAKPOINTS).named("alpha");
        let silent = DeclaringFactory::new(PromptCacheCapabilities::NONE).named("beta");
        let provider = session_provider().await;
        let config = r#"{"model":"m","system":["one","two"]}"#.to_string();

        let first = provider
            .with_default_cache_hints(&declaring, config.clone(), 0)
            .unwrap();
        let second = provider
            .with_default_cache_hints(&silent, config.clone(), 0)
            .unwrap();

        assert_eq!(hinted_sections(&first.config), [true, true]);
        assert_eq!(
            second.config, config,
            "no hints for a provider declaring none"
        );
        // The second provider was probed itself, not answered from the cache.
        assert_eq!(silent.builds(), 1);
    }

    #[tokio::test]
    async fn build_provider_preserves_typed_factory_error() {
        use crate::session::sqlite_storage::SqliteStorage;

        let registry = Arc::new(PluginRegistry::empty());
        registry.register_static(Arc::new(InvalidConfigFactory));
        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("connect test storage"),
        );
        let provider = SessionProvider::new(registry, storage, LLMParams::new());

        let error = match provider
            .build_provider(ProviderRequest::new("invalid-config", "test-model"))
            .await
        {
            Ok(_) => panic!("factory construction should fail"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            SessionError::ProviderError(LLMError::InvalidRequest(message))
                if message == "bad provider config"
        ));
    }

    async fn provider_with_profile(profile: LLMParams) -> SessionProvider {
        use crate::session::sqlite_storage::SqliteStorage;

        let storage = Arc::new(
            SqliteStorage::connect(":memory:".into())
                .await
                .expect("connect test storage"),
        );
        SessionProvider::new(Arc::new(PluginRegistry::empty()), storage, profile)
    }

    fn llama_profile() -> LLMParams {
        LLMParams::new()
            .provider("llama_cpp")
            .model("M")
            .max_tokens(16_384)
            .parameter("n_ctx", 32_768)
            .parameter("enable_thinking", false)
    }

    #[tokio::test]
    async fn selection_params_keeps_profile_parameters_for_profile_model() {
        let provider = provider_with_profile(llama_profile()).await;

        let params = provider.selection_params("llama_cpp", "M");

        assert_eq!(params, llama_profile());
        assert_eq!(params.max_tokens, Some(16_384));
        let custom = params.custom.expect("custom parameters are carried");
        assert_eq!(custom.get("n_ctx"), Some(&serde_json::json!(32_768)));
        assert_eq!(
            custom.get("enable_thinking"),
            Some(&serde_json::json!(false))
        );
    }

    #[tokio::test]
    async fn selection_params_drops_profile_parameters_for_other_model() {
        let provider = provider_with_profile(llama_profile()).await;

        let params = provider.selection_params("llama_cpp", "N");

        assert_eq!(params, LLMParams::new().provider("llama_cpp").model("N"));
        assert!(
            params.custom.is_none(),
            "n_ctx must not leak to other models"
        );
    }

    #[tokio::test]
    async fn selection_params_drops_profile_parameters_for_other_provider() {
        let provider = provider_with_profile(llama_profile()).await;

        let params = provider.selection_params("openai", "M");

        assert_eq!(params, LLMParams::new().provider("openai").model("M"));
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn explicit_owner_node_uses_local_factory_not_mesh() {
        use crate::session::sqlite_storage::SqliteStorage;
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let registry = Arc::new(PluginRegistry::empty());
        registry.register_static(Arc::new(InvalidConfigFactory));
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let provider =
            SessionProvider::new(registry, storage, LLMParams::new()).with_mesh(Some(mesh.clone()));
        let local_node = mesh.peer_id().to_string();
        let result = provider
            .build_provider(
                ProviderRequest::new("invalid-config", "test-model")
                    .with_provider_node_id(Some(&local_node)),
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::ProviderError(LLMError::InvalidRequest(_)))
        ));
        let other_node = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
            .to_string();
        let remote_error = provider
            .build_provider(
                ProviderRequest::new("invalid-config", "test-model")
                    .with_provider_node_id(Some(&other_node)),
            )
            .await
            .err()
            .expect("an unavailable remote host fails during negotiation");
        assert!(
            remote_error.to_string().contains("provider_host::"),
            "a different node must use mesh negotiation, not the local factory: {remote_error}"
        );
    }

    // ── resolve_api_key_with_preference tests ─────────────────────────────────

    #[tokio::test]
    async fn resolve_env_fallback_or_first_storage_error() {
        for (pref_error, store_error, env_key, expected) in [
            (Some("preference"), None, Some("env"), Ok(Some("env"))),
            (None, Some("store init"), Some("env"), Ok(Some("env"))),
            (None, Some("store read"), Some("env"), Ok(Some("env"))),
            (
                Some("preference"),
                Some("store read"),
                None,
                Err("preference"),
            ),
            (Some("preference"), None, None, Err("preference")),
            (None, Some("store read"), None, Err("store read")),
            (None, None, None, Ok(None)),
        ] {
            let preferred = pref_error.map_or(Ok(None), |message| {
                Err(SessionError::InvalidOperation(message.into()))
            });
            let mut use_oauth = false;
            let result = resolve_credential_chain(preferred, &mut use_oauth, |method| {
                std::future::ready(match method {
                    AuthMethod::ApiKey => store_error.map_or(Ok(None), |message| {
                        Err(SessionError::InvalidOperation(message.into()))
                    }),
                    AuthMethod::EnvVar => Ok(env_key.map(str::to_string)),
                    AuthMethod::OAuth => Ok(None),
                })
            })
            .await;
            match expected {
                Ok(key) => assert_eq!(result.unwrap().as_deref(), key),
                Err(message) => assert!(matches!(
                    result, Err(SessionError::InvalidOperation(actual)) if actual == message
                )),
            }
            assert!(!use_oauth);
        }
    }

    #[tokio::test]
    async fn resolve_preference_order_and_short_circuit() {
        use AuthMethod::{ApiKey, EnvVar, OAuth};
        for (preferred, winner, expected) in [
            (None, EnvVar, vec![OAuth, ApiKey, EnvVar]),
            (Some(ApiKey), EnvVar, vec![ApiKey, EnvVar]),
            (Some(ApiKey), OAuth, vec![ApiKey, EnvVar, OAuth]),
            (Some(EnvVar), ApiKey, vec![EnvVar, ApiKey]),
            (Some(OAuth), OAuth, vec![OAuth]),
        ] {
            let mut attempted = Vec::new();
            let mut use_oauth = false;
            let result = resolve_credential_chain(Ok(preferred), &mut use_oauth, |method| {
                let result = if method == winner {
                    Ok(Some("key".into()))
                } else if method == ApiKey {
                    Err(SessionError::InvalidOperation("stored key".into()))
                } else {
                    Ok(None)
                };
                attempted.push(method);
                std::future::ready(result)
            })
            .await
            .unwrap();
            assert_eq!(result.as_deref(), Some("key"));
            assert_eq!(attempted, expected);
            assert_eq!(use_oauth, winner == OAuth);
        }
    }

    #[tokio::test]
    async fn resolve_records_auth_source_on_current_span() {
        use AuthMethod::{ApiKey, EnvVar, OAuth};
        use querymt_agent_services::tools::trace_support::genai_trace::{attr, capture_info};

        // (winner, oauth refresh failed, expected auth.source)
        for (winner, refresh_failed, expected) in [
            (OAuth, false, "oauth"),
            (ApiKey, false, "api_key"),
            (EnvVar, false, "env_var"),
            (OAuth, true, "oauth_refresh_failed"),
        ] {
            let (_, spans) = capture_info(async {
                let span = tracing::info_span!(
                    "agent.provider.build",
                    auth.source = tracing::field::Empty
                );
                tracing::Instrument::instrument(
                    async {
                        let mut use_oauth = false;
                        let result = resolve_credential_chain(Ok(None), &mut use_oauth, |method| {
                            let result = if refresh_failed && method == OAuth {
                                Err(SessionError::CredentialRefresh {
                                    message: "login expired".into(),
                                    relogin_required: true,
                                })
                            } else if method == winner {
                                Ok(Some("secret-key".to_string()))
                            } else {
                                Ok(None)
                            };
                            std::future::ready(result)
                        })
                        .await;
                        assert_eq!(result.is_err(), refresh_failed);
                    },
                    span,
                )
                .await;
            })
            .await;
            let span = spans
                .iter()
                .find(|s| s.name == "agent.provider.build")
                .expect("span exported");
            assert_eq!(
                attr(span, "auth.source").map(|v| v.to_string()),
                Some(expected.to_string())
            );
            // Only the source kind is exported, never the credential.
            assert!(!format!("{spans:?}").contains("secret-key"));
        }
    }

    #[tokio::test]
    async fn resolve_oauth_refresh_failure_does_not_fall_back() {
        use AuthMethod::{ApiKey, EnvVar, OAuth};
        for preferred in [None, Some(OAuth)] {
            let mut attempted = Vec::new();
            let mut use_oauth = false;
            let result = resolve_credential_chain(Ok(preferred), &mut use_oauth, |method| {
                let result = match method {
                    OAuth => Err(SessionError::CredentialRefresh {
                        message: "login expired".into(),
                        relogin_required: true,
                    }),
                    ApiKey | EnvVar => Ok(Some("fallback".into())),
                };
                attempted.push(method);
                std::future::ready(result)
            })
            .await;
            assert!(matches!(
                result,
                Err(SessionError::CredentialRefresh {
                    relogin_required: true,
                    ..
                })
            ));
            assert_eq!(attempted, vec![OAuth]);
            assert!(!use_oauth);
        }
    }

    #[cfg(feature = "oauth")]
    #[tokio::test]
    async fn resolve_oauth_after_preference_error() {
        let mut use_oauth = false;
        let result = resolve_credential_chain(
            Err(SessionError::InvalidOperation("preference".into())),
            &mut use_oauth,
            |method| {
                assert_eq!(method, AuthMethod::OAuth);
                std::future::ready(Ok(Some("token".into())))
            },
        )
        .await
        .unwrap();
        assert_eq!(result.as_deref(), Some("token"));
        assert!(use_oauth);
    }

    #[test]
    fn static_pricing_omits_non_billable_oauth_providers() {
        assert!(SessionProvider::get_pricing("codex", "gpt-5").is_none());
        assert!(SessionProvider::get_pricing("kimi-code", "kimi-k2.5").is_none());
    }

    #[test]
    fn billing_basis_excludes_subscription_providers_without_catalog_pricing() {
        for provider in ["codex", "kimi-code"] {
            let basis = resolve_billing_basis(provider, "any-model", false);
            assert_eq!(basis.billing, BillingClass::Excluded);
            assert!(basis.pricing.is_none());
        }
        // An OAuth-resolved key on an otherwise metered provider is excluded too.
        let basis = resolve_billing_basis("openai", "gpt-5", true);
        assert_eq!(basis.billing, BillingClass::Excluded);
        assert!(basis.pricing.is_none());
    }

    #[test]
    fn helper_marks_non_billable_oauth_providers() {
        assert!(is_non_billable_oauth_provider("codex"));
        assert!(is_non_billable_oauth_provider("kimi-code"));
        assert!(!is_non_billable_oauth_provider("openai"));
    }
}
