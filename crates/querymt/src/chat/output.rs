use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::{fmt, str::FromStr};

use super::{ChatResponse, ChatRole, Content, FinishReason};
use crate::{FunctionCall, ToolCall, Usage};

/// Provider-specific fields retained by the structured chat contract.
pub type Extensions = Map<String, Value>;

/// A validated concrete MIME type used by structured media.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MediaType(mime::Mime);

impl MediaType {
    pub fn type_(&self) -> &str {
        self.0.type_().as_str()
    }

    pub fn subtype(&self) -> &str {
        self.0.subtype().as_str()
    }

    pub fn suffix(&self) -> Option<&str> {
        self.0.suffix().map(|suffix| suffix.as_str())
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.0.get_param(name).map(|value| value.as_str())
    }

    /// Iterate all MIME parameters in their parsed form.
    ///
    /// Needed by wire encoders that must keep parameters separate from
    /// structural markers (e.g. a data URL's `;base64` marker).
    pub fn params(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.params().map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl FromStr for MediaType {
    type Err = MediaTypeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = value
            .parse::<mime::Mime>()
            .map_err(|_| MediaTypeError::Invalid(value.to_string()))?;
        if parsed.type_() == mime::STAR || parsed.subtype() == mime::STAR {
            return Err(MediaTypeError::Wildcard(value.to_string()));
        }
        Ok(Self(parsed))
    }
}

impl TryFrom<String> for MediaType {
    type Error = MediaTypeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl AsRef<str> for MediaType {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MediaType").field(&self.as_ref()).finish()
    }
}

impl Serialize for MediaType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_ref())
    }
}

impl<'de> Deserialize<'de> for MediaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for MediaType {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "MediaType".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A concrete MIME media type without wildcards"
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaTypeError {
    #[error("invalid MIME type: {0}")]
    Invalid(String),
    #[error("wildcard MIME ranges are not concrete media types: {0}")]
    Wildcard(String),
}

/// Whether output came from an item-aware provider or was synthesized from legacy accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatOutputRepresentation {
    #[default]
    Structured,
    LegacyProjection,
}

fn is_structured_representation(representation: &ChatOutputRepresentation) -> bool {
    *representation == ChatOutputRepresentation::Structured
}

/// Origin information used to decide whether provider-specific state can be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChatOutputProvenance {
    pub provider: String,
    pub protocol: String,
    pub model: String,
    pub endpoint: String,
}

/// Provider-neutral response lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatOutputStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

/// Ordered, provider-neutral output from one generation attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default)]
    pub items: Vec<ChatOutputItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ChatOutputStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ChatOutputProvenance>,
    #[serde(default, skip_serializing_if = "is_structured_representation")]
    pub representation: ChatOutputRepresentation,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl Default for ChatOutput {
    fn default() -> Self {
        Self {
            response_id: None,
            items: Vec::new(),
            status: None,
            usage: None,
            finish_reason: None,
            provenance: None,
            representation: ChatOutputRepresentation::Structured,
            extensions: Extensions::new(),
        }
    }
}

impl ChatOutput {
    /// Build the limited structured representation available from a legacy response.
    pub fn from_legacy_response(response: &dyn ChatResponse) -> Self {
        let mut items = Vec::new();

        if let Some(thinking) = response.thinking()
            && !thinking.is_empty()
        {
            items.push(ChatOutputItem::Reasoning(ChatReasoningItem {
                id: None,
                summary: vec![ChatReasoningPart::text(thinking)],
                content: Vec::new(),
                encrypted_content: None,
                signature: None,
                status: None,
                extensions: Extensions::new(),
            }));
        }

        if let Some(text) = response.text()
            && !text.is_empty()
        {
            items.push(ChatOutputItem::Message(ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Text {
                    text,
                    annotations: Vec::new(),
                    extensions: Extensions::new(),
                }],
                extensions: Extensions::new(),
            }));
        }

        if let Some(calls) = response.tool_calls() {
            items.extend(calls.into_iter().map(|call| {
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: None,
                    call_id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                    status: None,
                    extensions: Extensions::new(),
                })
            }));
        }

        Self {
            items,
            status: response
                .finish_reason()
                .map(|_| ChatOutputStatus::Completed),
            usage: response.usage(),
            finish_reason: response.finish_reason(),
            representation: ChatOutputRepresentation::LegacyProjection,
            ..Self::default()
        }
    }

    /// Concatenate visible message text in canonical item/part order.
    pub fn text(&self) -> Option<String> {
        let text: String = self
            .items
            .iter()
            .filter_map(|item| match item {
                ChatOutputItem::Message(message) => Some(&message.parts),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                ChatMessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!text.is_empty()).then_some(text)
    }

    /// Concatenate visible reasoning while leaving encrypted continuation separate.
    pub fn thinking(&self) -> Option<String> {
        let parts: Vec<&str> = self
            .items
            .iter()
            .filter_map(|item| match item {
                ChatOutputItem::Reasoning(reasoning) => Some(reasoning),
                _ => None,
            })
            .flat_map(|reasoning| reasoning.summary.iter().chain(&reasoning.content))
            .map(|part| part.text.as_str())
            .filter(|text| !text.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Most recent reasoning signature, if any provider supplied one.
    pub fn signature(&self) -> Option<String> {
        self.items.iter().rev().find_map(|item| match item {
            ChatOutputItem::Reasoning(reasoning) => reasoning.signature.clone(),
            _ => None,
        })
    }

    /// Provider-visible text used for sizing estimates: message text, visible
    /// reasoning summaries, and raw function arguments. Encrypted continuation
    /// and opaque payloads are excluded because they are not portable content.
    pub fn estimate_text(&self) -> String {
        let mut chunks: Vec<&str> = Vec::new();
        for item in &self.items {
            match item {
                ChatOutputItem::Message(message) => {
                    for part in &message.parts {
                        match part {
                            ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                                chunks.push(text)
                            }
                            ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                                chunks.push(refusal)
                            }
                            _ => {}
                        }
                    }
                }
                ChatOutputItem::Reasoning(reasoning) => {
                    for part in reasoning.summary.iter().chain(&reasoning.content) {
                        if !part.text.is_empty() {
                            chunks.push(&part.text);
                        }
                    }
                }
                ChatOutputItem::FunctionCall(call) => {
                    if !call.arguments.is_empty() {
                        chunks.push(&call.arguments);
                    }
                }
                ChatOutputItem::Opaque(_) => {}
            }
        }
        chunks.join("\n")
    }

    /// Build deterministic portable content without interpreting opaque items.
    ///
    /// Function calls with invalid JSON remain in structured output but are not
    /// projected as executable `Content::ToolUse` blocks.
    pub fn portable_content(&self) -> Vec<Content> {
        self.portable_content_with(false)
    }

    /// Like [`ChatOutput::portable_content`], optionally attaching reasoning
    /// signatures to projected thinking blocks for same-origin replay.
    pub fn portable_content_with(&self, preserve_signatures: bool) -> Vec<Content> {
        let mut content = Vec::new();

        for item in &self.items {
            match item {
                ChatOutputItem::Message(message) => {
                    content.extend(message.parts.iter().filter_map(|part| match part {
                        ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                            Some(Content::text(text.clone()))
                        }
                        ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                            Some(Content::text(refusal.clone()))
                        }
                        ChatMessagePart::Media(media) => media.portable_content(),
                        _ => None,
                    }));
                }
                ChatOutputItem::Reasoning(reasoning) => {
                    let visible: Vec<&str> = reasoning
                        .summary
                        .iter()
                        .chain(&reasoning.content)
                        .map(|part| part.text.as_str())
                        .filter(|text| !text.is_empty())
                        .collect();
                    if !visible.is_empty() {
                        content.push(Content::Thinking {
                            text: visible.join("\n\n"),
                            signature: if preserve_signatures {
                                reasoning.signature.clone()
                            } else {
                                None
                            },
                        });
                    }
                }
                ChatOutputItem::FunctionCall(call) => {
                    if let Ok(arguments) = call.parse_arguments() {
                        content.push(Content::tool_use(
                            call.call_id.clone(),
                            call.name.clone(),
                            arguments,
                        ));
                    }
                }
                ChatOutputItem::Opaque(_) => {}
            }
        }

        content
    }

    /// Project supported function items without interpreting opaque items.
    pub fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        let calls: Vec<ToolCall> = self
            .items
            .iter()
            .filter_map(|item| match item {
                ChatOutputItem::FunctionCall(call) => Some(call.to_tool_call()),
                _ => None,
            })
            .collect();
        (!calls.is_empty()).then_some(calls)
    }
}

/// Return authoritative structured output, or synthesize a marked legacy projection.
pub fn normalize_chat_response(response: &dyn ChatResponse) -> ChatOutput {
    response
        .output()
        .cloned()
        .unwrap_or_else(|| ChatOutput::from_legacy_response(response))
}

/// Broad media category used for rendering and endpoint capability checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Document,
    Other,
}

/// The source form of structured media. Sources are preserved and never fetched implicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaSource {
    Inline {
        data: Vec<u8>,
    },
    DataUrl {
        url: String,
    },
    Url {
        url: String,
    },
    ProviderFile {
        file_id: String,
        origin: ChatOutputProvenance,
    },
}

/// Provider-neutral media shared by generated output and normalized tool results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPart {
    pub kind: MediaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<MediaType>,
    pub source: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaNormalizationError {
    #[error(transparent)]
    MediaType(#[from] MediaTypeError),
    #[error("inline media requires a MIME type")]
    MissingInlineMediaType,
    #[error("invalid data URL")]
    InvalidDataUrl,
    #[error("data URL MIME type {actual} conflicts with declared MIME type {declared}")]
    ConflictingDataUrlMediaType {
        declared: MediaType,
        actual: MediaType,
    },
}

/// Display decision that lets renderers fall back without changing canonical media.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaDisplayProjection<'a> {
    Renderable(&'a MediaPart),
    Attachment(&'a MediaPart),
}

impl MediaPart {
    pub fn new(
        kind: MediaKind,
        media_type: Option<MediaType>,
        source: MediaSource,
    ) -> Result<Self, MediaNormalizationError> {
        let media_type = match &source {
            MediaSource::Inline { .. } if media_type.is_none() => {
                return Err(MediaNormalizationError::MissingInlineMediaType);
            }
            MediaSource::DataUrl { url } => {
                let actual = data_url_media_type(url)?;
                if let Some(declared) = &media_type
                    && declared != &actual
                {
                    return Err(MediaNormalizationError::ConflictingDataUrlMediaType {
                        declared: declared.clone(),
                        actual,
                    });
                }
                Some(actual)
            }
            _ => media_type,
        };
        Ok(Self {
            kind,
            media_type,
            source,
            filename: None,
            detail: None,
            extensions: Extensions::new(),
        })
    }

    /// Normalize a legacy media block on demand; unrelated legacy history can still load first.
    pub fn from_legacy_content(content: &Content) -> Result<Option<Self>, MediaNormalizationError> {
        let normalized = match content {
            Content::Image { mime_type, data } => Self::new(
                MediaKind::Image,
                Some(mime_type.parse()?),
                MediaSource::Inline { data: data.clone() },
            )?,
            Content::ImageUrl { url } => Self::new(
                MediaKind::Image,
                None,
                MediaSource::Url { url: url.clone() },
            )?,
            Content::Pdf { data } => Self::new(
                MediaKind::Document,
                Some(
                    "application/pdf"
                        .parse()
                        .expect("static MIME type is valid"),
                ),
                MediaSource::Inline { data: data.clone() },
            )?,
            Content::Audio { mime_type, data } => Self::new(
                MediaKind::Audio,
                Some(mime_type.parse()?),
                MediaSource::Inline { data: data.clone() },
            )?,
            Content::ResourceLink {
                uri,
                name,
                mime_type,
                ..
            } => {
                let mut media = Self::new(
                    MediaKind::Other,
                    mime_type.as_deref().map(str::parse).transpose()?,
                    MediaSource::Url { url: uri.clone() },
                )?;
                media.filename.clone_from(name);
                media
            }
            _ => return Ok(None),
        };
        Ok(Some(normalized))
    }

    /// Build the legacy-compatible display projection without fetching referenced data.
    pub fn portable_content(&self) -> Option<Content> {
        match (&self.kind, &self.source) {
            (MediaKind::Image, MediaSource::Inline { data }) => Some(Content::Image {
                mime_type: self.media_type.as_ref()?.to_string(),
                data: data.clone(),
            }),
            (MediaKind::Audio, MediaSource::Inline { data }) => Some(Content::Audio {
                mime_type: self.media_type.as_ref()?.to_string(),
                data: data.clone(),
            }),
            (MediaKind::Document, MediaSource::Inline { data })
                if self.media_type.as_ref()?.type_() == "application"
                    && self.media_type.as_ref()?.subtype() == "pdf" =>
            {
                Some(Content::Pdf { data: data.clone() })
            }
            (MediaKind::Image, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
                Some(Content::ImageUrl { url: url.clone() })
            }
            (_, MediaSource::Url { url }) => Some(Content::ResourceLink {
                uri: url.clone(),
                name: self.filename.clone(),
                description: None,
                mime_type: self.media_type.as_ref().map(ToString::to_string),
            }),
            _ => None,
        }
    }

    pub fn display_projection(
        &self,
        supports: impl FnOnce(MediaKind, Option<&MediaType>) -> bool,
    ) -> MediaDisplayProjection<'_> {
        if supports(self.kind, self.media_type.as_ref()) {
            MediaDisplayProjection::Renderable(self)
        } else {
            MediaDisplayProjection::Attachment(self)
        }
    }
}

/// Normalize media nested in a legacy tool result without changing or executing other blocks.
pub fn normalize_tool_result_media(
    content: &Content,
) -> Result<Vec<MediaPart>, MediaNormalizationError> {
    let Content::ToolResult { content, .. } = content else {
        return Ok(Vec::new());
    };
    content
        .iter()
        .filter_map(|part| match MediaPart::from_legacy_content(part) {
            Ok(Some(media)) => Some(Ok(media)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn data_url_media_type(url: &str) -> Result<MediaType, MediaNormalizationError> {
    let rest = url
        .strip_prefix("data:")
        .ok_or(MediaNormalizationError::InvalidDataUrl)?;
    let (metadata, _) = rest
        .split_once(',')
        .ok_or(MediaNormalizationError::InvalidDataUrl)?;
    let metadata = metadata.strip_suffix(";base64").unwrap_or(metadata);
    let value = if metadata.is_empty() {
        "text/plain;charset=US-ASCII"
    } else if metadata.starts_with(';') {
        return Err(MediaNormalizationError::InvalidDataUrl);
    } else {
        metadata
    };
    value.parse().map_err(MediaNormalizationError::from)
}

/// An ordered generated output item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatOutputItem {
    Message(ChatMessageItem),
    Reasoning(ChatReasoningItem),
    FunctionCall(ChatFunctionCallItem),
    Opaque(ChatOpaqueItem),
}

/// A generated assistant message and its ordered parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: ChatRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ChatOutputStatus>,
    #[serde(default)]
    pub parts: Vec<ChatMessagePart>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

/// A generated message part. Unknown provider parts remain opaque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatMessagePart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<ChatTextAnnotation>,
        #[serde(default, flatten)]
        extensions: Extensions,
    },
    Refusal {
        refusal: String,
        #[serde(default, flatten)]
        extensions: Extensions,
    },
    Media(MediaPart),
    Opaque(ChatOpaquePart),
}

/// Annotation data attached to generated text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTextAnnotation {
    #[serde(rename = "type")]
    pub annotation_type: String,
    #[serde(default, flatten)]
    pub fields: Extensions,
}

/// Visible and opaque portions of a reasoning item.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatReasoningItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub summary: Vec<ChatReasoningPart>,
    #[serde(default)]
    pub content: Vec<ChatReasoningPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ChatOutputStatus>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl fmt::Debug for ChatReasoningItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatReasoningItem")
            .field("id", &self.id)
            .field("summary", &self.summary)
            .field("content", &self.content)
            .field(
                "encrypted_content",
                &self.encrypted_content.as_ref().map(|_| "[REDACTED]"),
            )
            .field("signature", &self.signature.as_ref().map(|_| "[REDACTED]"))
            .field("status", &self.status)
            .field("extensions", &self.extensions)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatReasoningPart {
    pub text: String,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl ChatReasoningPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            extensions: Extensions::new(),
        }
    }
}

/// A function call retaining distinct provider item and call identities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFunctionCallItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub call_id: String,
    pub name: String,
    /// Exact provider argument text. Parse only when validating execution.
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ChatOutputStatus>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl ChatFunctionCallItem {
    pub fn parse_arguments(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.arguments)
    }

    pub fn parse_arguments_as<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_str(&self.arguments)
    }

    pub fn to_tool_call(&self) -> ToolCall {
        ToolCall {
            id: self.call_id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

/// Unknown output retained without being interpreted or executed.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatOpaqueItem {
    pub original_type: String,
    pub payload: Value,
}

impl ChatOpaqueItem {
    /// Derive display-only media through an explicit codec without changing canonical replay data.
    pub fn media_display_projection(
        &self,
        recognize: impl FnOnce(&str, &Value) -> Option<MediaPart>,
    ) -> Option<MediaPart> {
        recognize(&self.original_type, &self.payload)
    }
}

/// Debug output redacts the opaque payload: it is provider-only replay state,
/// not display content.
impl fmt::Debug for ChatOpaqueItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatOpaqueItem")
            .field("original_type", &self.original_type)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// Unknown message part retained without guessing its semantics.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatOpaquePart {
    pub original_type: String,
    pub payload: Value,
}

/// Debug output redacts the opaque payload: it is provider-only replay state,
/// not display content.
impl fmt::Debug for ChatOpaquePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatOpaquePart")
            .field("original_type", &self.original_type)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ChatProvider, Tool};
    use crate::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
    use crate::embedding::EmbeddingProvider;
    use crate::error::LLMError;
    use async_trait::async_trait;

    #[test]
    fn structured_output_serde_preserves_order_and_provider_data() {
        let fixture = serde_json::json!({
            "response_id": "resp_1",
            "status": "completed",
            "provenance": {
                "provider": "fixture",
                "protocol": "responses",
                "model": "model-a",
                "endpoint": "https://example.invalid/v1/responses"
            },
            "provider_version": 2,
            "items": [
                {
                    "type": "reasoning",
                    "id": "reasoning_1",
                    "summary": [{"text": "first", "summary_kind": "concise"}],
                    "content": [],
                    "encrypted_content": "secret",
                    "signature": "signature",
                    "status": "completed",
                    "reasoning_vendor": true
                },
                {
                    "type": "message",
                    "id": "message_1",
                    "role": "Assistant",
                    "phase": "final_answer",
                    "status": "completed",
                    "parts": [
                        {
                            "type": "text",
                            "text": "answer",
                            "annotations": [{
                                "type": "citation",
                                "start": 0,
                                "end": 6
                            }],
                            "text_vendor": "kept"
                        },
                        {
                            "type": "refusal",
                            "refusal": "cannot continue",
                            "refusal_vendor": 7
                        },
                        {
                            "type": "opaque",
                            "original_type": "future_part",
                            "payload": {"unknown": [1, 2, 3]}
                        }
                    ],
                    "message_vendor": {"kept": true}
                },
                {
                    "type": "function_call",
                    "item_id": "item_1",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"query\":\"rust\"}",
                    "status": "completed",
                    "call_vendor": "kept"
                },
                {
                    "type": "reasoning",
                    "id": "reasoning_2",
                    "summary": [],
                    "content": [],
                    "encrypted_content": "encrypted-only"
                },
                {
                    "type": "opaque",
                    "original_type": "future_action",
                    "payload": {"required": true, "vendor_field": "kept"}
                }
            ]
        });

        let output: ChatOutput = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(output.items.len(), 5);
        assert!(matches!(output.items[0], ChatOutputItem::Reasoning(_)));
        assert!(matches!(output.items[1], ChatOutputItem::Message(_)));
        assert!(matches!(output.items[2], ChatOutputItem::FunctionCall(_)));
        assert!(matches!(output.items[3], ChatOutputItem::Reasoning(_)));
        assert!(matches!(output.items[4], ChatOutputItem::Opaque(_)));
        assert_eq!(
            output.extensions.get("provider_version"),
            Some(&serde_json::json!(2))
        );

        let encoded = serde_json::to_value(&output).unwrap();
        assert_eq!(encoded, fixture);

        let ChatOutputItem::Reasoning(encrypted_only) = &output.items[3] else {
            panic!("expected encrypted-only reasoning item");
        };
        assert!(encrypted_only.summary.is_empty());
        assert_eq!(
            encrypted_only.encrypted_content.as_deref(),
            Some("encrypted-only")
        );
        assert!(!format!("{encrypted_only:?}").contains("encrypted-only"));
    }

    #[test]
    fn invalid_function_arguments_remain_exact_and_unexecutable() {
        let raw = "{\"query\": [1,}";
        let call = ChatFunctionCallItem {
            item_id: Some("item_7".into()),
            call_id: "call_9".into(),
            name: "lookup".into(),
            arguments: raw.into(),
            status: Some(ChatOutputStatus::Completed),
            extensions: Extensions::new(),
        };

        assert!(call.parse_arguments().is_err());
        assert!(
            call.parse_arguments_as::<serde_json::Map<String, Value>>()
                .is_err()
        );
        assert_eq!(call.arguments, raw);
        assert_eq!(call.to_tool_call().id, "call_9");

        let roundtrip: ChatFunctionCallItem =
            serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert_eq!(roundtrip.arguments, raw);
        assert!(roundtrip.parse_arguments().is_err());
    }

    #[test]
    fn media_type_is_string_backed_and_rejects_invalid_or_wildcard_values() {
        let media_type: MediaType = "text/plain; charset=utf-8".parse().unwrap();
        assert_eq!(media_type.type_(), "text");
        assert_eq!(media_type.subtype(), "plain");
        assert_eq!(media_type.suffix(), None);
        assert_eq!(media_type.parameter("charset"), Some("utf-8"));

        let encoded = serde_json::to_value(&media_type).unwrap();
        assert!(encoded.is_string());
        let decoded: MediaType = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, media_type);
        assert!("not a mime".parse::<MediaType>().is_err());
        assert!(matches!(
            "image/*".parse::<MediaType>(),
            Err(MediaTypeError::Wildcard(_))
        ));

        let schema = schemars::schema_for!(MediaType);
        assert_eq!(schema.as_value()["type"], "string");
    }

    #[test]
    fn media_normalization_validates_sources_and_preserves_metadata() {
        assert_eq!(
            MediaPart::new(
                MediaKind::Image,
                None,
                MediaSource::Inline { data: vec![1] }
            ),
            Err(MediaNormalizationError::MissingInlineMediaType)
        );

        let defaulted = MediaPart::new(
            MediaKind::Document,
            None,
            MediaSource::DataUrl {
                url: "data:,hello".into(),
            },
        )
        .unwrap();
        assert_eq!(defaulted.media_type.unwrap().type_(), "text");

        let conflict = MediaPart::new(
            MediaKind::Image,
            Some("image/jpeg".parse().unwrap()),
            MediaSource::DataUrl {
                url: "data:image/png;base64,iVBORw0KGgo=".into(),
            },
        );
        assert!(matches!(
            conflict,
            Err(MediaNormalizationError::ConflictingDataUrlMediaType { .. })
        ));

        let origin = ChatOutputProvenance {
            provider: "fixture".into(),
            protocol: "responses".into(),
            model: "model-a".into(),
            endpoint: "endpoint-a".into(),
        };
        let mut reference = MediaPart::new(
            MediaKind::Document,
            None,
            MediaSource::ProviderFile {
                file_id: "file_1".into(),
                origin: origin.clone(),
            },
        )
        .unwrap();
        reference.filename = Some("report.bin".into());
        reference.detail = Some("preview".into());
        let decoded: MediaPart =
            serde_json::from_str(&serde_json::to_string(&reference).unwrap()).unwrap();
        assert_eq!(decoded, reference);
        assert!(decoded.portable_content().is_none());
        assert!(matches!(
            decoded.source,
            MediaSource::ProviderFile { origin: actual, .. } if actual == origin
        ));
    }

    #[test]
    fn legacy_media_normalizes_explicitly_and_display_falls_back_to_attachment() {
        let invalid_legacy: Content = serde_json::from_value(serde_json::json!({
            "type": "image",
            "mime_type": "invalid mime",
            "data": [1, 2, 3]
        }))
        .unwrap();
        assert!(matches!(
            MediaPart::from_legacy_content(&invalid_legacy),
            Err(MediaNormalizationError::MediaType(_))
        ));

        let mut unfamiliar = MediaPart::new(
            MediaKind::Image,
            Some("image/x-future".parse().unwrap()),
            MediaSource::Url {
                url: "https://example.invalid/image".into(),
            },
        )
        .unwrap();
        unfamiliar.filename = Some("future.img".into());
        assert!(matches!(
            unfamiliar.display_projection(|_, media_type| {
                media_type.is_some_and(|value| value.subtype() == "png")
            }),
            MediaDisplayProjection::Attachment(_)
        ));
        assert!(matches!(
            unfamiliar.display_projection(|kind, _| kind == MediaKind::Image),
            MediaDisplayProjection::Renderable(_)
        ));

        let opaque = ChatOpaqueItem {
            original_type: "future_media".into(),
            payload: serde_json::json!({"mime_type": "image/png", "data": "secret"}),
        };
        let recognized = opaque
            .media_display_projection(|item_type, payload| {
                (item_type == "future_media").then(|| {
                    MediaPart::new(
                        MediaKind::Image,
                        payload["mime_type"]
                            .as_str()
                            .map(str::parse)
                            .transpose()
                            .unwrap(),
                        MediaSource::Url {
                            url: "https://example.invalid/recognized".into(),
                        },
                    )
                    .unwrap()
                })
            })
            .unwrap();
        assert_eq!(recognized.kind, MediaKind::Image);
        assert_eq!(opaque.original_type, "future_media");
        assert_eq!(opaque.payload["data"], "secret");
    }

    #[test]
    fn output_media_projects_once_and_tool_result_normalization_keeps_order() {
        let media = MediaPart::new(
            MediaKind::Image,
            Some("image/png".parse().unwrap()),
            MediaSource::Inline {
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        let output = ChatOutput {
            items: vec![ChatOutputItem::Message(ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Media(media.clone())],
                extensions: Extensions::new(),
            })],
            ..ChatOutput::default()
        };
        assert_eq!(
            output.portable_content(),
            vec![Content::image("image/png", vec![1, 2, 3])]
        );

        let tool_result = Content::tool_result(
            "call_1",
            vec![
                Content::text("before"),
                Content::image("image/png", vec![1]),
                Content::pdf(vec![2]),
            ],
        );
        let normalized = normalize_tool_result_media(&tool_result).unwrap();
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].kind, MediaKind::Image);
        assert_eq!(normalized[1].kind, MediaKind::Document);
    }

    /// MIME spelling/parameters, source form, filename/detail, and provider
    /// reference scope survive the JSON wire format (the shared shape for
    /// storage, bindings, Extism, and remote transports) unchanged.
    #[test]
    fn media_metadata_and_sources_survive_wire_roundtrip() {
        let origin = ChatOutputProvenance {
            provider: "provider-a".into(),
            protocol: "responses".into(),
            model: "model-a".into(),
            endpoint: "https://a.invalid/v1/responses".into(),
        };

        let mut inline = MediaPart::new(
            MediaKind::Document,
            Some("application/pdf; charset=binary".parse().unwrap()),
            MediaSource::Inline {
                data: vec![0x25, 0x50, 0x44, 0x46],
            },
        )
        .unwrap();
        inline.filename = Some("report.pdf".into());
        inline.detail = Some("high".into());

        let mut reference = MediaPart::new(
            MediaKind::Document,
            Some("application/pdf".parse().unwrap()),
            MediaSource::ProviderFile {
                file_id: "file_abc".into(),
                origin: origin.clone(),
            },
        )
        .unwrap();
        reference.filename = Some("remote.pdf".into());
        reference.detail = Some("auto".into());

        let mut url_media = MediaPart::new(
            MediaKind::Image,
            None,
            MediaSource::Url {
                url: "https://example.invalid/pic.png".into(),
            },
        )
        .unwrap();
        url_media.filename = Some("pic.png".into());

        let roundtrip = |media: &MediaPart| -> MediaPart {
            serde_json::from_str(&serde_json::to_string(media).unwrap()).unwrap()
        };

        // MIME parameters survive (spelling may normalize, semantics must not).
        let decoded_inline = roundtrip(&inline);
        assert_eq!(decoded_inline, inline);
        let media_type = decoded_inline.media_type.as_ref().unwrap();
        assert_eq!(media_type.type_(), "application");
        assert_eq!(media_type.subtype(), "pdf");
        assert_eq!(media_type.parameter("charset"), Some("binary"));
        assert_eq!(decoded_inline.filename.as_deref(), Some("report.pdf"));
        assert_eq!(decoded_inline.detail.as_deref(), Some("high"));

        // Provider reference scope is preserved exactly, including origin.
        let decoded_reference = roundtrip(&reference);
        assert_eq!(decoded_reference, reference);
        match &decoded_reference.source {
            MediaSource::ProviderFile { file_id, origin: o } => {
                assert_eq!(file_id, "file_abc");
                assert_eq!(o, &origin);
            }
            other => panic!("expected provider file reference, got {other:?}"),
        }

        // An unresolved provider reference does not imply renderable bytes.
        assert!(
            decoded_reference.portable_content().is_none(),
            "provider file references must not fabricate portable bytes"
        );

        // URL source form, filename, and absent MIME metadata survive as-is.
        let decoded_url = roundtrip(&url_media);
        assert_eq!(decoded_url, url_media);
        assert!(decoded_url.media_type.is_none());
        assert_eq!(decoded_url.filename.as_deref(), Some("pic.png"));
        assert!(matches!(decoded_url.source, MediaSource::Url { .. }));
    }

    #[derive(Debug)]
    struct StructuredResponse {
        output: ChatOutput,
    }
    impl fmt::Display for StructuredResponse {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "structured response")
        }
    }

    impl ChatResponse for StructuredResponse {
        fn text(&self) -> Option<String> {
            self.output.text()
        }

        fn tool_calls(&self) -> Option<Vec<ToolCall>> {
            self.output.tool_calls()
        }

        fn finish_reason(&self) -> Option<FinishReason> {
            self.output.finish_reason
        }

        fn thinking(&self) -> Option<String> {
            self.output.thinking()
        }

        fn usage(&self) -> Option<Usage> {
            self.output.usage.clone()
        }

        fn output(&self) -> Option<&ChatOutput> {
            Some(&self.output)
        }
    }

    #[test]
    fn portable_projection_preserves_item_order_once() {
        let output = ChatOutput {
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: None,
                    summary: vec![ChatReasoningPart::text("reasoning")],
                    content: Vec::new(),
                    encrypted_content: None,
                    signature: None,
                    status: None,
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: None,
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text: "first".into(),
                        annotations: Vec::new(),
                        extensions: Extensions::new(),
                    }],
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("item_1".into()),
                    call_id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"query\":\"rust\"}".into(),
                    status: None,
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: None,
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![ChatMessagePart::Text {
                        text: "second".into(),
                        annotations: Vec::new(),
                        extensions: Extensions::new(),
                    }],
                    extensions: Extensions::new(),
                }),
            ],
            ..ChatOutput::default()
        };

        assert_eq!(
            output.portable_content(),
            vec![
                Content::thinking("reasoning"),
                Content::text("first"),
                Content::tool_use("call_1", "lookup", serde_json::json!({"query": "rust"})),
                Content::text("second"),
            ]
        );
    }

    #[test]
    fn invalid_function_arguments_are_not_projected_as_executable_content() {
        let output = ChatOutput {
            items: vec![ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: None,
                call_id: "call_invalid".into(),
                name: "lookup".into(),
                arguments: "{invalid".into(),
                status: None,
                extensions: Extensions::new(),
            })],
            ..ChatOutput::default()
        };

        assert!(output.portable_content().is_empty());
        assert_eq!(
            output.tool_calls().unwrap()[0].function.arguments,
            "{invalid"
        );
    }

    #[test]
    fn normalization_prefers_authoritative_structured_output() {
        let expected = ChatOutput {
            response_id: Some("resp_structured".into()),
            items: vec![ChatOutputItem::Message(ChatMessageItem {
                id: Some("message_1".into()),
                role: ChatRole::Assistant,
                phase: None,
                status: Some(ChatOutputStatus::Completed),
                parts: vec![ChatMessagePart::Text {
                    text: "hello".into(),
                    annotations: Vec::new(),
                    extensions: Extensions::new(),
                }],
                extensions: Extensions::new(),
            })],
            status: Some(ChatOutputStatus::Completed),
            finish_reason: Some(FinishReason::Stop),
            ..ChatOutput::default()
        };
        let response = StructuredResponse {
            output: expected.clone(),
        };

        assert_eq!(normalize_chat_response(&response), expected);
        assert_eq!(response.text().as_deref(), Some("hello"));
    }

    #[derive(Debug)]
    struct LegacyResponse;

    impl fmt::Display for LegacyResponse {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "legacy response")
        }
    }

    impl ChatResponse for LegacyResponse {
        fn text(&self) -> Option<String> {
            Some("legacy text".into())
        }

        fn tool_calls(&self) -> Option<Vec<ToolCall>> {
            Some(vec![ToolCall {
                id: "call_legacy".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "legacy_tool".into(),
                    arguments: "{\"value\":1}".into(),
                },
            }])
        }

        fn finish_reason(&self) -> Option<FinishReason> {
            Some(FinishReason::ToolCalls)
        }

        fn usage(&self) -> Option<Usage> {
            None
        }
    }

    struct LegacyProvider;

    #[async_trait]
    impl ChatProvider for LegacyProvider {
        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Box<dyn ChatResponse>, LLMError> {
            Ok(Box::new(LegacyResponse))
        }
    }

    #[async_trait]
    impl CompletionProvider for LegacyProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Ok(CompletionResponse {
                text: "legacy completion".into(),
            })
        }
    }

    #[async_trait]
    impl EmbeddingProvider for LegacyProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Ok(Vec::new())
        }
    }

    impl crate::LLMProvider for LegacyProvider {}

    #[tokio::test]
    async fn legacy_provider_remains_usable_through_dyn_llm_provider() {
        let provider: &dyn crate::LLMProvider = &LegacyProvider;
        let response = provider.chat(&[]).await.unwrap();

        assert!(response.output().is_none());
        let output = normalize_chat_response(response.as_ref());
        assert_eq!(
            output.representation,
            ChatOutputRepresentation::LegacyProjection
        );
        assert_eq!(output.text().as_deref(), Some("legacy text"));
        assert_eq!(output.tool_calls().unwrap()[0].id, "call_legacy");
    }

    fn reasoning_fixture(signature: Option<&str>) -> ChatReasoningItem {
        ChatReasoningItem {
            id: Some("reasoning_1".into()),
            summary: vec![ChatReasoningPart::text("visible reasoning")],
            content: Vec::new(),
            encrypted_content: Some("continuation".into()),
            signature: signature.map(str::to_string),
            status: None,
            extensions: Extensions::new(),
        }
    }

    #[test]
    fn signature_accessor_returns_latest_reasoning_signature() {
        let mut output = ChatOutput::default();
        assert!(output.signature().is_none());

        output
            .items
            .push(ChatOutputItem::Reasoning(reasoning_fixture(Some("sig_1"))));
        output.items.push(ChatOutputItem::Message(ChatMessageItem {
            id: None,
            role: ChatRole::Assistant,
            phase: None,
            status: None,
            parts: vec![ChatMessagePart::Text {
                text: "answer".into(),
                annotations: Vec::new(),
                extensions: Extensions::new(),
            }],
            extensions: Extensions::new(),
        }));
        output
            .items
            .push(ChatOutputItem::Reasoning(reasoning_fixture(Some("sig_2"))));

        assert_eq!(output.signature().as_deref(), Some("sig_2"));
    }

    #[test]
    fn portable_content_optionally_preserves_reasoning_signatures() {
        let mut output = ChatOutput::default();
        output
            .items
            .push(ChatOutputItem::Reasoning(reasoning_fixture(Some("sig_1"))));

        let portable = output.portable_content();
        assert!(portable.iter().all(
            |block| !matches!(block, Content::Thinking { signature: Some(_), .. })
        ));

        let same_origin = output.portable_content_with(true);
        assert!(same_origin.iter().any(|block| matches!(
            block,
            Content::Thinking { signature: Some(sig), .. } if sig == "sig_1"
        )));
    }

    /// The wire format shared by remote, plugin (Extism), and binding
    /// transports is serde JSON: structured output must survive a lossless
    /// round trip with byte-exact raw arguments and opaque payloads.
    #[test]
    fn chat_message_output_survives_wire_serialization_losslessly() {
        let fixture = serde_json::json!({
            "response_id": "resp_wire",
            "status": "completed",
            "provenance": {
                "provider": "openai",
                "protocol": "responses",
                "model": "gpt-5",
                "endpoint": "https://example.invalid/v1/responses"
            },
            "items": [
                {
                    "type": "reasoning",
                    "id": "reasoning_1",
                    "summary": [],
                    "content": [],
                    "encrypted_content": "sentinel-continuation",
                    "signature": "sentinel-signature"
                },
                {
                    "type": "function_call",
                    "item_id": "item_1",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{\"query\":\"rust\",\"raw\": 1 }",
                    "call_vendor": {"nested": [1, 2]}
                },
                {
                    "type": "opaque",
                    "original_type": "future_action",
                    "payload": {"required": true}
                }
            ]
        });
        let message = crate::chat::ChatMessage {
            role: ChatRole::Assistant,
            content: Vec::new(),
            cache: None,
            output: Some(serde_json::from_value::<ChatOutput>(fixture.clone()).unwrap()),
        };

        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: crate::chat::ChatMessage = serde_json::from_str(&encoded).unwrap();
        let output = decoded.output.expect("structured output survives");

        assert_eq!(serde_json::to_value(&output).unwrap(), fixture);
        let ChatOutputItem::FunctionCall(call) = &output.items[1] else {
            panic!("expected function call item");
        };
        assert_eq!(call.arguments, "{\"query\":\"rust\",\"raw\": 1 }");
    }

    /// Display-oriented tooling must never print provider-only continuation
    /// state. Debug output redacts it while the authorized stored value keeps
    /// the exact bytes needed for replay.
    #[test]
    fn debug_output_redacts_opaque_and_continuation_state() {
        const ENCRYPTED: &str = "sentinel-encrypted-continuation";
        const SIGNATURE: &str = "sentinel-signature-value";
        const OPAQUE: &str = "sentinel-opaque-payload";

        let fixture = serde_json::json!({
            "status": "completed",
            "items": [
                {
                    "type": "reasoning",
                    "id": "reasoning_1",
                    "summary": [{"text": "visible summary"}],
                    "content": [],
                    "encrypted_content": ENCRYPTED,
                    "signature": SIGNATURE
                },
                {
                    "type": "opaque",
                    "original_type": "future_action",
                    "payload": {"secret": OPAQUE}
                }
            ]
        });
        let output: ChatOutput = serde_json::from_value(fixture).unwrap();

        let debug = format!("{output:?}");
        assert!(debug.contains("[REDACTED]"), "redaction marker is present");
        assert!(
            !debug.contains(ENCRYPTED),
            "encrypted continuation must not appear in Debug output"
        );
        assert!(
            !debug.contains(SIGNATURE),
            "reasoning signatures must not appear in Debug output"
        );
        assert!(
            !debug.contains(OPAQUE),
            "opaque payloads must not appear in Debug output"
        );
        // Visible summary text is display content and may still be shown.
        assert!(debug.contains("visible summary"));

        // Authorized persistence is unaffected: serde keeps the exact bytes.
        let encoded = serde_json::to_string(&output).unwrap();
        assert!(encoded.contains(ENCRYPTED));
        assert!(encoded.contains(SIGNATURE));
        assert!(encoded.contains(OPAQUE));
    }
}
