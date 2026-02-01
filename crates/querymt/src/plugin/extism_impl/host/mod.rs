use crate::{
    chat::{ChatMessage, ChatProvider, ChatResponse, StreamChunk, Tool},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    plugin::{
        extism_impl::{
            ExtismChatRequest, ExtismChatResponse, ExtismEmbedRequest, ExtismSttRequest,
            ExtismSttResponse, ExtismTtsRequest, ExtismTtsResponse,
        },
        Fut, HTTPLLMProviderFactory, LLMProviderFactory,
    },
    providers::read_providers_from_cache,
    stt, tts, LLMProvider,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use extism::{convert::Json, Manifest, Plugin, PluginBuilder, Wasm};
use futures::FutureExt;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use std::sync::atomic::{AtomicBool, Ordering};
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
    };
}

#[derive(Clone)]
pub struct ExtismFactory {
    plugin: Arc<Mutex<Plugin>>,
    name: String,
    user_data: Option<extism::UserData<functions::HostState>>,
}

fn call_plugin_str(plugin: Arc<Mutex<Plugin>>, func: &str, arg: &Value) -> anyhow::Result<String> {
    let input = serde_json::to_string(arg)?;
    let input_bytes = input.into_bytes();

    let mut plug = plugin.lock().unwrap();
    let output_bytes: Vec<u8> = plug.call(func, &input_bytes)?;
    Ok(std::str::from_utf8(&output_bytes)?.to_string())
}

fn call_plugin_json(plugin: Arc<Mutex<Plugin>>, func: &str, arg: &Value) -> anyhow::Result<Value> {
    let r = call_plugin_str(plugin, func, arg)?;
    Ok(serde_json::from_str(&r)?)
}

impl ExtismFactory {
    #[instrument(name = "extism_factory.load", skip_all)]
    pub fn load(
        wasm_content: Vec<u8>,
        config: &Option<HashMap<String, toml::Value>>,
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

        let mut allowed_hosts: Vec<String> = Vec::new();
        if let Some(runtime_cfg) = config {
            if let Some(hosts) = runtime_cfg.get("allowed_hosts") {
                allowed_hosts.append(
                    &mut hosts
                        .clone()
                        .try_into()
                        .map_err(|e| LLMError::GenericError(format!("{:#}", e)))?,
                );
            }
        }

        let name = call_plugin_str(init_plugin.clone(), "name", &Value::Null)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;
        if let Some(base_url) = call_plugin_str(init_plugin.clone(), "base_url", &Value::Null)
            .ok()
            .and_then(|v| Url::parse(&v).ok())
            .and_then(|url| url.host_str().map(|s| s.to_string()))
        {
            allowed_hosts.push(base_url);
        }
        drop(init_plugin);

        let manifest = Manifest::new([Wasm::data(wasm_content.clone())])
            .with_allowed_hosts(allowed_hosts.clone().into_iter())
            .with_config(env_map.into_iter());

        let user_data =
            extism::UserData::new(functions::HostState::new(allowed_hosts, tokio_handle));
        let builder = with_host_functions!(PluginBuilder::new(manifest), user_data);

        let plugin = Arc::new(Mutex::new(
            builder
                .build()
                .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?,
        ));

        Ok(Self {
            plugin,
            name,
            user_data: Some(user_data),
        })
    }

    fn call(&self, func: &str, arg: &Value) -> anyhow::Result<String> {
        call_plugin_str(self.plugin.clone(), func, arg)
    }

    fn call_json(&self, func: &str, arg: &Value) -> anyhow::Result<Value> {
        call_plugin_json(self.plugin.clone(), func, arg)
    }
}

impl LLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn config_schema(&self) -> Value {
        self.call_json("config_schema", &Value::Null)
            .expect("config_schema() must return valid JSON")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError> {
        let _from_cfg = self
            .call("from_config", cfg)
            .map_err(|e| LLMError::PluginError(format!("{:#}", e)))?;

        let provider = ExtismProvider {
            plugin: self.plugin.clone(),
            config: cfg.clone(),
            user_data: self.user_data.clone(),
        };
        Ok(Box::new(provider))
    }

    fn list_models<'a>(&'a self, cfg: &Value) -> Fut<'a, Result<Vec<String>, LLMError>> {
        // list_models can do host HTTP calls, so run the Extism VM call off the Tokio runtime
        // thread to avoid deadlocks on current-thread runtimes.
        let cfg_to = cfg.clone();
        let plugin = self.plugin.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let mut plug = plugin.lock().unwrap();
                let out: Json<Vec<String>> = plug
                    .call_get_error_code("list_models", Json(cfg_to))
                    .map_err(|(e, code)| decode_plugin_error(e, code))?;
                Ok::<_, LLMError>(out.0)
            })
            .await
            .map_err(|e| LLMError::PluginError(format!("Extism list_models join error: {:#}", e)))?
        }
        .boxed()
    }

    fn as_http(&self) -> Option<&dyn crate::plugin::http::HTTPLLMProviderFactory> {
        Some(self)
    }
}

impl HTTPLLMProviderFactory for ExtismFactory {
    fn name(&self) -> &str {
        (self as &dyn LLMProviderFactory).name()
    }

    fn config_schema(&self) -> Value {
        (self as &dyn LLMProviderFactory).config_schema()
    }

    fn from_config(&self, _cfg: &Value) -> Result<Box<dyn crate::HTTPLLMProvider>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
    }

    fn list_models_request(&self, _cfg: &Value) -> Result<http::Request<Vec<u8>>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
    }

    fn parse_list_models(&self, _resp: http::Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Err(LLMError::PluginError(
            "ExtismProvider should not be used as HTTPLLMProviderFactory".into(),
        ))
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
}

impl ExtismProvider {
    fn user_data_required(&self) -> Result<extism::UserData<functions::HostState>, LLMError> {
        self.user_data.clone().ok_or_else(|| {
            LLMError::PluginError("No UserData found for Extism provider".into())
        })
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

        let join = tokio::task::spawn_blocking(move || {
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

    #[instrument(name = "extism_provider.chat_with_tools", skip_all)]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let arg = ExtismChatRequest {
            cfg: self.config.clone(),
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

    #[instrument(name = "extism_provider.chat_stream_with_tools", skip_all)]
    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LLMError>> + Send>>,
        LLMError,
    > {
        if !self.supports_streaming() {
            return Err(LLMError::NotImplemented(
                "Streaming not supported by this plugin".into(),
            ));
        }

        use crate::plugin::extism_impl::ExtismChatChunk;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

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
        if let Some(obj) = cfg.as_object_mut() {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
        }
        let arg = ExtismChatRequest {
            cfg,
            messages: messages.to_vec(),
            tools: tools.map(|v| v.to_vec()),
        };

        std::thread::spawn(move || {
            log::debug!("Extism plugin chat_stream thread started");
            let mut plug = plugin.lock().unwrap();
            let res: Result<(), (extism::Error, i32)> =
                plug.call_get_error_code("chat_stream", Json(arg));

            if let Err((e, _code)) = res {
                let cancel_state = user_data_clone
                    .get()
                    .ok()
                    .map(|s| s.lock().unwrap().cancel_state)
                    .unwrap_or(functions::CancelState::NotCancelled);

                match cancel_state {
                    functions::CancelState::NotCancelled => {
                        log::error!("chat_stream plugin call failed: {:#}", e);
                    }
                    functions::CancelState::CancelledByConsumerDrop => {
                        log::info!("chat_stream stopped due to cancellation: {:#}", e);
                    }
                    functions::CancelState::YieldReceiverDropped => {
                        log::warn!(
                            "chat_stream stopped after receiver dropped (not explicit cancel): {:#}",
                            e
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
                        log::info!("Extism stream cancelled: consumer dropped stream");
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

        let guard = StreamCancelGuard {
            user_data: self.user_data.clone().unwrap(),
        };

        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|bytes| {
                let chunk_res: Result<StreamChunk, LLMError> = serde_json::from_slice::<
                    ExtismChatChunk,
                >(&bytes)
                .map(|c| c.chunk)
                .map_err(|e| LLMError::PluginError(format!("Failed to deserialize chunk: {}", e)));
                (chunk_res, rx)
            })
        });

        Ok(Box::pin(GuardedStream {
            inner: Box::pin(stream),
            _guard: guard,
        }))
    }
}

#[async_trait]
impl EmbeddingProvider for ExtismProvider {
    #[instrument(name = "extism_provider.embed", skip_all)]
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let arg = ExtismEmbedRequest {
            cfg: self.config.clone(),
            inputs: input,
        };

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
    #[instrument(name = "extism_provider.complete", skip_all)]
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let arg = ExtismCompleteRequest {
            cfg: self.config.clone(),
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
        let cfg = self.config.clone();
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
        let cfg = self.config.clone();
        let text = req.text.clone();
        let model = req.model.clone();
        let voice = req.voice.clone();
        let format = req.format.clone();
        let speed = req.speed;

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
                    voice,
                    format,
                    speed,
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
}
