use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use futures::StreamExt;
use futures::stream as futures_stream;
use querymt::{
    FunctionCall, LLMProvider, ToolCall, Usage,
    chat::{ChatMessage, ChatRole, ImageMime, MessageType, StreamChunk, Tool},
    completion::CompletionRequest as QmtCompletionRequest,
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
    pub messages: Option<Vec<MessageIn>>,
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
struct CompletionHttpRequest {
    pub prompt: Option<String>,
    pub suffix: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default, flatten)]
    pub options: HashMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EmbeddingInput {
    Single(String),
    Multi(Vec<String>),
}

#[derive(Deserialize)]
struct EmbeddingsHttpRequest {
    pub input: Option<EmbeddingInput>,
    pub model: Option<String>,
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

#[derive(Deserialize)]
struct MessageIn {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContentIn>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoning_content"
    )]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContentIn {
    Text(String),
    Parts(Vec<MessageContentPartIn>),
}

#[derive(Deserialize)]
struct MessageContentPartIn {
    #[serde(rename = "type")]
    part_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<MessageImageUrlIn>,
    #[serde(default)]
    source: Option<MessageContentSourceIn>,
}

#[derive(Deserialize)]
struct MessageImageUrlIn {
    url: String,
}

#[derive(Deserialize)]
struct MessageContentSourceIn {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct Message {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reasoning_content"
    )]
    pub reasoning_content: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Serialize)]
struct CompletionHttpResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
}

#[derive(Serialize)]
struct CompletionChoice {
    pub index: usize,
    pub text: String,
    pub finish_reason: String,
}

#[derive(Serialize)]
struct EmbeddingsHttpResponse {
    pub object: String,
    pub model: String,
    pub data: Vec<EmbeddingData>,
}

#[derive(Serialize)]
struct ModelsHttpResponse {
    pub object: String,
    pub data: Vec<ModelInfo>,
}

#[derive(Serialize)]
struct ModelInfo {
    pub id: String,
}

#[derive(Serialize)]
struct EmbeddingData {
    pub object: String,
    pub embedding: Vec<f32>,
    pub index: usize,
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
    last_usage: Option<Usage>,
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
        .route("/v1/completions", post(handle_completion))
        .route("/v1/embeddings", post(handle_embeddings))
        .route("/v1/models", get(handle_models))
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

    let (provider_id, model_name) = resolve_provider_and_model(&req)?;

    normalize_system_option_for_provider(&provider_id, &mut options)?;

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
                reasoning_content: response.thinking(),
                tool_calls,
                tool_call_id: None,
                tool_name: None,
            },
            finish_reason: finish_reason.to_string(),
        }],
        usage: response.usage(),
    })
    .into_response())
}

async fn handle_completion(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<CompletionHttpRequest>,
) -> Result<Json<CompletionHttpResponse>, (StatusCode, String)> {
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

    let prompt = req
        .prompt
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "Prompt is required".to_string()))?;
    let model = req
        .model
        .as_ref()
        .ok_or((StatusCode::BAD_REQUEST, "Model is required".to_string()))?;

    let mut options = req.options.clone();
    let (provider_id, model_name) = resolve_provider_and_model_from_model(model)?;

    normalize_system_option_for_provider(&provider_id, &mut options)?;

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

    let completion_req = QmtCompletionRequest {
        prompt,
        suffix: req.suffix.clone(),
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };

    let resp = provider.complete(&completion_req).await.map_err(|e| {
        error!(provider = %provider_id, error = %e, "completion request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    Ok(Json(CompletionHttpResponse {
        id: format!("cmpl-{}", Uuid::new_v4()),
        object: "text_completion".to_string(),
        created: now_unix_seconds(),
        model: model_name,
        choices: vec![CompletionChoice {
            index: 0,
            text: resp.text,
            finish_reason: "stop".to_string(),
        }],
    }))
}

async fn handle_embeddings(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(req): Json<EmbeddingsHttpRequest>,
) -> Result<Json<EmbeddingsHttpResponse>, (StatusCode, String)> {
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

    let model = req
        .model
        .as_ref()
        .ok_or((StatusCode::BAD_REQUEST, "Model is required".to_string()))?;

    let inputs: Vec<String> = match req.input {
        Some(EmbeddingInput::Single(s)) => vec![s],
        Some(EmbeddingInput::Multi(v)) => v,
        None => {
            return Err((StatusCode::BAD_REQUEST, "Input is required".to_string()));
        }
    };
    if inputs.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Input must not be empty".to_string(),
        ));
    }

    let mut options = req.options.clone();
    let (provider_id, model_name) = resolve_provider_and_model_from_model(model)?;

    normalize_system_option_for_provider(&provider_id, &mut options)?;

    info!(provider = %provider_id, model = %model_name, "building provider");
    let provider = build_provider(
        &state.registry,
        &provider_id,
        Some(&model_name),
        None,
        None,
        Some(&options),
    )
    .await
    .map_err(|e| {
        error!(provider = %provider_id, error = %e, "failed to build provider");
        (StatusCode::BAD_REQUEST, e.to_string())
    })?;

    let embeddings = provider.embed(inputs).await.map_err(|e| {
        error!(provider = %provider_id, error = %e, "embedding request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;

    let data = embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| EmbeddingData {
            object: "embedding".to_string(),
            embedding,
            index,
        })
        .collect();

    Ok(Json(EmbeddingsHttpResponse {
        object: "list".to_string(),
        model: model_name,
        data,
    }))
}

async fn handle_models(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<ModelsHttpResponse>, (StatusCode, String)> {
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

    let providers: Vec<String> = state
        .registry
        .config
        .providers
        .iter()
        .map(|p| p.name.clone())
        .filter(|name| ensure_allowed_provider(name).is_ok())
        .collect();

    let mut out = Vec::new();
    for provider_id in providers {
        let factory = state.registry.get(&provider_id).await.ok_or((
            StatusCode::BAD_REQUEST,
            format!("Unknown provider: {provider_id}"),
        ))?;

        let provider_cfg = state
            .registry
            .config
            .providers
            .iter()
            .find(|cfg| cfg.name == provider_id)
            .ok_or((
                StatusCode::BAD_REQUEST,
                format!("Unknown provider: {provider_id}"),
            ))?;

        let cfg = base_provider_config(provider_cfg).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build provider config: {e}"),
            )
        })?;

        let schema: Value = serde_json::from_str(&factory.config_schema()).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to parse config schema: {e}"),
            )
        })?;
        let pruned_cfg = prune_config_by_schema(&cfg, &schema);
        let pruned_cfg_str = serde_json::to_string(&pruned_cfg).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize config: {e}"),
            )
        })?;

        let models = match factory.list_models(&pruned_cfg_str).await {
            Ok(models) => models,
            Err(e) => {
                // Model listing is best-effort: some providers require mandatory config fields
                // (e.g. a local model_path) that may only be provided per-request.
                warn!(provider = %provider_id, error = %e, "list_models failed; skipping provider");
                continue;
            }
        };

        for m in models {
            out.push(ModelInfo {
                id: format!("{provider_id}:{m}"),
            });
        }
    }

    Ok(Json(ModelsHttpResponse {
        object: "list".to_string(),
        data: out,
    }))
}

async fn handle_chain_request(
    state: ServerState,
    req: ChatRequest,
) -> Result<Json<ChatResponse>, (StatusCode, String)> {
    let mut provider_ids = Vec::new();
    let mut memory: HashMap<String, String> = HashMap::new();
    let options = req.options.clone();

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
        let (provider_id, model_name) = resolve_provider_and_model(&req)?;
        provider_ids.push(provider_id.clone());
        info!(provider = %provider_id, model = %model_name, "processing chain initial step");

        let template = req
            .messages
            .as_ref()
            .and_then(|messages| messages.last())
            .and_then(|msg| match msg.content.as_ref() {
                Some(MessageContentIn::Text(s)) => Some(s.clone()),
                Some(MessageContentIn::Parts(_)) => None,
                None => None,
            })
            .ok_or((
                StatusCode::BAD_REQUEST,
                "Chain initial step requires a text message".to_string(),
            ))?;

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
        format!("No response found for step {last_step_id}"),
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
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
            },
            finish_reason: "stop".to_string(),
        }],
        usage: None,
    }))
}

fn resolve_provider_and_model(req: &ChatRequest) -> Result<(String, String), (StatusCode, String)> {
    let model = req
        .model
        .as_ref()
        .ok_or((StatusCode::BAD_REQUEST, "Model is required".to_string()))?;
    resolve_provider_and_model_from_model(model)
}

fn resolve_provider_and_model_from_model(
    model: &str,
) -> Result<(String, String), (StatusCode, String)> {
    let (provider_id, model_name) = model
        .split_once(':')
        .ok_or((StatusCode::BAD_REQUEST, "Invalid model format".to_string()))?;
    ensure_allowed_provider(provider_id)?;
    Ok((provider_id.to_string(), model_name.to_string()))
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

fn normalize_system_option_for_provider(
    provider_id: &str,
    options: &mut HashMap<String, Value>,
) -> Result<(), (StatusCode, String)> {
    if provider_id != "llama_cpp" {
        return Ok(());
    }

    let Some(val) = options.get_mut("system") else {
        return Ok(());
    };

    match val {
        Value::Null => {
            options.remove("system");
        }
        Value::String(s) => {
            *val = Value::Array(vec![Value::String(s.clone())]);
        }
        Value::Array(arr) => {
            if !arr.iter().all(|v| matches!(v, Value::String(_))) {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "system must be a string or array of strings".to_string(),
                ));
            }
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "system must be a string or array of strings".to_string(),
            ));
        }
    }

    Ok(())
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
        .ok_or_else(|| LLMError::InvalidRequest(format!("Unknown provider: {provider_id}")))?;

    let provider_cfg = registry
        .config
        .providers
        .iter()
        .find(|cfg| cfg.name == provider_id)
        .ok_or_else(|| LLMError::InvalidRequest(format!("Unknown provider: {provider_id}")))?;

    let mut cfg = base_provider_config(provider_cfg)?;

    if let Some(extra) = extra_options {
        merge_extra_options(&mut cfg, extra);
    }

    if let Some(model) = model_override {
        set_json_field(&mut cfg, "model", Value::String(model.to_string()));
    } else if !has_json_field(&cfg, "model") {
        return Err(LLMError::InvalidRequest(format!(
            "Provider '{provider_id}' requires a model"
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

fn map_request_messages(
    messages: Vec<MessageIn>,
) -> Result<Vec<ChatMessage>, (StatusCode, String)> {
    let mut out = Vec::with_capacity(messages.len());

    for msg in messages {
        let thinking = msg.reasoning_content.clone();

        let content = match msg.content {
            None => MessageContentIn::Text(String::new()),
            Some(c) => c,
        };
        let has_tool_calls = msg
            .tool_calls
            .as_ref()
            .map(|calls| !calls.is_empty())
            .unwrap_or(false);
        let is_tool_result = msg.role == "tool" || msg.tool_call_id.is_some();

        let content_text_only =
            |content: MessageContentIn| -> Result<String, (StatusCode, String)> {
                match content {
                    MessageContentIn::Text(s) => Ok(s),
                    MessageContentIn::Parts(parts) => {
                        let mut texts = Vec::new();
                        for part in parts {
                            if part.part_type == "text" {
                                if let Some(t) = part.text {
                                    if !t.is_empty() {
                                        texts.push(t);
                                    }
                                    continue;
                                }
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    "content part `text` requires `text` field".to_string(),
                                ));
                            }
                            return Err((
                                StatusCode::BAD_REQUEST,
                                "tool messages only support text content".to_string(),
                            ));
                        }
                        Ok(texts.join("\n"))
                    }
                }
            };

        let parse_image_mime = |media_type: &str| -> Result<ImageMime, (StatusCode, String)> {
            match media_type {
                "image/jpeg" => Ok(ImageMime::JPEG),
                "image/png" => Ok(ImageMime::PNG),
                "image/gif" => Ok(ImageMime::GIF),
                "image/webp" => Ok(ImageMime::WEBP),
                other => Err((
                    StatusCode::BAD_REQUEST,
                    format!("unsupported image media_type: {other}"),
                )),
            }
        };

        let decode_base64 = |data: &str| -> Result<Vec<u8>, (StatusCode, String)> {
            BASE64.decode(data.as_bytes()).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("invalid base64 payload: {e}"),
                )
            })
        };

        if has_tool_calls {
            let content = content_text_only(content)?;
            out.push(ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::ToolUse(msg.tool_calls.unwrap_or_default()),
                content,
                thinking,
                cache: None,
            });
            continue;
        }

        if is_tool_result {
            let content = content_text_only(content)?;
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
                thinking,
                cache: None,
            });
            continue;
        }

        let role = match msg.role.as_str() {
            "assistant" => ChatRole::Assistant,
            _ => ChatRole::User,
        };

        match content {
            MessageContentIn::Text(content) => {
                out.push(ChatMessage {
                    role,
                    message_type: MessageType::Text,
                    content,
                    thinking,
                    cache: None,
                });
            }
            MessageContentIn::Parts(parts) => {
                let mut text_parts = Vec::new();
                let mut attachments = Vec::new();

                for part in parts {
                    match part.part_type.as_str() {
                        "text" => {
                            let t = part.text.ok_or((
                                StatusCode::BAD_REQUEST,
                                "content part `text` requires `text` field".to_string(),
                            ))?;
                            if !t.is_empty() {
                                text_parts.push(t);
                            }
                        }
                        "image_url" => {
                            let url = part.image_url.ok_or((
                                StatusCode::BAD_REQUEST,
                                "content part `image_url` requires `image_url` field".to_string(),
                            ))?;
                            attachments.push(MessageType::ImageURL(url.url));
                        }
                        "image" => {
                            let src = part.source.ok_or((
                                StatusCode::BAD_REQUEST,
                                "content part `image` requires `source` field".to_string(),
                            ))?;
                            if src.source_type != "base64" {
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    "only base64 sources are supported".to_string(),
                                ));
                            }
                            let mime = parse_image_mime(&src.media_type)?;
                            let bytes = decode_base64(&src.data)?;
                            attachments.push(MessageType::Image((mime, bytes)));
                        }
                        "document" | "pdf" => {
                            let src = part.source.ok_or((
                                StatusCode::BAD_REQUEST,
                                "content part `document` requires `source` field".to_string(),
                            ))?;
                            if src.source_type != "base64" {
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    "only base64 sources are supported".to_string(),
                                ));
                            }
                            if src.media_type != "application/pdf" {
                                return Err((
                                    StatusCode::BAD_REQUEST,
                                    format!("unsupported document media_type: {}", src.media_type),
                                ));
                            }
                            let bytes = decode_base64(&src.data)?;
                            attachments.push(MessageType::Pdf(bytes));
                        }
                        other => {
                            return Err((
                                StatusCode::BAD_REQUEST,
                                format!("unsupported content part type: {other}"),
                            ));
                        }
                    }
                }

                let combined_text = text_parts.join("\n");
                if attachments.is_empty() {
                    out.push(ChatMessage {
                        role,
                        message_type: MessageType::Text,
                        content: combined_text,
                        thinking,
                        cache: None,
                    });
                } else if attachments.len() == 1 {
                    out.push(ChatMessage {
                        role,
                        message_type: attachments.remove(0),
                        content: combined_text,
                        thinking,
                        cache: None,
                    });
                } else {
                    if !combined_text.is_empty() {
                        out.push(ChatMessage {
                            role: role.clone(),
                            message_type: MessageType::Text,
                            content: combined_text,
                            thinking: thinking.clone(),
                            cache: None,
                        });
                    }
                    for attachment in attachments {
                        out.push(ChatMessage {
                            role: role.clone(),
                            message_type: attachment,
                            content: String::new(),
                            thinking: thinking.clone(),
                            cache: None,
                        });
                    }
                }
            }
        }
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
        StreamChunk::Usage(usage) => {
            state.last_usage = Some(usage.clone());
            events.push(
                Event::default().data(
                    json!({
                        "id": stream_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [],
                        "usage": usage,
                    })
                    .to_string(),
                ),
            );
        }
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

            let mut payload = json!({
                "id": stream_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish_reason
                }]
            });
            if let Some(usage) = state.last_usage.clone() {
                payload["usage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
            }

            events.push(Event::default().data(payload.to_string()));
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
        let pattern = format!("{{{{{k}}}}}");
        out = out.replace(&pattern, v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_user(content: Option<MessageContentIn>) -> MessageIn {
        MessageIn {
            role: "user".to_string(),
            content,
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
        }
    }

    #[test]
    fn normalize_system_llama_cpp_string_to_vec() {
        let mut options = HashMap::new();
        options.insert("system".to_string(), Value::String("hello".to_string()));

        normalize_system_option_for_provider("llama_cpp", &mut options).unwrap();
        assert_eq!(
            options.get("system"),
            Some(&Value::Array(vec![Value::String("hello".to_string())]))
        );
    }

    #[test]
    fn normalize_system_non_llama_cpp_unchanged() {
        let mut options = HashMap::new();
        options.insert("system".to_string(), Value::String("hello".to_string()));

        normalize_system_option_for_provider("mrs", &mut options).unwrap();
        assert_eq!(
            options.get("system"),
            Some(&Value::String("hello".to_string()))
        );
    }

    #[test]
    fn map_request_messages_text_string() {
        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Text(
            "hi".to_string(),
        )))])
        .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, ChatRole::User);
        assert_eq!(out[0].message_type, MessageType::Text);
        assert_eq!(out[0].content, "hi");
    }

    #[test]
    fn map_request_messages_parts_text_join() {
        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "text".to_string(),
                text: Some("a".to_string()),
                image_url: None,
                source: None,
            },
            MessageContentPartIn {
                part_type: "text".to_string(),
                text: Some("b".to_string()),
                image_url: None,
                source: None,
            },
        ])))])
        .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_type, MessageType::Text);
        assert_eq!(out[0].content, "a\nb");
    }

    #[test]
    fn map_request_messages_parts_image_url_single_attachment() {
        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "text".to_string(),
                text: Some("caption".to_string()),
                image_url: None,
                source: None,
            },
            MessageContentPartIn {
                part_type: "image_url".to_string(),
                text: None,
                image_url: Some(MessageImageUrlIn {
                    url: "https://example.com/img.png".to_string(),
                }),
                source: None,
            },
        ])))])
        .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, ChatRole::User);
        assert_eq!(out[0].content, "caption");
        assert_eq!(
            out[0].message_type,
            MessageType::ImageURL("https://example.com/img.png".to_string())
        );
    }

    #[test]
    fn map_request_messages_parts_inline_image_base64() {
        let raw = b"pngbytes".to_vec();
        let encoded = BASE64.encode(&raw);

        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "image".to_string(),
                text: None,
                image_url: None,
                source: Some(MessageContentSourceIn {
                    source_type: "base64".to_string(),
                    media_type: "image/png".to_string(),
                    data: encoded,
                }),
            },
        ])))])
        .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, ChatRole::User);
        assert_eq!(out[0].content, "");
        assert_eq!(
            out[0].message_type,
            MessageType::Image((ImageMime::PNG, raw))
        );
    }

    #[test]
    fn map_request_messages_parts_inline_pdf_base64() {
        let raw = b"pdfbytes".to_vec();
        let encoded = BASE64.encode(&raw);

        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "document".to_string(),
                text: None,
                image_url: None,
                source: Some(MessageContentSourceIn {
                    source_type: "base64".to_string(),
                    media_type: "application/pdf".to_string(),
                    data: encoded,
                }),
            },
        ])))])
        .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_type, MessageType::Pdf(raw));
    }

    #[test]
    fn map_request_messages_multiple_attachments_expands() {
        let img = BASE64.encode(b"img");
        let pdf = BASE64.encode(b"pdf");

        let out = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "text".to_string(),
                text: Some("t".to_string()),
                image_url: None,
                source: None,
            },
            MessageContentPartIn {
                part_type: "image".to_string(),
                text: None,
                image_url: None,
                source: Some(MessageContentSourceIn {
                    source_type: "base64".to_string(),
                    media_type: "image/png".to_string(),
                    data: img,
                }),
            },
            MessageContentPartIn {
                part_type: "document".to_string(),
                text: None,
                image_url: None,
                source: Some(MessageContentSourceIn {
                    source_type: "base64".to_string(),
                    media_type: "application/pdf".to_string(),
                    data: pdf,
                }),
            },
        ])))])
        .unwrap();

        // 1 text + 2 attachments
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].message_type, MessageType::Text);
        assert_eq!(out[1].role, ChatRole::User);
        assert!(matches!(out[1].message_type, MessageType::Image(_)));
        assert_eq!(out[2].role, ChatRole::User);
        assert!(matches!(out[2].message_type, MessageType::Pdf(_)));
    }

    #[test]
    fn map_request_messages_invalid_base64_errors() {
        let err = map_request_messages(vec![msg_user(Some(MessageContentIn::Parts(vec![
            MessageContentPartIn {
                part_type: "image".to_string(),
                text: None,
                image_url: None,
                source: Some(MessageContentSourceIn {
                    source_type: "base64".to_string(),
                    media_type: "image/png".to_string(),
                    data: "not base64".to_string(),
                }),
            },
        ])))])
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn render_stream_chunk_usage_updates_state_and_emits_event() {
        let mut state = StreamState::default();
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 0,
            cache_read: 0,
            cache_write: 0,
        };

        let events = render_stream_chunk(
            "id",
            0,
            "model",
            StreamChunk::Usage(usage.clone()),
            &mut state,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(state.last_usage, Some(usage));
    }
}
