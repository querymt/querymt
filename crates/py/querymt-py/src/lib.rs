use ::querymt::chat::{
    ChatInputPart, ChatMessage, ChatRole, FinishReason, MediaKind, StreamChunk, Tool,
};
use ::querymt::dynamic::PluginRegistryDynamicExt;
use ::querymt::plugin::host::PluginRegistry;
use ::querymt::{LLMBuilder, LLMProvider, ToolCall, Usage};
use anyhow::{Result, anyhow};
use base64::Engine;
use futures_util::StreamExt;
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PySequence};
use querymt_remote::{
    LanDiscovery, LanMeshConfig, MeshChatProvider, MeshRuntimeConfig, MeshRuntimeHandle,
    ModelAllowlistBackend, ProviderShare, RegistryProviderBackend, StaticCatalogBackend,
    bootstrap_mesh_runtime, find_provider_on_mesh,
};
use serde_json::{Map, Number, Value};
use std::future;
use std::sync::Arc;
use std::time::Duration;

#[pyclass(name = "Registry")]
struct PyRegistry {
    inner: Arc<PluginRegistry>,
}

#[pyclass(name = "Provider")]
struct PyProvider {
    inner: Arc<dyn LLMProvider>,
}

#[pyclass(name = "ChatStream")]
struct PyChatStream {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Result<PyStreamChunk, String>>>>,
}

#[pyclass(name = "MeshRuntime")]
struct PyMeshRuntime {
    inner: MeshRuntimeHandle,
}

#[pyclass(name = "ProviderShare")]
struct PyProviderShare {
    _runtime: MeshRuntimeHandle,
    _share: ProviderShare,
}

#[pyclass(name = "ChatResponse")]
struct PyChatResponse {
    #[pyo3(get)]
    text: Option<String>,
    #[pyo3(get)]
    thinking: Option<String>,
    #[pyo3(get)]
    finish_reason: Option<String>,
    #[pyo3(get)]
    usage: Option<PyUsage>,
    #[pyo3(get)]
    tool_calls: Vec<PyToolCall>,
    /// Canonical structured output as JSON.
    output: Option<Value>,
}

#[pyclass(name = "Usage", skip_from_py_object)]
#[derive(Clone)]
struct PyUsage {
    #[pyo3(get)]
    input_tokens: u32,
    #[pyo3(get)]
    output_tokens: u32,
    #[pyo3(get)]
    reasoning_tokens: u32,
    #[pyo3(get)]
    cache_read: u32,
    #[pyo3(get)]
    cache_write: u32,
}

#[pyclass(name = "ToolCall", skip_from_py_object)]
#[derive(Clone)]
struct PyToolCall {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    call_type: String,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    arguments: String,
}

#[pyclass(name = "StreamChunk", skip_from_py_object)]
#[derive(Clone)]
struct PyStreamChunk {
    #[pyo3(get)]
    kind: String,
    data: Value,
}

#[pymethods]
impl PyRegistry {
    #[staticmethod]
    fn default<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let registry = default_registry().await.map_err(into_py_err)?;
            Python::attach(|py| {
                Py::new(
                    py,
                    PyRegistry {
                        inner: Arc::new(registry),
                    },
                )
            })
        })
    }

    #[staticmethod]
    fn from_path<'py>(py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut registry = PluginRegistry::from_path(&path).map_err(into_py_err)?;
            registry.register_dynamic_loaders();
            Python::attach(|py| {
                Py::new(
                    py,
                    PyRegistry {
                        inner: Arc::new(registry),
                    },
                )
            })
        })
    }

    #[staticmethod]
    fn empty(py: Python<'_>) -> PyResult<Py<Self>> {
        Py::new(
            py,
            PyRegistry {
                inner: Arc::new(PluginRegistry::empty()),
            },
        )
    }

    fn load_all_plugins<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let registry = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            registry.load_all_plugins().await;
            Python::attach(|py| Ok(py.None()))
        })
    }

    fn list_providers(&self) -> Vec<String> {
        self.inner
            .list_provider_names()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn list_models<'py>(&self, py: Python<'py>, provider: String) -> PyResult<Bound<'py, PyAny>> {
        let registry = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let models = registry.list_models(&provider).await.map_err(into_py_err)?;
            Python::attach(|py| Ok(models.into_pyobject(py)?.into_any().unbind()))
        })
    }

    #[pyo3(signature = (provider, model, params=None, api_key=None, base_url=None))]
    fn provider<'py>(
        &self,
        py: Python<'py>,
        provider: String,
        model: String,
        params: Option<Py<PyAny>>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let registry = Arc::clone(&self.inner);
        let params_json =
            python_opt_to_json(params.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let provider =
                build_provider(&registry, &provider, &model, params_json, api_key, base_url)
                    .await
                    .map_err(into_py_err)?;
            Python::attach(|py| Py::new(py, PyProvider { inner: provider }))
        })
    }
}

#[pymethods]
impl PyProvider {
    fn chat<'py>(&self, py: Python<'py>, messages: Py<PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let provider = Arc::clone(&self.inner);
        let messages = py_messages_to_rust(messages.bind(py)).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = provider.chat(&messages).await.map_err(into_py_err)?;
            let response = chat_output_to_python(&response);
            Python::attach(|py| Py::new(py, response))
        })
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    #[pyo3(signature = (messages, tools=None))]
    fn chat_with_tools<'py>(
        &self,
        py: Python<'py>,
        messages: Py<PyAny>,
        tools: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let provider = Arc::clone(&self.inner);
        let messages = py_messages_to_rust(messages.bind(py)).map_err(into_py_err)?;
        let tools = python_tools_to_rust(tools.as_ref().map(|value| value.bind(py)))
            .map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = provider
                .chat_with_tools(&messages, tools.as_deref())
                .await
                .map_err(into_py_err)?;
            let response = chat_output_to_python(&response);
            Python::attach(|py| Py::new(py, response))
        })
    }

    fn chat_stream<'py>(
        &self,
        py: Python<'py>,
        messages: Py<PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let provider = Arc::clone(&self.inner);
        let messages = py_messages_to_rust(messages.bind(py)).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let stream = provider.chat_stream(&messages).await.map_err(into_py_err)?;
            let stream = stream_to_python(stream);
            Python::attach(|py| Py::new(py, stream))
        })
    }

    #[pyo3(signature = (messages, tools=None))]
    fn chat_stream_with_tools<'py>(
        &self,
        py: Python<'py>,
        messages: Py<PyAny>,
        tools: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let provider = Arc::clone(&self.inner);
        let messages = py_messages_to_rust(messages.bind(py)).map_err(into_py_err)?;
        let tools = python_tools_to_rust(tools.as_ref().map(|value| value.bind(py)))
            .map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let stream = provider
                .chat_stream_with_tools(&messages, tools.as_deref())
                .await
                .map_err(into_py_err)?;
            let stream = stream_to_python(stream);
            Python::attach(|py| Py::new(py, stream))
        })
    }
}

#[pymethods]
impl PyMeshRuntime {
    #[staticmethod]
    #[pyo3(signature = (node_name=None, listen=None, request_timeout_secs=300, stream_reconnect_grace_secs=120))]
    fn lan<'py>(
        py: Python<'py>,
        node_name: Option<String>,
        listen: Option<String>,
        request_timeout_secs: u64,
        stream_reconnect_grace_secs: u64,
    ) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let runtime = bootstrap_mesh_runtime(&MeshRuntimeConfig {
                enabled: true,
                lan: Some(LanMeshConfig {
                    listen: Some(listen.unwrap_or_else(|| "/ip4/0.0.0.0/tcp/0".to_string())),
                    discovery: LanDiscovery::Mdns,
                    directory: querymt_remote::mesh_runtime_config::DirectoryMode::Cached,
                }),
                iroh_enabled: false,
                iroh_scopes: Vec::new(),
                identity_file: None,
                request_timeout: Duration::from_secs(request_timeout_secs),
                stream_reconnect_grace: Duration::from_secs(stream_reconnect_grace_secs),
                node_name,
                peers: Vec::new(),
                auto_fallback: false,
            })
            .await
            .map_err(into_py_err)?;
            Python::attach(|py| Py::new(py, PyMeshRuntime { inner: runtime }))
        })
    }

    #[getter]
    fn peer_id(&self) -> String {
        self.inner.peer_id().to_string()
    }

    fn known_peers(&self) -> Vec<String> {
        self.inner
            .known_peer_ids()
            .into_iter()
            .map(|peer| peer.to_string())
            .collect()
    }

    fn active_scopes(&self) -> Vec<String> {
        self.inner
            .active_scopes()
            .into_iter()
            .map(|scope| scope.to_string())
            .collect()
    }

    #[pyo3(signature = (registry, provider, allowed_models, label=None))]
    fn share_provider<'py>(
        &self,
        py: Python<'py>,
        registry: PyRef<'py, PyRegistry>,
        provider: String,
        allowed_models: Vec<String>,
        label: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let runtime = self.inner.clone();
        let registry = Arc::clone(&registry.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let backend =
                ModelAllowlistBackend::new(RegistryProviderBackend::new(Arc::clone(&registry)))
                    .allow_models(provider.clone(), allowed_models.clone());
            let catalog = StaticCatalogBackend::provider_models(
                runtime.peer_id().to_string(),
                label,
                provider,
                allowed_models,
            );
            let share = ProviderShare::new(Arc::new(backend), Arc::new(catalog));
            share.register_on_mesh(&runtime).await;
            Python::attach(|py| {
                Py::new(
                    py,
                    PyProviderShare {
                        _runtime: runtime,
                        _share: share,
                    },
                )
            })
        })
    }

    #[pyo3(signature = (provider, model, params=None))]
    fn find_provider<'py>(
        &self,
        py: Python<'py>,
        provider: String,
        model: String,
        params: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let runtime = self.inner.clone();
        let params_json =
            python_opt_to_json(params.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let node_id = find_provider_on_mesh(runtime.as_mesh_handle(), &provider)
                .await
                .ok_or_else(|| anyhow!("provider '{}' not found on mesh", provider))
                .map_err(into_py_err)?;
            let provider = Arc::new(
                MeshChatProvider::from_node_id(
                    runtime.as_mesh_handle(),
                    &node_id,
                    &provider,
                    &model,
                )
                .with_params(params_json),
            ) as Arc<dyn LLMProvider>;
            Python::attach(|py| Py::new(py, PyProvider { inner: provider }))
        })
    }
}

#[pymethods]
impl PyProviderShare {
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            future::pending::<()>().await;
            #[allow(unreachable_code)]
            Python::attach(|py| Ok(py.None()))
        })
    }
}

#[pymethods]
impl PyChatResponse {
    fn __str__(&self) -> String {
        self.text.clone().unwrap_or_default()
    }

    #[getter]
    fn output<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match &self.output {
            Some(value) => json_to_python(py, value),
            None => Ok(py.None().into_bound(py).to_owned()),
        }
    }
}

#[pymethods]
impl PyStreamChunk {
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_python(py, &self.data)
    }
}

#[pymethods]
impl PyChatStream {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(slf: PyRef<'py, Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = Arc::clone(&slf.rx);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(Ok(chunk)) => Python::attach(|py| Py::new(py, chunk)),
                Some(Err(err)) => Err(PyRuntimeError::new_err(err)),
                None => Err(PyStopAsyncIteration::new_err("stream ended")),
            }
        })
    }
}

fn chat_output_to_python(output: &::querymt::chat::ChatOutput) -> PyChatResponse {
    PyChatResponse {
        text: output.text(),
        thinking: output.thinking(),
        finish_reason: output.finish_reason.map(finish_reason_to_string),
        usage: output.usage.clone().map(usage_to_python),
        tool_calls: output
            .tool_calls()
            .unwrap_or_default()
            .into_iter()
            .map(tool_call_to_python)
            .collect(),
        output: serde_json::to_value(output).ok(),
    }
}

fn finish_reason_to_string(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::Error => "error",
        FinishReason::Other => "other",
        FinishReason::Unknown => "unknown",
    }
    .to_string()
}

fn usage_to_python(usage: Usage) -> PyUsage {
    PyUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
    }
}

fn tool_call_to_python(call: ToolCall) -> PyToolCall {
    PyToolCall {
        id: call.id,
        call_type: call.call_type,
        name: call.function.name,
        arguments: call.function.arguments,
    }
}

fn stream_to_python(
    mut stream: std::pin::Pin<
        Box<
            dyn futures_util::Stream<Item = Result<StreamChunk, ::querymt::error::LLMError>> + Send,
        >,
    >,
) -> PyChatStream {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let mapped = item
                .map(stream_chunk_to_python)
                .map_err(|err| err.to_string());
            if tx.send(mapped).await.is_err() {
                break;
            }
        }
    });
    PyChatStream {
        rx: Arc::new(tokio::sync::Mutex::new(rx)),
    }
}

fn stream_chunk_to_python(chunk: StreamChunk) -> PyStreamChunk {
    let (kind, data) = match chunk {
        StreamChunk::Structured(event) => (
            "structured",
            // Serialization failure must not panic across the FFI boundary.
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        StreamChunk::Text(text) => ("text", serde_json::json!({ "text": text })),
        StreamChunk::Thinking(text) => ("thinking", serde_json::json!({ "text": text })),
        StreamChunk::ThinkingSignature(signature) => (
            "thinking_signature",
            serde_json::json!({ "signature": signature }),
        ),
        StreamChunk::ToolUseStart { index, id, name } => (
            "tool_use_start",
            serde_json::json!({ "index": index, "id": id, "name": name }),
        ),
        StreamChunk::ToolUseInputDelta {
            index,
            partial_json,
        } => (
            "tool_use_input_delta",
            serde_json::json!({ "index": index, "partial_json": partial_json }),
        ),
        StreamChunk::ToolUseComplete {
            index, tool_call, ..
        } => (
            "tool_use_complete",
            serde_json::json!({
                "index": index,
                "tool_call": {
                    "id": tool_call.id,
                    "call_type": tool_call.call_type,
                    "function": {
                        "name": tool_call.function.name,
                        "arguments": tool_call.function.arguments,
                    }
                }
            }),
        ),
        StreamChunk::Usage(usage) => (
            "usage",
            serde_json::json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
                "cache_read": usage.cache_read,
                "cache_write": usage.cache_write,
            }),
        ),
        StreamChunk::Done { finish_reason } => (
            "done",
            serde_json::json!({ "finish_reason": finish_reason_to_string(finish_reason) }),
        ),
    };

    PyStreamChunk {
        kind: kind.to_string(),
        data,
    }
}

fn json_to_python<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Bool(v) => Ok(<pyo3::Bound<'_, pyo3::types::PyBool> as Clone>::clone(
            &pyo3::types::PyBool::new(py, *v),
        )
        .into_any()),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                Ok(i.into_pyobject(py)?.into_any())
            } else if let Some(u) = v.as_u64() {
                Ok(u.into_pyobject(py)?.into_any())
            } else if let Some(f) = v.as_f64() {
                Ok(f.into_pyobject(py)?.into_any())
            } else {
                Ok(py.None().into_bound(py))
            }
        }
        Value::String(v) => Ok(v.into_pyobject(py)?.into_any()),
        Value::Array(items) => {
            let out = PyList::empty(py);
            for item in items {
                out.append(json_to_python(py, item)?)?;
            }
            Ok(out.into_any())
        }
        Value::Object(map) => {
            let out = PyDict::new(py);
            for (key, value) in map {
                out.set_item(key, json_to_python(py, value)?)?;
            }
            Ok(out.into_any())
        }
    }
}

async fn default_registry() -> Result<PluginRegistry> {
    if let Err(err) = ::querymt::providers::update_providers_if_stale().await {
        log::warn!("Failed to update providers metadata cache: {}", err);
    }

    let cfg_path = querymt_utils::providers::get_providers_config(None).await?;
    let mut registry = PluginRegistry::from_path(&cfg_path)?;
    registry.register_dynamic_loaders();
    Ok(registry)
}

async fn build_provider(
    registry: &PluginRegistry,
    provider: &str,
    model: &str,
    params: Option<Value>,
    api_key: Option<String>,
    base_url: Option<String>,
) -> Result<Arc<dyn LLMProvider>> {
    let mut builder = LLMBuilder::new().provider(provider).model(model);
    if let Some(params) = params.as_ref() {
        builder = builder.parameters_from_value(params);
    }
    if let Some(api_key) = api_key {
        builder = builder.api_key(api_key);
    }
    if let Some(base_url) = base_url {
        builder = builder.base_url(base_url);
    }
    let provider = builder.build_with(registry).await?;
    Ok(Arc::from(provider))
}

fn python_tools_to_rust(tools: Option<&Bound<'_, PyAny>>) -> Result<Option<Vec<Tool>>> {
    let Some(tools) = tools else {
        return Ok(None);
    };

    let seq = tools
        .cast::<PySequence>()
        .map_err(|_| anyhow!("tools must be a sequence"))?;
    let mut out = Vec::with_capacity(seq.len()?);
    for item in seq.try_iter()? {
        let item = item?;
        let value = python_to_json(&item)?;
        let tool: Tool = serde_json::from_value(value)?;
        out.push(tool);
    }
    Ok(Some(out))
}

fn py_messages_to_rust(messages: &Bound<'_, PyAny>) -> Result<Vec<ChatMessage>> {
    let seq = messages
        .cast::<PySequence>()
        .map_err(|_| anyhow!("messages must be a sequence"))?;
    let mut out = Vec::with_capacity(seq.len()?);
    for item in seq.try_iter()? {
        let item = item?;
        let dict = item
            .cast::<PyDict>()
            .map_err(|_| anyhow!("each message must be a dict"))?;

        out.push(py_message_to_rust(dict)?);
    }
    Ok(out)
}

fn py_message_to_rust(message: &Bound<'_, PyDict>) -> Result<ChatMessage> {
    let role = message
        .get_item("role")?
        .ok_or_else(|| anyhow!("message.role is required"))?
        .extract::<String>()?;

    // Optional canonical structured output, passed through as JSON so Python
    // callers can replay item-aware history losslessly. An assistant turn
    // carries exactly one authoritative payload, so `output` takes precedence
    // over any portable `content` projection.
    let output = match message.get_item("output")? {
        Some(value) if !value.is_none() => {
            let json = python_to_json(&value)?;
            Some(
                serde_json::from_value::<::querymt::chat::ChatOutput>(json)
                    .map_err(|e| anyhow!("message.output is not valid structured output: {e}"))?,
            )
        }
        _ => None,
    };

    let input = match message.get_item("input")? {
        Some(value) if !value.is_none() => {
            let json = python_to_json(&value)?;
            Some(
                serde_json::from_value::<Vec<ChatInputPart>>(json)
                    .map_err(|e| anyhow!("message.input is not valid canonical input: {e}"))?,
            )
        }
        _ => None,
    };
    let content = message.get_item("content")?;

    if output.is_some() && (input.is_some() || content.is_some()) {
        return Err(anyhow!(
            "message cannot mix canonical output with input or legacy content"
        ));
    }
    if input.is_some() && content.is_some() {
        return Err(anyhow!(
            "message cannot mix canonical input with legacy content"
        ));
    }

    match role.as_str() {
        "assistant" => {
            if let Some(output) = output {
                return ChatMessage::try_from_assistant_output(output).map_err(Into::into);
            }
            if let Some(input) = input {
                return Ok(ChatMessage::from_user_parts(input).with_role(ChatRole::Assistant));
            }
            let content = content.ok_or_else(|| anyhow!("assistant message requires output"))?;
            let input_parts = py_content_to_rust(&content)?;
            Ok(ChatMessage::from_user_parts(input_parts).with_role(ChatRole::Assistant))
        }
        "user" => {
            if output.is_some() {
                return Err(anyhow!("user messages cannot carry structured output"));
            }
            if let Some(input) = input {
                return Ok(ChatMessage::from_user_parts(input));
            }
            let content = content.ok_or_else(|| anyhow!("user message requires input"))?;
            let parts = py_content_to_rust(&content)?;
            Ok(ChatMessage::from_user_parts(parts))
        }
        "tool" => {
            if output.is_some() {
                return Err(anyhow!("tool messages cannot carry structured output"));
            }
            if let Some(input) = input {
                return Ok(ChatMessage::from_user_parts(input).with_role(ChatRole::Assistant));
            }
            let content = content.ok_or_else(|| anyhow!("tool message requires input"))?;
            let parts = py_content_to_rust(&content)?;
            Ok(ChatMessage::from_user_parts(parts).with_role(ChatRole::Assistant))
        }
        other => Err(anyhow!("unsupported role '{}'", other)),
    }
}

/// Read a legacy Python `content` value into canonical input parts.
fn py_content_to_rust(content: &Bound<'_, PyAny>) -> Result<Vec<ChatInputPart>> {
    if let Ok(text) = content.extract::<String>() {
        return Ok(vec![ChatInputPart::text(text)]);
    }

    if let Ok(blocks) = content.cast::<PyList>() {
        let mut parts = Vec::new();
        for item in blocks.iter() {
            let dict = item
                .cast::<PyDict>()
                .map_err(|_| anyhow!("each content block must be a dict"))?;
            parts.push(py_block_to_rust(&dict)?);
        }
        return Ok(parts);
    }

    Err(anyhow!(
        "message.content must be a string or a list of content block dicts"
    ))
}

fn py_block_to_rust(block: &Bound<'_, PyDict>) -> Result<ChatInputPart> {
    let kind = block
        .get_item("type")?
        .ok_or_else(|| anyhow!("content block type is required"))?
        .extract::<String>()?;

    match kind.as_str() {
        "text" => Ok(ChatInputPart::text(
            block
                .get_item("text")?
                .ok_or_else(|| anyhow!("text block requires 'text'"))?
                .extract::<String>()?,
        )),
        "image" => Ok(inline_media_part(
            MediaKind::Image,
            &block
                .get_item("mime_type")?
                .ok_or_else(|| anyhow!("image block requires 'mime_type'"))?
                .extract::<String>()?,
            decode_bytes(block, "data")?,
        )?),
        "image_url" => Ok(url_media_part(
            MediaKind::Image,
            block
                .get_item("url")?
                .ok_or_else(|| anyhow!("image_url block requires 'url'"))?
                .extract::<String>()?,
            None,
        )?),
        "pdf" => Ok(inline_media_part(
            MediaKind::Document,
            "application/pdf",
            decode_bytes(block, "data")?,
        )?),
        "audio" => Ok(inline_media_part(
            MediaKind::Audio,
            &block
                .get_item("mime_type")?
                .ok_or_else(|| anyhow!("audio block requires 'mime_type'"))?
                .extract::<String>()?,
            decode_bytes(block, "data")?,
        )?),
        "resource_link" => Ok(url_media_part(
            MediaKind::Other,
            block
                .get_item("uri")?
                .ok_or_else(|| anyhow!("resource_link block requires 'uri'"))?
                .extract::<String>()?,
            optional_string(block, "name")?,
        )?),
        // Generated semantics: no ordinary-input representation. Rejected in a
        // plain `content` list and only meaningful via `output`.
        "thinking" | "tool_use" => Err(anyhow!(
            "'{kind}' is generated content and cannot appear in message.content; \
             pass it in the assistant message's structured 'output' instead"
        )),
        "tool_result" => py_tool_result_to_rust(block),
        other => Err(anyhow!("unsupported content block type '{}'", other)),
    }
}

/// Convert a `tool_result` block into a bounded correlated input part.
///
/// The inner parts are [`ToolResultPart`]s, which cannot nest another result, so
/// nesting is rejected explicitly instead of recursing.
fn py_tool_result_to_rust(block: &Bound<'_, PyDict>) -> Result<ChatInputPart> {
    let mut result = ::querymt::chat::ToolResult::new(
        block
            .get_item("id")?
            .ok_or_else(|| anyhow!("tool_result block requires 'id'"))?
            .extract::<String>()?,
    );
    result.name = optional_string(block, "name")?;
    result.is_error = optional_bool(block, "is_error")?.unwrap_or(false);

    let content = block
        .get_item("content")?
        .ok_or_else(|| anyhow!("tool_result block requires 'content'"))?;

    if let Ok(text) = content.extract::<String>() {
        result
            .parts
            .push(::querymt::chat::ToolResultPart::text(text));
        return Ok(ChatInputPart::tool_result(result));
    }

    let blocks = content
        .cast::<PyList>()
        .map_err(|_| anyhow!("tool_result content must be a string or a list of block dicts"))?;

    for item in blocks.iter() {
        let dict = item
            .cast::<PyDict>()
            .map_err(|_| anyhow!("each tool_result content block must be a dict"))?;
        let kind = block_type_name(&dict)?;
        match kind.as_str() {
            "text" => result.parts.push(::querymt::chat::ToolResultPart::text(
                dict.get_item("text")?
                    .ok_or_else(|| anyhow!("text block requires 'text'"))?
                    .extract::<String>()?,
            )),
            "image" | "pdf" | "audio" => {
                let (kind, mime) = match kind.as_str() {
                    "image" => (
                        MediaKind::Image,
                        Some(
                            dict.get_item("mime_type")?
                                .ok_or_else(|| anyhow!("image block requires 'mime_type'"))?
                                .extract::<String>()?,
                        ),
                    ),
                    "pdf" => (MediaKind::Document, Some("application/pdf".to_string())),
                    _ => (
                        MediaKind::Audio,
                        Some(
                            dict.get_item("mime_type")?
                                .ok_or_else(|| anyhow!("audio block requires 'mime_type'"))?
                                .extract::<String>()?,
                        ),
                    ),
                };
                result
                    .parts
                    .push(::querymt::chat::ToolResultPart::Attachment(Box::new(
                        block_attachment(kind, mime, decode_bytes(&dict, "data")?)?,
                    )));
            }
            other => {
                return Err(anyhow!(
                    "tool_result content cannot contain '{other}' blocks; \
                     a tool result holds only text and media parts"
                ));
            }
        }
    }

    Ok(ChatInputPart::tool_result(result))
}

/// Build a validated inline media input part.
fn inline_media_part(kind: MediaKind, mime_type: &str, data: Vec<u8>) -> Result<ChatInputPart> {
    Ok(ChatInputPart::attachment(block_attachment(
        kind,
        Some(mime_type.to_string()),
        data,
    )?))
}

/// Build a validated URL-sourced media input part.
fn url_media_part(kind: MediaKind, url: String, filename: Option<String>) -> Result<ChatInputPart> {
    let mut media =
        ::querymt::chat::MediaPart::new(kind, None, ::querymt::chat::MediaSource::Url { url })
            .map_err(|e| anyhow!("invalid media attachment: {e}"))?;
    media.filename = filename;
    Ok(ChatInputPart::attachment(media))
}

/// Build a validated inline attachment.
fn block_attachment(
    kind: MediaKind,
    mime_type: Option<String>,
    data: Vec<u8>,
) -> Result<::querymt::chat::MediaPart> {
    let media_type = mime_type
        .map(|mime| mime.parse::<::querymt::chat::MediaType>())
        .transpose()
        .map_err(|e| anyhow!("invalid media type: {e}"))?;
    ::querymt::chat::MediaPart::new(
        kind,
        media_type,
        ::querymt::chat::MediaSource::Inline { data },
    )
    .map_err(|e| anyhow!("invalid media attachment: {e}"))
}

fn block_type_name(block: &Bound<'_, PyDict>) -> Result<String> {
    block
        .get_item("type")?
        .ok_or_else(|| anyhow!("content block type is required"))?
        .extract::<String>()
        .map_err(Into::into)
}

fn optional_string(block: &Bound<'_, PyDict>, key: &str) -> Result<Option<String>> {
    block
        .get_item(key)?
        .map(|v| v.extract::<String>())
        .transpose()
        .map_err(Into::into)
}

fn optional_bool(block: &Bound<'_, PyDict>, key: &str) -> Result<Option<bool>> {
    block
        .get_item(key)?
        .map(|v| v.extract::<bool>())
        .transpose()
        .map_err(Into::into)
}

fn decode_bytes(block: &Bound<'_, PyDict>, key: &str) -> Result<Vec<u8>> {
    let block_type = block_type_name(block)?;
    let value = block
        .get_item(key)?
        .ok_or_else(|| anyhow!("{} block requires '{}'", block_type, key))?;

    if let Ok(data) = value.extract::<Vec<u8>>() {
        return Ok(data);
    }

    let encoded = value.extract::<String>()?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| anyhow!("invalid base64 for '{}': {}", key, err))
}

fn python_opt_to_json(value: Option<&Bound<'_, PyAny>>) -> Result<Option<Value>> {
    value.map(python_to_json).transpose()
}

fn python_to_json(value: &Bound<'_, PyAny>) -> Result<Value> {
    if value.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(v) = value.extract::<bool>() {
        return Ok(Value::Bool(v));
    }
    if let Ok(v) = value.extract::<i64>() {
        return Ok(Value::Number(v.into()));
    }
    if let Ok(v) = value.extract::<f64>() {
        return Ok(Value::Number(
            Number::from_f64(v).ok_or_else(|| anyhow!("invalid float value"))?,
        ));
    }
    if let Ok(v) = value.extract::<String>() {
        return Ok(Value::String(v));
    }
    if let Ok(list) = value.cast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(python_to_json(&item)?);
        }
        return Ok(Value::Array(out));
    }
    if let Ok(dict) = value.cast::<PyDict>() {
        let mut out = Map::new();
        for (k, v) in dict.iter() {
            out.insert(k.extract::<String>()?, python_to_json(&v)?);
        }
        return Ok(Value::Object(out));
    }
    Err(anyhow!("value is not JSON-serializable"))
}

fn into_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[pyfunction]
#[pyo3(signature = (input))]
fn user_message<'py>(py: Python<'py>, input: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    message_dict(py, "user", "input", input.bind(py))
}

#[pyfunction]
#[pyo3(signature = (output))]
fn assistant_message<'py>(py: Python<'py>, output: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    message_dict(py, "assistant", "output", output.bind(py))
}

#[pyfunction]
#[pyo3(signature = (text))]
fn text_part<'py>(py: Python<'py>, text: String) -> PyResult<Bound<'py, PyDict>> {
    block_dict(py, [("type", "text"), ("text", &text)])
}

#[pyfunction]
#[pyo3(signature = (kind, mime_type, data, filename=None, detail=None))]
fn inline_attachment<'py>(
    py: Python<'py>,
    kind: String,
    mime_type: String,
    data: Py<PyAny>,
    filename: Option<String>,
    detail: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let source = PyDict::new(py);
    source.set_item("type", "inline")?;
    source.set_item("data", data.bind(py).extract::<Vec<u8>>()?)?;
    attachment_part(py, kind, Some(mime_type), source.as_any(), filename, detail)
}

#[pyfunction]
#[pyo3(signature = (kind, url, media_type=None, filename=None, detail=None))]
fn url_attachment<'py>(
    py: Python<'py>,
    kind: String,
    url: String,
    media_type: Option<String>,
    filename: Option<String>,
    detail: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let source = PyDict::new(py);
    source.set_item("type", "url")?;
    source.set_item("url", url)?;
    attachment_part(py, kind, media_type, source.as_any(), filename, detail)
}

#[pyfunction]
#[pyo3(signature = (call_id, parts, name=None, is_error=false))]
fn tool_result<'py>(
    py: Python<'py>,
    call_id: String,
    parts: Py<PyAny>,
    name: Option<String>,
    is_error: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let part = PyDict::new(py);
    part.set_item("type", "tool_result")?;
    part.set_item("call_id", call_id)?;
    if let Some(name) = name {
        part.set_item("name", name)?;
    }
    part.set_item("is_error", is_error)?;
    part.set_item("parts", parts.bind(py))?;
    Ok(part)
}

#[pyfunction]
#[pyo3(signature = (name, description, parameters, tool_type="function".to_string()))]
fn function_tool<'py>(
    py: Python<'py>,
    name: String,
    description: String,
    parameters: Py<PyAny>,
    tool_type: String,
) -> PyResult<Bound<'py, PyDict>> {
    let function = PyDict::new(py);
    function.set_item("name", name)?;
    function.set_item("description", description)?;
    function.set_item("parameters", parameters.bind(py))?;

    let tool = PyDict::new(py);
    tool.set_item("type", tool_type)?;
    tool.set_item("function", function)?;
    Ok(tool)
}

fn message_dict<'py>(
    py: Python<'py>,
    role: &str,
    payload_key: &str,
    payload: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let msg = PyDict::new(py);
    msg.set_item("role", role)?;
    msg.set_item(payload_key, payload)?;
    Ok(msg)
}

fn block_dict<'py, const N: usize>(
    py: Python<'py>,
    entries: [(&str, &str); N],
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    for (key, value) in entries {
        block.set_item(key, value)?;
    }
    Ok(block)
}

fn attachment_part<'py>(
    py: Python<'py>,
    kind: String,
    media_type: Option<String>,
    source: &Bound<'py, PyAny>,
    filename: Option<String>,
    detail: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let part = PyDict::new(py);
    part.set_item("type", "attachment")?;
    part.set_item("kind", kind)?;
    if let Some(media_type) = media_type {
        part.set_item("media_type", media_type)?;
    }
    part.set_item("source", source)?;
    if let Some(filename) = filename {
        part.set_item("filename", filename)?;
    }
    if let Some(detail) = detail {
        part.set_item("detail", detail)?;
    }
    Ok(part)
}

#[pymodule]
fn querymt(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyRegistry>()?;
    module.add_class::<PyProvider>()?;
    module.add_class::<PyChatStream>()?;
    module.add_class::<PyMeshRuntime>()?;
    module.add_class::<PyProviderShare>()?;
    module.add_class::<PyChatResponse>()?;
    module.add_class::<PyUsage>()?;
    module.add_class::<PyToolCall>()?;
    module.add_class::<PyStreamChunk>()?;
    module.add_function(wrap_pyfunction!(user_message, module)?)?;
    module.add_function(wrap_pyfunction!(assistant_message, module)?)?;
    module.add_function(wrap_pyfunction!(text_part, module)?)?;
    module.add_function(wrap_pyfunction!(inline_attachment, module)?)?;
    module.add_function(wrap_pyfunction!(url_attachment, module)?)?;
    module.add_function(wrap_pyfunction!(tool_result, module)?)?;
    module.add_function(wrap_pyfunction!(function_tool, module)?)?;
    module.add(
        "__all__",
        vec![
            "Registry",
            "Provider",
            "ChatStream",
            "MeshRuntime",
            "ProviderShare",
            "ChatResponse",
            "Usage",
            "ToolCall",
            "StreamChunk",
            "user_message",
            "assistant_message",
            "text_part",
            "inline_attachment",
            "url_attachment",
            "tool_result",
            "function_tool",
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::Python;
    use pyo3::types::PyDict;
    use std::sync::Once;

    fn with_python(f: impl for<'py> FnOnce(Python<'py>)) {
        static INIT: Once = Once::new();
        INIT.call_once(Python::initialize);
        Python::attach(f);
    }

    #[test]
    fn converts_string_content_message() {
        with_python(|py| {
            let msg = PyDict::new(py);
            msg.set_item("role", "user").unwrap();
            msg.set_item("content", "hello").unwrap();
            let out = py_message_to_rust(&msg).unwrap();
            assert_eq!(out.role, ChatRole::User);
            assert_eq!(out.text(), "hello");

            let saved = serde_json::to_value(&out).unwrap();
            assert!(saved.get("content").is_none());
            assert_eq!(saved["input"][0]["type"], "text");
            assert_eq!(saved["input"][0]["text"], "hello");
        });
    }

    #[test]
    fn converts_block_content_message() {
        with_python(|py| {
            let msg = PyDict::new(py);
            let block = PyDict::new(py);
            block.set_item("type", "text").unwrap();
            block.set_item("text", "hello").unwrap();
            let blocks = PyList::new(py, [block]).unwrap();
            msg.set_item("role", "assistant").unwrap();
            msg.set_item("content", blocks).unwrap();
            let out = py_message_to_rust(&msg).unwrap();
            assert_eq!(out.role, ChatRole::Assistant);
            assert_eq!(out.text(), "hello");
        });
    }

    #[test]
    fn converts_tool_result_with_nested_blocks() {
        with_python(|py| {
            let msg = PyDict::new(py);
            let block = PyDict::new(py);
            let nested = PyDict::new(py);
            nested.set_item("type", "text").unwrap();
            nested.set_item("text", "tool output").unwrap();
            block.set_item("type", "tool_result").unwrap();
            block.set_item("id", "call-1").unwrap();
            block.set_item("name", "lookup").unwrap();
            block.set_item("is_error", true).unwrap();
            block
                .set_item("content", PyList::new(py, [nested]).unwrap())
                .unwrap();
            msg.set_item("role", "tool").unwrap();
            msg.set_item("content", PyList::new(py, [block]).unwrap())
                .unwrap();
            let out = py_message_to_rust(&msg).unwrap();
            assert_eq!(out.role, ChatRole::Assistant);
            assert!(out.has_tool_result());

            let saved = serde_json::to_value(&out).unwrap();
            assert!(saved.get("content").is_none());
            assert_eq!(saved["input"][0]["type"], "tool_result");
            assert_eq!(saved["input"][0]["parts"][0]["type"], "text");
        });
    }

    #[test]
    fn converts_binary_content_from_base64() {
        with_python(|py| {
            let image = PyDict::new(py);
            image.set_item("type", "image").unwrap();
            image.set_item("mime_type", "image/png").unwrap();
            image.set_item("data", "aGVsbG8=").unwrap();
            let content = py_block_to_rust(&image).unwrap();
            match content {
                ChatInputPart::Attachment(media) => {
                    let ::querymt::chat::MediaSource::Inline { data } = media.source() else {
                        panic!("expected inline media");
                    };
                    assert_eq!(media.media_type().map(|m| m.as_ref()), Some("image/png"));
                    assert_eq!(data, b"hello");
                }
                _ => panic!("expected an inline image input part"),
            }
        });
    }

    #[test]
    fn converts_stream_chunk_to_python() {
        let chunk = stream_chunk_to_python(StreamChunk::ToolUseStart {
            index: 2,
            id: "call-1".to_string(),
            name: "lookup".to_string(),
        });
        assert_eq!(chunk.kind, "tool_use_start");
        assert_eq!(chunk.data["index"], 2);
        assert_eq!(chunk.data["id"], "call-1");
        assert_eq!(chunk.data["name"], "lookup");
    }

    #[test]
    fn converts_python_tools_to_rust() {
        with_python(|py| {
            let params = PyDict::new(py);
            params.set_item("type", "object").unwrap();
            params.set_item("properties", PyDict::new(py)).unwrap();
            params.set_item("required", PyList::empty(py)).unwrap();

            let tool = function_tool(
                py,
                "lookup_weather".to_string(),
                "Look up weather".to_string(),
                params.into_any().unbind(),
                "function".to_string(),
            )
            .unwrap();
            let tools = PyList::empty(py);
            tools.append(tool).unwrap();

            let parsed = python_tools_to_rust(Some(&tools.into_any()))
                .unwrap()
                .unwrap();
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].function.name, "lookup_weather");
        });
    }
}
