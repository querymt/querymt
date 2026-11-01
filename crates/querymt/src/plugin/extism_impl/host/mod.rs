#[cfg(not(feature = "model-registry"))]
use crate::providers::ProvidersRegistry;
use crate::{
    HTTPLLMProvider, LLMProvider,
    adapters::LLMProviderFromHTTP,
    auth::ApiKeyResolver,
    chat::{
        ChatCall, ChatCapabilities, ChatMessage, ChatOutput, ChatProvider, StreamChunk, Tool,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{
        CompletionProvider, CompletionRequest, CompletionResponse, http::HTTPCompletionProvider,
    },
    embedding::{EmbeddingProvider, http::HTTPEmbeddingProvider},
    error::LLMError,
    plugin::{
        Fut, HTTPLLMProviderFactory, LLMProviderFactory,
        extism_impl::{
            ExtismChatChunk, ExtismChatChunkParseRequest, ExtismChatParseRequest,
            ExtismChatPrepareRequest, ExtismChatRequest, ExtismChatResponse,
            ExtismChatStreamParserRequest, ExtismCompleteParseRequest, ExtismEmbedParseRequest,
            ExtismEmbedRequest, ExtismListModelsParseRequest, ExtismListModelsRequest,
            ExtismPreparedChatRequest, ExtismSttRequest, ExtismSttResponse, ExtismTtsRequest,
            ExtismTtsResponse, ExtismVoiceConfig, ITEM_AWARE_CHAT_CONTRACT_VERSION,
            PREPARED_CHAT_CONTRACT_VERSION, SerializableHttpRequest, SerializableHttpResponse,
            messages_require_item_aware_contract,
        },
    },
    stt, tts,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use extism::{Manifest, Plugin, PluginBuilder, Wasm, convert::Json};
use futures::FutureExt;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
#[cfg(feature = "tracing")]
use tracing::instrument;
use url::Url;

mod loader;
pub use loader::ExtismLoader;

mod functions;

use super::{ExtismCompleteRequest, PluginError};

/// Decode a `(Error, i32)` pair from `call_get_error_code` into a typed [`LLMError`].
///
/// The error code identifies the [`LLMError`] variant, and the error string
/// is JSON-serialized [`PluginError`] payload.
fn decode_plugin_error(e: extism::Error, code: i32) -> LLMError {
    let raw = format!("{:#}", e);
    PluginError::decode(code, &raw)
}

fn parse_config_json(cfg: &str) -> Result<Value, LLMError> {
    serde_json::from_str(cfg).map_err(LLMError::from)
}

fn decode_classified_plugin_error(error: PluginError) -> LLMError {
    LLMError::from_payload(error.payload)
}

macro_rules! with_host_functions {
    ($builder:expr, $user_data:expr) => {
        $builder
            .with_wasi(true)
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_http_request",
                [extism::PTR],
                [extism::PTR],
                $user_data.clone(),
                functions::qmt_http_request,
            )
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_http_stream_open",
                [extism::PTR],
                [extism::PTR],
                $user_data.clone(),
                functions::qmt_http_stream_open,
            )
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_http_stream_next",
                [extism::PTR],
                [extism::PTR],
                $user_data.clone(),
                functions::qmt_http_stream_next,
            )
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_http_stream_close",
                [extism::PTR],
                [],
                $user_data.clone(),
                functions::qmt_http_stream_close,
            )
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_yield_chunk",
                [extism::PTR],
                [],
                $user_data.clone(),
                functions::qmt_yield_chunk,
            )
            .with_function_in_namespace(
                "extism:host/user",
                "qmt_log",
                [extism::PTR],
                [],
                $user_data.clone(),
                functions::qmt_log,
            )
    };
}

#[derive(Clone)]
pub struct ExtismFactory {
    plugin: Arc<Mutex<Plugin>>,
    name: String,
    user_data: Option<extism::UserData<functions::HostState>>,
    allowed_hosts: Vec<String>,
}

fn call_plugin_str(plugin: Arc<Mutex<Plugin>>, func: &str, arg: &Value) -> anyhow::Result<String> {
    let input = serde_json::to_string(arg)?;
    let input_bytes = input.into_bytes();

    let mut plug = plugin.lock().unwrap();
    let output_bytes: Vec<u8> = plug.call(func, &input_bytes)?;
    Ok(std::str::from_utf8(&output_bytes)?.to_string())
}

fn add_allowed_host(allowed_hosts: &mut Vec<String>, host: String) {
    if !host.is_empty() && !allowed_hosts.iter().any(|h| h == &host) {
        allowed_hosts.push(host);
    }
}

fn host_from_base_url(base_url: &str) -> Option<String> {
    Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

fn configured_base_url(config: &Option<HashMap<String, toml::Value>>) -> Option<String> {
    config
        .as_ref()
        .and_then(|runtime_cfg| runtime_cfg.get("base_url"))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

fn configured_allowed_hosts(
    config: &Option<HashMap<String, toml::Value>>,
) -> Result<Vec<String>, LLMError> {
    let Some(runtime_cfg) = config else {
        return Ok(Vec::new());
    };
    let Some(hosts) = runtime_cfg.get("allowed_hosts") else {
        return Ok(Vec::new());
    };

    hosts
        .clone()
        .try_into()
        .map_err(|e| LLMError::GenericError(format!("{:#}", e)))
}

fn build_allowed_hosts(
    config: &Option<HashMap<String, toml::Value>>,
    plugin_default_base_url: Option<&str>,
) -> Result<Vec<String>, LLMError> {
    let mut allowed_hosts = Vec::new();

    for host in configured_allowed_hosts(config)? {
        add_allowed_host(&mut allowed_hosts, host);
    }

    if let Some(base_url) = configured_base_url(config) {
        if let Some(host) = host_from_base_url(&base_url) {
            add_allowed_host(&mut allowed_hosts, host);
        } else {
            log::warn!(
                "Ignoring invalid configured base_url while deriving allowed hosts: {}",
                base_url
            );
        }
        return Ok(allowed_hosts);
    }

    if let Some(base_url) = plugin_default_base_url
        && let Some(host) = host_from_base_url(base_url)
    {
        add_allowed_host(&mut allowed_hosts, host);
    }

    Ok(allowed_hosts)
}

impl ExtismFactory {
    #[cfg_attr(
        feature = "tracing",
        instrument(name = "extism_factory.load", skip_all)
    )]
    pub fn load(
        wasm_content: Vec<u8>,
        config: &Option<HashMap<String, toml::Value>>,
        config_name: Option<&str>,
    ) -> Result<Self, LLMError> {
        let mut env_map: HashMap<_, _> = std::env::vars().collect();

        #[cfg(feature = "model-registry")]
        let providers_registry = crate::providers::providers_registry()?;
        #[cfg(not(feature = "model-registry"))]
        let providers_registry = std::sync::Arc::new(ProvidersRegistry {
            providers: HashMap::new(),
        });
        env_map.insert(
            "PROVIDERS_REGISTRY_DATA".to_string(),
            serde_json::to_string(&*providers_registry)?,
        );

        let initial_manifest =
            Manifest::new([Wasm::data(wasm_content.clone())]).with_config(env_map.iter());

        let tokio_handle = tokio::runtime::Handle::current();
        let init_user_data = extism::UserData::new(functions::HostState::new(
            "unknown".to_string(),
            Vec::<String>::new(),
            tokio_handle.clone(),
        ));
        let init_builder =
            with_host_functions!(PluginBuilder::new(initial_manifest), init_user_data);

        let init_plugin = Arc::new(Mutex::new(
            init_builder
                .build()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?,
        ));

        let plugin_name = call_plugin_str(init_plugin.clone(), "name", &Value::Null)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
        let name = if let Some(cfg_name) = config_name {
            if cfg_name != plugin_name {
                log::warn!(
                    "Plugin name mismatch: config name is '{}', but plugin reports '{}'. Using config name.",
                    cfg_name,
                    plugin_name
                );
            }
            cfg_name.to_string()
        } else {
            plugin_name
        };
        let plugin_default_base_url =
            call_plugin_str(init_plugin.clone(), "base_url", &Value::Null).ok();
        let allowed_hosts = build_allowed_hosts(config, plugin_default_base_url.as_deref())?;
        drop(init_plugin);

        log::debug!(
            "Extism provider '{}' allowed hosts: {}",
            name,
            allowed_hosts.join(", ")
        );

        let manifest = Manifest::new([Wasm::data(wasm_content.clone())])
            .with_allowed_hosts(allowed_hosts.clone().into_iter())
            .with_config(env_map.into_iter());

        let user_data = extism::UserData::new(functions::HostState::new(
            name.clone(),
            allowed_hosts.clone(),
            tokio_handle,
        ));
        let builder = with_host_functions!(PluginBuilder::new(manifest), user_data);

        let plugin = Arc::new(Mutex::new(
            builder
                .build()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?,
        ));

        {
            let mut plugin_guard = plugin.lock().unwrap();
            let max_level: usize = match log::max_level() {
                log::LevelFilter::Off => 0,
                log::LevelFilter::Error => 1,
                log::LevelFilter::Warn => 2,
                log::LevelFilter::Info => 3,
                log::LevelFilter::Debug => 4,
                log::LevelFilter::Trace => 5,
            };
            let _: () = plugin_guard
                .call("init_logging", Json(max_level))
                .map_err(|e| {
                    LLMError::PluginError(format!("failed to init plugin logging: {:#}", e))
                })?;
        }

        Ok(Self {
            plugin,
            name,
            user_data: Some(user_data),
            allowed_hosts,
        })
    }

    fn call(&self, func: &str, arg: &Value) -> anyhow::Result<String> {
        call_plugin_str(self.plugin.clone(), func, arg)
    }

    fn validate_runtime_base_url(&self, cfg: &Value) -> Result<(), LLMError> {
        validate_runtime_base_url(&self.name, &self.allowed_hosts, cfg)
    }

    fn validate_plugin_config(&self, cfg: &Value) -> Result<(), LLMError> {
        let mut plug = self.plugin.lock().unwrap();
        let _: Json<Value> = plug
            .call_get_error_code("from_config", Json(cfg.clone()))
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        Ok(())
    }
}

fn validate_runtime_base_url(
    provider_name: &str,
    allowed_hosts: &[String],
    cfg: &Value,
) -> Result<(), LLMError> {
    let Some(base_url) = cfg.get("base_url").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(host) = host_from_base_url(base_url) else {
        return Err(LLMError::InvalidRequest(format!(
            "Invalid base_url for provider '{}': {}",
            provider_name, base_url
        )));
    };
    if allowed_hosts.is_empty() || allowed_hosts.iter().any(|h| h == &host) {
        return Ok(());
    }

    Err(LLMError::InvalidRequest(format!(
        "Provider '{}' base_url host '{}' is not allowed by the loaded plugin manifest. Add it to [providers.config].allowed_hosts or set [providers.config].base_url before building.",
        provider_name, host
    )))
}

impl ExtismFactory {
    fn supports_http_adapter_abi(&self) -> bool {
        let plug = self.plugin.lock().unwrap();
        plug.function_exists("prepare_chat_request")
            && plug.function_exists("parse_chat_response")
            && plug.function_exists("list_models_request")
            && plug.function_exists("parse_list_models_response")
    }
}

impl LLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn config_schema(&self) -> String {
        self.call("config_schema", &Value::Null)
            .expect("config_schema() must return valid JSON string")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg_value = parse_config_json(cfg)?;
        self.validate_runtime_base_url(&cfg_value)?;

        self.validate_plugin_config(&cfg_value)?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg_value,
            user_data: self.user_data.clone(),
            key_resolver: None,
        };

        if self.supports_http_adapter_abi() {
            let http_provider: Box<dyn HTTPLLMProvider> = Box::new(provider);
            return Ok(Box::new(LLMProviderFromHTTP::new(http_provider)));
        }

        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        // list_models can do host HTTP calls, so run the Extism VM call off the Tokio runtime
        // thread to avoid deadlocks on current-thread runtimes.
        let cfg_value = match parse_config_json(cfg) {
            Ok(value) => value,
            Err(error) => return Box::pin(async move { Err(error) }),
        };

        if self.supports_http_adapter_abi() {
            enum ModelList {
                Static(Vec<String>),
                Request(Box<http::Request<Vec<u8>>>),
            }

            let factory = self.clone();
            let cfg = cfg.to_owned();
            let caller_span = tracing::Span::current();
            return async move {
                // Even short WASM calls can invoke synchronous WASI, which blocks on Tokio.
                let prepared = tokio::task::spawn_blocking(move || {
                    let _guard = caller_span.enter();
                    if let Some(result) = factory.list_models_static(&cfg) {
                        return result.map(ModelList::Static);
                    }
                    factory
                        .list_models_request(&cfg)
                        .map(Box::new)
                        .map(ModelList::Request)
                })
                .await
                .map_err(|e| {
                    LLMError::PluginError(format!(
                        "Extism list_models preparation join error: {e:#}"
                    ))
                })??;

                let req = match prepared {
                    ModelList::Static(models) => return Ok(models),
                    ModelList::Request(req) => *req,
                };
                let resp = crate::outbound::call_outbound(req).await?;
                let factory = self.clone();
                let caller_span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    let _guard = caller_span.enter();
                    factory.parse_list_models(resp)
                })
                .await
                .map_err(|e| {
                    LLMError::PluginError(format!("Extism list_models parsing join error: {e:#}"))
                })?
            }
            .boxed();
        }

        let plugin = self.plugin.clone();
        let user_data = self.user_data.clone();
        let caller_span = tracing::Span::current();
        async move {
            tokio::task::spawn_blocking(move || {
                let _guard = caller_span.enter();
                let mut plug = plugin.lock().unwrap();

                // Reset any stale cancellation state left over from a previously dropped future
                // (e.g. a cancelled chat/stream call). Without this reset, the cancel_watch_rx is
                // still `true`, causing qmt_http_request to return HTTP 499 immediately and the
                // plugin to surface {"kind":"Cancelled"} on every subsequent list_models call.
                if let Some(ud) = &user_data
                    && let Ok(state) = ud.get()
                {
                    let mut state_guard = state.lock().unwrap();
                    state_guard.cancel_state = functions::CancelState::NotCancelled;
                    let _ = state_guard.cancel_watch_tx.send(false);
                }

                let out: Json<Vec<String>> = plug
                    .call_get_error_code("list_models", Json(cfg_value))
                    .map_err(|(e, code)| decode_plugin_error(e, code))?;
                Ok::<_, LLMError>(out.0)
            })
            .await
            .map_err(|e| LLMError::PluginError(format!("Extism list_models join error: {:#}", e)))?
        }
        .boxed()
    }

    fn as_http(&self) -> Option<&dyn crate::plugin::http::HTTPLLMProviderFactory> {
        // Only return Some if the plugin is HTTP-based
        // Check if plugin exports the api_key_name function (exported by impl_extism_http_plugin!)
        let is_http_based = self.supports_http_adapter_abi();

        if is_http_based {
            log::debug!(
                "Extism plugin '{}' detected as HTTP-based provider",
                self.name
            );
            Some(self)
        } else {
            log::debug!(
                "Extism plugin '{}' detected as non-HTTP provider",
                self.name
            );
            None
        }
    }
}

impl HTTPLLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        (self as &dyn LLMProviderFactory).name()
    }

    fn config_schema(&self) -> String {
        (self as &dyn LLMProviderFactory).config_schema()
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
        let cfg_value = parse_config_json(cfg)?;
        self.validate_runtime_base_url(&cfg_value)?;

        self.validate_plugin_config(&cfg_value)?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg_value,
            user_data: self.user_data.clone(),
            key_resolver: None,
        };
        Ok(Box::new(provider))
    }

    fn list_models_static(&self, cfg: &str) -> Option<Result<Vec<String>, LLMError>> {
        let cfg_value = match parse_config_json(cfg) {
            Ok(value) => value,
            Err(error) => return Some(Err(error)),
        };

        let mut plug = self.plugin.lock().unwrap();
        if !plug.function_exists("list_models_static") {
            return None;
        }

        let out: Result<Json<Option<Vec<String>>>, (extism::Error, i32)> = plug
            .call_get_error_code(
                "list_models_static",
                Json(ExtismListModelsRequest { cfg: cfg_value }),
            );

        match out {
            Ok(Json(Some(models))) => Some(Ok(models)),
            Ok(Json(None)) => None,
            Err((e, code)) => Some(Err(decode_plugin_error(e, code))),
        }
    }

    fn list_models_request(&self, cfg: &str) -> Result<http::Request<Vec<u8>>, LLMError> {
        let cfg_value = parse_config_json(cfg)?;
        self.validate_runtime_base_url(&cfg_value)?;

        let mut plug = self.plugin.lock().unwrap();
        let req: Json<SerializableHttpRequest> = plug
            .call_get_error_code(
                "list_models_request",
                Json(ExtismListModelsRequest { cfg: cfg_value }),
            )
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        Ok(req.0.req)
    }

    fn parse_list_models(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        let mut plug = self.plugin.lock().unwrap();
        let out: Json<Vec<String>> = plug
            .call_get_error_code(
                "parse_list_models_response",
                Json(ExtismListModelsParseRequest {
                    resp: SerializableHttpResponse { resp },
                }),
            )
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        Ok(out.0)
    }

    fn api_key_name(&self) -> Option<String> {
        self.call("api_key_name", &Value::Null) // → Result<&'static str, _>
            .ok() // → Option<&'static str>
            .filter(|s| !s.is_empty())
    }
}

pub struct ExtismProvider {
    plugin: Arc<Mutex<Plugin>>,
    config: Value,
    user_data: Option<extism::UserData<functions::HostState>>,
    key_resolver: Option<Arc<dyn ApiKeyResolver>>,
}

impl ExtismProvider {
    fn item_aware_contract_version(&self) -> Result<Option<u32>, LLMError> {
        self.call_short_blocking(
            "item_aware_chat_contract_version",
            Self::probe_item_aware_contract_version,
        )
    }

    /// Probe the advertised item-aware contract version through an
    /// already-locked plugin.
    fn probe_item_aware_contract_version(plug: &mut Plugin) -> Result<Option<u32>, LLMError> {
        if !plug.function_exists("item_aware_chat_contract_version") {
            return Ok(None);
        }
        let version: Json<u32> = plug
            .call_get_error_code("item_aware_chat_contract_version", ())
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        Ok(Some(version.0))
    }

    fn validate_item_aware_messages(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Option<u32>, LLMError> {
        if !messages_require_item_aware_contract(messages) {
            return Ok(None);
        }
        Self::ensure_item_aware_contract_version(self.item_aware_contract_version()?)
    }

    fn ensure_item_aware_contract_version(version: Option<u32>) -> Result<Option<u32>, LLMError> {
        if version != Some(ITEM_AWARE_CHAT_CONTRACT_VERSION) {
            return Err(LLMError::InvalidRequest(format!(
                "Extism plugin does not support item-aware chat contract version {}; advertised version: {}",
                ITEM_AWARE_CHAT_CONTRACT_VERSION,
                version.map_or_else(|| "none".to_string(), |value| value.to_string())
            )));
        }
        Ok(version)
    }

    fn user_data_required(&self) -> Result<extism::UserData<functions::HostState>, LLMError> {
        self.user_data
            .clone()
            .ok_or_else(|| LLMError::PluginError("No UserData found for Extism provider".into()))
    }

    fn effective_config(&self) -> Result<Value, LLMError> {
        let mut cfg = self.config.clone();
        if let Some(ref resolver) = self.key_resolver
            && let Some(obj) = cfg.as_object_mut()
        {
            obj.insert(
                "api_key".to_string(),
                serde_json::Value::String(resolver.current()),
            );
        }
        Ok(cfg)
    }

    /// Asks the plugin what it supports for chat.
    ///
    /// A plugin without a `capabilities` export falls back to streaming being
    /// available when all stream-parser exports exist, and no per-request
    /// options, so options are never sent to plugins that cannot honour them.
    fn plugin_capabilities(&self) -> ChatCapabilities {
        let Ok(mut plug) = self.plugin.lock() else {
            return ChatCapabilities::NONE;
        };
        if plug.function_exists("capabilities") {
            return plug
                .call::<_, Json<ChatCapabilities>>("capabilities", Json(self.config.clone()))
                .map(|Json(caps)| caps)
                .unwrap_or(ChatCapabilities::NONE);
        }
        ChatCapabilities::NONE.with_streaming(
            ["start", "parse", "finish", "close"]
                .iter()
                .all(|op| plug.function_exists(format!("chat_stream_parser_{op}"))),
        )
    }

    fn call_short_blocking<T, F>(&self, op: &'static str, f: F) -> Result<T, LLMError>
    where
        F: FnOnce(&mut Plugin) -> Result<T, LLMError>,
    {
        let mut plug = self
            .plugin
            .lock()
            .map_err(|_| LLMError::PluginError(format!("Extism {op} mutex poisoned")))?;
        f(&mut plug)
    }

    async fn call_blocking_with_cancel<T, F>(&self, op: &'static str, f: F) -> Result<T, LLMError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Plugin) -> Result<T, LLMError> + Send + 'static,
    {
        let plugin = self.plugin.clone();
        let user_data = self.user_data_required()?;
        let started = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));

        struct CancelGuard {
            user_data: extism::UserData<functions::HostState>,
            started: Arc<AtomicBool>,
            cancelled: Arc<AtomicBool>,
            armed: bool,
        }

        impl Drop for CancelGuard {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }

                // Always mark the call as cancelled so that if the blocking task is still waiting
                // for the plugin mutex, it can bail out once it acquires it.
                self.cancelled.store(true, Ordering::SeqCst);

                // Only send a cancellation signal to the host if this future actually acquired the
                // plugin mutex (otherwise we might cancel an unrelated in-flight call).
                if !self.started.load(Ordering::SeqCst) {
                    return;
                }

                if let Ok(state) = self.user_data.get() {
                    let mut state_guard = state.lock().unwrap();
                    state_guard.cancel_state = functions::CancelState::CancelledByConsumerDrop;
                    let _ = state_guard.cancel_watch_tx.send(true);
                }
            }
        }

        let mut guard = CancelGuard {
            user_data: user_data.clone(),
            started: started.clone(),
            cancelled: cancelled.clone(),
            armed: true,
        };

        let user_data_for_call = user_data.clone();
        let started_for_call = started.clone();
        let cancelled_for_call = cancelled.clone();

        // Capture the current tracing span so host function callbacks
        // (e.g. `host_reqwest_http`) inside `spawn_blocking` are parented
        // under `http_adapter.chat_stream` rather than
        // appearing as orphaned root spans.
        let caller_span = tracing::Span::current();

        let join = tokio::task::spawn_blocking(move || {
            let _guard = caller_span.enter();

            let mut plug = plugin.lock().unwrap();

            // Reset cancellation for this call (must happen only after acquiring the plugin mutex
            // so we don't race/cancel an unrelated in-flight call).
            if let Ok(state) = user_data_for_call.get() {
                let mut state_guard = state.lock().unwrap();
                state_guard.cancel_state = functions::CancelState::NotCancelled;
                let _ = state_guard.cancel_watch_tx.send(false);
            }

            started_for_call.store(true, Ordering::SeqCst);

            if cancelled_for_call.load(Ordering::SeqCst) {
                return Err(LLMError::Cancelled);
            }

            f(&mut plug)
        })
        .await;

        // The plugin call finished (success or error); don't emit cancellation on drop.
        guard.armed = false;

        join.map_err(|e| LLMError::PluginError(format!("Extism {op} join error: {:#}", e)))?
    }
}

/// Chat on a bare [`ExtismProvider`] is unsupported: plugins chat through the
/// prepared-chat contract, driven by [`LLMProviderFromHTTP`].
#[async_trait]
impl ChatProvider for ExtismProvider {
    async fn chat(&self, _call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        Err(LLMError::PluginError(
            "Provider plugin lacks the prepared-chat contract; rebuild the plugin".into(),
        ))
    }
}

#[async_trait]
impl EmbeddingProvider for ExtismProvider {
    #[cfg_attr(
        feature = "tracing",
        instrument(name = "extism_provider.embed", skip_all)
    )]
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let mut cfg = self.config.clone();

        // Refresh OAuth token if resolver is present
        if let Some(ref resolver) = self.key_resolver {
            resolver.resolve().await?;
            if let Some(obj) = cfg.as_object_mut() {
                obj.insert(
                    "api_key".to_string(),
                    serde_json::Value::String(resolver.current()),
                );
            }
        }

        let arg = ExtismEmbedRequest { cfg, inputs: input };

        self.call_blocking_with_cancel("embed", move |plug| {
            let out: Json<Vec<Vec<f32>>> = plug
                .call_get_error_code("embed", Json(arg))
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })
        .await
    }
}

#[async_trait]
impl CompletionProvider for ExtismProvider {
    #[cfg_attr(
        feature = "tracing",
        instrument(name = "extism_provider.complete", skip_all)
    )]
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let mut cfg = self.config.clone();

        // Refresh OAuth token if resolver is present
        if let Some(ref resolver) = self.key_resolver {
            resolver.resolve().await?;
            if let Some(obj) = cfg.as_object_mut() {
                obj.insert(
                    "api_key".to_string(),
                    serde_json::Value::String(resolver.current()),
                );
            }
        }

        let arg = ExtismCompleteRequest {
            cfg,
            req: req.clone(),
        };

        self.call_blocking_with_cancel("complete", move |plug| {
            let out: Json<CompletionResponse> = plug
                .call_get_error_code("complete", Json(arg))
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })
        .await
    }
}

#[async_trait]
impl LLMProvider for ExtismProvider {
    async fn transcribe(&self, req: &stt::SttRequest) -> Result<stt::SttResponse, LLMError> {
        let mut cfg = self.config.clone();

        // Refresh OAuth token if resolver is present
        if let Some(ref resolver) = self.key_resolver {
            resolver.resolve().await?;
            if let Some(obj) = cfg.as_object_mut() {
                obj.insert(
                    "api_key".to_string(),
                    serde_json::Value::String(resolver.current()),
                );
            }
        }

        let audio_base64 = BASE64.encode(&req.audio);
        let filename = req.filename.clone();
        let mime_type = req.mime_type.clone();
        let model = req.model.clone();
        let language = req.language.clone();

        let out = self
            .call_blocking_with_cancel("transcribe", move |plug| {
                if !plug.function_exists("transcribe") {
                    return Err(LLMError::NotImplemented(
                        "STT not supported by this plugin".into(),
                    ));
                }

                let arg = ExtismSttRequest {
                    cfg,
                    audio_base64,
                    filename,
                    mime_type,
                    model,
                    language,
                };

                let out: Json<ExtismSttResponse> = plug
                    .call_get_error_code("transcribe", Json(arg))
                    .map_err(|(e, code)| decode_plugin_error(e, code))?;
                Ok(out.0)
            })
            .await?;

        Ok(stt::SttResponse {
            text: out.text,
            language: None,
        })
    }

    async fn speech(&self, req: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        let mut cfg = self.config.clone();

        // Refresh OAuth token if resolver is present
        if let Some(ref resolver) = self.key_resolver {
            resolver.resolve().await?;
            if let Some(obj) = cfg.as_object_mut() {
                obj.insert(
                    "api_key".to_string(),
                    serde_json::Value::String(resolver.current()),
                );
            }
        }

        let text = req.text.clone();
        let model = req.model.clone();
        let voice_config = req
            .voice_config
            .as_ref()
            .map(ExtismVoiceConfig::from_voice_config);
        let format = req.format.clone();
        let speed = req.speed;
        let language = req.language.clone();

        let out = self
            .call_blocking_with_cancel("speech", move |plug| {
                if !plug.function_exists("speech") {
                    return Err(LLMError::NotImplemented(
                        "TTS not supported by this plugin".into(),
                    ));
                }

                let arg = ExtismTtsRequest {
                    cfg,
                    text,
                    model,
                    voice_config,
                    format,
                    speed,
                    language,
                };

                let out: Json<ExtismTtsResponse> = plug
                    .call_get_error_code("speech", Json(arg))
                    .map_err(|(e, code)| decode_plugin_error(e, code))?;
                Ok(out.0)
            })
            .await?;

        out.into_tts_response()
            .map_err(|e| LLMError::PluginError(e.to_string()))
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }
}

struct ExtismStreamParser {
    plugin: Arc<Mutex<Plugin>>,
    parser_id: i64,
    closed: bool,
}

impl ExtismStreamParser {
    fn close(&mut self) -> Result<(), LLMError> {
        if self.closed {
            return Ok(());
        }
        let mut plug = self.plugin.lock().map_err(|_| {
            LLMError::PluginError("Extism stream parser close lock poisoned".into())
        })?;
        let _: () = plug
            .call_get_error_code("chat_stream_parser_close", Json(self.parser_id))
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for ExtismStreamParser {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            log::warn!(
                "Failed to close Extism stream parser {}: {:#}",
                self.parser_id,
                e
            );
        }
    }
}

impl ChatStreamParser for ExtismStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        let mut plug = self.plugin.lock().unwrap();
        let out: Json<Vec<ExtismChatChunk>> = plug
            .call_get_error_code(
                "chat_stream_parser_parse",
                Json(ExtismChatChunkParseRequest {
                    parser_id: self.parser_id,
                    chunk: chunk.to_vec(),
                }),
            )
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        Ok(out.0.into_iter().map(|item| item.chunk).collect())
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LLMError> {
        let mut plug = self.plugin.lock().unwrap();
        let out: Json<Vec<ExtismChatChunk>> = plug
            .call_get_error_code("chat_stream_parser_finish", Json(self.parser_id))
            .map_err(|(e, code)| decode_plugin_error(e, code))?;
        drop(plug);
        self.close()?;
        Ok(out.0.into_iter().map(|item| item.chunk).collect())
    }
}

impl HTTPChatProvider for ExtismProvider {
    fn capabilities(&self) -> ChatCapabilities {
        self.plugin_capabilities()
    }

    fn prepare_chat_request(
        &self,
        call: &ChatCall<'_>,
        stream: bool,
    ) -> Result<crate::chat::http::PreparedChatRequest, LLMError> {
        let item_aware_contract_version = self.validate_item_aware_messages(call.messages)?;
        let cfg = self.effective_config()?;
        let chat = ExtismChatRequest {
            cfg,
            messages: call.messages.to_vec(),
            tools: call.tools.map(<[Tool]>::to_vec),
            item_aware_contract_version,
            options: call.options.non_empty(),
        };
        self.call_short_blocking("prepare_chat_request", move |plug| {
            if !plug.function_exists("prepared_chat_contract_version") {
                return Err(LLMError::PluginError(
                    "Provider plugin lacks the prepared-chat context contract; rebuild the plugin"
                        .into(),
                ));
            }
            let version: Json<u32> = plug
                .call_get_error_code("prepared_chat_contract_version", ())
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            if version.0 != PREPARED_CHAT_CONTRACT_VERSION {
                return Err(LLMError::PluginError(format!(
                    "Prepared-chat contract mismatch: expected {}, got {}; rebuild the plugin",
                    PREPARED_CHAT_CONTRACT_VERSION, version.0,
                )));
            }
            let out: Json<ExtismPreparedChatRequest> = plug
                .call_get_error_code(
                    "prepare_chat_request",
                    Json(ExtismChatPrepareRequest { chat, stream }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(crate::chat::http::PreparedChatRequest {
                request: out.0.request.req,
                context: out.0.context,
            })
        })
    }

    fn classify_chat_error_async<'a>(
        &'a self,
        response: &'a http::Response<Vec<u8>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LLMError> + Send + 'a>> {
        Box::pin(async move {
            {
                let plug = match self.plugin.lock() {
                    Ok(plug) => plug,
                    Err(_) => {
                        return LLMError::PluginError(
                            "Extism classify_chat_error mutex poisoned".into(),
                        );
                    }
                };
                if !plug.function_exists("classify_chat_error") {
                    return self.classify_chat_error(response);
                }
            }

            let cfg = match self.effective_config() {
                Ok(cfg) => cfg,
                Err(error) => return error,
            };
            let response = response.clone();
            match self
                .call_blocking_with_cancel("classify_chat_error", move |plug| {
                    let error: Json<PluginError> = plug
                        .call_get_error_code(
                            "classify_chat_error",
                            Json(ExtismChatParseRequest {
                                cfg,
                                resp: SerializableHttpResponse { resp: response },
                                context: Value::Null,
                            }),
                        )
                        .map_err(|(e, code)| decode_plugin_error(e, code))?;
                    Ok(error.0)
                })
                .await
            {
                Ok(error) => decode_classified_plugin_error(error),
                Err(error) => error,
            }
        })
    }

    fn parse_chat(
        &self,
        resp: http::Response<Vec<u8>>,
        context: Value,
    ) -> Result<ChatOutput, LLMError> {
        let cfg = self.effective_config()?;
        let out = self.call_short_blocking("parse_chat_response", move |plug| {
            let out: Json<ExtismChatResponse> = plug
                .call_get_error_code(
                    "parse_chat_response",
                    Json(ExtismChatParseRequest {
                        cfg,
                        resp: SerializableHttpResponse { resp },
                        context,
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })?;

        Ok(out.into_canonical_output())
    }

    fn chat_stream_parser(&self, context: Value) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        let cfg = self.effective_config()?;
        let parser_id = self.call_short_blocking("chat_stream_parser_start", move |plug| {
            let out: Json<i64> = plug
                .call_get_error_code(
                    "chat_stream_parser_start",
                    Json(ExtismChatStreamParserRequest { cfg, context }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })?;

        Ok(Box::new(ExtismStreamParser {
            plugin: self.plugin.clone(),
            parser_id,
            closed: false,
        }))
    }
}

impl HTTPCompletionProvider for ExtismProvider {
    fn complete_request(
        &self,
        req: &CompletionRequest,
    ) -> Result<http::Request<Vec<u8>>, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("complete_request", move |plug| {
            let out: Json<SerializableHttpRequest> = plug
                .call_get_error_code(
                    "complete_request",
                    Json(ExtismCompleteRequest {
                        cfg,
                        req: req.clone(),
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0.req)
        })
    }

    fn parse_complete(
        &self,
        resp: http::Response<Vec<u8>>,
    ) -> Result<CompletionResponse, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("parse_complete_response", move |plug| {
            let out: Json<CompletionResponse> = plug
                .call_get_error_code(
                    "parse_complete_response",
                    Json(ExtismCompleteParseRequest {
                        cfg,
                        resp: SerializableHttpResponse { resp },
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })
    }
}

impl HTTPEmbeddingProvider for ExtismProvider {
    fn embed_request(&self, inputs: &[String]) -> Result<http::Request<Vec<u8>>, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("embed_request", move |plug| {
            let out: Json<SerializableHttpRequest> = plug
                .call_get_error_code(
                    "embed_request",
                    Json(ExtismEmbedRequest {
                        cfg,
                        inputs: inputs.to_vec(),
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0.req)
        })
    }

    fn parse_embed(&self, resp: http::Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("parse_embed_response", move |plug| {
            let out: Json<Vec<Vec<f32>>> = plug
                .call_get_error_code(
                    "parse_embed_response",
                    Json(ExtismEmbedParseRequest {
                        cfg,
                        resp: SerializableHttpResponse { resp },
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })
    }
}

impl HTTPLLMProvider for ExtismProvider {
    fn tools(&self) -> Option<&[Tool]> {
        None
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each export writes to WASI stdout, exercising Wasmtime's synchronous Tokio bridge.
    fn export(name: &str, output: &str) -> String {
        let stores = output
            .bytes()
            .enumerate()
            .map(|(i, byte)| {
                format!(
                    "(call $store_u8 (i64.add (local.get $ptr) (i64.const {i})) (i32.const {byte}))"
                )
            })
            .collect::<String>();
        let len = output.len();
        format!(
            r#"(func (export "{name}") (result i32)
                (local $ptr i64)
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 0) (i32.const 0)))
                (local.set $ptr (call $alloc (i64.const {len})))
                {stores}
                (call $output_set (local.get $ptr) (i64.const {len}))
                (i32.const 0))"#
        )
    }

    /// Builds a plugin exporting each `(name, json_output)` pair.
    fn wat_plugin(exports: &[(&str, &str)]) -> Plugin {
        let funcs = exports
            .iter()
            .map(|(name, output)| export(name, output))
            .collect::<String>();
        let wasm = format!(
            r#"(module
                (import "wasi_snapshot_preview1" "fd_write"
                    (func $fd_write (param i32 i32 i32 i32) (result i32)))
                (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
                (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
                (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
                (memory (export "memory") 1)
                {funcs})"#
        );
        PluginBuilder::new(Manifest::new([Wasm::data(wasm.into_bytes())]))
            .with_wasi(true)
            .build()
            .expect("test plugin should build")
    }

    fn provider_over(plugin: Plugin) -> ExtismProvider {
        ExtismProvider {
            plugin: Arc::new(Mutex::new(plugin)),
            config: serde_json::json!({}),
            user_data: None,
            key_resolver: None,
        }
    }

    #[test]
    fn plugin_without_prepared_chat_contract_asks_for_a_rebuild() {
        let provider = provider_over(wat_plugin(&[("prepare_chat_request", "{}")]));

        let error = provider
            .prepare_chat_request(&ChatCall::new(&[]), false)
            .err()
            .expect("plugin without the contract export must be rejected");

        assert!(error.to_string().contains("rebuild the plugin"), "{error}");
    }

    #[test]
    fn plugin_built_against_the_previous_contract_asks_for_a_rebuild() {
        let previous = (PREPARED_CHAT_CONTRACT_VERSION - 1).to_string();
        let provider = provider_over(wat_plugin(&[
            ("prepared_chat_contract_version", &previous),
            ("prepare_chat_request", "{}"),
        ]));

        let error = provider
            .prepare_chat_request(&ChatCall::new(&[]), false)
            .err()
            .expect("old contract must be rejected");

        let message = error.to_string();
        assert!(message.contains("rebuild the plugin"), "{message}");
        assert!(
            message.contains(&format!("expected {PREPARED_CHAT_CONTRACT_VERSION}")),
            "{message}"
        );
    }

    #[test]
    fn capabilities_come_from_the_plugin_export() {
        let caps = r#"{"streaming":true,"options":{"max_tokens":true,"reasoning_effort":false}}"#;
        let provider = provider_over(wat_plugin(&[("capabilities", caps)]));

        let caps = HTTPChatProvider::capabilities(&provider);

        assert!(caps.streaming);
        assert!(caps.options.max_tokens && !caps.options.reasoning_effort);
    }

    #[test]
    fn plugin_without_capabilities_export_supports_no_options() {
        let provider = provider_over(wat_plugin(&[]));

        assert_eq!(
            HTTPChatProvider::capabilities(&provider),
            crate::chat::ChatCapabilities::NONE
        );
    }

    fn model_listing_factory(static_models: Option<&str>, request_json: &str) -> ExtismFactory {
        let mut exports = vec![
            ("prepare_chat_request", "{}"),
            ("parse_chat_response", "{}"),
            ("list_models_request", request_json),
            ("parse_list_models_response", r#"["parsed-model"]"#),
        ];
        // A missing static export is distinct from one returning `null`.
        if let Some(models) = static_models {
            exports.push(("list_models_static", models));
        }
        let plugin = wat_plugin(&exports);
        ExtismFactory {
            plugin: Arc::new(Mutex::new(plugin)),
            name: "test".to_string(),
            user_data: None,
            allowed_hosts: vec!["127.0.0.1".to_string()],
        }
    }

    async fn check_http_model_listing_wasi() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let static_factory = model_listing_factory(Some(r#"["static-model"]"#), "invalid");
        let models = LLMProviderFactory::list_models(&static_factory, "{}")
            .await
            .expect("static listing should not construct an HTTP request");
        assert_eq!(models, ["static-model"]);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let mut received = 0;
                while !buffer[..received]
                    .windows(4)
                    .any(|bytes| bytes == b"\r\n\r\n")
                {
                    assert!(
                        received < buffer.len(),
                        "request headers exceed buffer size"
                    );
                    let read = socket.read(&mut buffer[received..]).await.unwrap();
                    assert!(
                        read > 0,
                        "connection closed before request headers completed"
                    );
                    received += read;
                }
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
        });
        let request = SerializableHttpRequest {
            req: http::Request::builder()
                .uri(format!("http://{addr}/models"))
                .body(Vec::new())
                .unwrap(),
        };
        // Cover both a missing static export and a static export returning None.
        for static_models in [None, Some("null")] {
            let factory =
                model_listing_factory(static_models, &serde_json::to_string(&request).unwrap());
            let models = LLMProviderFactory::list_models(&factory, "{}")
                .await
                .expect("HTTP listing should prepare and parse off the async thread");
            assert_eq!(models, ["parsed-model"]);
        }
        server.await.unwrap();

        let invalid_factory = model_listing_factory(Some("null"), "invalid");
        assert!(
            LLMProviderFactory::list_models(&invalid_factory, "{}")
                .await
                .is_err()
        );
        assert!(matches!(
            LLMProviderFactory::list_models(&invalid_factory, "{").await,
            Err(LLMError::JsonError(_))
        ));
    }

    #[tokio::test]
    async fn http_model_listing_wasi_current_thread() {
        check_http_model_listing_wasi().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_model_listing_wasi_multi_thread() {
        check_http_model_listing_wasi().await;
    }

    #[test]
    fn malformed_config_is_json_error() {
        let error = parse_config_json("{").expect_err("config should be rejected");

        assert!(!error.is_retryable());
        assert!(matches!(error, LLMError::JsonError(_)));
    }

    #[test]
    fn build_allowed_hosts_prefers_configured_base_url_over_plugin_default() {
        let config = Some(HashMap::from([(
            "base_url".to_string(),
            toml::Value::String("https://token-plan-sgp.xiaomimimo.com/v1".to_string()),
        )]));

        let allowed_hosts =
            build_allowed_hosts(&config, Some("https://api.openai.com/v1")).expect("allowed hosts");

        assert_eq!(
            allowed_hosts,
            vec!["token-plan-sgp.xiaomimimo.com".to_string()]
        );
    }

    #[test]
    fn build_allowed_hosts_uses_plugin_default_without_configured_base_url() {
        let allowed_hosts =
            build_allowed_hosts(&None, Some("https://api.openai.com/v1")).expect("allowed hosts");

        assert_eq!(allowed_hosts, vec!["api.openai.com".to_string()]);
    }

    #[test]
    fn build_allowed_hosts_merges_explicit_hosts_with_configured_base_url() {
        let config = Some(HashMap::from([
            (
                "allowed_hosts".to_string(),
                toml::Value::Array(vec![toml::Value::String("foo.example.com".to_string())]),
            ),
            (
                "base_url".to_string(),
                toml::Value::String("https://token-plan-sgp.xiaomimimo.com/v1".to_string()),
            ),
        ]));

        let allowed_hosts =
            build_allowed_hosts(&config, Some("https://api.openai.com/v1")).expect("allowed hosts");

        assert_eq!(
            allowed_hosts,
            vec![
                "foo.example.com".to_string(),
                "token-plan-sgp.xiaomimimo.com".to_string()
            ]
        );
    }

    #[test]
    fn runtime_base_url_validation_rejects_hosts_outside_allowlist() {
        let err = validate_runtime_base_url(
            "openai",
            &["api.openai.com".to_string()],
            &serde_json::json!({
                "base_url": "https://example.invalid/v1"
            }),
        )
        .expect_err("host should be rejected");

        assert!(
            matches!(err, LLMError::InvalidRequest(message) if message.contains("allowed_hosts"))
        );
    }

    #[test]
    fn decode_classified_plugin_error_preserves_structured_context() {
        let error = crate::error::ProviderFailure::new(
            crate::error::ProviderErrorKind::ServerOverloaded,
            "plugin busy",
        )
        .with_code(Some("server_is_overloaded".into()))
        .with_request_id(Some("req-plugin".into()))
        .with_retry_after_secs(Some(2))
        .into();

        let decoded = decode_classified_plugin_error(PluginError::from_llm_error(&error));

        match decoded {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.message(), "plugin busy");
                assert_eq!(
                    failure.kind(),
                    crate::error::ProviderErrorKind::ServerOverloaded
                );
                assert_eq!(failure.code(), Some("server_is_overloaded"));
                assert_eq!(failure.request_id(), Some("req-plugin"));
                assert_eq!(failure.retry_after_secs(), Some(2));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }
}
