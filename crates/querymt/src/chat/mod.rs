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

/// Role of a participant in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatRole {
    /// The user/human participant in the conversation
    User,
    /// The AI assistant participant in the conversation
    Assistant,
}

/// The supported MIME type of an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ImageMime {
    /// JPEG image
    JPEG,
    /// PNG image
    PNG,
    /// GIF image
    GIF,
    /// WebP image
    WEBP,
}

impl ImageMime {
    pub fn mime_type(&self) -> &'static str {
        match self {
            ImageMime::JPEG => "image/jpeg",
            ImageMime::PNG => "image/png",
            ImageMime::GIF => "image/gif",
            ImageMime::WEBP => "image/webp",
        }
    }
}

/// The type of a message in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MessageType {
    /// A text message
    #[default]
    Text,
    /// An image message
    Image((ImageMime, Vec<u8>)),
    /// PDF message
    Pdf(Vec<u8>),
    /// An image URL message
    ImageURL(String),
    /// A tool use
    ToolUse(Vec<ToolCall>),
    /// Tool result
    ToolResult(Vec<ToolCall>),
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// The role of who sent this message (user or assistant)
    pub role: ChatRole,
    /// The type of the message (text, image, audio, video, etc)
    pub message_type: MessageType,
    /// The text content of the message
    pub content: String,
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
        let content = response.text().unwrap_or_default();
        let tool_calls = response.tool_calls();
        let message_type = if let Some(calls) = tool_calls.clone() {
            MessageType::ToolUse(calls)
        } else {
            MessageType::Text
        };
        ChatMessage {
            role: ChatRole::Assistant,
            message_type,
            content,
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
    /// Create a new builder for a user message
    pub fn user() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::User)
    }

    /// Create a new builder for an assistant message
    pub fn assistant() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::Assistant)
    }
}

/// Builder for ChatMessage
#[derive(Debug)]
pub struct ChatMessageBuilder {
    role: ChatRole,
    message_type: MessageType,
    content: String,
}

impl ChatMessageBuilder {
    /// Create a new ChatMessageBuilder with specified role
    pub fn new(role: ChatRole) -> Self {
        Self {
            role,
            message_type: MessageType::default(),
            content: String::new(),
        }
    }

    /// Set the message content
    pub fn content<S: Into<String>>(mut self, content: S) -> Self {
        self.content = content.into();
        self
    }

    /// Set the message type as Image
    pub fn image(mut self, image_mime: ImageMime, raw_bytes: Vec<u8>) -> Self {
        self.message_type = MessageType::Image((image_mime, raw_bytes));
        self
    }

    /// Set the message type as Image
    pub fn pdf(mut self, raw_bytes: Vec<u8>) -> Self {
        self.message_type = MessageType::Pdf(raw_bytes);
        self
    }

    /// Set the message type as ImageURL
    pub fn image_url(mut self, url: impl Into<String>) -> Self {
        self.message_type = MessageType::ImageURL(url.into());
        self
    }

    /// Set the message type as ToolUse
    pub fn tool_use(mut self, tools: Vec<ToolCall>) -> Self {
        self.message_type = MessageType::ToolUse(tools);
        self
    }

    /// Set the message type as ToolResult
    pub fn tool_result(mut self, tools: Vec<ToolCall>) -> Self {
        self.message_type = MessageType::ToolResult(tools);
        self
    }

    /// Build the ChatMessage
    pub fn build(self) -> ChatMessage {
        ChatMessage {
            role: self.role,
            message_type: self.message_type,
            content: self.content,
        }
    }
}
