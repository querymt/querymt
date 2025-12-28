use either::*;
use http::{
    header::{AUTHORIZATION, CONTENT_TYPE},
    Method, Request, Response,
};
use querymt::{
    chat::{
        ChatMessage, ChatResponse, ChatRole, MessageType, StreamChunk, StructuredOutputFormat,
        Tool, ToolChoice,
    },
    error::LLMError,
    handle_http_error, FunctionCall, ToolCall, Usage,
};
use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Schema, SchemaObject, SingleOrVec},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use url::Url;

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    Schema::Object(SchemaObject {
        metadata: None,
        // say "this is a string"
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
        // with the "uri" format
        format: Some("uri".to_string()),
        ..Default::default()
    })
}

/// Individual message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct OpenAIChatMessage<'a> {
    #[allow(dead_code)]
    role: &'a str,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "either::serde_untagged_optional"
    )]
    content: Option<Either<Vec<MessageContent<'a>>, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIFunctionCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionPayload<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    content_type: &'a str,
    function: OpenAIFunctionPayload<'a>,
}

#[derive(Serialize, Debug)]
struct MessageContent<'a> {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    message_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrlContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "content")]
    tool_output: Option<&'a str>,
}

/// Individual image message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct ImageUrlContent<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct OpenAIEmbeddingRequest {
    model: String,
    input: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

/// Request payload for OpenAI's chat API endpoint.
#[derive(Serialize, Debug)]
struct OpenAIChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAIResponseFormat>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

pub struct DisplayableToolCall(pub ToolCall);
impl std::fmt::Display for DisplayableToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{\n  \"id\": \"{}\",\n  \"type\": \"{}\",\n  \"function\": {}\n}}",
            self.0.id,
            self.0.call_type,
            DisplayableFunctionCall(self.0.function.clone())
        )
    }
}

pub struct DisplayableFunctionCall(pub FunctionCall);
impl std::fmt::Display for DisplayableFunctionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{\n  \"name\": \"{}\",\n  \"arguments\": {}\n}}",
            self.0.name, self.0.arguments
        )
    }
}

/// Response from OpenAI's chat API endpoint.
#[derive(Deserialize, Debug)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChatChoice>,
    usage: Option<Usage>,
}

/// Individual choice within an OpenAI chat API response.
#[derive(Deserialize, Debug)]
struct OpenAIChatChoice {
    message: OpenAIChatMsg,
}

/// Message content within an OpenAI chat API response.
#[derive(Deserialize, Debug)]
struct OpenAIChatMsg {
    #[allow(dead_code)]
    role: String,
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Deserialize, Debug)]
struct OpenAIEmbeddingData {
    embedding: Vec<f32>,
}
#[derive(Deserialize, Debug)]
struct OpenAIEmbeddingResponse {
    data: Vec<OpenAIEmbeddingData>,
}

/// An object specifying the format that the model must output.
///Setting to `{ "type": "json_schema", "json_schema": {...} }` enables Structured Outputs which ensures the model will match your supplied JSON schema. Learn more in the [Structured Outputs guide](https://platform.openai.com/docs/guides/structured-outputs).
/// Setting to `{ "type": "json_object" }` enables the older JSON mode, which ensures the message the model generates is valid JSON. Using `json_schema` is preferred for models that support it.
#[derive(Deserialize, Debug, Serialize)]
enum OpenAIResponseType {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_schema")]
    JsonSchema,
    #[serde(rename = "json_object")]
    JsonObject,
}

#[derive(Deserialize, Debug, Serialize)]
struct OpenAIResponseFormat {
    #[serde(rename = "type")]
    response_type: OpenAIResponseType,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<StructuredOutputFormat>,
}

impl From<StructuredOutputFormat> for OpenAIResponseFormat {
    /// Modify the schema to ensure that it meets OpenAI's requirements.
    fn from(structured_response_format: StructuredOutputFormat) -> Self {
        // It's possible to pass a StructuredOutputJsonSchema without an actual schema.
        // In this case, just pass the StructuredOutputJsonSchema object without modifying it.
        match structured_response_format.schema {
            None => OpenAIResponseFormat {
                response_type: OpenAIResponseType::JsonSchema,
                json_schema: Some(structured_response_format),
            },
            Some(mut schema) => {
                // Although [OpenAI's specifications](https://platform.openai.com/docs/guides/structured-outputs?api-mode=chat#additionalproperties-false-must-always-be-set-in-objects) say that the "additionalProperties" field is required, my testing shows that it is not.
                // Just to be safe, add it to the schema if it is missing.
                schema = if schema.get("additionalProperties").is_none() {
                    schema["additionalProperties"] = serde_json::json!(false);
                    schema
                } else {
                    schema
                };

                OpenAIResponseFormat {
                    response_type: OpenAIResponseType::JsonSchema,
                    json_schema: Some(StructuredOutputFormat {
                        name: structured_response_format.name,
                        description: structured_response_format.description,
                        schema: Some(schema),
                        strict: structured_response_format.strict,
                    }),
                }
            }
        }
    }
}

impl ChatResponse for OpenAIChatResponse {
    fn text(&self) -> Option<String> {
        self.choices.first().and_then(|c| c.message.content.clone())
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.choices
            .first()
            .and_then(|c| c.message.tool_calls.clone())
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
}

impl std::fmt::Display for OpenAIChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (
            &self.choices.first().unwrap().message.content,
            &self.choices.first().unwrap().message.tool_calls,
        ) {
            (Some(content), Some(tool_calls)) => {
                for tool_call in tool_calls {
                    write!(f, "{}", DisplayableToolCall(tool_call.clone()))?;
                }
                write!(f, "{}", content)
            }
            (Some(content), None) => write!(f, "{}", content),
            (None, Some(tool_calls)) => {
                for tool_call in tool_calls {
                    write!(f, "{}", DisplayableToolCall(tool_call.clone()))?;
                }
                Ok(())
            }
            (None, None) => write!(f, ""),
        }
    }
}

pub trait OpenAIProviderConfig {
    fn api_key(&self) -> &str;
    fn base_url(&self) -> &Url;
    fn model(&self) -> &str;
    fn max_tokens(&self) -> Option<&u32>;
    fn temperature(&self) -> Option<&f32>;
    fn system(&self) -> Option<&str>;
    fn timeout_seconds(&self) -> Option<&u64>;
    fn stream(&self) -> Option<&bool>;
    fn top_p(&self) -> Option<&f32>;
    fn top_k(&self) -> Option<&u32>;
    fn tools(&self) -> Option<&[Tool]>;
    fn tool_choice(&self) -> Option<&ToolChoice>;
    fn embedding_encoding_format(&self) -> Option<&str>;
    fn embedding_dimensions(&self) -> Option<&u32>;
    fn reasoning_effort(&self) -> Option<&String> {
        None
    }
    fn json_schema(&self) -> Option<&StructuredOutputFormat>;
    fn extra_body(&self) -> Option<Map<String, Value>> {
        None
    }
}

pub fn openai_embed_request<C: OpenAIProviderConfig>(
    cfg: &C,
    inputs: &[String],
) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = match cfg.api_key().into() {
        Some(key) => key,
        None => return Err(LLMError::AuthError("Missing API key".to_string())),
    };

    let emb_format = cfg
        .embedding_encoding_format()
        .unwrap_or_else(|| "float".into());

    let body = OpenAIEmbeddingRequest {
        model: cfg.model().into(),
        input: inputs.to_vec(),
        encoding_format: Some(emb_format.into()),
        dimensions: cfg.embedding_dimensions().copied(),
    };

    let url = cfg
        .base_url()
        .join("embeddings")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    let json_body = serde_json::to_vec(&body).unwrap();
    Ok(Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header(CONTENT_TYPE, "application/json")
        .body(json_body)?)
}

pub fn openai_parse_embed<C: OpenAIProviderConfig>(
    _cfg: &C,
    resp: Response<Vec<u8>>,
) -> Result<Vec<Vec<f32>>, LLMError> {
    let json_resp: OpenAIEmbeddingResponse = serde_json::from_slice(resp.body())?;
    let embeddings = json_resp.data.into_iter().map(|d| d.embedding).collect();
    Ok(embeddings)
}

pub fn openai_chat_request<C: OpenAIProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = match cfg.api_key().into() {
        Some(key) => key,
        None => return Err(LLMError::AuthError("Missing API key".into())),
    };

    // Clone the messages to have an owned mutable vector.
    let messages = messages.to_vec();

    let mut openai_msgs: Vec<OpenAIChatMessage> = vec![];

    for msg in messages {
        if let MessageType::ToolResult(ref results) = msg.message_type {
            for result in results {
                openai_msgs.push(
                    // Clone strings to own them
                    OpenAIChatMessage {
                        role: "tool",
                        tool_call_id: Some(result.id.clone()),
                        tool_calls: None,
                        content: Some(Right(result.function.arguments.clone())),
                    },
                );
            }
        } else {
            openai_msgs.push(chat_message_to_api_message(msg))
        }
    }

    if let Some(system) = cfg.system() {
        openai_msgs.insert(
            0,
            OpenAIChatMessage {
                role: "system",
                content: Some(Left(vec![MessageContent {
                    message_type: Some("text"),
                    text: Some(system),
                    image_url: None,
                    tool_call_id: None,
                    tool_output: None,
                }])),
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    // Build the response format object
    let response_format: Option<OpenAIResponseFormat> = cfg.json_schema().cloned().map(Into::into);

    let request_tools = tools
        .map(|t| t.to_vec())
        .or_else(|| cfg.tools().map(|t| t.to_vec()));

    let request_tool_choice = if request_tools.is_some() {
        cfg.tool_choice().cloned()
    } else {
        None
    };

    let body = OpenAIChatRequest {
        model: &cfg.model(),
        messages: openai_msgs,
        max_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        stream: *cfg.stream().unwrap_or(&false),
        top_p: cfg.top_p().copied(),
        top_k: cfg.top_k().copied(),
        tools: request_tools,
        tool_choice: request_tool_choice,
        reasoning_effort: cfg.reasoning_effort().cloned(),
        response_format,
        extra_body: cfg.extra_body(),
    };

    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("chat/completions")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    Ok(Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header(CONTENT_TYPE, "application/json")
        .body(json_body)?)
}

pub fn openai_parse_chat<C: OpenAIProviderConfig>(
    _cfg: &C,
    response: Response<Vec<u8>>,
) -> Result<Box<dyn ChatResponse>, LLMError> {
    // If we got a non-200 response, let's get the error details
    handle_http_error!(response);

    // Parse the successful response
    let json_resp: Result<OpenAIChatResponse, serde_json::Error> =
        serde_json::from_slice(&response.body());

    let resp_text: String = "".to_string();
    match json_resp {
        Ok(response) => Ok(Box::new(response)),
        Err(e) => Err(LLMError::ResponseFormatError {
            message: format!("Failed to decode API response: {}", e),
            raw_response: resp_text,
        }),
    }
}

// Create an owned OpenAIChatMessage that doesn't borrow from any temporary variables
fn chat_message_to_api_message(chat_msg: ChatMessage) -> OpenAIChatMessage<'static> {
    // For other message types, create an owned OpenAIChatMessage
    OpenAIChatMessage {
        role: match chat_msg.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        },
        tool_call_id: None,
        content: match &chat_msg.message_type {
            MessageType::Text => Some(Right(chat_msg.content.clone())),
            // Image case is handled separately above
            MessageType::Image(_) => unreachable!(),
            MessageType::Pdf(_) => unimplemented!(),
            MessageType::ImageURL(url) => {
                // Clone the URL to create an owned version
                let owned_url = url.clone();
                // Leak the string to get a 'static reference
                let url_str = Box::leak(owned_url.into_boxed_str());
                Some(Left(vec![MessageContent {
                    message_type: Some("image_url"),
                    text: None,
                    image_url: Some(ImageUrlContent { url: url_str }),
                    tool_output: None,
                    tool_call_id: None,
                }]))
            }
            MessageType::ToolUse(_) => None,
            MessageType::ToolResult(_) => None,
        },
        tool_calls: match &chat_msg.message_type {
            MessageType::ToolUse(calls) => {
                let owned_calls: Vec<OpenAIFunctionCall<'static>> = calls
                    .iter()
                    .map(|c| {
                        let owned_id = c.id.clone();
                        let owned_name = c.function.name.clone();
                        let owned_args = c.function.arguments.clone();

                        // Need to leak these strings to create 'static references
                        // This is a deliberate choice to solve the lifetime issue
                        // The small memory leak is acceptable in this context
                        let id_str = Box::leak(owned_id.into_boxed_str());
                        let name_str = Box::leak(owned_name.into_boxed_str());
                        let args_str = Box::leak(owned_args.into_boxed_str());

                        OpenAIFunctionCall {
                            id: id_str,
                            content_type: "function",
                            function: OpenAIFunctionPayload {
                                name: name_str,
                                arguments: args_str,
                            },
                        }
                    })
                    .collect();
                Some(owned_calls)
            }
            _ => None,
        },
    }
}

pub fn openai_list_models_request(
    base_url: &Url,
    cfg: &Value,
) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or(LLMError::InvalidRequest(
            "Could not find api_key".to_string(),
        ))?;

    let model_list_url = base_url.join("models")?;
    Ok(Request::builder()
        .method(Method::GET)
        .uri(model_list_url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header(CONTENT_TYPE, "application/json")
        .body(Vec::new())?)
}

pub fn openai_parse_list_models(response: &Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
    let resp_json: Value = serde_json::from_slice(response.body())?;
    let arr = resp_json
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LLMError::InvalidRequest("`data` missing or not an array".into()))?;

    let names = arr
        .iter()
        .filter_map(|m| m.get("id"))
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();

    Ok(names)
}

// ============================================================================
// Streaming Support
// ============================================================================

/// Streaming response chunk from OpenAI's API
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamChunk {
    pub choices: Vec<OpenAIStreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Individual choice in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamChoice {
    pub index: usize,
    pub delta: OpenAIStreamDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Delta content in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIStreamToolCall>>,
}

/// Tool call in a streaming response (fields are optional for incremental updates)
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    pub function: OpenAIStreamFunction,
}

/// Function call in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Arguments are always present but may be an empty string
    #[serde(default)]
    pub arguments: String,
}

/// State for tracking incremental tool call assembly
#[derive(Default, Debug)]
pub struct OpenAIToolUseState {
    pub id: String,
    pub name: String,
    pub arguments_buffer: String,
    pub started: bool,
}

/// Parse an OpenAI SSE chunk into StreamChunk events
pub fn parse_openai_sse_chunk(
    chunk: &[u8],
    tool_states: &mut HashMap<usize, OpenAIToolUseState>,
) -> Result<Vec<StreamChunk>, LLMError> {
    // Skip empty chunks
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(chunk);
    let mut results = Vec::new();
    let mut done_emitted = false;

    for line in text.lines() {
        // Stop processing if we've already emitted Done
        if done_emitted {
            break;
        }

        let line = line.trim();

        // Skip empty lines
        if line.is_empty() {
            continue;
        }

        // Extract SSE data payload
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue, // Skip non-data lines
        };

        // Handle stream end
        if data == "[DONE]" {
            // Emit remaining tool completions
            for (index, state) in tool_states.drain() {
                if state.started {
                    results.push(StreamChunk::ToolUseComplete {
                        index,
                        tool_call: ToolCall {
                            id: state.id,
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: state.name,
                                arguments: state.arguments_buffer,
                            },
                        },
                    });
                }
            }
            results.push(StreamChunk::Done {
                stop_reason: "end_turn".to_string(),
            });
            done_emitted = true;
            continue;
        }

        // Parse JSON chunk
        let stream_chunk: OpenAIStreamChunk =
            serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse OpenAI stream chunk: {}", e),
                raw_response: data.to_string(),
            })?;

        // Process each choice
        for choice in &stream_chunk.choices {
            // Handle text content
            if let Some(content) = &choice.delta.content {
                if !content.is_empty() {
                    results.push(StreamChunk::Text(content.clone()));
                }
            }

            // Handle tool calls
            if let Some(tool_calls) = &choice.delta.tool_calls {
                for tc in tool_calls {
                    let index = tc.index.unwrap_or(0);
                    let state = tool_states.entry(index).or_default();

                    // First chunk: has id and name
                    if let Some(id) = &tc.id {
                        state.id = id.clone();
                    }
                    if let Some(name) = &tc.function.name {
                        state.name = name.clone();

                        // Emit ToolUseStart on first occurrence
                        if !state.started {
                            state.started = true;
                            results.push(StreamChunk::ToolUseStart {
                                index,
                                id: state.id.clone(),
                                name: state.name.clone(),
                            });
                        }
                    }

                    // Accumulate arguments
                    if !tc.function.arguments.is_empty() {
                        state.arguments_buffer.push_str(&tc.function.arguments);
                        results.push(StreamChunk::ToolUseInputDelta {
                            index,
                            partial_json: tc.function.arguments.clone(),
                        });
                    }
                }
            }

            // Handle finish_reason
            if let Some(finish_reason) = &choice.finish_reason {
                // Emit tool completions before done
                for (index, state) in tool_states.drain() {
                    if state.started {
                        results.push(StreamChunk::ToolUseComplete {
                            index,
                            tool_call: ToolCall {
                                id: state.id,
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name: state.name,
                                    arguments: state.arguments_buffer,
                                },
                            },
                        });
                    }
                }

                // Map finish_reason to unified stop_reason
                let stop_reason = match finish_reason.as_str() {
                    "tool_calls" => "tool_use",
                    "stop" => "end_turn",
                    "length" => "max_tokens",
                    other => other,
                };

                results.push(StreamChunk::Done {
                    stop_reason: stop_reason.to_string(),
                });
                done_emitted = true;
            }
        }

        // Handle usage metadata (typically in final chunk)
        if let Some(usage) = stream_chunk.usage {
            results.push(StreamChunk::Usage(usage));
        }
    }

    Ok(results)
}
