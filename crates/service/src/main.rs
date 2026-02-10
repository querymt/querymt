use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::post,
};
use clap::Parser;
use futures::StreamExt;
use futures::stream as futures_stream;
use querymt::{
    FunctionCall, LLMProvider, ToolCall,
    chat::{ChatMessage, ChatRole, MessageType, StreamChunk, Tool},
    error::LLMError,
    plugin::{
        default_providers_path,
        extism_impl::host::ExtismLoader,
        host::native::NativeLoader,
        host::{PluginRegistry, ProviderConfig},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    convert::Infallible,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Address to bind the service to
    #[arg(long, default_value = "0.0.0.0:8080")]
    addr: String,
    /// Path to providers config file
    #[arg(long)]
    providers: Option<PathBuf>,
    /// Optional auth key required for requests (Bearer token)
    #[arg(long)]
    auth_key: Option<String>,
}

#[derive(Clone)]
struct ServerState {
    registry: Arc<PluginRegistry>,
    auth_key: Option<String>,
}

#[derive(Deserialize)]
struct ChatRequest {
    pub messages: Option<Vec<Message>>,
    pub model: Option<String>,
    #[serde(default)]
    pub steps: Vec<ChainStepRequest>,
    #[serde(default)]
    pub response_transform: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, flatten)]
    pub options: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct ChainStepRequest {
    pub provider_id: String,
    pub id: String,
    pub template: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub response_transform: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Message {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Serialize)]
struct ChatResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
}

#[derive(Serialize)]
struct Choice {
    pub index: usize,
    pub message: Message,
    pub finish_reason: String,
}

#[derive(Default)]
struct StreamState {
    tool_states: HashMap<usize, ToolUseState>,
    saw_tool_calls: bool,
    finished: bool,
    stop_reason: Option<String>,
}

#[derive(Default)]
struct ToolUseState {
    id: String,
    name: String,
    arguments_buffer: String,
    started: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("querymt_service=info,tower_http=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    let args = Args::parse();
    let providers_path = args.providers.unwrap_or_else(default_providers_path);

    info!(
        addr = %args.addr,
        providers = %providers_path.display(),
        auth = %args.auth_key.as_ref().map(|_| "enabled").unwrap_or("disabled"),
        "starting service"
    );

    let mut registry = PluginRegistry::from_path(providers_path)?;
    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));

    let state = ServerState {
        registry: Arc::new(registry),
        auth_key: args.auth_key,
    };

    let app = Router::new()
        .route("/v1/chat/completions", post(handle_chat))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_chat(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, (StatusCode, String)> {
    if let Some(key) = &state.auth_key {
        let auth_header = headers.get("Authorization").ok_or((
            StatusCode::UNAUTHORIZED,
            "Missing authorization".to_string(),
        ))?;

        let auth_str = auth_header.to_str().map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                "Invalid authorization header".to_string(),
            )
        })?;

        if !auth_str.starts_with("Bearer ") || &auth_str[7..] != key {
            warn!("unauthorized request");
            return Err((StatusCode::UNAUTHORIZED, "Invalid API key".to_string()));
        }
    }

    let mut options = req.options.clone();
    let stream_requested = read_stream_flag(&options)?;

    if stream_requested && !req.steps.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Streaming is not supported for chain requests".to_string(),
        ));
    }

    if !req.steps.is_empty() {
        info!(steps = req.steps.len(), "processing chain request");
        return handle_chain_request(state, req)
            .await
            .map(IntoResponse::into_response);
    }

    let target_provider = take_target_provider(&mut options)?;
    let (provider_id, model_name) = resolve_provider_and_model(&req, target_provider.as_deref())?;
    info!(provider = %provider_id, model = %model_name, "building provider");
    let provider = build_provider(
        &state.registry,
        &provider_id,
        Some(&model_name),
        req.temperature,
        req.max_tokens,
        Some(&options),
    )
    .await
    .map_err(|e| {
        error!(provider = %provider_id, error = %e, "failed to build provider");
        (StatusCode::BAD_REQUEST, e.to_string())
    })?;

    let messages = map_request_messages(req.messages.unwrap_or_default())?;

    if stream_requested {
        if !provider.supports_streaming() {
            return Err((
                StatusCode::BAD_REQUEST,
                "Provider does not support streaming".to_string(),
            ));
        }

        info!(provider = %provider_id, model = %model_name, "starting stream");

        let stream = provider
            .chat_stream_with_tools(&messages, req.tools.as_deref())
            .await
            .map_err(|e| {
                error!(provider = %provider_id, error = %e, "stream request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;

        let id = format!("chatcmpl-{}", Uuid::new_v4());
        let created = now_unix_seconds();
        let model = model_name.clone();

        let sse_stream = stream
            .scan(StreamState::default(), move |state, item| {
                let mut events = Vec::new();
                if state.finished {
                    return futures::future::ready(None);
                }

                match item {
                    Ok(chunk) => {
                        events.extend(render_stream_chunk(&id, created, &model, chunk, state));
                        if state.finished {
                            events.push(Event::default().data("[DONE]"));
                        }
                    }
                    Err(e) => {
                        error!(provider = %provider_id, error = %e, "stream chunk failed");
                        let err_event = Event::default().data(
                            json!({
                                "error": {
                                    "message": e.to_string(),
                                    "type": "server_error"
                                }
                            })
                            .to_string(),
                        );
                        events.push(err_event);
                        events.push(Event::default().data("[DONE]"));
                        state.finished = true;
                    }
                }

                futures::future::ready(Some(events))
            })
            .flat_map(|events| futures_stream::iter(events.into_iter().map(Ok::<_, Infallible>)));

        let response = Sse::new(sse_stream).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        );

        return Ok(response.into_response());
    }

    let response = if let Some(tools) = req.tools.as_deref() {
        provider
            .chat_with_tools(&messages, Some(tools))
            .await
            .map_err(|e| {
                error!(provider = %provider_id, error = %e, "chat request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?
    } else {
        provider.chat(&messages).await.map_err(|e| {
            error!(provider = %provider_id, error = %e, "chat request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?
    };

    let tool_calls = response.tool_calls();
    let finish_reason = if tool_calls.as_ref().map(|v| !v.is_empty()).unwrap_or(false) {
        "tool_calls"
    } else {
        "stop"
    };

    Ok(Json(ChatResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: now_unix_seconds(),
        model: model_name,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: "assistant".to_string(),
                content: response.text(),
                tool_calls,
                tool_call_id: None,
                tool_name: None,
            },
            finish_reason: finish_reason.to_string(),
        }],
    })
    .into_response())
}

async fn handle_chain_request(
    state: ServerState,
    req: ChatRequest,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    let mut provider_ids = Vec::new();
    let mut memory: HashMap<String, String> = HashMap::new();
    let mut options = req.options.clone();
    let target_provider = take_target_provider(&mut options)?;

    let last_step_id = if let Some(last_step) = req.steps.last() {
        last_step.id.clone()
    } else if req.model.is_some() {
        "initial".to_string()
    } else {
        return Err((StatusCode::BAD_REQUEST, "No steps provided".to_string()));
    };

    let transform_response = |resp: String, transform: &str| -> String {
        match transform {
            "extract_think" => resp
                .lines()
                .skip_while(|line| !line.contains("<think>"))
                .take_while(|line| !line.contains("</think>"))
                .map(|line| line.replace("<think>", "").trim().to_string())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            "trim_whitespace" => resp.trim().to_string(),
            "extract_json" => {
                let json_start = resp.find("```json").unwrap_or(0);
                let json_end = resp.find("```").unwrap_or(resp.len());
                let json_str = &resp[json_start..json_end];
                serde_json::from_str::<String>(json_str)
                    .unwrap_or_else(|_| "Invalid JSON response".to_string())
            }
            _ => resp.to_string(),
        }
    };

    if req.model.is_some() {
        let (provider_id, model_name) =
            resolve_provider_and_model(&req, target_provider.as_deref())?;
        provider_ids.push(provider_id.clone());
        info!(provider = %provider_id, model = %model_name, "processing chain initial step");

        let template = req
            .messages
            .as_ref()
            .and_then(|messages| messages.last())
            .and_then(|msg| msg.content.clone())
            .ok_or((StatusCode::BAD_REQUEST, "No messages provided".to_string()))?;

        let provider = build_provider(
            &state.registry,
            &provider_id,
            Some(&model_name),
            req.temperature,
            req.max_tokens,
            Some(&options),
        )
        .await
        .map_err(|e| {
            error!(provider = %provider_id, error = %e, "failed to build provider");
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

        let response = provider
            .chat(&[ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: template,
                thinking: None,
                cache: None,
            }])
            .await
            .map_err(|e| {
                error!(provider = %provider_id, error = %e, "chain initial request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;

        let transform = req.response_transform.unwrap_or_default();
        let text = transform_response(response.text().unwrap_or_default(), &transform);
        memory.insert("initial".to_string(), text);
    }

    for step in req.steps.into_iter() {
        ensure_allowed_provider(&step.provider_id)?;
        provider_ids.push(step.provider_id.clone());
        let prompt = replace_template(&step.template, &memory);
        info!(provider = %step.provider_id, step = %step.id, "processing chain step");

        let provider = build_provider(
            &state.registry,
            &step.provider_id,
            None,
            step.temperature,
            step.max_tokens,
            Some(&options),
        )
        .await
        .map_err(|e| {
            error!(provider = %step.provider_id, error = %e, "failed to build provider");
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

        let response = provider
            .chat(&[ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: prompt,
                thinking: None,
                cache: None,
            }])
            .await
            .map_err(|e| {
                error!(provider = %step.provider_id, error = %e, "chain step request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;

        let transform = step.response_transform.unwrap_or_default();
        let text = transform_response(response.text().unwrap_or_default(), &transform);
        memory.insert(step.id, text);
    }

    let final_response = memory.get(&last_step_id).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("No response found for step {}", last_step_id),
    ))?;

    Ok(Json(ChatResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        model: provider_ids.join(","),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: "assistant".to_string(),
                content: Some(final_response.to_string()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
            },
            finish_reason: "stop".to_string(),
        }],
    }))
}

fn resolve_provider_and_model(
    req: &ChatRequest,
    target_provider: Option<&str>,
) -> Result<(String, String), (StatusCode, String)> {
    let model = req
        .model
        .as_ref()
        .ok_or((StatusCode::BAD_REQUEST, "Model is required".to_string()))?;

    if let Some(target) = target_provider {
        ensure_allowed_provider(target)?;
        let model_name = model
            .split_once(':')
            .map(|(_, name)| name)
            .unwrap_or(model.as_str());
        Ok((target.to_string(), model_name.to_string()))
    } else {
        let (provider_id, model_name) = model
            .split_once(':')
            .ok_or((StatusCode::BAD_REQUEST, "Invalid model format".to_string()))?;
        ensure_allowed_provider(provider_id)?;
        Ok((provider_id.to_string(), model_name.to_string()))
    }
}

fn ensure_allowed_provider(provider_id: &str) -> Result<(), (StatusCode, String)> {
    match provider_id {
        "llama_cpp" | "mrs" => Ok(()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            "Only llama_cpp and mrs providers are supported".to_string(),
        )),
    }
}

fn take_target_provider(
    options: &mut HashMap<String, Value>,
) -> Result<Option<String>, (StatusCode, String)> {
    match options.remove("target_provider") {
        None => Ok(None),
        Some(Value::String(val)) => Ok(Some(val)),
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            "target_provider must be a string".to_string(),
        )),
    }
}

async fn build_provider(
    registry: &PluginRegistry,
    provider_id: &str,
    model_override: Option<&str>,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    extra_options: Option<&HashMap<String, Value>>,
) -> Result<Box<dyn LLMProvider>, LLMError> {
    let factory = registry
        .get(provider_id)
        .await
        .ok_or_else(|| LLMError::InvalidRequest(format!("Unknown provider: {}", provider_id)))?;

    let provider_cfg = registry
        .config
        .providers
        .iter()
        .find(|cfg| cfg.name == provider_id)
        .ok_or_else(|| LLMError::InvalidRequest(format!("Unknown provider: {}", provider_id)))?;

    let mut cfg = base_provider_config(provider_cfg)?;

    if let Some(extra) = extra_options {
        merge_extra_options(&mut cfg, extra);
    }

    if let Some(model) = model_override {
        set_json_field(&mut cfg, "model", Value::String(model.to_string()));
    } else if !has_json_field(&cfg, "model") {
        return Err(LLMError::InvalidRequest(format!(
            "Provider '{}' requires a model",
            provider_id
        )));
    }

    if let Some(temp) = temperature {
        set_json_field(&mut cfg, "temperature", Value::from(temp));
    }

    if let Some(max) = max_tokens {
        set_json_field(&mut cfg, "max_tokens", Value::from(max));
    }

    if let Some(http_factory) = factory.as_http() {
        if !has_json_field(&cfg, "api_key") {
            if let Some(env_name) = http_factory.api_key_name() {
                if let Ok(val) = std::env::var(env_name) {
                    set_json_field(&mut cfg, "api_key", Value::String(val));
                }
            }
        }
    }

    let schema: Value = serde_json::from_str(&factory.config_schema())
        .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
    let pruned_cfg = prune_config_by_schema(&cfg, &schema);
    let pruned_cfg_str =
        serde_json::to_string(&pruned_cfg).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
    factory.from_config(&pruned_cfg_str)
}

fn base_provider_config(provider_cfg: &ProviderConfig) -> Result<Value, LLMError> {
    let mut map = Map::new();
    if let Some(cfg) = &provider_cfg.config {
        for (key, value) in cfg {
            let json_value =
                serde_json::to_value(value).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
            map.insert(key.clone(), json_value);
        }
    }
    Ok(Value::Object(map))
}

fn has_json_field(cfg: &Value, key: &str) -> bool {
    cfg.get(key)
        .and_then(|val| match val {
            Value::Null => None,
            _ => Some(val),
        })
        .is_some()
}

fn set_json_field(cfg: &mut Value, key: &str, value: Value) {
    if let Value::Object(map) = cfg {
        map.insert(key.to_string(), value);
    }
}

fn merge_extra_options(cfg: &mut Value, options: &HashMap<String, Value>) {
    for (key, value) in options {
        set_json_field(cfg, key, value.clone());
    }
}

fn map_request_messages(messages: Vec<Message>) -> Result<Vec<ChatMessage>, (StatusCode, String)> {
    let mut out = Vec::with_capacity(messages.len());

    for msg in messages {
        let content = msg.content.unwrap_or_default();
        let has_tool_calls = msg
            .tool_calls
            .as_ref()
            .map(|calls| !calls.is_empty())
            .unwrap_or(false);
        let is_tool_result = msg.role == "tool" || msg.tool_call_id.is_some();

        if has_tool_calls {
            out.push(ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::ToolUse(msg.tool_calls.unwrap_or_default()),
                content,
                thinking: None,
                cache: None,
            });
            continue;
        }

        if is_tool_result {
            let call_id = msg.tool_call_id.ok_or((
                StatusCode::BAD_REQUEST,
                "tool_call_id is required for tool messages".to_string(),
            ))?;
            let name = msg.tool_name.unwrap_or_else(|| {
                warn!("tool_name missing for tool result message");
                "unknown".to_string()
            });

            let tool_call = ToolCall {
                id: call_id,
                call_type: "function".to_string(),
                function: FunctionCall {
                    name,
                    arguments: content.clone(),
                },
            };

            out.push(ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::ToolResult(vec![tool_call]),
                content,
                thinking: None,
                cache: None,
            });
            continue;
        }

        let role = match msg.role.as_str() {
            "assistant" => ChatRole::Assistant,
            _ => ChatRole::User,
        };

        out.push(ChatMessage {
            role,
            message_type: MessageType::Text,
            content,
            thinking: None,
            cache: None,
        });
    }

    Ok(out)
}

fn read_stream_flag(options: &HashMap<String, Value>) -> Result<bool, (StatusCode, String)> {
    match options.get("stream") {
        None => Ok(false),
        Some(Value::Bool(val)) => Ok(*val),
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            "stream must be a boolean".to_string(),
        )),
    }
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn render_stream_chunk(
    stream_id: &str,
    created: u64,
    model: &str,
    chunk: StreamChunk,
    state: &mut StreamState,
) -> Vec<Event> {
    let mut events = Vec::new();
    match chunk {
        StreamChunk::Text(text) => {
            if text.is_empty() {
                return events;
            }
            events.push(
                Event::default().data(
                    json!({
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {"content": text},
                            "finish_reason": null
                        }]
                    })
                    .to_string(),
                ),
            );
        }
        StreamChunk::Thinking(text) => {
            if text.is_empty() {
                return events;
            }
            events.push(
                Event::default().data(
                    json!({
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {"reasoning_content": text},
                            "finish_reason": null
                        }]
                    })
                    .to_string(),
                ),
            );
        }
        StreamChunk::ToolUseStart { index, id, name } => {
            state.saw_tool_calls = true;
            let entry = state.tool_states.entry(index).or_default();
            entry.id = id.clone();
            entry.name = name.clone();
            entry.started = true;

            events.push(
                Event::default().data(
                    json!({
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": index,
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name}
                                }]
                            },
                            "finish_reason": null
                        }]
                    })
                    .to_string(),
                ),
            );
        }
        StreamChunk::ToolUseInputDelta {
            index,
            partial_json,
        } => {
            let entry = state.tool_states.entry(index).or_default();
            entry.arguments_buffer.push_str(&partial_json);
            events.push(Event::default().data(
                json!({
                    "id": stream_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": if entry.id.is_empty() { Value::Null } else { json!(entry.id) },
                                "type": "function",
                                "function": {"arguments": partial_json}
                            }]
                        },
                        "finish_reason": null
                    }]
                })
                .to_string(),
            ));
        }
        StreamChunk::ToolUseComplete { index, tool_call } => {
            let entry = state.tool_states.entry(index).or_default();
            entry.id = tool_call.id.clone();
            entry.name = tool_call.function.name.clone();
            entry.arguments_buffer = tool_call.function.arguments.clone();
        }
        StreamChunk::Usage(_) => {}
        StreamChunk::Done { stop_reason } => {
            state.stop_reason = Some(stop_reason);
            let finish_reason = if state.saw_tool_calls {
                "tool_calls"
            } else {
                match state.stop_reason.as_deref() {
                    Some("length") => "length",
                    Some("content_filter") => "content_filter",
                    _ => "stop",
                }
            };

            events.push(
                Event::default().data(
                    json!({
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": finish_reason
                        }]
                    })
                    .to_string(),
                ),
            );
            state.finished = true;
        }
    }

    events
}

fn prune_config_by_schema(cfg: &Value, schema: &Value) -> Value {
    match (cfg, schema.get("properties")) {
        (Value::Object(cfg_map), Some(Value::Object(props))) => {
            let mut out = Map::with_capacity(cfg_map.len());
            for (k, v) in cfg_map {
                if let Some(prop_schema) = props.get(k) {
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
        _ => cfg.clone(),
    }
}

fn replace_template(input: &str, memory: &HashMap<String, String>) -> String {
    let mut out = input.to_string();
    for (k, v) in memory {
        let pattern = format!("{{{{{}}}}}", k);
        out = out.replace(&pattern, v);
    }
    out
}
