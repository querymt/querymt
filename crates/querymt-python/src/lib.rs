use ::querymt::chat::{ChatMessage, ChatRole, Content, FinishReason, StreamChunk, Tool};
use base64::Engine;
use ::querymt::dynamic::PluginRegistryDynamicExt;
use ::querymt::plugin::host::PluginRegistry;
use ::querymt::{LLMBuilder, LLMProvider, ToolCall, Usage};
use anyhow::{Result, anyhow};
use pyo3::exceptions::{PyRuntimeError, PyStopAsyncIteration};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PySequence};
use querymt_remote::{
    LanDiscovery, LanMeshConfig, MeshChatProvider, MeshRuntimeConfig, MeshRuntimeHandle,
    ModelAllowlistBackend, ProviderShare, RegistryProviderBackend, StaticCatalogBackend,
    bootstrap_mesh_runtime, find_provider_on_mesh,
};
use futures_util::StreamExt;
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
    #[pyo3(get)]
    content: Vec<PyContentBlock>,
}

#[pyclass(name = "Usage")]
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

#[pyclass(name = "ToolCall")]
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

#[pyclass(name = "ContentBlock")]
#[derive(Clone)]
struct PyContentBlock {
    #[pyo3(get)]
    kind: String,
    data: Value,
}

#[pyclass(name = "StreamChunk")]
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
            Python::with_gil(|py| Py::new(py, PyRegistry { inner: Arc::new(registry) }))
        })
    }

    #[staticmethod]
    fn from_path<'py>(py: Python<'py>, path: String) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut registry = PluginRegistry::from_path(&path).map_err(into_py_err)?;
            registry.register_dynamic_loaders();
            Python::with_gil(|py| Py::new(py, PyRegistry { inner: Arc::new(registry) }))
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
            Python::with_gil(|py| Ok(py.None()))
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
            Python::with_gil(|py| Ok(models.into_pyobject(py)?.into_any().unbind()))
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
        let params_json = python_opt_to_json(params.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let provider = build_provider(&registry, &provider, &model, params_json, api_key, base_url)
                .await
                .map_err(into_py_err)?;
            Python::with_gil(|py| Py::new(py, PyProvider { inner: provider }))
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
            let response = chat_response_to_python(response.as_ref());
            Python::with_gil(|py| Py::new(py, response))
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
        let tools = python_tools_to_rust(tools.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = provider
                .chat_with_tools(&messages, tools.as_deref())
                .await
                .map_err(into_py_err)?;
            let response = chat_response_to_python(response.as_ref());
            Python::with_gil(|py| Py::new(py, response))
        })
    }

    fn chat_stream<'py>(&self, py: Python<'py>, messages: Py<PyAny>) -> PyResult<Bound<'py, PyAny>> {
        let provider = Arc::clone(&self.inner);
        let messages = py_messages_to_rust(messages.bind(py)).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let stream = provider.chat_stream(&messages).await.map_err(into_py_err)?;
            let stream = stream_to_python(stream);
            Python::with_gil(|py| Py::new(py, stream))
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
        let tools = python_tools_to_rust(tools.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let stream = provider
                .chat_stream_with_tools(&messages, tools.as_deref())
                .await
                .map_err(into_py_err)?;
            let stream = stream_to_python(stream);
            Python::with_gil(|py| Py::new(py, stream))
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
            Python::with_gil(|py| Py::new(py, PyMeshRuntime { inner: runtime }))
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
            let backend = ModelAllowlistBackend::new(RegistryProviderBackend::new(Arc::clone(&registry)))
                .allow_models(provider.clone(), allowed_models.clone());
            let catalog = StaticCatalogBackend::provider_models(
                runtime.peer_id().to_string(),
                label,
                provider,
                allowed_models,
            );
            let share = ProviderShare::new(Arc::new(backend), Arc::new(catalog));
            share.register_on_mesh(&runtime).await;
            Python::with_gil(|py| {
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
        let params_json = python_opt_to_json(params.as_ref().map(|value| value.bind(py))).map_err(into_py_err)?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let node_id = find_provider_on_mesh(runtime.as_mesh_handle(), &provider)
                .await
                .ok_or_else(|| anyhow!("provider '{}' not found on mesh", provider))
                .map_err(into_py_err)?;
            let provider = Arc::new(
                MeshChatProvider::from_node_id(runtime.as_mesh_handle(), &node_id, &provider, &model)
                    .with_params(params_json),
            ) as Arc<dyn LLMProvider>;
            Python::with_gil(|py| Py::new(py, PyProvider { inner: provider }))
        })
    }
}

#[pymethods]
impl PyProviderShare {
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            future::pending::<()>().await;
            #[allow(unreachable_code)]
            Python::with_gil(|py| Ok(py.None()))
        })
    }
}

#[pymethods]
impl PyChatResponse {
    fn __str__(&self) -> String {
        self.text.clone().unwrap_or_default()
    }
}

#[pymethods]
impl PyContentBlock {
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_python(py, &self.data)
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
                Some(Ok(chunk)) => Python::with_gil(|py| Py::new(py, chunk)),
                Some(Err(err)) => Err(PyRuntimeError::new_err(err)),
                None => Err(PyStopAsyncIteration::new_err("stream ended")),
            }
        })
    }
}

fn chat_response_to_python(response: &dyn ::querymt::chat::ChatResponse) -> PyChatResponse {
    let message = ChatMessage::from(response);
    PyChatResponse {
        text: response.text(),
        thinking: response.thinking(),
        finish_reason: response.finish_reason().map(finish_reason_to_string),
        usage: response.usage().map(usage_to_python),
        tool_calls: response
            .tool_calls()
            .unwrap_or_default()
            .into_iter()
            .map(tool_call_to_python)
            .collect(),
        content: message
            .content
            .iter()
            .map(content_block_to_python)
            .collect(),
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

fn content_block_to_python(content: &Content) -> PyContentBlock {
    let data = serde_json::to_value(content)
        .unwrap_or_else(|_| Value::String(format!("{content}")));
    let kind = match content {
        Content::Text { .. } => "text",
        Content::Image { .. } => "image",
        Content::ImageUrl { .. } => "image_url",
        Content::Pdf { .. } => "pdf",
        Content::Audio { .. } => "audio",
        Content::Thinking { .. } => "thinking",
        Content::ToolUse { .. } => "tool_use",
        Content::ToolResult { .. } => "tool_result",
        Content::ResourceLink { .. } => "resource_link",
    }
    .to_string();

    PyContentBlock { kind, data }
}

fn stream_to_python(
    mut stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<StreamChunk, ::querymt::error::LLMError>> + Send>,
    >,
) -> PyChatStream {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            let mapped = item.map(stream_chunk_to_python).map_err(|err| err.to_string());
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
        StreamChunk::ToolUseInputDelta { index, partial_json } => (
            "tool_use_input_delta",
            serde_json::json!({ "index": index, "partial_json": partial_json }),
        ),
        StreamChunk::ToolUseComplete { index, tool_call } => (
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
        Value::Bool(v) => Ok(<pyo3::Bound<'_, pyo3::types::PyBool> as Clone>::clone(&pyo3::types::PyBool::new(py, *v)).into_any()),
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
        .downcast::<PySequence>()
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
        .downcast::<PySequence>()
        .map_err(|_| anyhow!("messages must be a sequence"))?;
    let mut out = Vec::with_capacity(seq.len()?);
    for item in seq.try_iter()? {
        let item = item?;
        let dict = item
            .downcast::<PyDict>()
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
    let content = message
        .get_item("content")?
        .ok_or_else(|| anyhow!("message.content is required"))?;
    let blocks = py_content_to_rust(&content)?;

    match role.as_str() {
        "user" => Ok(ChatMessage::from_user(blocks)),
        "assistant" => Ok(ChatMessage::from_assistant(blocks)),
        "tool" => Ok(ChatMessage {
            role: ChatRole::Assistant,
            content: blocks,
            cache: None,
        }),
        other => Err(anyhow!("unsupported role '{}'", other)),
    }
}

fn py_content_to_rust(content: &Bound<'_, PyAny>) -> Result<Vec<Content>> {
    if let Ok(text) = content.extract::<String>() {
        return Ok(vec![Content::text(text)]);
    }

    if let Ok(blocks) = content.downcast::<PyList>() {
        let mut out = Vec::with_capacity(blocks.len());
        for item in blocks.iter() {
            let dict = item
                .downcast::<PyDict>()
                .map_err(|_| anyhow!("each content block must be a dict"))?;


            out.push(py_block_to_rust(&dict)?);
        }
        return Ok(out);
    }

    Err(anyhow!(
        "message.content must be a string or a list of content block dicts"
    ))
}

fn py_block_to_rust(block: &Bound<'_, PyDict>) -> Result<Content> {
    let kind = block
        .get_item("type")?
        .ok_or_else(|| anyhow!("content block type is required"))?
        .extract::<String>()?;

    match kind.as_str() {
        "text" => Ok(Content::text(
            block
                .get_item("text")?
                .ok_or_else(|| anyhow!("text block requires 'text'"))?
                .extract::<String>()?,
        )),
        "thinking" => Ok(Content::Thinking {
            text: block
                .get_item("text")?
                .ok_or_else(|| anyhow!("thinking block requires 'text'"))?
                .extract::<String>()?,
            signature: optional_string(block, "signature")?,
        }),
        "image" => Ok(Content::image(
            block
                .get_item("mime_type")?
                .ok_or_else(|| anyhow!("image block requires 'mime_type'"))?
                .extract::<String>()?,
            decode_bytes(block, "data")?,
        )),
        "image_url" => Ok(Content::image_url(
            block
                .get_item("url")?
                .ok_or_else(|| anyhow!("image_url block requires 'url'"))?
                .extract::<String>()?,
        )),
        "pdf" => Ok(Content::pdf(decode_bytes(block, "data")?)),
        "audio" => Ok(Content::audio(
            block
                .get_item("mime_type")?
                .ok_or_else(|| anyhow!("audio block requires 'mime_type'"))?
                .extract::<String>()?,
            decode_bytes(block, "data")?,
        )),
        "tool_use" => {
            let args = block
                .get_item("arguments")?
                .ok_or_else(|| anyhow!("tool_use block requires 'arguments'"))?;
            Ok(Content::tool_use(
                block
                    .get_item("id")?
                    .ok_or_else(|| anyhow!("tool_use block requires 'id'"))?
                    .extract::<String>()?,
                block
                    .get_item("name")?
                    .ok_or_else(|| anyhow!("tool_use block requires 'name'"))?
                    .extract::<String>()?,
                python_to_json(&args)?,
            ))
        }
        "tool_result" => Ok(Content::ToolResult {
            id: block
                .get_item("id")?
                .ok_or_else(|| anyhow!("tool_result block requires 'id'"))?
                .extract::<String>()?,
            name: optional_string(block, "name")?,
            is_error: optional_bool(block, "is_error")?.unwrap_or(false),
            content: py_nested_content(block, "content")?,
        }),
        "resource_link" => Ok(Content::ResourceLink {
            uri: block
                .get_item("uri")?
                .ok_or_else(|| anyhow!("resource_link block requires 'uri'"))?
                .extract::<String>()?,
            name: optional_string(block, "name")?,
            description: optional_string(block, "description")?,
            mime_type: optional_string(block, "mime_type")?,
        }),
        other => Err(anyhow!("unsupported content block type '{}'", other)),
    }
}

fn py_nested_content(block: &Bound<'_, PyDict>, key: &str) -> Result<Vec<Content>> {
    let block_type = block_type_name(block)?;
    let value = block
        .get_item(key)?
        .ok_or_else(|| anyhow!("{} block requires '{}'", block_type, key))?;
    py_content_to_rust(&value)
}

fn block_type_name(block: &Bound<'_, PyDict>) -> Result<String> {
    block
        .get_item("type")?
        .ok_or_else(|| anyhow!("content block type is required"))?
        .extract::<String>()
        .map_err(Into::into)
}

fn optional_string(block: &Bound<'_, PyDict>, key: &str) -> Result<Option<String>> {
    block.get_item(key)?.map(|v| v.extract::<String>()).transpose().map_err(Into::into)
}

fn optional_bool(block: &Bound<'_, PyDict>, key: &str) -> Result<Option<bool>> {
    block.get_item(key)?.map(|v| v.extract::<bool>()).transpose().map_err(Into::into)
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
    if let Ok(list) = value.downcast::<PyList>() {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            out.push(python_to_json(&item)?);
        }
        return Ok(Value::Array(out));
    }
    if let Ok(dict) = value.downcast::<PyDict>() {
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
#[pyo3(signature = (content))]
fn user_message<'py>(py: Python<'py>, content: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    message_dict(py, "user", content.bind(py))
}

#[pyfunction]
#[pyo3(signature = (content))]
fn assistant_message<'py>(py: Python<'py>, content: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    message_dict(py, "assistant", content.bind(py))
}

#[pyfunction]
#[pyo3(signature = (text))]
fn text_block<'py>(py: Python<'py>, text: String) -> PyResult<Bound<'py, PyDict>> {
    block_dict(py, [("type", "text"), ("text", &text)])
}

#[pyfunction]
#[pyo3(signature = (text, signature=None))]
fn thinking_block<'py>(
    py: Python<'py>,
    text: String,
    signature: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", "thinking")?;
    block.set_item("text", text)?;
    if let Some(signature) = signature {
        block.set_item("signature", signature)?;
    }
    Ok(block)
}

#[pyfunction]
#[pyo3(signature = (mime_type, data))]
fn image_block<'py>(py: Python<'py>, mime_type: String, data: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    binary_block(py, "image", mime_type, data.bind(py))
}

#[pyfunction]
#[pyo3(signature = (url))]
fn image_url_block<'py>(py: Python<'py>, url: String) -> PyResult<Bound<'py, PyDict>> {
    block_dict(py, [("type", "image_url"), ("url", &url)])
}

#[pyfunction]
#[pyo3(signature = (data))]
fn pdf_block<'py>(py: Python<'py>, data: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", "pdf")?;
    block.set_item("data", data.bind(py))?;
    Ok(block)
}

#[pyfunction]
#[pyo3(signature = (mime_type, data))]
fn audio_block<'py>(py: Python<'py>, mime_type: String, data: Py<PyAny>) -> PyResult<Bound<'py, PyDict>> {
    binary_block(py, "audio", mime_type, data.bind(py))
}

#[pyfunction]
#[pyo3(signature = (id, name, arguments))]
fn tool_use_block<'py>(
    py: Python<'py>,
    id: String,
    name: String,
    arguments: Py<PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", "tool_use")?;
    block.set_item("id", id)?;
    block.set_item("name", name)?;
    block.set_item("arguments", arguments.bind(py))?;
    Ok(block)
}

#[pyfunction]
#[pyo3(signature = (id, content, name=None, is_error=false))]
fn tool_result_block<'py>(
    py: Python<'py>,
    id: String,
    content: Py<PyAny>,
    name: Option<String>,
    is_error: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", "tool_result")?;
    block.set_item("id", id)?;
    if let Some(name) = name {
        block.set_item("name", name)?;
    }
    block.set_item("is_error", is_error)?;
    block.set_item("content", content.bind(py))?;
    Ok(block)
}

#[pyfunction]
#[pyo3(signature = (uri, name=None, description=None, mime_type=None))]
fn resource_link_block<'py>(
    py: Python<'py>,
    uri: String,
    name: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", "resource_link")?;
    block.set_item("uri", uri)?;
    if let Some(name) = name {
        block.set_item("name", name)?;
    }
    if let Some(description) = description {
        block.set_item("description", description)?;
    }
    if let Some(mime_type) = mime_type {
        block.set_item("mime_type", mime_type)?;
    }
    Ok(block)
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

fn message_dict<'py>(py: Python<'py>, role: &str, content: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let msg = PyDict::new(py);
    msg.set_item("role", role)?;
    msg.set_item("content", content)?;
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

fn binary_block<'py>(
    py: Python<'py>,
    block_type: &str,
    mime_type: String,
    data: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyDict>> {
    let block = PyDict::new(py);
    block.set_item("type", block_type)?;
    block.set_item("mime_type", mime_type)?;
    block.set_item("data", data)?;
    Ok(block)
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
    module.add_class::<PyContentBlock>()?;
    module.add_class::<PyStreamChunk>()?;
    module.add_function(wrap_pyfunction!(user_message, module)?)?;
    module.add_function(wrap_pyfunction!(assistant_message, module)?)?;
    module.add_function(wrap_pyfunction!(text_block, module)?)?;
    module.add_function(wrap_pyfunction!(thinking_block, module)?)?;
    module.add_function(wrap_pyfunction!(image_block, module)?)?;
    module.add_function(wrap_pyfunction!(image_url_block, module)?)?;
    module.add_function(wrap_pyfunction!(pdf_block, module)?)?;
    module.add_function(wrap_pyfunction!(audio_block, module)?)?;
    module.add_function(wrap_pyfunction!(tool_use_block, module)?)?;
    module.add_function(wrap_pyfunction!(tool_result_block, module)?)?;
    module.add_function(wrap_pyfunction!(resource_link_block, module)?)?;
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
            "ContentBlock",
            "StreamChunk",
            "user_message",
            "assistant_message",
            "text_block",
            "thinking_block",
            "image_block",
            "image_url_block",
            "pdf_block",
            "audio_block",
            "tool_use_block",
            "tool_result_block",
            "resource_link_block",
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

    #[test]
    fn converts_string_content_message() {
        Python::with_gil(|py| {
            let msg = PyDict::new(py);
            msg.set_item("role", "user").unwrap();
            msg.set_item("content", "hello").unwrap();
            let out = py_message_to_rust(&msg).unwrap();
            assert_eq!(out.role, ChatRole::User);
            assert_eq!(out.text(), "hello");
        });
    }

    #[test]
    fn converts_block_content_message() {
        Python::with_gil(|py| {
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
        Python::with_gil(|py| {
            let msg = PyDict::new(py);
            let block = PyDict::new(py);
            let nested = PyDict::new(py);
            nested.set_item("type", "text").unwrap();
            nested.set_item("text", "tool output").unwrap();
            block.set_item("type", "tool_result").unwrap();
            block.set_item("id", "call-1").unwrap();
            block.set_item("name", "lookup").unwrap();
            block.set_item("is_error", True).unwrap();
            block.set_item("content", PyList::new(py, [nested]).unwrap()).unwrap();
            msg.set_item("role", "tool").unwrap();
            msg.set_item("content", PyList::new(py, [block]).unwrap()).unwrap();
            let out = py_message_to_rust(&msg).unwrap();
            assert_eq!(out.role, ChatRole::Assistant);
            assert!(out.has_tool_result());
        });
    }

    #[test]
    fn converts_binary_content_from_base64() {
        Python::with_gil(|py| {
            let image = PyDict::new(py);
            image.set_item("type", "image").unwrap();
            image.set_item("mime_type", "image/png").unwrap();
            image.set_item("data", "aGVsbG8=").unwrap();
            let content = py_block_to_rust(&image).unwrap();
            match content {
                Content::Image { mime_type, data } => {
                    assert_eq!(mime_type, "image/png");
                    assert_eq!(data, b"hello");
                }
                other => panic!("unexpected content: {other:?}"),
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
        Python::with_gil(|py| {
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

            let parsed = python_tools_to_rust(Some(&tools.into_any())).unwrap().unwrap();
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].function.name, "lookup_weather");
        });
    }
}
