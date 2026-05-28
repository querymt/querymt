use crate::{
    HTTPLLMProvider, LLMProvider,
    adapters::LLMProviderFromHTTP,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatProvider, ChatResponse, StreamChunk, Tool,
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
            ExtismChatRequest, ExtismChatResponse, ExtismCompleteParseRequest,
            ExtismEmbedParseRequest, ExtismEmbedRequest, ExtismListModelsParseRequest,
            ExtismListModelsRequest, ExtismSttRequest, ExtismSttResponse, ExtismTtsRequest,
            ExtismTtsResponse, ExtismVoiceConfig, SerializableHttpRequest,
            SerializableHttpResponse,
        },
    },
    providers::read_providers_from_cache,
    stt, tts,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use extism::{Manifest, Plugin, PluginBuilder, Wasm, convert::Json};
use futures::{FutureExt, StreamExt};
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

fn header_token_hint(value: Option<&http::HeaderValue>) -> String {
    let Some(value) = value else {
        return "<missing>".to_string();
    };
    let Ok(value_str) = value.to_str() else {
        return "<non-utf8>".to_string();
    };
    let mut parts = value_str.splitn(2, ' ');
    let scheme = parts.next().unwrap_or("<unknown>");
    let token = parts.next().unwrap_or("");
    if token.is_empty() {
        return format!("{scheme} <empty>");
    }
    let len = token.chars().count();
    if len <= 10 {
        return format!("{scheme} <redacted>");
    }
    let prefix: String = token.chars().take(6).collect();
    let suffix: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{scheme} {prefix}...{suffix}")
}

fn decode_stream_item(item: Result<Vec<u8>, LLMError>) -> Result<StreamChunk, LLMError> {
    match item {
        Ok(bytes) => serde_json::from_slice::<crate::plugin::extism_impl::ExtismChatChunk>(&bytes)
            .map(|c| c.chunk)
            .map_err(|e| LLMError::PluginError(format!("Failed to deserialize chunk: {}", e))),
        Err(llm_err) => Err(llm_err),
    }
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

        let v = read_providers_from_cache()?;
        env_map.insert(
            "PROVIDERS_REGISTRY_DATA".to_string(),
            serde_json::to_string(&v)?,
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
        plug.function_exists("chat_request")
            && plug.function_exists("chat_stream_request")
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
        let cfg_value: Value = serde_json::from_str(cfg)
            .map_err(|e| LLMError::PluginError(format!("Invalid JSON config: {:#}", e)))?;
        self.validate_runtime_base_url(&cfg_value)?;

        let _from_cfg = self
            .call("from_config", &cfg_value)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg_value,
            user_data: self.user_data.clone(),
            key_resolver: None,
        };

        if self.supports_http_adapter_abi() {
            let http_provider: Arc<dyn HTTPLLMProvider> = Arc::new(provider);
            return Ok(Box::new(LLMProviderFromHTTP::new(http_provider)));
        }

        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &str) -> Fut<'a, Result<Vec<String>, LLMError>> {
        // list_models can do host HTTP calls, so run the Extism VM call off the Tokio runtime
        // thread to avoid deadlocks on current-thread runtimes.
        let cfg_value: Value = match serde_json::from_str(cfg) {
            Ok(v) => v,
            Err(e) => {
                return Box::pin(async move {
                    Err(LLMError::PluginError(format!(
                        "Invalid JSON config: {:#}",
                        e
                    )))
                });
            }
        };

        if self.supports_http_adapter_abi() {
            if let Some(result) = <Self as HTTPLLMProviderFactory>::list_models_static(self, cfg) {
                return Box::pin(async move { result });
            }

            let req = match <Self as HTTPLLMProviderFactory>::list_models_request(self, cfg) {
                Ok(req) => req,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            return async move {
                let resp = crate::outbound::call_outbound(req).await?;
                <Self as HTTPLLMProviderFactory>::parse_list_models(self, resp)
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
        let cfg_value: Value = serde_json::from_str(cfg)
            .map_err(|e| LLMError::PluginError(format!("Invalid JSON config: {:#}", e)))?;
        self.validate_runtime_base_url(&cfg_value)?;

        let _from_cfg = self
            .call("from_config", &cfg_value)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg_value,
            user_data: self.user_data.clone(),
            key_resolver: None,
        };
        Ok(Box::new(provider))
    }

    fn list_models_static(&self, cfg: &str) -> Option<Result<Vec<String>, LLMError>> {
        let cfg_value: Value = match serde_json::from_str(cfg) {
            Ok(v) => v,
            Err(e) => {
                return Some(Err(LLMError::PluginError(format!(
                    "Invalid JSON config: {:#}",
                    e
                ))));
            }
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
        let cfg_value: Value = serde_json::from_str(cfg)
            .map_err(|e| LLMError::PluginError(format!("Invalid JSON config: {:#}", e)))?;
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

    fn call_short_blocking<T, F>(&self, op: &'static str, f: F) -> Result<T, LLMError>
    where
        F: FnOnce(&mut Plugin) -> Result<T, LLMError>,
    {
        let mut plug = self.plugin.lock().unwrap();
        f(&mut plug).map_err(|e| LLMError::PluginError(format!("Extism {op} failed: {:#}", e)))
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
        // under `extism_provider.chat_stream_with_tools` rather than
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

#[async_trait]
impl ChatProvider for ExtismProvider {
    fn supports_streaming(&self) -> bool {
        let mut plug = self.plugin.lock().unwrap();
        if !plug.function_exists("supports_streaming") {
            return false;
        }
        let res: Result<Json<bool>, _> = plug.call("supports_streaming", Json(self.config.clone()));
        match res {
            Ok(Json(supported)) => supported,
            Err(_) => false,
        }
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "extism_provider.chat_with_tools", skip_all)
    )]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
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

        let arg = ExtismChatRequest {
            cfg,
            messages: messages.to_vec(),
            tools: tools.map(|v| v.to_vec()),
        };
        // chat can do host HTTP calls, so run the Extism VM call off the Tokio runtime thread to
        // avoid deadlocks on current-thread runtimes. Also wire cancellation so dropping the
        // future can interrupt host HTTP and release the plugin mutex.
        let out = self
            .call_blocking_with_cancel("chat", move |plug| {
                let out: Json<ExtismChatResponse> = plug
                    .call_get_error_code("chat", Json(arg))
                    .map_err(|(e, code)| decode_plugin_error(e, code))?;
                Ok(out.0)
            })
            .await?;

        Ok(Box::new(out) as Box<dyn ChatResponse>)
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "extism_provider.chat_stream_with_tools", skip_all)
    )]
    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        if !ChatProvider::supports_streaming(self) {
            return Err(LLMError::NotImplemented(
                "Streaming not supported by this plugin".into(),
            ));
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        if let Some(user_data) = &self.user_data {
            let state = user_data
                .get()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
            let mut state_guard = state.lock().unwrap();
            state_guard.cancel_state = functions::CancelState::NotCancelled;
            let _ = state_guard.cancel_watch_tx.send(false);
            state_guard.yield_tx = Some(tx);
        } else {
            return Err(LLMError::PluginError(
                "No UserData found for streaming".into(),
            ));
        }

        let plugin = self.plugin.clone();
        let user_data_clone = self.user_data.clone().unwrap();
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

        if let Some(obj) = cfg.as_object_mut() {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
        }
        let arg = ExtismChatRequest {
            cfg,
            messages: messages.to_vec(),
            tools: tools.map(|v| v.to_vec()),
        };

        let caller_span = tracing::Span::current();

        tokio::task::spawn_blocking(move || {
            let _guard = caller_span.enter();
            log::debug!("Extism plugin chat_stream thread started");
            let mut plug = plugin.lock().unwrap();
            let res: Result<(), (extism::Error, i32)> =
                plug.call_get_error_code("chat_stream", Json(arg));

            if let Err((e, code)) = res {
                // Decode the error using the proper error code from WithReturnCode
                let llm_error = decode_plugin_error(e, code);

                let cancel_state = user_data_clone
                    .get()
                    .ok()
                    .map(|s| s.lock().unwrap().cancel_state)
                    .unwrap_or(functions::CancelState::NotCancelled);

                match cancel_state {
                    functions::CancelState::NotCancelled => {
                        log::error!("chat_stream plugin call failed: {:#}", llm_error);
                        // Propagate error to stream consumer so it surfaces in UI
                        if let Ok(state) = user_data_clone.get() {
                            let state_guard = state.lock().unwrap();
                            if let Some(tx) = &state_guard.yield_tx {
                                let _ = tx.send(Err(llm_error));
                            }
                        }
                    }
                    functions::CancelState::CancelledByConsumerDrop => {
                        log::info!("chat_stream stopped due to cancellation: {:#}", llm_error);
                    }
                    functions::CancelState::YieldReceiverDropped => {
                        log::warn!(
                            "chat_stream stopped after receiver dropped (not explicit cancel): {:#}",
                            llm_error
                        );
                    }
                }
            }

            log::debug!("Extism plugin chat_stream thread finished, clearing yield_tx");
            if let Ok(state) = user_data_clone.get() {
                let mut state_guard = state.lock().unwrap();
                state_guard.yield_tx = None;
            }
        });

        // If the consumer drops this stream, mark the host state as cancelled.
        struct StreamCancelGuard {
            user_data: extism::UserData<functions::HostState>,
        }

        impl Drop for StreamCancelGuard {
            fn drop(&mut self) {
                if let Ok(state) = self.user_data.get() {
                    let mut state_guard = state.lock().unwrap();

                    // Only treat this as a "cancellation" if the stream was still actively wired.
                    let was_active = state_guard.yield_tx.is_some();
                    if was_active {
                        state_guard.cancel_state = functions::CancelState::CancelledByConsumerDrop;
                        let _ = state_guard.cancel_watch_tx.send(true);
                        state_guard.yield_tx = None;
                        log::debug!("Extism stream cancelled: consumer dropped stream");
                    } else {
                        log::debug!(
                            "Extism stream dropped after completion/error (not a user cancel)"
                        );
                    }
                }
            }
        }

        struct GuardedStream {
            inner: std::pin::Pin<
                Box<dyn futures::Stream<Item = Result<StreamChunk, LLMError>> + Send>,
            >,
            _guard: StreamCancelGuard,
        }

        impl futures::Stream for GuardedStream {
            type Item = Result<StreamChunk, LLMError>;

            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                self.inner.as_mut().poll_next(cx)
            }
        }

        let first_item = rx.recv().await.ok_or_else(|| {
            LLMError::PluginError("Extism streaming ended before the first chunk".into())
        })?;

        match decode_stream_item(first_item) {
            Ok(first_chunk) => {
                let guard = StreamCancelGuard {
                    user_data: self.user_data.clone().unwrap(),
                };

                let stream = futures::stream::once(async move { Ok(first_chunk) }).chain(
                    futures::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|item| (decode_stream_item(item), rx))
                    }),
                );

                Ok(Box::pin(GuardedStream {
                    inner: Box::pin(stream),
                    _guard: guard,
                }))
            }
            Err(err) => Err(err),
        }
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

        Ok(stt::SttResponse { text: out.text })
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
}

impl Drop for ExtismStreamParser {
    fn drop(&mut self) {
        if let Ok(mut plug) = self.plugin.lock() {
            let _: Result<(), (extism::Error, i32)> =
                plug.call_get_error_code("chat_stream_parser_close", Json(self.parser_id));
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
        Ok(out.0.into_iter().map(|item| item.chunk).collect())
    }
}

impl HTTPChatProvider for ExtismProvider {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<http::Request<Vec<u8>>, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("chat_request", move |plug| {
            let req: Json<SerializableHttpRequest> = plug
                .call_get_error_code(
                    "chat_request",
                    Json(ExtismChatRequest {
                        cfg,
                        messages: messages.to_vec(),
                        tools: tools.map(|v| v.to_vec()),
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            let req = req.0.req;
            let auth_header = req.headers().get(http::header::AUTHORIZATION);
            let auth_hint = header_token_hint(auth_header);
            log::debug!(
                "Extism HTTP chat_request built: method={} uri={} has_authorization={} auth_hint={} body_len={}",
                req.method(),
                req.uri(),
                auth_header.is_some(),
                auth_hint,
                req.body().len()
            );
            Ok(req)
        })
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<http::Request<Vec<u8>>, LLMError> {
        let cfg = self.effective_config()?;
        self.call_short_blocking("chat_stream_request", move |plug| {
            let req: Json<SerializableHttpRequest> = plug
                .call_get_error_code(
                    "chat_stream_request",
                    Json(ExtismChatRequest {
                        cfg,
                        messages: messages.to_vec(),
                        tools: tools.map(|v| v.to_vec()),
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            let req = req.0.req;
            let auth_header = req.headers().get(http::header::AUTHORIZATION);
            let auth_hint = header_token_hint(auth_header);
            log::debug!(
                "Extism HTTP chat_stream_request built: method={} uri={} has_authorization={} auth_hint={} body_len={}",
                req.method(),
                req.uri(),
                auth_header.is_some(),
                auth_hint,
                req.body().len()
            );
            Ok(req)
        })
    }

    fn parse_chat(&self, resp: http::Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        let cfg = self.effective_config()?;
        let out = self.call_short_blocking("parse_chat_response", move |plug| {
            let out: Json<ExtismChatResponse> = plug
                .call_get_error_code(
                    "parse_chat_response",
                    Json(ExtismChatParseRequest {
                        cfg,
                        resp: SerializableHttpResponse { resp },
                    }),
                )
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })?;

        Ok(Box::new(out) as Box<dyn ChatResponse>)
    }

    fn supports_streaming(&self) -> bool {
        let plug = self.plugin.lock().unwrap();
        plug.function_exists("chat_stream_parser_start")
            && plug.function_exists("chat_stream_parser_parse")
            && plug.function_exists("chat_stream_parser_finish")
            && plug.function_exists("chat_stream_parser_close")
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        let cfg = self.effective_config()?;
        let parser_id = self.call_short_blocking("chat_stream_parser_start", move |plug| {
            let out: Json<i64> = plug
                .call_get_error_code("chat_stream_parser_start", Json(cfg))
                .map_err(|(e, code)| decode_plugin_error(e, code))?;
            Ok(out.0)
        })?;

        Ok(Box::new(ExtismStreamParser {
            plugin: self.plugin.clone(),
            parser_id,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::StreamChunk;

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
    fn decode_stream_item_returns_chunk_for_valid_payload() {
        let bytes = serde_json::to_vec(&crate::plugin::extism_impl::ExtismChatChunk {
            chunk: StreamChunk::Text("hello".into()),
            usage: None,
        })
        .expect("serialize chunk");

        let chunk = decode_stream_item(Ok(bytes)).expect("chunk should decode");
        assert!(matches!(chunk, StreamChunk::Text(text) if text == "hello"));
    }

    #[test]
    fn decode_stream_item_preserves_llm_error() {
        let decoded = decode_stream_item(Err(LLMError::HttpStatus {
            status_code: 503,
            message: "upstream connect error".into(),
            retry_after_secs: None,
        }))
        .expect_err("error should propagate");
        assert!(matches!(
            decoded,
            LLMError::HttpStatus {
                status_code: 503,
                ..
            }
        ));
    }
}
