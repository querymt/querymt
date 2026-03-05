use async_trait::async_trait;
use schemars::schema::{
    InstanceType, Metadata, ObjectValidation, Schema, SchemaObject, SingleOrVec,
};
use schemars::{gen::SchemaGenerator, JsonSchema};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

use crate::{error::LLMError, ToolCall, Usage};
use futures::Stream;
use std::pin::Pin;

pub mod http;

// ---------------------------------------------------------------------------
// Content — a single content block within a message
// ---------------------------------------------------------------------------

/// A content block within a message.
///
/// Messages are composed of one or more `Content` blocks, allowing mixed content
/// such as text, images, tool calls, and tool results within a single message.
/// This aligns with how major LLM APIs (Anthropic, OpenAI, Google, MCP) model
/// message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text
    Text { text: String },
    /// Base64-encoded image
    Image { mime_type: String, data: Vec<u8> },
    /// Image referenced by URL
    ImageUrl { url: String },
    /// PDF document
    Pdf { data: Vec<u8> },
    /// Audio data
    Audio { mime_type: String, data: Vec<u8> },
    /// Model reasoning / chain-of-thought
    Thinking { text: String },
    /// Tool invocation requested by the model
    ToolUse {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// Result of a tool invocation (can itself contain mixed content)
    ToolResult {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        is_error: bool,
        content: Vec<Content>,
    },
    /// A link to a resource, identified by URI.
    /// Carries optional metadata (name, description, MIME type).
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

impl Content {
    /// Create a text content block.
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into() }
    }

    /// Create an image content block.
    pub fn image(mime: impl Into<String>, data: Vec<u8>) -> Self {
        Content::Image {
            mime_type: mime.into(),
            data,
        }
    }

    /// Create an image URL content block.
    pub fn image_url(url: impl Into<String>) -> Self {
        Content::ImageUrl { url: url.into() }
    }

    /// Create a PDF content block.
    pub fn pdf(data: Vec<u8>) -> Self {
        Content::Pdf { data }
    }

    /// Create an audio content block.
    pub fn audio(mime: impl Into<String>, data: Vec<u8>) -> Self {
        Content::Audio {
            mime_type: mime.into(),
            data,
        }
    }

    /// Create a thinking content block.
    pub fn thinking(s: impl Into<String>) -> Self {
        Content::Thinking { text: s.into() }
    }

    /// Create a tool use content block.
    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Content::ToolUse {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    /// Create a tool result content block.
    pub fn tool_result(id: impl Into<String>, content: Vec<Content>) -> Self {
        Content::ToolResult {
            id: id.into(),
            name: None,
            is_error: false,
            content,
        }
    }

    /// Create a resource link content block.
    pub fn resource_link(uri: impl Into<String>) -> Self {
        Content::ResourceLink {
            uri: uri.into(),
            name: None,
            description: None,
            mime_type: None,
        }
    }

    /// Create an error tool result content block.
    pub fn tool_result_error(id: impl Into<String>, content: Vec<Content>) -> Self {
        Content::ToolResult {
            id: id.into(),
            name: None,
            is_error: true,
            content,
        }
    }

    /// Returns the text if this is a `Text` block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Content::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Returns true if this is a `ToolUse` block.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, Content::ToolUse { .. })
    }

    /// Returns true if this is a `ToolResult` block.
    pub fn is_tool_result(&self) -> bool {
        matches!(self, Content::ToolResult { .. })
    }
}

impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Content::Text { text: a }, Content::Text { text: b }) => a == b,
            (
                Content::Image {
                    mime_type: ma,
                    data: da,
                },
                Content::Image {
                    mime_type: mb,
                    data: db,
                },
            ) => ma == mb && da == db,
            (Content::ImageUrl { url: a }, Content::ImageUrl { url: b }) => a == b,
            (Content::Pdf { data: a }, Content::Pdf { data: b }) => a == b,
            (
                Content::Audio {
                    mime_type: ma,
                    data: da,
                },
                Content::Audio {
                    mime_type: mb,
                    data: db,
                },
            ) => ma == mb && da == db,
            (Content::Thinking { text: a }, Content::Thinking { text: b }) => a == b,
            (
                Content::ToolUse {
                    id: ia,
                    name: na,
                    arguments: aa,
                },
                Content::ToolUse {
                    id: ib,
                    name: nb,
                    arguments: ab,
                },
            ) => ia == ib && na == nb && aa == ab,
            (
                Content::ToolResult {
                    id: ia,
                    name: na,
                    is_error: ea,
                    content: ca,
                },
                Content::ToolResult {
                    id: ib,
                    name: nb,
                    is_error: eb,
                    content: cb,
                },
            ) => ia == ib && na == nb && ea == eb && ca == cb,
            (
                Content::ResourceLink {
                    uri: ua,
                    name: na,
                    description: da,
                    mime_type: ma,
                },
                Content::ResourceLink {
                    uri: ub,
                    name: nb,
                    description: db,
                    mime_type: mb,
                },
            ) => ua == ub && na == nb && da == db && ma == mb,
            _ => false,
        }
    }
}

impl Eq for Content {}

impl fmt::Display for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Content::Text { text } => write!(f, "{}", text),
            Content::Image { mime_type, data } => {
                write!(f, "[Image: {}, {} bytes]", mime_type, data.len())
            }
            Content::ImageUrl { url } => write!(f, "[Image URL: {}]", url),
            Content::Pdf { data } => write!(f, "[PDF: {} bytes]", data.len()),
            Content::Audio { mime_type, data } => {
                write!(f, "[Audio: {}, {} bytes]", mime_type, data.len())
            }
            Content::Thinking { text } => write!(f, "[Thinking: {}]", text),
            Content::ToolUse { id, name, .. } => write!(f, "[ToolUse: {} ({})]", name, id),
            Content::ToolResult {
                id,
                is_error,
                content,
                ..
            } => {
                let label = if *is_error { "ToolError" } else { "ToolResult" };
                write!(f, "[{}: {}, {} blocks]", label, id, content.len())
            }
            Content::ResourceLink { uri, name, .. } => {
                if let Some(name) = name {
                    write!(f, "[Resource: {} ({})]", name, uri)
                } else {
                    write!(f, "[Resource: {}]", uri)
                }
            }
        }
    }
}

/// Extract `<think>...</think>` blocks from text, returning (thinking, clean_content).
///
/// This handles the common pattern where local models (Qwen3, DeepSeek, QwQ)
/// output `<think>...</think>` inline in their response text.
///
/// Returns `(thinking_content, clean_content)` where:
/// - `thinking_content` is `Some(reasoning)` if `<think>` blocks were found, `None` otherwise
/// - `clean_content` is the text with all `<think>...</think>` blocks removed and trimmed
///
/// # Examples
///
/// ```
/// use querymt::chat::extract_thinking;
///
/// let (thinking, content) = extract_thinking("<think>reasoning here</think>\n\nHello!");
/// assert_eq!(thinking, Some("reasoning here".to_string()));
/// assert_eq!(content, "Hello!");
///
/// let (thinking, content) = extract_thinking("No thinking here");
/// assert_eq!(thinking, None);
/// assert_eq!(content, "No thinking here");
/// ```
pub fn extract_thinking(text: &str) -> (Option<String>, String) {
    const OPEN_TAG: &str = "<think>";
    const CLOSE_TAG: &str = "</think>";

    let mut thinking_parts = Vec::new();
    let mut clean_parts = Vec::new();
    let mut remaining = text;

    loop {
        match remaining.find(OPEN_TAG) {
            Some(open_pos) => {
                // Add text before the <think> tag to clean parts
                let before = &remaining[..open_pos];
                if !before.is_empty() {
                    clean_parts.push(before);
                }

                let after_open = &remaining[open_pos + OPEN_TAG.len()..];
                match after_open.find(CLOSE_TAG) {
                    Some(close_pos) => {
                        // Found a complete <think>...</think> block
                        let thinking_content = &after_open[..close_pos];
                        let trimmed = thinking_content.trim();
                        if !trimmed.is_empty() {
                            thinking_parts.push(trimmed.to_string());
                        }
                        remaining = &after_open[close_pos + CLOSE_TAG.len()..];
                    }
                    None => {
                        // Unclosed <think> tag — treat the rest as thinking content
                        let thinking_content = after_open.trim();
                        if !thinking_content.is_empty() {
                            thinking_parts.push(thinking_content.to_string());
                        }
                        break;
                    }
                }
            }
            None => {
                // No more <think> tags
                if !remaining.is_empty() {
                    clean_parts.push(remaining);
                }
                break;
            }
        }
    }

    if thinking_parts.is_empty() {
        (None, text.to_string())
    } else {
        let thinking = thinking_parts.join("\n\n");
        let clean = clean_parts.join("").trim().to_string();
        (Some(thinking), clean)
    }
}

/// Role of a participant in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatRole {
    /// The user/human participant in the conversation
    User,
    /// The AI assistant participant in the conversation
    Assistant,
}

/// Cache hint for providers that support prompt caching.
/// When set on a message, the provider may use it to mark cache breakpoints,
/// allowing the conversation prefix up to this point to be cached and reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    /// Ephemeral cache breakpoint. Providers that support caching (e.g., Anthropic)
    /// will cache the conversation prefix up to and including this message.
    /// The optional TTL specifies cache lifetime in seconds.
    /// If None, the provider uses its default TTL.
    Ephemeral {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_seconds: Option<u64>,
    },
}

/// The type of reasoning effort for a message in a chat conversation.
pub enum ReasoningEffort {
    /// Low reasoning effort
    Low,
    /// Medium reasoning effort
    Medium,
    /// High reasoning effort
    High,
}

/// A single message in a chat conversation.
///
/// Messages contain a role (user or assistant) and a vector of `Content` blocks,
/// allowing mixed content such as text, images, tool calls, and tool results
/// within a single message. This aligns with how major LLM APIs model messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// The role of who sent this message (user or assistant)
    pub role: ChatRole,
    /// Content blocks for this message.
    pub content: Vec<Content>,
    /// Optional cache hint. Providers that support caching (e.g., Anthropic)
    /// will translate this into provider-specific cache breakpoint markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
}

/// Represents a parameter in a function tool
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ParameterProperty {
    /// The type of the parameter (e.g. "string", "number", "array", etc)
    #[serde(rename = "type")]
    pub property_type: String,
    /// Description of what the parameter does
    pub description: String,
    /// When type is "array", this defines the type of the array items
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ParameterProperty>>,
    /// When type is "enum", this defines the possible values for the parameter
    #[serde(skip_serializing_if = "Option::is_none", rename = "enum")]
    pub enum_list: Option<Vec<String>>,
}

/// Represents the parameters schema for a function tool
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ParametersSchema {
    /// The type of the parameters object (usually "object")
    #[serde(rename = "type")]
    pub schema_type: String,
    /// Map of parameter names to their properties
    pub properties: HashMap<String, ParameterProperty>,
    /// List of required parameter names
    pub required: Vec<String>,
}

/// Represents a function definition for a tool
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FunctionTool {
    /// The name of the function
    pub name: String,
    /// Description of what the function does
    pub description: String,
    /// The parameters schema for the function
    pub parameters: Value,
}

/// Defines rules for structured output responses based on [OpenAI's structured output requirements](https://platform.openai.com/docs/api-reference/chat/create#chat-create-response_format).
/// Individual providers may have additional requirements or restrictions, but these should be handled by each provider's backend implementation.
///
/// If you plan on deserializing into this struct, make sure the source text has a `"name"` field, since that's technically the only thing required by OpenAI.
///
/// ## Example
///
/// ```
/// use llm::chat::StructuredOutputFormat;
/// use serde_json::json;
///
/// let response_format = r#"
///     {
///         "name": "Student",
///         "description": "A student object",
///         "schema": {
///             "type": "object",
///             "properties": {
///                 "name": {
///                     "type": "string"
///                 },
///                 "age": {
///                     "type": "integer"
///                 },
///                 "is_student": {
///                     "type": "boolean"
///                 }
///             },
///             "required": ["name", "age", "is_student"]
///         }
///     }
/// "#;
/// let structured_output: StructuredOutputFormat = serde_json::from_str(response_format).unwrap();
/// assert_eq!(structured_output.name, "Student");
/// assert_eq!(structured_output.description, Some("A student object".to_string()));
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
pub struct StructuredOutputFormat {
    /// Name of the schema
    pub name: String,
    /// The description of the schema
    pub description: Option<String>,
    /// The JSON schema for the structured output
    pub schema: Option<Value>,
    /// Whether to enable strict schema adherence
    pub strict: Option<bool>,
}

/// Represents a tool that can be used in chat
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Tool {
    /// The type of tool (e.g. "function")
    #[serde(rename = "type")]
    pub tool_type: String,
    /// The function definition if this is a function tool
    pub function: FunctionTool,
}

/// Compile-time ABI guard: ensures Tool and FunctionTool struct sizes are consistent
/// across all compilation units (host binary and cdylib plugins).
///
/// This catches serde_json feature mismatches where `preserve_order` changes
/// `serde_json::Value` from 32 bytes (BTreeMap) to 72 bytes (IndexMap on 64-bit,
/// 48 bytes on 32-bit), which propagates through FunctionTool.parameters and causes
/// ABI incompatibility.
///
/// See: commit d893ffaee7637b6a673e72002772b77ead019382 (LLMProviderFactory fix)
/// and this fix that pins serde_json features at the workspace level.
const _: () = {
    // Calculate expected sizes from actual component sizes, accounting for alignment.
    // This handles different pointer widths (64-bit native vs 32-bit WASM).
    const STRING_SIZE: usize = std::mem::size_of::<String>();
    const VALUE_SIZE: usize = std::mem::size_of::<Value>();
    const VALUE_ALIGN: usize = std::mem::align_of::<Value>();

    // Helper to round up to next multiple of alignment
    const fn align_up(size: usize, align: usize) -> usize {
        (size + align - 1) & !(align - 1)
    }

    // FunctionTool = name (String) + description (String) + parameters (Value)
    // No padding needed: String fields are adjacent, then Value at end
    const EXPECTED_FUNCTION_TOOL_SIZE: usize = STRING_SIZE + STRING_SIZE + VALUE_SIZE;

    // Tool = tool_type (String) + function (FunctionTool)
    // Need to align String to FunctionTool's alignment (which matches Value's alignment)
    const EXPECTED_TOOL_SIZE: usize =
        align_up(STRING_SIZE, VALUE_ALIGN) + EXPECTED_FUNCTION_TOOL_SIZE;

    // Verify preserve_order is enabled by checking Value uses IndexMap (larger than BTreeMap).
    // With preserve_order: Value = 72 bytes on 64-bit, 48 bytes on 32-bit
    // Without preserve_order: Value = 32 bytes on 64-bit (BTreeMap is smaller)
    const MIN_VALUE_SIZE_FOR_PRESERVE_ORDER: usize = std::mem::size_of::<usize>() * 6;
    assert!(
        VALUE_SIZE >= MIN_VALUE_SIZE_FOR_PRESERVE_ORDER,
        "serde_json::Value too small - preserve_order feature may be disabled!"
    );

    assert!(
        std::mem::size_of::<Tool>() == EXPECTED_TOOL_SIZE,
        "Tool size mismatch! Unexpected struct layout change."
    );
    assert!(
        std::mem::size_of::<FunctionTool>() == EXPECTED_FUNCTION_TOOL_SIZE,
        "FunctionTool size mismatch! Unexpected struct layout change."
    );
};

/// Tool choice determines how the LLM uses available tools.
/// The behavior is standardized across different LLM providers.
#[derive(Debug, Clone, Default)]
pub enum ToolChoice {
    /// Model can use any tool, but it must use at least one.
    /// This is useful when you want to force the model to use tools.
    Any,

    /// Model can use any tool, and may elect to use none.
    /// This is the default behavior and gives the model flexibility.
    #[default]
    Auto,

    /// Model must use the specified tool and only the specified tool.
    /// The string parameter is the name of the required tool.
    /// This is useful when you want the model to call a specific function.
    Tool(String),

    /// Explicitly disables the use of tools.
    /// The model will not use any tools even if they are provided.
    None,
}

impl Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ToolChoice::Any => serializer.serialize_str("required"),
            ToolChoice::Auto => serializer.serialize_str("auto"),
            ToolChoice::None => serializer.serialize_str("none"),
            ToolChoice::Tool(name) => {
                use serde::ser::SerializeMap;

                // For tool_choice: {"type": "function", "function": {"name": "function_name"}}
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;

                // Inner function object
                let mut function_obj = std::collections::HashMap::new();
                function_obj.insert("name", name.as_str());

                map.serialize_entry("function", &function_obj)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ToolChoiceVisitor;

        impl<'de> Visitor<'de> for ToolChoiceVisitor {
            type Value = ToolChoice;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string (`required`, `auto`, `none`) or an object `{ type: \"function\", function: { name: ... } }`")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "required" => Ok(ToolChoice::Any),
                    "auto" => Ok(ToolChoice::Auto),
                    "none" => Ok(ToolChoice::None),
                    other => Err(de::Error::unknown_variant(
                        other,
                        &["required", "auto", "none"],
                    )),
                }
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut seen_name: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => {
                            let t: String = map.next_value()?;
                            if t != "function" {
                                return Err(de::Error::invalid_value(
                                    de::Unexpected::Str(&t),
                                    &"function",
                                ));
                            }
                        }
                        "function" => {
                            // function is an object with a `name` field
                            let func_map: serde_json::Map<String, serde_json::Value> =
                                map.next_value()?;
                            if let Some(serde_json::Value::String(name)) = func_map.get("name") {
                                seen_name = Some(name.clone());
                            } else {
                                return Err(de::Error::missing_field("name"));
                            }
                        }
                        _ => {
                            // skip unexpected keys
                            let _ignored: serde_json::Value = map.next_value()?;
                        }
                    }
                }
                // ensure we got a function name
                let name = seen_name.ok_or_else(|| de::Error::missing_field("function"))?;
                Ok(ToolChoice::Tool(name))
            }
        }

        deserializer.deserialize_any(ToolChoiceVisitor)
    }
}

impl JsonSchema for ToolChoice {
    fn schema_name() -> String {
        "ToolChoice".to_string()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        // string variant schema
        let str_schema = SchemaObject {
            instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "One of the string options: \"required\", \"auto\", \"none\"".to_string(),
                ),
                ..Default::default()
            })),
            enum_values: Some(vec![
                serde_json::Value::String("required".to_string()),
                serde_json::Value::String("auto".to_string()),
                serde_json::Value::String("none".to_string()),
            ]),
            ..Default::default()
        };

        // function object schema
        let mut func_obj = ObjectValidation::default();
        func_obj.required.insert("type".to_string());
        func_obj.required.insert("function".to_string());

        // "type": "function"
        func_obj.properties.insert(
            "type".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                enum_values: Some(vec![serde_json::Value::String("function".to_string())]),
                ..Default::default()
            }),
        );

        // "function": { name: string }
        let mut inner = ObjectValidation::default();
        inner.required.insert("name".to_string());
        inner.properties.insert(
            "name".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
                ..Default::default()
            }),
        );
        func_obj.properties.insert(
            "function".to_string(),
            Schema::Object(SchemaObject {
                instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Object))),
                object: Some(Box::new(inner)),
                ..Default::default()
            }),
        );

        // combine via anyOf
        let mut schema = SchemaObject::default();
        schema.subschemas = Some(Box::new(schemars::schema::SubschemaValidation {
            any_of: Some(vec![
                Schema::Object(str_schema),
                Schema::Object(SchemaObject {
                    instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::Object))),
                    object: Some(Box::new(func_obj)),
                    ..Default::default()
                }),
            ]),
            ..Default::default()
        }));

        Schema::Object(schema)
    }
}

pub trait ChatResponse: std::fmt::Debug + std::fmt::Display + Send {
    fn text(&self) -> Option<String>;
    fn tool_calls(&self) -> Option<Vec<ToolCall>>;
    fn finish_reason(&self) -> Option<FinishReason>;
    fn thinking(&self) -> Option<String> {
        None
    }
    fn usage(&self) -> Option<Usage>;
}

impl From<&dyn ChatResponse> for ChatMessage {
    fn from(response: &dyn ChatResponse) -> Self {
        let mut content = Vec::new();

        if let Some(t) = response.thinking() {
            if !t.is_empty() {
                content.push(Content::thinking(t));
            }
        }
        if let Some(text) = response.text() {
            if !text.is_empty() {
                content.push(Content::text(text));
            }
        }
        if let Some(calls) = response.tool_calls() {
            for call in calls {
                content.push(Content::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                });
            }
        }

        ChatMessage {
            role: ChatRole::Assistant,
            content,
            cache: None,
        }
    }
}

impl From<Box<dyn ChatResponse>> for ChatMessage {
    fn from(response: Box<dyn ChatResponse>) -> Self {
        ChatMessage::from(response.as_ref())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamChunk {
    /// Text content delta
    Text(String),

    /// Thinking/reasoning content delta from the model.
    /// This is emitted separately from `Text` so consumers can display or
    /// store reasoning content differently (e.g., dimmed text, separate field).
    Thinking(String),

    /// Tool use block started (contains tool id and name)
    ToolUseStart {
        /// The index of this content block in the response
        index: usize,
        /// The unique ID for this tool use
        id: String,
        /// The name of the tool being called
        name: String,
    },

    /// Tool use input JSON delta (partial JSON string)
    ToolUseInputDelta {
        /// The index of this content block
        index: usize,
        /// Partial JSON string for the tool input
        partial_json: String,
    },

    /// Tool use block complete with assembled ToolCall
    ToolUseComplete {
        /// The index of this content block
        index: usize,
        /// The complete tool call with id, name, and parsed arguments
        tool_call: ToolCall,
    },

    /// Usage metadata containing token counts
    Usage(Usage),

    /// Stream ended with stop reason
    Done {
        /// The reason the stream stopped (e.g., "end_turn", "tool_use")
        stop_reason: String,
    },
}

/// Unified ChatProvider trait that combines all chat capabilities.
///
/// This trait provides a single interface for both synchronous and streaming chat interactions,
/// with or without tool support. Providers can implement the methods they support and rely on
/// default implementations for others.
///
/// # Examples
///
/// ## Basic usage without tools
/// ```rust,ignore
/// let response = provider.chat(&messages).await?;
/// ```
///
/// ## With tools
/// ```rust,ignore
/// let response = provider.chat_with_tools(&messages, Some(&tools)).await?;
/// ```
///
/// ## Streaming
/// ```rust,ignore
/// let mut stream = provider.chat_stream(&messages).await?;
/// while let Some(chunk) = stream.next().await {
///     // Process chunk
/// }
/// ```
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Returns true if the provider supports streaming responses.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// Basic chat interaction without tools.
    ///
    /// This is a convenience method that delegates to `chat_with_tools` with `None` for tools.
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.chat_with_tools(messages, None).await
    }

    /// Chat interaction with tools.
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history
    /// * `tools` - Optional list of tools available to the model. Pass `None` to disable tools
    ///   for this specific call, even if the provider has tools configured.
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError>;

    /// Basic streaming chat interaction.
    ///
    /// This is a convenience method that delegates to `chat_stream_with_tools` with `None` for tools.
    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>, LLMError> {
        self.chat_stream_with_tools(messages, None).await
    }

    /// Streaming chat interaction with tools.
    ///
    /// Returns a stream of `StreamChunk` events which can include text deltas, tool use events,
    /// and completion signals.
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history
    /// * `tools` - Optional list of tools available to the model
    ///
    /// # Default Implementation
    ///
    /// By default, this returns a `NotImplemented` error. Providers that support streaming
    /// should override this method.
    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk, LLMError>> + Send>>, LLMError> {
        let _ = (messages, tools);
        Err(LLMError::NotImplemented(
            "Streaming with tools not supported by this provider".into(),
        ))
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasoningEffort::Low => write!(f, "low"),
            ReasoningEffort::Medium => write!(f, "medium"),
            ReasoningEffort::High => write!(f, "high"),
        }
    }
}

impl ChatMessage {
    /// Create a new builder for a user message.
    pub fn user() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::User)
    }

    /// Create a new builder for an assistant message.
    pub fn assistant() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::Assistant)
    }

    /// Convenience: create a user message from content blocks.
    pub fn from_user(content: Vec<Content>) -> Self {
        ChatMessage {
            role: ChatRole::User,
            content,
            cache: None,
        }
    }

    /// Convenience: create an assistant message from content blocks.
    pub fn from_assistant(content: Vec<Content>) -> Self {
        ChatMessage {
            role: ChatRole::Assistant,
            content,
            cache: None,
        }
    }

    /// Extract concatenated text from all `Content::Text` blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Check if the message contains any `Content::ToolUse` blocks.
    pub fn has_tool_use(&self) -> bool {
        self.content.iter().any(|b| b.is_tool_use())
    }

    /// Extract all `Content::ToolUse` blocks.
    pub fn tool_uses(&self) -> Vec<&Content> {
        self.content.iter().filter(|b| b.is_tool_use()).collect()
    }

    /// Check if the message contains any `Content::ToolResult` blocks.
    pub fn has_tool_result(&self) -> bool {
        self.content.iter().any(|b| b.is_tool_result())
    }

    /// Extract the first thinking block text, if any.
    pub fn thinking(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            Content::Thinking { text } => Some(text.as_str()),
            _ => None,
        })
    }
}

/// Builder for ChatMessage.
///
/// Accumulates `Content` blocks and produces a `ChatMessage`.
#[derive(Debug)]
pub struct ChatMessageBuilder {
    role: ChatRole,
    content: Vec<Content>,
    cache: Option<CacheHint>,
}

impl ChatMessageBuilder {
    /// Create a new ChatMessageBuilder with specified role.
    pub fn new(role: ChatRole) -> Self {
        Self {
            role,
            content: Vec::new(),
            cache: None,
        }
    }

    /// Append a text content block. If called multiple times, multiple text blocks are added.
    pub fn text(mut self, s: impl Into<String>) -> Self {
        self.content.push(Content::text(s));
        self
    }

    /// Append a thinking/reasoning content block.
    /// Empty strings are ignored.
    pub fn thinking(mut self, s: impl Into<String>) -> Self {
        let t = s.into();
        if !t.is_empty() {
            self.content.push(Content::thinking(t));
        }
        self
    }

    /// Append an image content block.
    pub fn image(mut self, mime: impl Into<String>, data: Vec<u8>) -> Self {
        self.content.push(Content::image(mime, data));
        self
    }

    /// Append an image URL content block.
    pub fn image_url(mut self, url: impl Into<String>) -> Self {
        self.content.push(Content::image_url(url));
        self
    }

    /// Append a PDF content block.
    pub fn pdf(mut self, data: Vec<u8>) -> Self {
        self.content.push(Content::pdf(data));
        self
    }

    /// Append a tool use content block.
    pub fn tool_use(mut self, id: String, name: String, args: Value) -> Self {
        self.content.push(Content::tool_use(id, name, args));
        self
    }

    /// Append a tool result content block.
    pub fn tool_result(
        mut self,
        id: String,
        name: Option<String>,
        is_error: bool,
        inner: Vec<Content>,
    ) -> Self {
        self.content.push(Content::ToolResult {
            id,
            name,
            is_error,
            content: inner,
        });
        self
    }

    /// Append an arbitrary content block.
    pub fn block(mut self, block: Content) -> Self {
        self.content.push(block);
        self
    }

    /// Set cache hint for this message.
    pub fn cache(mut self, cache: CacheHint) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Build the ChatMessage.
    pub fn build(self) -> ChatMessage {
        ChatMessage {
            role: self.role,
            content: self.content,
            cache: self.cache,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_thinking_handles_multiple_blocks() {
        let input = "start <think>reasoning 1</think> middle <think>reasoning 2</think> end";
        let (thinking, content) = extract_thinking(input);

        assert_eq!(thinking, Some("reasoning 1\n\nreasoning 2".to_string()));
        assert_eq!(content, "start  middle  end");
    }

    #[test]
    fn extract_thinking_handles_unclosed_block() {
        let input = "before <think>streamed rationale still open";
        let (thinking, content) = extract_thinking(input);

        assert_eq!(thinking, Some("streamed rationale still open".to_string()));
        assert_eq!(content, "before");
    }

    #[test]
    fn extract_thinking_returns_original_when_no_blocks_present() {
        let input = "plain response";
        let (thinking, content) = extract_thinking(input);

        assert_eq!(thinking, None);
        assert_eq!(content, "plain response");
    }

    #[test]
    fn content_text_constructor() {
        let c = Content::text("hello");
        assert_eq!(
            c,
            Content::Text {
                text: "hello".into()
            }
        );
        assert_eq!(c.as_text(), Some("hello"));
    }

    #[test]
    fn content_tool_result_constructor() {
        let c = Content::tool_result("id1", vec![Content::text("ok")]);
        match c {
            Content::ToolResult {
                id,
                name,
                is_error,
                content,
            } => {
                assert_eq!(id, "id1");
                assert_eq!(name, None);
                assert!(!is_error);
                assert_eq!(content.len(), 1);
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn builder_produces_correct_blocks() {
        let msg = ChatMessage::user()
            .text("Hello")
            .image("image/png", vec![1, 2, 3])
            .build();

        assert_eq!(msg.role, ChatRole::User);
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.text(), "Hello");
    }

    #[test]
    fn builder_thinking_skips_empty() {
        let msg = ChatMessage::assistant()
            .thinking("")
            .text("response")
            .build();

        assert_eq!(msg.content.len(), 1);
        assert!(msg.thinking().is_none());
    }

    #[test]
    fn chat_message_has_tool_use() {
        let msg = ChatMessage::assistant()
            .text("Let me search")
            .tool_use(
                "t1".into(),
                "search".into(),
                serde_json::json!({"q": "rust"}),
            )
            .build();

        assert!(msg.has_tool_use());
        assert_eq!(msg.tool_uses().len(), 1);
        assert!(!msg.has_tool_result());
    }

    #[test]
    fn content_serde_roundtrip() {
        let blocks = vec![
            Content::text("hello"),
            Content::image("image/png", vec![1, 2]),
            Content::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                arguments: serde_json::json!({"q": "test"}),
            },
            Content::ToolResult {
                id: "t1".into(),
                name: Some("search".into()),
                is_error: false,
                content: vec![Content::text("found it")],
            },
        ];

        let json = serde_json::to_string(&blocks).unwrap();
        let roundtripped: Vec<Content> = serde_json::from_str(&json).unwrap();
        assert_eq!(blocks, roundtripped);
    }
}
