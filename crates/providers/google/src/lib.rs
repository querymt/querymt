//! Google Gemini API client implementation for chat and completion functionality.
//!
//! This module provides integration with Google's Gemini models through their API.
//! It implements chat, completion and embedding capabilities via the Gemini API.
//!
//! # Features
//! - Chat conversations with system prompts and message history
//! - Text completion requests
//! - Configuration options for temperature, tokens, top_p, top_k etc.
//! - Streaming support
//!
//! # Example
//! ```no_run
//! use llm::backends::google::Google;
//! use llm::chat::{ChatMessage, ChatRole, ChatProvider, Tool};
//!
//! #[tokio::main]
//! async fn main() {
//! let client = Google::new(
//!     "your-api-key",
//!     None, // Use default model
//!     Some(1000), // Max tokens
//!     Some(0.7), // Temperature
//!     None, // Default timeout
//!     None, // No system prompt
//!     None, // No streaming
//!     None, // Default top_p
//!     None, // Default top_k
//!     None, // No JSON schema
//!     None, // No tools
//! );
//!
//! let messages = vec![
//!     ChatMessage::user().content("Hello!").build()
//! ];
//!
//! let response = client.chat(&messages).await.unwrap();
//! println!("{}", response);
//! }
//! ```

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{header::CONTENT_TYPE, Method, Request, Response};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, ChatRole, MessageType,
        StructuredOutputFormat, Tool,
    },
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
    FunctionCall, HTTPLLMProvider, ToolCall,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Client for interacting with Google's Gemini API.
///
/// This struct holds the configuration and state needed to make requests to the Gemini API.
/// It implements the [`ChatProvider`], [`CompletionProvider`], and [`EmbeddingProvider`] traits.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Google {
    /// API key for authentication with Google's API
    pub api_key: String,
    /// Model identifier (e.g. "gemini-1.5-flash")
    pub model: String,
    /// Maximum number of tokens to generate in responses
    pub max_tokens: Option<u32>,
    /// Sampling temperature between 0.0 and 1.0
    pub temperature: Option<f32>,
    /// Optional system prompt to set context
    pub system: Option<String>,
    /// Request timeout in seconds
    pub timeout_seconds: Option<u64>,
    /// Whether to stream responses
    pub stream: Option<bool>,
    /// Top-p sampling parameter
    pub top_p: Option<f32>,
    /// Top-k sampling parameter
    pub top_k: Option<u32>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Available tools for function calling
    pub tools: Option<Vec<Tool>>,
}

/// Request body for chat completions
#[derive(Serialize)]
struct GoogleChatRequest<'a> {
    /// List of conversation messages
    contents: Vec<GoogleChatContent<'a>>,
    /// Optional generation parameters
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    generation_config: Option<GoogleGenerationConfig>,
    /// Tools that the model can use
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GoogleTool>>,
}

/// Individual message in a chat conversation
#[derive(Serialize)]
struct GoogleChatContent<'a> {
    /// Role of the message sender ("user", "model", or "system")
    role: &'a str,
    /// Content parts of the message
    parts: Vec<GoogleContentPart<'a>>,
}

/// Text content within a chat message
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum GoogleContentPart<'a> {
    /// The actual text content
    #[serde(rename = "text")]
    Text(&'a str),
    InlineData(GoogleInlineData),
    FunctionCall(GoogleFunctionCall),
    #[serde(rename = "functionResponse")]
    FunctionResponse(GoogleFunctionResponse),
}

#[derive(Serialize)]
struct GoogleInlineData {
    mime_type: String,
    data: String,
}

/// Configuration parameters for text generation
#[derive(Serialize)]
struct GoogleGenerationConfig {
    /// Maximum tokens to generate
    #[serde(skip_serializing_if = "Option::is_none", rename = "maxOutputTokens")]
    max_output_tokens: Option<u32>,
    /// Sampling temperature
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    /// Top-p sampling parameter
    #[serde(skip_serializing_if = "Option::is_none", rename = "topP")]
    top_p: Option<f32>,
    /// Top-k sampling parameter
    #[serde(skip_serializing_if = "Option::is_none", rename = "topK")]
    top_k: Option<u32>,
    /// The MIME type of the response
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<GoogleResponseMimeType>,
    /// A schema for structured output
    #[serde(skip_serializing_if = "Option::is_none")]
    response_schema: Option<Value>,
}

/// Response from the chat completion API
#[derive(Deserialize, Debug)]
struct GoogleChatResponse {
    /// Generated completion candidates
    candidates: Vec<GoogleCandidate>,
}

impl std::fmt::Display for GoogleChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Individual completion candidate
#[derive(Deserialize, Debug)]
struct GoogleCandidate {
    /// Content of the candidate response
    content: GoogleResponseContent,
}

/// Content block within a response
#[derive(Deserialize, Debug)]
struct GoogleResponseContent {
    /// Parts making up the content (might be absent when only function calls are present)
    #[serde(default)]
    parts: Vec<GoogleResponsePart>,
    /// Function calls if any are used - can be a single object or array
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GoogleFunctionCall>,
    /// Function calls as array (newer format in some responses)
    #[serde(skip_serializing_if = "Option::is_none")]
    function_calls: Option<Vec<GoogleFunctionCall>>,
}

impl ChatResponse for GoogleChatResponse {
    fn text(&self) -> Option<String> {
        self.candidates
            .first()
            .map(|c| c.content.parts.iter().map(|p| p.text.clone()).collect())
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.candidates.first().and_then(|c| {
            // First check for function calls at the part level (new API format)
            let part_function_calls: Vec<ToolCall> = c
                .content
                .parts
                .iter()
                .filter_map(|part| {
                    part.function_call.as_ref().map(|f| ToolCall {
                        id: format!("call_{}", f.name),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: f.name.clone(),
                            arguments: serde_json::to_string(&f.args).unwrap_or_default(),
                        },
                    })
                })
                .collect();

            if !part_function_calls.is_empty() {
                return Some(part_function_calls);
            }

            // Otherwise check for function_calls/function_call at the content level (older format)
            if let Some(fc) = &c.content.function_calls {
                // Process array of function calls
                Some(
                    fc.iter()
                        .map(|f| ToolCall {
                            id: format!("call_{}", f.name),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: f.name.clone(),
                                arguments: serde_json::to_string(&f.args).unwrap_or_default(),
                            },
                        })
                        .collect(),
                )
            } else {
                c.content.function_call.as_ref().map(|f| {
                    vec![ToolCall {
                        id: format!("call_{}", f.name),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: f.name.clone(),
                            arguments: serde_json::to_string(&f.args).unwrap_or_default(),
                        },
                    }]
                })
            }
        })
    }
}

/// Individual part of response content
#[derive(Deserialize, Debug)]
struct GoogleResponsePart {
    /// Text content of this part (may be absent if functionCall is present)
    #[serde(default)]
    text: String,
    /// Function call contained in this part
    #[serde(rename = "functionCall")]
    function_call: Option<GoogleFunctionCall>,
}

/// MIME type of the response
#[derive(Deserialize, Debug, Serialize)]
enum GoogleResponseMimeType {
    /// Plain text response
    #[serde(rename = "text/plain")]
    PlainText,
    /// JSON response
    #[serde(rename = "application/json")]
    Json,
    /// ENUM as a string response in the response candidates.
    #[serde(rename = "text/x.enum")]
    Enum,
}

/// Google's function calling tool definition
#[derive(Serialize, Debug)]
struct GoogleTool {
    /// The function declarations array
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GoogleFunctionDeclaration>,
}

/// Google function declaration, similar to OpenAI's function definition
#[derive(Serialize, Debug)]
struct GoogleFunctionDeclaration {
    /// Name of the function
    name: String,
    /// Description of what the function does
    description: String,
    /// Parameters for the function
    parameters: GoogleFunctionParameters,
}

impl From<&Tool> for GoogleFunctionDeclaration {
    fn from(tool: &Tool) -> Self {
        let properties_value = serde_json::to_value(&tool.function.parameters.properties)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

        GoogleFunctionDeclaration {
            name: tool.function.name.clone(),
            description: tool.function.description.clone(),
            parameters: GoogleFunctionParameters {
                schema_type: "object".to_string(),
                properties: properties_value,
                required: tool.function.parameters.required.clone(),
            },
        }
    }
}

/// Google function parameters schema
#[derive(Serialize, Debug)]
struct GoogleFunctionParameters {
    /// The type of parameters object (usually "object")
    #[serde(rename = "type")]
    schema_type: String,
    /// Map of parameter names to their properties
    properties: Value,
    /// List of required parameter names
    required: Vec<String>,
}

/// Google function call object in response
#[derive(Deserialize, Debug, Serialize)]
struct GoogleFunctionCall {
    /// Name of the function to call
    name: String,
    /// Arguments for the function call as structured JSON
    #[serde(default)]
    args: Value,
}

/// Google function response wrapper for function results
///
/// Format follows Google's Gemini API specification for function calling results:
/// https://ai.google.dev/docs/function_calling
///
/// The expected format is:
/// {
///   "role": "function",
///   "parts": [{
///     "functionResponse": {
///       "name": "function_name",
///       "response": {
///         "name": "function_name",
///         "content": { ... } // JSON content returned by the function
///       }
///     }
///   }]
/// }
#[derive(Deserialize, Debug, Serialize)]
struct GoogleFunctionResponse {
    /// Name of the function that was called
    name: String,
    /// Response from the function as structured JSON
    response: GoogleFunctionResponseContent,
}

#[derive(Deserialize, Debug, Serialize)]
struct GoogleFunctionResponseContent {
    /// Name of the function that was called
    name: String,
    /// Content of the function response
    content: Value,
}

/// Request body for embedding content
#[derive(Serialize)]
struct GoogleEmbeddingRequest<'a> {
    model: &'a str,
    content: GoogleEmbeddingContent<'a>,
}

#[derive(Serialize)]
struct GoogleEmbeddingContent<'a> {
    parts: Vec<GoogleContentPart<'a>>,
}

/// Response from the embedding API
#[derive(Deserialize)]
struct GoogleEmbeddingResponse {
    embedding: GoogleEmbedding,
}

#[derive(Deserialize)]
struct GoogleEmbedding {
    values: Vec<f32>,
}

impl Google {
    fn default_base_url() -> Url {
        Url::parse("https://generativelanguage.googleapis.com/v1beta/models/").unwrap()
    }
}

impl HTTPChatProvider for Google {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        if self.api_key.is_empty() {
            return Err(LLMError::AuthError("Missing Google API key".into()));
        }

        let mut chat_contents = Vec::with_capacity(messages.len());

        // Add system message if present
        if let Some(system) = &self.system {
            chat_contents.push(GoogleChatContent {
                role: "user",
                parts: vec![GoogleContentPart::Text(system)],
            });
        }

        // Add conversation messages in pairs to maintain context
        for msg in messages {
            // For tool results, we need to use "function" role
            let role = match &msg.message_type {
                MessageType::ToolResult(_) => "function",
                _ => match msg.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "model",
                },
            };

            chat_contents.push(GoogleChatContent {
                role,
                parts: match &msg.message_type {
                    MessageType::Text => vec![GoogleContentPart::Text(&msg.content)],
                    MessageType::Image((image_mime, raw_bytes)) => {
                        vec![GoogleContentPart::InlineData(GoogleInlineData {
                            mime_type: image_mime.mime_type().to_string(),
                            data: BASE64.encode(raw_bytes),
                        })]
                    }
                    MessageType::ImageURL(_) => unimplemented!(),
                    MessageType::Pdf(raw_bytes) => {
                        vec![GoogleContentPart::InlineData(GoogleInlineData {
                            mime_type: "application/pdf".to_string(),
                            data: BASE64.encode(raw_bytes),
                        })]
                    }
                    MessageType::ToolUse(calls) => calls
                        .iter()
                        .map(|call| {
                            GoogleContentPart::FunctionCall(GoogleFunctionCall {
                                name: call.function.name.clone(),
                                args: serde_json::from_str(&call.function.arguments)
                                    .unwrap_or(serde_json::Value::Null),
                            })
                        })
                        .collect(),
                    MessageType::ToolResult(result) => result
                        .iter()
                        .map(|result| {
                            let parsed_args =
                                serde_json::from_str::<Value>(&result.function.arguments)
                                    .unwrap_or(serde_json::Value::Null);

                            GoogleContentPart::FunctionResponse(GoogleFunctionResponse {
                                name: result.function.name.clone(),
                                response: GoogleFunctionResponseContent {
                                    name: result.function.name.clone(),
                                    content: parsed_args,
                                },
                            })
                        })
                        .collect(),
                },
            });
        }

        // Convert tools to Google's format if provided
        let google_tools = tools.map(|t| {
            vec![GoogleTool {
                function_declarations: t.iter().map(GoogleFunctionDeclaration::from).collect(),
            }]
        });

        // Build generation config
        let generation_config = {
            // If json_schema and json_schema.schema are not None, use json_schema.schema as the response schema and set response_mime_type to JSON
            // Google's API doesn't need the schema to have a "name" field, so we can just use the schema directly.
            let (response_mime_type, response_schema) = if let Some(json_schema) = &self.json_schema
            {
                if let Some(schema) = &json_schema.schema {
                    // If the schema has an "additionalProperties" field (as required by OpenAI), remove it as Google's API doesn't support it
                    let mut schema = schema.clone();

                    if let Some(obj) = schema.as_object_mut() {
                        obj.remove("additionalProperties");
                    }

                    (Some(GoogleResponseMimeType::Json), Some(schema))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

            Some(GoogleGenerationConfig {
                max_output_tokens: self.max_tokens,
                temperature: self.temperature,
                top_p: self.top_p,
                top_k: self.top_k,
                response_mime_type,
                response_schema,
            })
        };

        let req_body = GoogleChatRequest {
            contents: chat_contents,
            generation_config,
            tools: google_tools,
        };

        let json_body = serde_json::to_vec(&req_body)?;

        let path = format!(
            "{}{}:generateContent",
            Google::default_base_url().path(),
            &self.model
        );
        let mut url = Google::default_base_url()
            .join(&path)
            .map_err(|e| LLMError::HttpError(e.to_string()))?;
        url.set_query(Some(&format!("key={}", &self.api_key)));

        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(json_body)?)
    }

    fn parse_chat(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Box<dyn ChatResponse>, Box<dyn std::error::Error>> {
        if !resp.status().is_success() {
            let status = resp.status();
            let error_text: String = "".to_string();
            return Err(Box::new(LLMError::ResponseFormatError {
                message: format!("API returned error status: {}", status),
                raw_response: error_text,
            }));
        }

        let json_resp: Result<GoogleChatResponse, serde_json::Error> =
            serde_json::from_slice(&resp.body());

        match json_resp {
            Ok(response) => Ok(Box::new(response)),
            Err(e) => {
                // Return a more descriptive error with the raw response
                Err(Box::new(LLMError::ResponseFormatError {
                    message: format!("Failed to decode Google API response: {}", e),
                    raw_response: String::from_utf8(resp.into_body())?,
                }))
            }
        }
    }
}

impl HTTPCompletionProvider for Google {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let chat_message = ChatMessage::user().content(req.prompt.clone()).build();
        self.chat_request(&[chat_message], None)
    }

    fn parse_complete(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error>> {
        let chat_response = self.parse_chat(resp)?;
        if let Some(text) = chat_response.text() {
            Ok(CompletionResponse { text })
        } else {
            Err(Box::new(LLMError::ProviderError(
                "No answer returned by Google".to_string(),
            )))
        }
    }
}

impl HTTPEmbeddingProvider for Google {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        if self.api_key.is_empty() {
            return Err(LLMError::AuthError("Missing Google API key".to_string()));
        }
        let embedding_model = "text-embedding-004";

        //let mut embeddings = Vec::new();

        // Process each text separately as Gemini API accepts one text at a time
        let mut json_body;
        for text in inputs {
            let req_body = GoogleEmbeddingRequest {
                model: "models/text-embedding-004",
                content: GoogleEmbeddingContent {
                    parts: vec![GoogleContentPart::Text(&text)],
                },
            };
            json_body = serde_json::to_vec(&req_body)?;
        }

        let mut url = Google::default_base_url()
            .join(embedding_model)
            .map_err(|e| LLMError::HttpError(e.to_string()))?
            .join(":embedContent")
            .map_err(|e| LLMError::HttpError(e.to_string()))?;
        url.set_query(Some(&format!("key={}", &self.api_key)));

        Err(LLMError::ProviderError("asd".to_string()))
        /*
        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(json_body)?)
            */
    }

    fn parse_embed(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        let embedding_resp: GoogleEmbeddingResponse = serde_json::from_slice(resp.body())?;
        let x = embedding_resp.embedding.values;
        //Ok(embedding_resp.embedding.values)
        Err(Box::new(LLMError::ProviderError("asd".to_string())))
    }
}

impl HTTPLLMProvider for Google {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

struct GoogleFactory;
impl HTTPLLMProviderFactory for GoogleFactory {
    fn name(&self) -> &str {
        "google"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("GEMINI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let mut base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Google::default_base_url(),
        };

        match cfg.get("api_key").and_then(Value::as_str) {
            Some(api_key) => {
                base_url.set_query(Some(&format!("key={}", api_key)));

                Ok(Request::builder()
                    .method(Method::GET)
                    .header(CONTENT_TYPE, "application/json")
                    .uri(base_url.as_str())
                    .body(Vec::new())?)
            }
            None => Err(LLMError::AuthError("Missing Google API key".into())),
        }
    }

    fn parse_list_models(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let resp_json: Value = serde_json::from_slice(&resp.body())?;
        let arr = resp_json
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| LLMError::InvalidRequest("`models` missing or not an array".into()))?;

        let names = arr
            .iter()
            .filter_map(|m| m.get("name"))
            .filter_map(Value::as_str)
            .filter_map(|v| v.strip_prefix("models/"))
            .map(String::from)
            .collect();
        Ok(names)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Google);
        serde_json::to_value(&schema.schema).expect("Google JSON Schema should always serialize")
    }

    fn from_config(
        &self,
        cfg: &Value,
    ) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn std::error::Error>> {
        let provider: Google = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("Google config error: {}", e)))?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(GoogleFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Google, GoogleFactory};
    use querymt::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Google,
        factory = GoogleFactory,
        name   = "google",
    }
}
