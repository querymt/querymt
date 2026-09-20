use async_trait::async_trait;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

use crate::{ToolCall, Usage, error::LLMError};
use futures::Stream;
use std::pin::Pin;

pub mod http;
mod migration;
pub mod output;
pub mod streaming;

pub use output::{
    ChatFunctionCallItem, ChatInputError, ChatInputPart, ChatMessageItem, ChatMessagePart,
    ChatMessagePayload, ChatOpaqueItem, ChatOpaquePart, ChatOutput, ChatOutputItem,
    ChatOutputProvenance, ChatOutputStatus, ChatReasoningItem, ChatReasoningPart,
    ChatTextAnnotation, Extensions, MediaDisplayProjection, MediaKind, MediaNormalizationError,
    MediaPart, MediaSource, MediaType, MediaTypeError, ToolResult, ToolResultPart,
    empty_message_part,
};
pub use streaming::{
    ChatMessagePartDelta, ChatStreamAccumulator, ChatStreamAccumulatorError, ChatStreamFinish,
    LegacyStreamProjection, ReasoningPartKind, StructuredStreamEvent,
};

// ---------------------------------------------------------------------------
// Content — the legacy recursive content block
// ---------------------------------------------------------------------------
//
// The legacy `Content` type now lives in `migration` and is crate-private: it is
// reachable only for reading old persisted histories and serialized transports.
// Canonical values are `ChatInputPart` (supplied input) and `ChatOutput`
// (generated output).

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

/// The type of reasoning effort for a model's reasoning/thinking feature.
///
/// Providers that support reasoning map these levels to their own API format:
/// - **OpenAI-compatible**: passed as `reasoning_effort` string in the request body
/// - **Anthropic**: mapped to `thinking.budget_tokens` (or adaptive mode for newer models)
/// - **Google**: mapped to `thinkingConfig.thinkingBudget` or `thinkingConfig.thinkingLevel`
/// - **Ollama**: any effort level enables `think: true`
/// - **Providers without support**: warned and ignored
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Low reasoning effort — minimal thinking, fastest responses
    Low,
    /// Medium reasoning effort — balanced thinking
    Medium,
    /// High reasoning effort — thorough thinking
    High,
    /// Maximum reasoning effort — deepest thinking, highest budget
    Max,
}

/// A single message in a chat conversation.
///
/// Each turn carries exactly one authoritative payload: canonical input parts
/// (user/tool-result turns) or structured output (assistant turns). Portable and
/// display projections are derived on demand from structured output and are
/// never stored as an independently mutable second source of truth.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// The role of who sent this message (user or assistant)
    pub role: ChatRole,
    /// The exclusive authoritative payload for this turn.
    payload: ChatMessagePayload,
    /// Optional cache hint. Providers that support caching (e.g., Anthropic)
    /// will translate this into provider-specific cache breakpoint markers.
    pub cache: Option<CacheHint>,
}

/// Serialization writes only the canonical exclusive payload. Legacy `content`
/// is never emitted; new records carry either `input` or `output`.
impl Serialize for ChatMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ChatMessage", 3)?;
        state.serialize_field("role", &self.role)?;
        match &self.payload {
            ChatMessagePayload::Input(parts) => state.serialize_field("input", parts)?,
            ChatMessagePayload::Output(output) => state.serialize_field("output", output)?,
        }
        if self.cache.is_some() {
            state.serialize_field("cache", &self.cache)?;
        }
        state.end()
    }
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
    /// Whether the provider should enforce strict schema adherence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
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

    // FunctionTool = name + description + parameters + optional strictness, rounded
    // up to Value's alignment.
    const EXPECTED_FUNCTION_TOOL_SIZE: usize = align_up(
        STRING_SIZE + STRING_SIZE + VALUE_SIZE + std::mem::size_of::<Option<bool>>(),
        VALUE_ALIGN,
    );

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
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ToolChoice".into()
    }

    fn json_schema(_gen: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                {
                    "type": "string",
                    "description": "One of the string options: \"required\", \"auto\", \"none\"",
                    "enum": ["required", "auto", "none"]
                },
                {
                    "type": "object",
                    "required": ["type", "function"],
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["function"]
                        },
                        "function": {
                            "type": "object",
                            "required": ["name"],
                            "properties": {
                                "name": { "type": "string" }
                            }
                        }
                    }
                }
            ]
        })
    }
}

impl From<ChatOutput> for ChatMessage {
    fn from(output: ChatOutput) -> Self {
        ChatMessage {
            role: ChatRole::Assistant,
            payload: ChatMessagePayload::Output(output),
            cache: None,
        }
    }
}

impl From<&ChatOutput> for ChatMessage {
    fn from(output: &ChatOutput) -> Self {
        ChatMessage::from(output.clone())
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
    /// Item-aware response metadata, lifecycle, delta, or snapshot event.
    Structured(StructuredStreamEvent),

    /// Text content delta
    Text(String),

    /// Thinking/reasoning content delta from the model.
    /// This is emitted separately from `Text` so consumers can display or
    /// store reasoning content differently (e.g., dimmed text, separate field).
    Thinking(String),

    /// Signature for the current thinking block (providers that require
    /// signed thinking replay emit this once the block is complete).
    ThinkingSignature(String),

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

    /// Stream ended with finish reason
    Done {
        /// The typed finish reason from the provider, mapped at emission time
        /// using the same logic as the canonical output finish reason.
        finish_reason: FinishReason,
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
    async fn chat(&self, messages: &[ChatMessage]) -> Result<ChatOutput, LLMError> {
        self.chat_with_tools(messages, None).await
    }

    /// Chat interaction with tools.
    ///
    /// Returns the authoritative item-aware [`ChatOutput`]. Text, visible
    /// reasoning, executable function calls, usage, and finish reason are
    /// derived projections of that value.
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
    ) -> Result<ChatOutput, LLMError>;

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

/// Whether a stream chunk is a semantic terminal.
///
/// Both the legacy `Done` marker and the canonical structured
/// [`StructuredStreamEvent::ResponseTerminal`] are terminals. Transports must
/// treat either as terminal for acknowledgement, buffering, lifecycle
/// completion, and receiver shutdown; they must not wait for a legacy `Done`
/// that a canonical-only provider will never send.
pub fn chunk_is_terminal(chunk: &StreamChunk) -> bool {
    matches!(
        chunk,
        StreamChunk::Done { .. }
            | StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal { .. })
    )
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReasoningEffort::Low => write!(f, "low"),
            ReasoningEffort::Medium => write!(f, "medium"),
            ReasoningEffort::High => write!(f, "high"),
            ReasoningEffort::Max => write!(f, "max"),
        }
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            "max" => Ok(ReasoningEffort::Max),
            other => Err(format!(
                "unknown reasoning effort '{}', expected one of: low, medium, high, max",
                other
            )),
        }
    }
}

/// Validation failure for a message whose role and payload conflict.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatMessageConsistencyError {
    #[error("structured output is only valid on assistant messages")]
    StructuredOutputOnUserMessage,
    #[error("structured message item role does not match its assistant envelope")]
    StructuredItemRoleMismatch,
}

impl ChatMessage {
    /// Validate that the payload matches the message role.
    ///
    /// Assistant input payloads are valid explicit portable projections. Only
    /// structured output is role-restricted because it represents generated
    /// assistant semantics.
    pub fn validate_output_consistency(&self) -> Result<(), ChatMessageConsistencyError> {
        match (&self.role, &self.payload) {
            (ChatRole::Assistant, ChatMessagePayload::Output(output)) => {
                if output.items.iter().any(|item| {
                    matches!(
                        item,
                        ChatOutputItem::Message(message) if message.role != self.role
                    )
                }) {
                    return Err(ChatMessageConsistencyError::StructuredItemRoleMismatch);
                }
                Ok(())
            }
            (ChatRole::User, ChatMessagePayload::Output(_)) => {
                Err(ChatMessageConsistencyError::StructuredOutputOnUserMessage)
            }
            (_, ChatMessagePayload::Input(_)) => Ok(()),
        }
    }

    /// Replace the authoritative structured output payload.
    pub fn replace_output(
        &mut self,
        output: ChatOutput,
    ) -> Result<(), ChatMessageConsistencyError> {
        if self.role != ChatRole::Assistant {
            return Err(ChatMessageConsistencyError::StructuredOutputOnUserMessage);
        }
        if output.items.iter().any(|item| {
            matches!(
                item,
                ChatOutputItem::Message(message) if message.role != self.role
            )
        }) {
            return Err(ChatMessageConsistencyError::StructuredItemRoleMismatch);
        }
        self.payload = ChatMessagePayload::Output(output);
        Ok(())
    }

    /// Replace the authoritative payload with portable input parts.
    ///
    /// This is valid for both user turns and explicitly downgraded assistant
    /// turns. Any previous structured continuation is discarded.
    pub fn replace_input(&mut self, parts: Vec<ChatInputPart>) {
        self.payload = ChatMessagePayload::Input(parts);
    }

    /// Explicitly remove origin-scoped continuation from structured output.
    ///
    /// Portable message, reasoning, and function-call semantics remain canonical
    /// output so call IDs and names survive cross-provider replay. The discarded
    /// identities and encrypted state cannot be restored afterwards.
    pub fn into_portable(mut self) -> Self {
        self.payload = match self.payload {
            ChatMessagePayload::Input(parts) => ChatMessagePayload::Input(parts),
            ChatMessagePayload::Output(output) => {
                ChatMessagePayload::Output(output.into_portable())
            }
        };
        self
    }

    /// Borrow the authoritative payload.
    pub fn payload(&self) -> &ChatMessagePayload {
        &self.payload
    }

    /// Borrow authoritative input parts without allocating.
    pub fn input(&self) -> Option<&[ChatInputPart]> {
        self.payload.as_input()
    }

    /// Append an input part to an input payload.
    ///
    /// Returns `false` (leaving the message unchanged) if the payload is
    /// structured output, since the two forms are exclusive.
    pub fn push_input_part(&mut self, part: ChatInputPart) -> bool {
        match &mut self.payload {
            ChatMessagePayload::Input(parts) => {
                parts.push(part);
                true
            }
            ChatMessagePayload::Output(_) => false,
        }
    }

    /// Set the message role.
    ///
    /// A structured output payload cannot be moved onto a user turn; in that
    /// case the output is projected to portable input parts instead.
    pub fn with_role(mut self, role: ChatRole) -> Self {
        if role == ChatRole::User && self.payload.is_output() {
            self.payload = ChatMessagePayload::Input(self.payload.into_portable());
        }
        self.role = role;
        self
    }

    /// Create an owned lossy portable projection of this message's payload.
    pub fn portable_input_parts(&self) -> Vec<ChatInputPart> {
        self.payload.portable_parts()
    }

    /// Compatibility alias for [`Self::portable_input_parts`].
    pub fn input_parts(&self) -> Vec<ChatInputPart> {
        self.portable_input_parts()
    }

    /// Borrow the authoritative structured output, if this is an output turn.
    pub fn output(&self) -> Option<&ChatOutput> {
        self.payload.as_output()
    }

    /// Clear structured authority, converting to an empty portable input payload.
    pub fn clear_output(&mut self) -> Option<ChatOutput> {
        match std::mem::replace(&mut self.payload, ChatMessagePayload::Input(Vec::new())) {
            ChatMessagePayload::Output(output) => Some(output),
            other => {
                self.payload = other;
                None
            }
        }
    }

    /// Create a new builder for a user message.
    pub fn user() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::User)
    }

    /// Create a new builder for an assistant message.
    pub fn assistant() -> ChatMessageBuilder {
        ChatMessageBuilder::new(ChatRole::Assistant)
    }

    /// Convenience: create a user message from canonical input parts.
    pub fn from_user_parts(parts: Vec<ChatInputPart>) -> Self {
        ChatMessage {
            role: ChatRole::User,
            payload: ChatMessagePayload::Input(parts),
            cache: None,
        }
    }

    /// Convenience: create an assistant message from structured output.
    pub fn from_assistant_output(output: ChatOutput) -> Self {
        ChatMessage {
            role: ChatRole::Assistant,
            payload: ChatMessagePayload::Output(output),
            cache: None,
        }
    }

    /// Extract concatenated text from canonical input parts or output text.
    pub fn text(&self) -> String {
        match &self.payload {
            ChatMessagePayload::Input(parts) => parts
                .iter()
                .filter_map(|part| part.as_text())
                .collect::<Vec<_>>()
                .join(""),
            ChatMessagePayload::Output(output) => output.text().unwrap_or_default(),
        }
    }

    /// Check whether the message carries a supported local function call.
    pub fn has_tool_use(&self) -> bool {
        self.function_calls().next().is_some()
    }

    /// Iterate over canonical generated function-call items without allocation.
    pub fn function_calls(&self) -> impl Iterator<Item = &ChatFunctionCallItem> {
        self.output()
            .into_iter()
            .flat_map(|output| output.function_calls())
    }

    /// Compatibility projection of supported local function calls.
    pub fn tool_uses(&self) -> Vec<&ChatFunctionCallItem> {
        self.function_calls().collect()
    }

    /// Check whether the message carries any tool result input part.
    pub fn has_tool_result(&self) -> bool {
        match &self.payload {
            ChatMessagePayload::Input(parts) => parts.iter().any(ChatInputPart::is_tool_result),
            ChatMessagePayload::Output(_) => false,
        }
    }

    /// Extract visible reasoning text, if any.
    pub fn thinking(&self) -> Option<String> {
        match &self.payload {
            ChatMessagePayload::Input(_) => None,
            ChatMessagePayload::Output(output) => output.thinking(),
        }
    }
}

/// Deserialization DTO accepting canonical and legacy message records.
///
/// Old `{ role, content }` records, the transitional
/// `{ role, content, output }` shape, and the canonical `{ role, input }` or
/// `{ role, output }` shape all normalize into the exclusive payload here,
/// once, at the migration boundary. A transitional record whose `content` and
/// `output` disagree is rejected as a stale duplicate representation.
#[derive(Deserialize)]
struct ChatMessageDto {
    role: ChatRole,
    #[serde(default)]
    content: Option<Vec<migration::Content>>,
    #[serde(default)]
    cache: Option<CacheHint>,
    #[serde(default)]
    output: Option<ChatOutput>,
    #[serde(default)]
    input: Option<Vec<ChatInputPart>>,
}

impl<'de> Deserialize<'de> for ChatMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let dto = ChatMessageDto::deserialize(deserializer)?;

        let payload = match (dto.output, dto.input, dto.content) {
            // Canonical exclusive shapes.
            (Some(output), None, None) => ChatMessagePayload::Output(output),
            (None, Some(parts), None) => ChatMessagePayload::Input(parts),
            // Transitional `{ role, content, output }` records: `output` is
            // authoritative. Any non-empty `content` must be a projection of it,
            // otherwise the record is a stale duplicate representation.
            (Some(output), None, Some(content)) if !content.is_empty() => {
                let portable =
                    migration::output_matches_legacy_projection(&content, &output, false);
                let native = migration::output_matches_legacy_projection(&content, &output, true);
                if !portable && !native {
                    return Err(de::Error::custom(
                        "message content does not match the structured output projection",
                    ));
                }
                ChatMessagePayload::Output(output)
            }
            (Some(output), None, Some(_)) => ChatMessagePayload::Output(output),
            // Legacy records. The role decides the path: an assistant turn is
            // generated output and collapses into exactly one `ChatOutput`,
            // while user/tool turns normalize into canonical input parts.
            (None, None, Some(content)) if dto.role == ChatRole::Assistant => {
                ChatMessagePayload::Output(
                    migration::normalize_legacy_assistant(content).map_err(de::Error::custom)?,
                )
            }
            (None, None, Some(content)) => ChatMessagePayload::Input(
                migration::normalize_legacy_input(&content).map_err(de::Error::custom)?,
            ),
            (None, None, None) => ChatMessagePayload::Input(Vec::new()),
            _ => {
                return Err(de::Error::custom(
                    "message record mixes exclusive payload representations",
                ));
            }
        };

        Ok(ChatMessage {
            role: dto.role,
            payload,
            cache: dto.cache,
        })
    }
}

/// Builder for ChatMessage.
///
/// Accumulates canonical input parts and produces a `ChatMessage`.
#[derive(Debug)]
pub struct ChatMessageBuilder {
    role: ChatRole,
    parts: Vec<ChatInputPart>,
    /// Ordered generated output items (reasoning, messages, function calls).
    output_items: Vec<ChatOutputItem>,
    cache: Option<CacheHint>,
}

impl ChatMessageBuilder {
    /// Create a new ChatMessageBuilder with specified role.
    pub fn new(role: ChatRole) -> Self {
        Self {
            role,
            parts: Vec::new(),
            output_items: Vec::new(),
            cache: None,
        }
    }

    fn push_message_part(&mut self, part: ChatMessagePart) {
        if let Some(ChatOutputItem::Message(message)) = self.output_items.last_mut() {
            message.parts.push(part);
            return;
        }
        self.output_items
            .push(ChatOutputItem::Message(ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![part],
                extensions: Default::default(),
            }));
    }

    /// Append text in message order.
    pub fn text(mut self, s: impl Into<String>) -> Self {
        let text = s.into();
        if self.role == ChatRole::Assistant {
            self.push_message_part(ChatMessagePart::Text {
                text,
                annotations: Vec::new(),
                extensions: Default::default(),
            });
        } else {
            self.parts.push(ChatInputPart::text(text));
        }
        self
    }

    /// Append visible reasoning text.
    pub fn thinking(mut self, s: impl Into<String>) -> Self {
        let text = s.into();
        if text.is_empty() {
            return self;
        }
        if self.role == ChatRole::Assistant {
            self.output_items
                .push(ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: None,
                    summary: Vec::new(),
                    content: vec![ChatReasoningPart::text(text)],
                    encrypted_content: None,
                    signature: None,
                    status: None,
                    extensions: Default::default(),
                }));
        } else {
            self.parts.push(ChatInputPart::text(text));
        }
        self
    }

    /// Append a validated inline image.
    pub fn image(self, media_type: MediaType, data: Vec<u8>) -> Self {
        let media = MediaPart::new(
            MediaKind::Image,
            Some(media_type),
            MediaSource::Inline { data },
        )
        .expect("typed inline image satisfies media invariants");
        self.attachment(media)
    }

    /// Parse and append an inline image, returning malformed MIME explicitly.
    pub fn try_image(
        self,
        mime: impl AsRef<str>,
        data: Vec<u8>,
    ) -> Result<Self, MediaNormalizationError> {
        let media_type = mime.as_ref().parse()?;
        let media = MediaPart::new(
            MediaKind::Image,
            Some(media_type),
            MediaSource::Inline { data },
        )?;
        Ok(self.attachment(media))
    }

    /// Append an image referenced by URL.
    pub fn image_url(self, url: impl Into<String>) -> Self {
        let media = MediaPart::new(MediaKind::Image, None, MediaSource::Url { url: url.into() })
            .expect("URL attachment without MIME is valid");
        self.attachment(media)
    }

    /// Append an inline PDF document.
    pub fn pdf(self, data: Vec<u8>) -> Self {
        let media = MediaPart::new(
            MediaKind::Document,
            Some("application/pdf".parse().expect("static MIME is valid")),
            MediaSource::Inline { data },
        )
        .expect("typed inline PDF satisfies media invariants");
        self.attachment(media)
    }

    /// Append a validated attachment without changing its generated/input role.
    pub fn attachment(mut self, media: MediaPart) -> Self {
        if self.role == ChatRole::Assistant {
            self.push_message_part(ChatMessagePart::Media(Box::new(media)));
        } else {
            self.parts.push(ChatInputPart::attachment(media));
        }
        self
    }

    /// Append a generated function call.
    pub fn tool_use(mut self, id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        assert_eq!(
            self.role,
            ChatRole::Assistant,
            "function calls can only be built on assistant messages"
        );
        self.output_items
            .push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: None,
                call_id: id.into(),
                name: name.into(),
                arguments: args.to_string(),
                status: None,
                extensions: Default::default(),
            }));
        self
    }

    /// Append an already constructed correlated tool result.
    pub fn tool_result_value(mut self, result: ToolResult) -> Self {
        assert_eq!(
            self.role,
            ChatRole::User,
            "tool results can only be built on user messages"
        );
        self.parts.push(ChatInputPart::tool_result(result));
        self
    }

    /// Append a correlated tool result from its component fields.
    pub fn tool_result(
        self,
        id: String,
        name: Option<String>,
        is_error: bool,
        parts: Vec<ToolResultPart>,
    ) -> Self {
        let mut result = ToolResult::new(id);
        result.name = name;
        result.is_error = is_error;
        result.parts = parts;
        self.tool_result_value(result)
    }

    /// Append an arbitrary canonical input part.
    pub fn part(mut self, part: ChatInputPart) -> Self {
        assert_eq!(
            self.role,
            ChatRole::User,
            "canonical input parts can only be appended to user messages; use attachment() or text() for assistant output"
        );
        self.parts.push(part);
        self
    }

    /// Set cache hint for this message.
    pub fn cache(mut self, cache: CacheHint) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Build the canonical message.
    pub fn build(self) -> ChatMessage {
        let payload = if self.role == ChatRole::Assistant {
            ChatMessagePayload::Output(ChatOutput {
                items: self.output_items,
                status: Some(ChatOutputStatus::Completed),
                ..ChatOutput::default()
            })
        } else {
            ChatMessagePayload::Input(self.parts)
        };
        ChatMessage {
            role: self.role,
            payload,
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
    fn builder_produces_correct_blocks() {
        let msg = ChatMessage::user()
            .text("Hello")
            .image("image/png".parse().unwrap(), vec![1, 2, 3])
            .build();

        assert_eq!(msg.role, ChatRole::User);
        assert_eq!(msg.input_parts().len(), 2);
        assert_eq!(msg.text(), "Hello");
    }

    #[test]
    fn assistant_builder_preserves_text_attachment_and_call_order() {
        let media = MediaPart::new(
            MediaKind::Image,
            Some("image/png".parse().unwrap()),
            MediaSource::Inline {
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        let message = ChatMessage::assistant()
            .text("before")
            .attachment(media)
            .tool_use("call_1", "lookup", serde_json::json!({"q": "rust"}))
            .text("after")
            .build();

        let output = message.output().expect("assistant output");
        assert_eq!(output.items.len(), 3);
        let ChatOutputItem::Message(first) = &output.items[0] else {
            panic!("expected first message item");
        };
        assert_eq!(first.parts.len(), 2);
        assert!(matches!(first.parts[1], ChatMessagePart::Media(_)));
        assert!(matches!(output.items[1], ChatOutputItem::FunctionCall(_)));
        assert!(matches!(output.items[2], ChatOutputItem::Message(_)));
    }

    #[test]
    fn portable_assistant_message_remains_valid_and_keeps_calls() {
        let message = ChatMessage::assistant()
            .thinking("visible")
            .tool_use("call_1", "lookup", serde_json::json!({"q": "rust"}))
            .build()
            .into_portable();

        assert!(message.validate_output_consistency().is_ok());
        let output = message.output().expect("portable canonical output");
        assert!(!output.requires_item_aware_fidelity());
        assert_eq!(output.function_calls().count(), 1);
    }

    #[test]
    fn invalid_builder_mime_is_explicit() {
        let error = ChatMessage::user()
            .try_image("not a mime", vec![1])
            .expect_err("invalid MIME must not be ignored");
        assert!(matches!(error, MediaNormalizationError::MediaType(_)));
    }

    #[test]
    fn builder_thinking_skips_empty() {
        let msg = ChatMessage::assistant()
            .thinking("")
            .text("response")
            .build();

        assert_eq!(msg.input_parts().len(), 1);
        assert!(msg.thinking().is_none());
    }

    #[test]
    fn chat_message_has_tool_use() {
        // Generated function calls live only in structured output. The
        // assistant builder records them there rather than as ordinary input,
        // so they are never silently dropped.
        let msg = ChatMessage::assistant()
            .text("Let me search")
            .tool_use("t1", "search", serde_json::json!({"q": "rust"}))
            .build();

        let output = msg.output().expect("assistant output payload");
        assert_eq!(output.tool_calls().unwrap().len(), 1);
        assert!(msg.has_tool_use());
        assert_eq!(msg.tool_uses().len(), 1);
        assert!(!msg.has_tool_result());
        // The call is not also accepted as an ordinary input part variant.
        assert!(msg.payload.as_input().is_none());
    }

    #[test]
    fn function_tool_omits_unspecified_strictness() {
        let legacy = serde_json::json!({
            "name": "lookup",
            "description": "Look up data",
            "parameters": {"type": "object"}
        });

        let tool: FunctionTool = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(tool.strict, None);
        assert_eq!(serde_json::to_value(&tool).unwrap(), legacy);

        let strict = FunctionTool {
            strict: Some(true),
            ..tool
        };
        assert_eq!(serde_json::to_value(strict).unwrap()["strict"], true);
    }

    #[test]
    fn old_message_json_normalizes_to_canonical_payload() {
        // A legacy user turn normalizes into canonical input parts.
        let user_json = serde_json::json!({
            "role": "User",
            "content": [{"type": "text", "text": "hello"}]
        });
        let user_message: ChatMessage = serde_json::from_value(user_json).unwrap();
        assert_eq!(
            user_message.input_parts(),
            vec![ChatInputPart::text("hello")]
        );
        assert_eq!(
            serde_json::to_value(&user_message).unwrap(),
            serde_json::json!({
                "role": "User",
                "input": [{"type": "text", "text": "hello"}]
            })
        );

        // A legacy assistant turn is generated output and collapses into one
        // authoritative `ChatOutput` payload, not independently mutable input.
        let assistant_json = serde_json::json!({
            "role": "Assistant",
            "content": [{"type": "text", "text": "hi there"}]
        });
        let assistant_message: ChatMessage = serde_json::from_value(assistant_json).unwrap();
        let output = assistant_message
            .output()
            .expect("assistant output payload");
        assert_eq!(output.text().as_deref(), Some("hi there"));
        assert!(assistant_message.payload.as_input().is_none());
        // Canonical serialization emits only the exclusive canonical payload.
        let saved = serde_json::to_value(&assistant_message).unwrap();
        assert!(saved.get("content").is_none());
        assert!(saved.get("output").is_some());
        assert!(saved.get("input").is_none());
    }

    #[test]
    fn transitional_message_with_stale_projection_is_rejected() {
        let output = ChatOutput::from_projections(
            None,
            Some("authoritative".into()),
            None,
            None,
            Some(FinishReason::Stop),
        );
        let stale = serde_json::json!({
            "role": "Assistant",
            "content": [{"type": "text", "text": "stale edit"}],
            "output": serde_json::to_value(&output).unwrap()
        });

        let err = serde_json::from_value::<ChatMessage>(stale).unwrap_err();
        assert!(err.to_string().contains("does not match"));
    }

    #[test]
    fn structured_output_rejects_user_and_mismatched_item_roles() {
        let output = ChatOutput {
            items: vec![ChatOutputItem::Message(ChatMessageItem {
                id: None,
                role: ChatRole::User,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Text {
                    text: "wrong role".into(),
                    annotations: Vec::new(),
                    extensions: Extensions::new(),
                }],
                extensions: Extensions::new(),
            })],
            ..ChatOutput::default()
        };
        let mut user = ChatMessage::from_user_parts(Vec::new());
        assert_eq!(
            user.replace_output(output.clone()),
            Err(ChatMessageConsistencyError::StructuredOutputOnUserMessage)
        );

        let mut assistant = ChatMessage::from_assistant_output(ChatOutput::default());
        assert_eq!(
            assistant.replace_output(output),
            Err(ChatMessageConsistencyError::StructuredItemRoleMismatch)
        );
    }

    #[test]
    fn tool_choice_schema_has_any_of() {
        let schema = schemars::schema_for!(ToolChoice);
        let schema_json = serde_json::to_string_pretty(&schema).unwrap();
        assert!(
            schema_json.contains("anyOf"),
            "schema should contain anyOf: {schema_json}"
        );
        assert!(
            schema_json.contains("auto"),
            "schema should contain 'auto' enum value: {schema_json}"
        );
        assert!(
            schema_json.contains("required"),
            "schema should contain 'required': {schema_json}"
        );
        assert!(
            schema_json.contains("function"),
            "schema should contain 'function': {schema_json}"
        );
    }
}
