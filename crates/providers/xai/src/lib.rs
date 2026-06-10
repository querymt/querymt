use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use qmt_codex::api::{
    CodexToolUseState, codex_parse_chat_with_state, codex_parse_stream_chunk_with_state,
};
use qmt_openai::{
    AuthType,
    api::{
        OpenAIProviderConfig, openai_chat_request, openai_embed_request,
        openai_list_models_request, openai_parse_chat, openai_parse_embed,
        openai_parse_list_models, parse_openai_sse_chunk, url_schema,
    },
};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatResponse, ChatRole, Content, ReasoningEffort, StreamChunk,
        StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    handle_http_error,
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

const XAI_ADDITIONAL_LIST_MODELS: &[&str] = &["grok-composer-2.5-fast"];

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Xai {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Xai::default_base_url")]
    pub base_url: Url,
    #[serde(default)]
    pub api_key: String,
    /// Optional: Explicitly specify authentication type.
    /// xAI API keys and OAuth access tokens are both sent as Bearer tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default, deserialize_with = "querymt::params::deserialize_system_vec")]
    pub system: Vec<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    /// Embedding parameters
    pub embedding_encoding_format: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,

    /// Optional resolver for dynamic credential refresh (e.g., OAuth tokens).
    #[serde(skip)]
    #[schemars(skip)]
    pub key_resolver: Option<Arc<dyn ApiKeyResolver>>,
    /// Conversation ID for x-grok-conv-id header (prompt caching).
    #[serde(skip)]
    #[schemars(skip)]
    pub conversation_id: Option<String>,
}

#[derive(Serialize)]
struct XaiCompletionRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<&'a u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<&'a f32>,
}

#[derive(Deserialize)]
struct XaiCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String, //TODO: Either<String, Vec<String>>,
}

impl OpenAIProviderConfig for Xai {
    fn api_key(&self) -> &str {
        &self.api_key
    }

    fn auth_type(&self) -> Option<&AuthType> {
        self.auth_type.as_ref()
    }

    fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> Option<&u32> {
        self.max_tokens.as_ref()
    }

    fn temperature(&self) -> Option<&f32> {
        self.temperature.as_ref()
    }

    fn system(&self) -> &[String] {
        &self.system
    }

    fn timeout_seconds(&self) -> Option<&u64> {
        self.timeout_seconds.as_ref()
    }

    fn stream(&self) -> Option<&bool> {
        self.stream.as_ref()
    }

    fn top_p(&self) -> Option<&f32> {
        self.top_p.as_ref()
    }

    fn top_k(&self) -> Option<&u32> {
        self.top_k.as_ref()
    }

    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    fn embedding_encoding_format(&self) -> Option<&str> {
        self.embedding_encoding_format.as_deref()
    }

    fn embedding_dimensions(&self) -> Option<&u32> {
        self.embedding_dimensions.as_ref()
    }

    fn reasoning_effort(&self) -> Option<querymt::chat::ReasoningEffort> {
        self.reasoning_effort
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }
}

impl HTTPChatProvider for Xai {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg = self.with_resolved_key();
        let use_responses = self.should_use_responses_api();
        let mut request = if use_responses {
            // Responses-style path for xAI (modeled after codex_responses)
            xai_responses_chat_request(&cfg, messages, tools)?
        } else {
            openai_chat_request(&cfg, messages, tools)?
        };
        // Auto-inject x-grok-conv-id for xAI endpoints (Responses or host x.ai)
        let host = self.base_url.host_str().unwrap_or("");
        let auto_inject = use_responses || host.contains("x.ai");
        if auto_inject {
            let conv_id = self
                .conversation_id
                .clone()
                .unwrap_or_else(|| "session-auto".to_string());
            let (mut parts, body) = request.into_parts();
            parts.headers.insert(
                http::header::HeaderName::from_static("x-grok-conv-id"),
                conv_id.parse().unwrap(),
            );
            request = Request::from_parts(parts, body);
        }
        Ok(request)
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut cfg = self.with_resolved_key();
        cfg.stream = Some(true);
        let use_responses = cfg.should_use_responses_api();
        let mut request = if use_responses {
            xai_responses_chat_request(&cfg, messages, tools)?
        } else {
            openai_chat_request(&cfg, messages, tools)?
        };
        let host = cfg.base_url.host_str().unwrap_or("");
        let auto_inject = use_responses || host.contains("x.ai");
        if auto_inject {
            let conv_id = cfg
                .conversation_id
                .clone()
                .unwrap_or_else(|| "session-auto".to_string());
            let (mut parts, body) = request.into_parts();
            parts.headers.insert(
                http::header::HeaderName::from_static("x-grok-conv-id"),
                conv_id.parse().unwrap(),
            );
            request = Request::from_parts(parts, body);
        }
        Ok(request)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        if self.should_use_responses_api() {
            let tool_state_buffer = Arc::new(Mutex::new(HashMap::new()));
            codex_parse_chat_with_state(response, &tool_state_buffer)
        } else {
            openai_parse_chat(self, response)
        }
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(XaiStreamParser::new(
            self.should_use_responses_api(),
        )))
    }
}

impl HTTPEmbeddingProvider for Xai {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg = self.with_resolved_key();
        let mut request = openai_embed_request(&cfg, inputs)?;
        let host = self.base_url.host_str().unwrap_or("");
        let auto_inject = host.contains("x.ai") || self.should_use_responses_api();
        if auto_inject {
            let conv_id = self
                .conversation_id
                .clone()
                .unwrap_or_else(|| "session-auto".to_string());
            let (mut parts, body) = request.into_parts();
            parts.headers.insert(
                http::header::HeaderName::from_static("x-grok-conv-id"),
                conv_id.parse().unwrap(),
            );
            request = Request::from_parts(parts, body);
        }
        Ok(request)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Xai {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let api_key = self.resolved_key();
        if api_key.is_empty() {
            return Err(LLMError::AuthError("Missing xAI auth token".to_string()));
        }

        let body = XaiCompletionRequest {
            model: self.model(),
            prompt: &req.prompt,
            suffix: req.suffix.as_deref(),
            max_tokens: req.max_tokens.as_ref(),
            temperature: req.temperature.as_ref(),
        };

        let json_body = serde_json::to_vec(&body)?;
        let url = self
            .base_url()
            .join("fim/completions")
            .map_err(|e| LLMError::HttpError(e.to_string()))?;

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(url.to_string())
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .header(CONTENT_TYPE, "application/json");
        let host = self.base_url.host_str().unwrap_or("");
        let auto_inject = host.contains("x.ai") || self.should_use_responses_api();
        if auto_inject {
            let conv_id = self
                .conversation_id
                .clone()
                .unwrap_or_else(|| "session-auto".to_string());
            builder = builder.header("x-grok-conv-id", conv_id);
        } else if let Some(ref conv_id) = self.conversation_id {
            builder = builder.header("x-grok-conv-id", conv_id);
        }
        Ok(builder.body(json_body)?)
    }

    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        handle_http_error!(resp);

        let json_resp: Result<XaiCompletionResponse, serde_json::Error> =
            serde_json::from_slice(resp.body());
        match json_resp {
            Ok(completion_response) => Ok(CompletionResponse {
                text: completion_response.choices[0].message.content.clone(), // FIXME
            }),
            Err(e) => Err(LLMError::JsonError(e)),
        }
    }
}

struct XaiStreamParser {
    use_responses_api: bool,
    codex_tool_state: Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
    openai_tool_state: HashMap<usize, qmt_openai::api::OpenAIToolUseState>,
}

impl XaiStreamParser {
    fn new(use_responses_api: bool) -> Self {
        Self {
            use_responses_api,
            codex_tool_state: Arc::new(Mutex::new(HashMap::new())),
            openai_tool_state: HashMap::new(),
        }
    }
}

impl ChatStreamParser for XaiStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        if self.use_responses_api {
            codex_parse_stream_chunk_with_state(chunk, &self.codex_tool_state)
        } else {
            parse_openai_sse_chunk(chunk, &mut self.openai_tool_state)
        }
    }
}

impl HTTPLLMProvider for Xai {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }
}

impl Xai {
    fn default_base_url() -> Url {
        Url::parse("https://api.x.ai/v1/").unwrap()
    }

    /// Detects when targeting xAI endpoints that support Responses-style API
    /// (e.g. base URL contains x.ai or explicit OAuth auth type).
    fn should_use_responses_api(&self) -> bool {
        let host = self.base_url.host_str().unwrap_or("");
        if host.contains("x.ai") {
            return true;
        }
        matches!(self.auth_type, Some(AuthType::OAuth))
    }

    fn resolved_key(&self) -> String {
        if let Some(ref resolver) = self.key_resolver {
            resolver.current()
        } else {
            self.api_key.clone()
        }
    }

    fn with_resolved_key(&self) -> Self {
        let mut cfg = self.clone();
        cfg.api_key = self.resolved_key();
        cfg.key_resolver = None;
        cfg
    }

    /// Set the conversation ID for x-grok-conv-id header (enables prompt caching).
    pub fn set_conversation_id(&mut self, id: impl Into<String>) {
        self.conversation_id = Some(id.into());
    }
}

#[derive(Serialize, Debug)]
struct XaiResponsesRequest<'a> {
    model: &'a str,
    input: Vec<XaiResponsesInputItem<'a>>,
    instructions: &'a str,
    store: bool,
    stream: bool,
    #[serde(rename = "max_output_tokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<XaiResponsesTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<XaiResponsesToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<XaiResponsesText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<XaiResponsesReasoning>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum XaiResponsesInputItem<'a> {
    #[serde(rename = "message")]
    Message {
        role: &'a str,
        content: Vec<XaiResponsesInputContent<'a>>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: &'a str,
        output: XaiResponsesFunctionOutput<'a>,
    },
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum XaiResponsesFunctionOutput<'a> {
    Text(String),
    Parts(Vec<XaiResponsesToolOutputPart<'a>>),
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum XaiResponsesToolOutputPart<'a> {
    #[serde(rename = "output_text")]
    OutputText { text: Cow<'a, str> },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum XaiResponsesInputContent<'a> {
    #[serde(rename = "input_text")]
    InputText { text: &'a str },
    #[serde(rename = "output_text")]
    OutputText { text: &'a str },
    #[serde(rename = "input_image")]
    InputImage { image_url: String },
}

#[derive(Serialize, Debug)]
struct XaiResponsesTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'a str,
    name: &'a str,
    description: &'a str,
    parameters: Value,
    strict: bool,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum XaiResponsesToolChoice {
    Mode(&'static str),
    Tool { r#type: &'static str, name: String },
}

impl From<&ToolChoice> for XaiResponsesToolChoice {
    fn from(choice: &ToolChoice) -> Self {
        match choice {
            ToolChoice::Any => XaiResponsesToolChoice::Mode("required"),
            ToolChoice::Auto => XaiResponsesToolChoice::Mode("auto"),
            ToolChoice::None => XaiResponsesToolChoice::Mode("none"),
            ToolChoice::Tool(name) => XaiResponsesToolChoice::Tool {
                r#type: "function",
                name: name.clone(),
            },
        }
    }
}

#[derive(Serialize, Debug)]
struct XaiResponsesText {
    format: XaiResponsesTextFormat,
}

#[derive(Serialize, Debug)]
struct XaiResponsesTextFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

impl From<StructuredOutputFormat> for XaiResponsesText {
    fn from(format: StructuredOutputFormat) -> Self {
        XaiResponsesText {
            format: XaiResponsesTextFormat {
                format_type: "json_schema",
                name: format.name,
                description: format.description,
                schema: format.schema.unwrap_or_else(|| serde_json::json!({})),
                strict: format.strict,
            },
        }
    }
}

#[derive(Serialize, Debug)]
struct XaiResponsesReasoning {
    effort: &'static str,
}

fn to_xai_responses_input(messages: &[ChatMessage]) -> Vec<XaiResponsesInputItem<'_>> {
    let mut inputs = Vec::with_capacity(messages.len());

    for msg in messages {
        let is_user = matches!(msg.role, ChatRole::User);
        let mut content_blocks = Vec::new();

        for block in &msg.content {
            match block {
                Content::Text { text } if !text.is_empty() => {
                    if is_user {
                        content_blocks.push(XaiResponsesInputContent::InputText { text });
                    } else {
                        content_blocks.push(XaiResponsesInputContent::OutputText { text });
                    }
                }
                Content::Image { mime_type, data } => {
                    content_blocks.push(XaiResponsesInputContent::InputImage {
                        image_url: format!("data:{};base64,{}", mime_type, STANDARD.encode(data)),
                    });
                }
                Content::ImageUrl { url } => {
                    content_blocks.push(XaiResponsesInputContent::InputImage {
                        image_url: url.clone(),
                    });
                }
                _ => {}
            }
        }

        if !content_blocks.is_empty() {
            inputs.push(XaiResponsesInputItem::Message {
                role: if is_user { "user" } else { "assistant" },
                content: content_blocks,
            });
        }

        for block in &msg.content {
            match block {
                Content::ToolUse {
                    id,
                    name,
                    arguments,
                } => inputs.push(XaiResponsesInputItem::FunctionCall {
                    call_id: id,
                    name,
                    arguments: serde_json::to_string(arguments).unwrap_or_default(),
                }),
                Content::ToolResult { id, content, .. } => {
                    let mut output_parts = Vec::new();
                    let mut text_only_parts = Vec::new();
                    let mut has_non_text = false;

                    for c in content {
                        match c {
                            Content::Text { text } => {
                                output_parts.push(XaiResponsesToolOutputPart::OutputText {
                                    text: Cow::Borrowed(text.as_str()),
                                });
                                text_only_parts.push(text.clone());
                            }
                            Content::Image { mime_type, data } => {
                                has_non_text = true;
                                output_parts.push(XaiResponsesToolOutputPart::InputImage {
                                    image_url: format!(
                                        "data:{};base64,{}",
                                        mime_type,
                                        STANDARD.encode(data)
                                    ),
                                });
                            }
                            Content::ImageUrl { url } => {
                                has_non_text = true;
                                output_parts.push(XaiResponsesToolOutputPart::InputImage {
                                    image_url: url.clone(),
                                });
                            }
                            Content::Pdf { data } => {
                                has_non_text = true;
                                let text = format!(
                                    "[PDF tool output not yet serialized natively ({} bytes)]",
                                    data.len()
                                );
                                output_parts.push(XaiResponsesToolOutputPart::OutputText {
                                    text: Cow::Owned(text),
                                });
                            }
                            Content::Audio { mime_type, data } => {
                                has_non_text = true;
                                let text = format!(
                                    "[Audio tool output not yet serialized natively ({}: {} bytes)]",
                                    mime_type,
                                    data.len()
                                );
                                output_parts.push(XaiResponsesToolOutputPart::OutputText {
                                    text: Cow::Owned(text),
                                });
                            }
                            _ => {}
                        }
                    }

                    let output = if has_non_text {
                        XaiResponsesFunctionOutput::Parts(output_parts)
                    } else {
                        XaiResponsesFunctionOutput::Text(text_only_parts.join("\n"))
                    };
                    inputs.push(XaiResponsesInputItem::FunctionCallOutput {
                        call_id: id,
                        output,
                    });
                }
                _ => {}
            }
        }
    }

    inputs
}

fn to_xai_responses_tools(tools: &[Tool]) -> Vec<XaiResponsesTool<'_>> {
    tools
        .iter()
        .map(|tool| XaiResponsesTool {
            tool_type: tool.tool_type.as_str(),
            name: tool.function.name.as_str(),
            description: tool.function.description.as_str(),
            parameters: sanitize_xai_schema(tool.function.parameters.clone()),
            strict: false,
        })
        .collect()
}

fn sanitize_xai_schema(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => sanitize_xai_schema_object(map),
        Value::Array(items) => {
            for item in items {
                sanitize_xai_schema_in_place(item);
            }
        }
        _ => {}
    }
    value
}

fn sanitize_xai_schema_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => sanitize_xai_schema_object(map),
        Value::Array(items) => {
            for item in items {
                sanitize_xai_schema_in_place(item);
            }
        }
        _ => {}
    }
}

fn sanitize_xai_schema_object(map: &mut Map<String, Value>) {
    map.remove("pattern");
    map.remove("format");
    for value in map.values_mut() {
        sanitize_xai_schema_in_place(value);
    }
}

fn xai_effort_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "xhigh",
    }
}

fn xai_model_supports_reasoning_effort(model: &str) -> bool {
    let name = model.trim().to_ascii_lowercase();
    let name = name.rsplit('/').next().unwrap_or(name.as_str());

    ["grok-3-mini", "grok-4.20-multi-agent", "grok-4.3"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn is_supported_xai_list_model(model: &str) -> bool {
    let model = model.strip_prefix("x-ai/").unwrap_or(model);
    model != "grok-imagine" && !model.starts_with("grok-imagine-")
}

fn xai_responses_chat_request<C: qmt_openai::api::OpenAIProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = cfg.api_key();
    if api_key.is_empty() {
        return Err(LLMError::AuthError("Missing xAI auth token".to_string()));
    }

    let request_tools = tools
        .or_else(|| cfg.tools())
        .map(to_xai_responses_tools)
        .filter(|tools| !tools.is_empty());
    let request_tool_choice = if request_tools.is_some() {
        cfg.tool_choice().map(XaiResponsesToolChoice::from)
    } else {
        None
    };
    let text = cfg.json_schema().cloned().map(XaiResponsesText::from);
    let reasoning = cfg
        .reasoning_effort()
        .filter(|_| xai_model_supports_reasoning_effort(cfg.model()))
        .map(|effort| XaiResponsesReasoning {
            effort: xai_effort_str(effort),
        });
    let instructions = cfg.system().join("\n");
    let body = XaiResponsesRequest {
        model: cfg.model(),
        input: to_xai_responses_input(messages),
        instructions: instructions.as_str(),
        store: false,
        stream: true,
        max_output_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        top_p: cfg.top_p().copied(),
        top_k: cfg.top_k().copied(),
        tools: request_tools,
        tool_choice: request_tool_choice,
        text,
        reasoning,
    };
    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("responses")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    Ok(Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header("OpenAI-Beta", "responses=experimental")
        .header(CONTENT_TYPE, "application/json")
        .body(json_body)?)
}

struct XaiFactory;

impl HTTPLLMProviderFactory for XaiFactory {
    fn name(&self) -> &str {
        "xai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("XAI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Xai::default_base_url(),
        };
        openai_list_models_request(&base_url, &cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        let mut models: Vec<String> = openai_parse_list_models(&resp)?
            .into_iter()
            .filter(|model| is_supported_xai_list_model(model))
            .collect();

        for model in XAI_ADDITIONAL_LIST_MODELS {
            if !models.iter().any(|listed| listed == model) {
                models.push((*model).to_string());
            }
        }

        Ok(models)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Xai);
        serde_json::to_string(&schema).expect("Xai JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Xai = serde_json::from_str(cfg)?;

        Ok(Box::new(provider))
    }
}

/// Creates an xAI HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(XaiFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(XaiFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Xai, XaiFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Xai,
        factory = XaiFactory,
        name   = "xai",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::auth::static_key;

    fn test_xai(api_key: &str) -> Xai {
        Xai {
            base_url: Xai::default_base_url(),
            api_key: api_key.to_string(),
            auth_type: None,
            model: "grok-test".to_string(),
            max_tokens: None,
            temperature: None,
            system: Vec::new(),
            timeout_seconds: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            embedding_encoding_format: None,
            embedding_dimensions: None,
            reasoning_effort: None,
            json_schema: None,
            key_resolver: None,
            conversation_id: None,
        }
    }

    fn auth_header(req: &Request<Vec<u8>>) -> Option<&str> {
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
    }

    #[test]
    fn deserialize_oauth_auth_type_without_api_key() {
        let cfg = serde_json::json!({
            "auth_type": "oauth",
            "model": "grok-test"
        });

        let xai: Xai = serde_json::from_value(cfg).expect("OAuth config should deserialize");
        assert_eq!(xai.auth_type, Some(AuthType::OAuth));
        assert!(xai.api_key.is_empty());
    }

    #[test]
    fn deserialize_existing_api_key_config_still_works() {
        let cfg = serde_json::json!({
            "api_key": "xai-api-key",
            "model": "grok-test"
        });

        let xai: Xai = serde_json::from_value(cfg).expect("API-key config should deserialize");
        assert_eq!(xai.api_key, "xai-api-key");
        assert_eq!(xai.auth_type, None);
    }

    #[test]
    fn parse_list_models_filters_grok_imagine_models_and_appends_additional_models() {
        let response = Response::builder()
            .status(200)
            .body(
                br#"{"data":[{"id":"grok-4"},{"id":"grok-imagine"},{"id":"grok-imagine-latest"},{"id":"grok-3"}]}"#
                    .to_vec(),
            )
            .expect("response should build");

        let models = XaiFactory
            .parse_list_models(response)
            .expect("model parsing should succeed");

        assert_eq!(models, vec!["grok-4", "grok-3", "grok-composer-2.5-fast"]);
    }

    #[test]
    fn parse_list_models_does_not_duplicate_additional_models() {
        let response = Response::builder()
            .status(200)
            .body(br#"{"data":[{"id":"grok-4"},{"id":"grok-composer-2.5-fast"}]}"#.to_vec())
            .expect("response should build");

        let models = XaiFactory
            .parse_list_models(response)
            .expect("model parsing should succeed");

        assert_eq!(models, vec!["grok-4", "grok-composer-2.5-fast"]);
    }

    #[test]
    fn parse_list_models_keeps_prefixed_upstream_model_and_appends_bare_additional_model() {
        let response = Response::builder()
            .status(200)
            .body(br#"{"data":[{"id":"x-ai/grok-composer-2.5-fast"}]}"#.to_vec())
            .expect("response should build");

        let models = XaiFactory
            .parse_list_models(response)
            .expect("model parsing should succeed");

        assert_eq!(
            models,
            vec!["x-ai/grok-composer-2.5-fast", "grok-composer-2.5-fast"]
        );
    }

    #[test]
    fn chat_request_uses_resolver_current_token() {
        let mut xai = test_xai("stale-token");
        xai.set_key_resolver(static_key("resolver-token"));
        let messages = vec![ChatMessage::user().text("hello").build()];

        let req = xai
            .chat_request(&messages, None)
            .expect("chat request should build");

        assert_eq!(auth_header(&req), Some("Bearer resolver-token"));
    }

    #[test]
    fn chat_stream_request_forces_stream_true_on_openai_chat_completions_path() {
        let mut xai = test_xai("xai-key");
        xai.base_url = Url::parse("https://api.openai-compatible.test/v1/").unwrap();

        let messages = vec![ChatMessage::user().text("hello").build()];
        let req = xai
            .chat_stream_request(&messages, None)
            .expect("chat stream request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be JSON");

        assert_eq!(body["stream"], Value::Bool(true));
    }

    #[test]
    fn responses_request_uses_responses_body_and_xai_headers() {
        let mut xai = test_xai("xai-key");
        xai.system = vec!["Be concise.".to_string(), "Use Grok.".to_string()];
        xai.max_tokens = Some(128);
        xai.temperature = Some(0.2);
        xai.top_p = Some(0.9);
        xai.top_k = Some(4);
        xai.tool_choice = Some(ToolChoice::Tool("lookup".to_string()));
        let messages = vec![
            ChatMessage::user()
                .text("hello")
                .image_url("https://example.test/image.png")
                .build(),
            ChatMessage::assistant()
                .tool_use("call-1", "lookup", serde_json::json!({"q":"hello"}))
                .build(),
            ChatMessage::user()
                .tool_result(
                    "call-1".to_string(),
                    None,
                    false,
                    vec![Content::text("answer")],
                )
                .build(),
        ];
        let tools = vec![Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "lookup".to_string(),
                description: "Look up data".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "q": {
                            "type": "string",
                            "pattern": "^[a-z]+$",
                            "format": "regex",
                            "items": { "type": "string", "format": "uuid" }
                        }
                    },
                    "format": "json-schema"
                }),
            },
        }];

        let req = xai
            .chat_request(&messages, Some(&tools))
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be JSON");

        assert!(req.uri().to_string().ends_with("/responses"));
        assert_eq!(body["model"], "grok-test");
        assert_eq!(body["instructions"], "Be concise.\nUse Grok.");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"], 128);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["top_k"], 4);
        assert!(body.get("input").is_some());
        assert!(body.get("messages").is_none());
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["description"], "Look up data");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["name"], "lookup");
        assert!(body["tool_choice"].get("function").is_none());
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert!(req.headers().get("ChatGPT-Account-ID").is_none());
        assert!(req.headers().get("originator").is_none());
        assert_eq!(
            req.headers()
                .get("OpenAI-Beta")
                .and_then(|v| v.to_str().ok()),
            Some("responses=experimental")
        );
    }

    #[test]
    fn responses_request_serializes_structured_output_text_format() {
        let mut xai = test_xai("xai-key");
        xai.json_schema = Some(StructuredOutputFormat {
            name: "Answer".to_string(),
            description: Some("A structured answer".to_string()),
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "answer": { "type": "string" }
                },
                "required": ["answer"]
            })),
            strict: Some(true),
        });
        let messages = vec![ChatMessage::user().text("hello").build()];

        let req = xai
            .chat_request(&messages, None)
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be JSON");

        assert!(body.get("response_format").is_none());
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "Answer");
        assert_eq!(body["text"]["format"]["description"], "A structured answer");
        assert_eq!(body["text"]["format"]["schema"]["type"], "object");
        assert_eq!(body["text"]["format"]["strict"], true);
    }

    #[test]
    fn responses_request_serializes_reasoning_only_for_effort_capable_models() {
        let messages = vec![ChatMessage::user().text("hello").build()];

        let mut capable = test_xai("xai-key");
        capable.model = "x-ai/grok-3-mini-fast".to_string();
        capable.reasoning_effort = Some(ReasoningEffort::Max);
        let req = capable
            .chat_request(&messages, None)
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be JSON");
        assert_eq!(body["reasoning"]["effort"], "xhigh");

        let mut unsupported = test_xai("xai-key");
        unsupported.model = "grok-code-fast-1".to_string();
        unsupported.reasoning_effort = Some(ReasoningEffort::High);
        let req = unsupported
            .chat_request(&messages, None)
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be JSON");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn responses_tool_schema_sanitizer_strips_nested_pattern_and_format() {
        let schema = serde_json::json!({
            "type": "object",
            "format": "top",
            "properties": {
                "name": { "type": "string", "pattern": "^[a-z]+$" },
                "items": {
                    "type": "array",
                    "items": [{ "type": "string", "format": "uuid" }]
                }
            }
        });

        let sanitized = sanitize_xai_schema(schema.clone());

        assert!(sanitized.get("format").is_none());
        assert!(sanitized["properties"]["name"].get("pattern").is_none());
        assert!(
            sanitized["properties"]["items"]["items"][0]
                .get("format")
                .is_none()
        );
        assert!(schema["properties"]["name"].get("pattern").is_some());
    }

    #[test]
    fn completion_request_uses_resolver_current_token() {
        let mut xai = test_xai("stale-token");
        xai.set_key_resolver(static_key("resolver-token"));
        let completion = CompletionRequest {
            prompt: "hello".to_string(),
            suffix: None,
            max_tokens: None,
            temperature: None,
        };

        let req = xai
            .complete_request(&completion)
            .expect("completion request should build");

        assert_eq!(auth_header(&req), Some("Bearer resolver-token"));
    }
}
