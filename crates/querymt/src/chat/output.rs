use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::{fmt, str::FromStr};

use super::{ChatMessagePartDelta, ChatRole, FinishReason};
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
        self.0
            .params()
            .map(|(name, value)| (name.as_str(), value.as_str()))
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

impl fmt::Display for ChatOutput {
    /// Render the visible text projection of the canonical output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text().unwrap_or_default())
    }
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
            extensions: Extensions::new(),
        }
    }
}

impl ChatOutput {
    /// Build a limited canonical output from already-normalized projections.
    pub fn from_projections(
        thinking: Option<String>,
        text: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
        usage: Option<Usage>,
        finish_reason: Option<FinishReason>,
    ) -> Self {
        let mut items = Vec::new();

        if let Some(thinking) = thinking
            && !thinking.is_empty()
        {
            items.push(ChatOutputItem::Reasoning(ChatReasoningItem {
                id: None,
                summary: Vec::new(),
                content: vec![ChatReasoningPart::text(thinking)],
                encrypted_content: None,
                signature: None,
                status: None,
                extensions: Extensions::new(),
            }));
        }

        if let Some(text) = text
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

        if let Some(calls) = tool_calls {
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
            status: finish_reason.map(|_| ChatOutputStatus::Completed),
            usage,
            finish_reason,
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

    pub fn usage(&self) -> Option<&Usage> {
        self.usage.as_ref()
    }

    /// Whether this response completed successfully and may authorize local calls.
    pub fn is_successful(&self) -> bool {
        self.status == Some(ChatOutputStatus::Completed)
    }

    /// Whether this output retains semantics a legacy boundary cannot preserve.
    pub fn requires_item_aware_fidelity(&self) -> bool {
        self.items.iter().any(ChatOutputItem::is_native)
    }

    /// Iterate over canonical function-call items without allocation or projection.
    pub fn function_calls(&self) -> impl Iterator<Item = &ChatFunctionCallItem> {
        self.items.iter().filter_map(|item| match item {
            ChatOutputItem::FunctionCall(call) => Some(call),
            _ => None,
        })
    }

    /// Project supported function items to the legacy `ToolCall` shape.
    pub fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        let calls: Vec<ToolCall> = self
            .function_calls()
            .map(ChatFunctionCallItem::to_tool_call)
            .collect();
        (!calls.is_empty()).then_some(calls)
    }

    /// Return executable calls only after successful response-level validation.
    pub fn executable_tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.is_successful().then(|| self.tool_calls()).flatten()
    }

    /// Consume this output and remove origin-scoped continuation while retaining
    /// portable messages, visible reasoning, and call/result correlation.
    pub fn into_portable(mut self) -> Self {
        self.response_id = None;
        self.provenance = None;
        self.extensions.clear();
        self.items.retain_mut(|item| match item {
            ChatOutputItem::Message(message) => {
                message.id = None;
                message.phase = None;
                message.extensions.clear();
                for part in &mut message.parts {
                    match part {
                        ChatMessagePart::Text {
                            annotations,
                            extensions,
                            ..
                        } => {
                            annotations.clear();
                            extensions.clear();
                        }
                        ChatMessagePart::Refusal { extensions, .. } => extensions.clear(),
                        ChatMessagePart::Media(media) => media.extensions.clear(),
                        ChatMessagePart::Opaque(_) => return false,
                    }
                }
                true
            }
            ChatOutputItem::Reasoning(reasoning) => {
                reasoning.id = None;
                reasoning.encrypted_content = None;
                reasoning.signature = None;
                reasoning.status = None;
                reasoning.extensions.clear();
                for part in reasoning.summary.iter_mut().chain(&mut reasoning.content) {
                    part.extensions.clear();
                }
                !reasoning.visible_text().is_empty()
            }
            ChatOutputItem::FunctionCall(call) => {
                call.item_id = None;
                call.status = None;
                call.extensions.clear();
                true
            }
            ChatOutputItem::Opaque(_) => false,
        });
        self
    }

    /// Build a lossy display projection of visible text and attachments.
    /// Function calls remain available through `function_calls()` instead of
    /// being misrepresented as ordinary argument text.
    pub fn portable_input_parts(&self) -> Vec<ChatInputPart> {
        let mut parts = Vec::new();

        for item in &self.items {
            match item {
                ChatOutputItem::Message(message) => {
                    for part in &message.parts {
                        match part {
                            ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                                parts.push(ChatInputPart::text(text.clone()));
                            }
                            ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                                parts.push(ChatInputPart::text(refusal.clone()));
                            }
                            ChatMessagePart::Media(media) => {
                                parts.push(ChatInputPart::attachment((**media).clone()));
                            }
                            _ => {}
                        }
                    }
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
                        parts.push(ChatInputPart::text(visible.join("\n\n")));
                    }
                }
                ChatOutputItem::FunctionCall(_) | ChatOutputItem::Opaque(_) => {}
            }
        }

        parts
    }
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
///
/// Fields carry cross-field invariants (inline bytes require a media type; a
/// data URL's declared type must agree with its payload). Deserialization runs
/// through the same validation as [`MediaPart::new`], so serialized input cannot
/// construct a state rejected by ordinary constructors.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaPart {
    pub kind: MediaKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_type: Option<MediaType>,
    source: MediaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl<'de> Deserialize<'de> for MediaPart {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // A permissive DTO collects the raw fields; construction then re-runs the
        // cross-field validation so serde cannot bypass the invariants.
        #[derive(Deserialize)]
        struct MediaPartDto {
            kind: MediaKind,
            #[serde(default)]
            media_type: Option<MediaType>,
            source: MediaSource,
            #[serde(default)]
            filename: Option<String>,
            #[serde(default)]
            detail: Option<String>,
            #[serde(default, flatten)]
            extensions: Extensions,
        }

        let dto = MediaPartDto::deserialize(deserializer)?;
        let mut media =
            MediaPart::new(dto.kind, dto.media_type, dto.source).map_err(de::Error::custom)?;
        media.filename = dto.filename;
        media.detail = dto.detail;
        media.extensions = dto.extensions;
        Ok(media)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaNormalizationError {
    #[error(transparent)]
    MediaType(#[from] MediaTypeError),
    #[error("inline media requires a MIME type")]
    MissingInlineMediaType,
    #[error("invalid data URL")]
    InvalidDataUrl,
    // Media types are boxed so the error stays small enough to return by value.
    #[error(
        "data URL MIME type {} conflicts with declared MIME type {}",
        actual.as_ref(),
        declared.as_ref()
    )]
    ConflictingDataUrlMediaType {
        declared: Box<MediaType>,
        actual: Box<MediaType>,
    },
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
                        declared: Box::new(declared.clone()),
                        actual: Box::new(actual),
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

    pub fn media_type(&self) -> Option<&MediaType> {
        self.media_type.as_ref()
    }

    pub fn source(&self) -> &MediaSource {
        &self.source
    }

    /// Replace source and MIME metadata only when the pair is valid.
    pub fn set_source(
        &mut self,
        media_type: Option<MediaType>,
        source: MediaSource,
    ) -> Result<(), MediaNormalizationError> {
        let replacement = Self::new(self.kind, media_type, source)?;
        self.media_type = replacement.media_type;
        self.source = replacement.source;
        Ok(())
    }

    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
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

impl ChatOutputItem {
    /// Whether this item retains native provider semantics (identity, opaque
    /// state, or continuation) that a plain portable projection would lose.
    ///
    /// Fidelity is derived from the retained fields themselves rather than a
    /// caller-set marker on the output as a whole.
    pub fn is_native(&self) -> bool {
        match self {
            ChatOutputItem::Message(item) => {
                item.id.is_some()
                    || item.phase.is_some()
                    || !item.extensions.is_empty()
                    || item.parts.iter().any(|part| match part {
                        ChatMessagePart::Text {
                            annotations,
                            extensions,
                            ..
                        } => !annotations.is_empty() || !extensions.is_empty(),
                        ChatMessagePart::Refusal { extensions, .. } => !extensions.is_empty(),
                        ChatMessagePart::Media(media) => !media.extensions.is_empty(),
                        ChatMessagePart::Opaque(_) => true,
                    })
            }
            ChatOutputItem::Reasoning(item) => {
                item.id.is_some()
                    || item.encrypted_content.is_some()
                    || item.signature.is_some()
                    || !item.extensions.is_empty()
                    || item
                        .summary
                        .iter()
                        .chain(item.content.iter())
                        .any(|part| !part.extensions.is_empty())
            }
            ChatOutputItem::FunctionCall(item) => {
                item.item_id.is_some() || item.status.is_some() || !item.extensions.is_empty()
            }
            ChatOutputItem::Opaque(_) => true,
        }
    }
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
    Media(Box<MediaPart>),
    Opaque(ChatOpaquePart),
}

impl ChatMessagePart {
    /// Whether this is an empty placeholder created by the accumulator for a
    /// part index a provider addressed before declaring the part itself.
    pub(crate) fn is_empty_placeholder(&self) -> bool {
        match self {
            ChatMessagePart::Text { text, .. } => text.is_empty(),
            ChatMessagePart::Refusal { refusal, .. } => refusal.is_empty(),
            ChatMessagePart::Media(_) | ChatMessagePart::Opaque(_) => false,
        }
    }

    /// Materialize a typed part from an indexed delta addressed before the
    /// provider declared the part, so ordinary part/delta ordering survives.
    pub(crate) fn into_message_part(self, delta: &ChatMessagePartDelta) -> Self {
        match self {
            ChatMessagePart::Text {
                text,
                annotations,
                extensions,
            } => {
                let mut text = text;
                if let ChatMessagePartDelta::Text { delta } = delta {
                    text.push_str(delta);
                }
                ChatMessagePart::Text {
                    text,
                    annotations,
                    extensions,
                }
            }
            ChatMessagePart::Refusal {
                refusal,
                extensions,
            } => {
                let mut refusal = refusal;
                if let ChatMessagePartDelta::Refusal { delta } = delta {
                    refusal.push_str(delta);
                }
                ChatMessagePart::Refusal {
                    refusal,
                    extensions,
                }
            }
            other => other,
        }
    }
}

/// Create the empty placeholder used for indexed parts addressed before
/// declaration. It is always a `Text` part because text is the common case and
/// its first delta is either text (kept) or a type mismatch (reported).
pub fn empty_message_part() -> ChatMessagePart {
    ChatMessagePart::Text {
        text: String::new(),
        annotations: Vec::new(),
        extensions: Extensions::new(),
    }
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
            .field("extensions", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatReasoningPart {
    pub text: String,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl ChatReasoningItem {
    /// Concatenated visible reasoning text (summary followed by content).
    ///
    /// Encrypted continuation and provider signatures are deliberately excluded:
    /// this is display/estimation text, not replayable state.
    pub fn visible_text(&self) -> String {
        let parts: Vec<&str> = self
            .summary
            .iter()
            .chain(&self.content)
            .map(|part| part.text.as_str())
            .filter(|text| !text.is_empty())
            .collect();
        parts.join("\n\n")
    }
}

impl ChatReasoningPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            extensions: Extensions::new(),
        }
    }

    /// Responses `reasoning_text` that can be replayed natively.
    ///
    /// Legacy `{ "type": "text" }` parts stay readable but are not promoted.
    pub fn reasoning_text_for_replay(&self) -> Option<&str> {
        if self.text.is_empty() {
            return None;
        }
        match self.extensions.get("type").and_then(Value::as_str) {
            None | Some("reasoning_text") => Some(self.text.as_str()),
            _ => None,
        }
    }
}

/// A function call retaining distinct provider item and call identities.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

impl fmt::Debug for ChatFunctionCallItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatFunctionCallItem")
            .field("item_id", &self.item_id)
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("arguments", &self.arguments)
            .field("status", &self.status)
            .field("extensions", &"[REDACTED]")
            .finish()
    }
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

// ---------------------------------------------------------------------------
// Canonical input model
// ---------------------------------------------------------------------------

/// One ordered part of ordinary (supplied) chat input.
///
/// Generated reasoning and function calls are output-only concepts; they are
/// deliberately not representable here. Use [`ChatOutput`] items for replaying
/// generated semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatInputPart {
    /// Plain user/assistant text.
    Text { text: String },
    /// A validated attachment (image, audio, video, document, or resource link).
    Attachment(Box<MediaPart>),
    /// The result of a previously requested tool call.
    ToolResult(ToolResult),
}

impl ChatInputPart {
    /// Create a text input part.
    pub fn text(text: impl Into<String>) -> Self {
        ChatInputPart::Text { text: text.into() }
    }

    /// Create a validated attachment input part.
    pub fn attachment(media: MediaPart) -> Self {
        ChatInputPart::Attachment(Box::new(media))
    }

    /// Create a correlated tool-result input part.
    pub fn tool_result(result: ToolResult) -> Self {
        ChatInputPart::ToolResult(result)
    }

    /// Returns the text if this is a text part.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ChatInputPart::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Returns the attachment if this is an attachment part.
    pub fn as_attachment(&self) -> Option<&MediaPart> {
        match self {
            ChatInputPart::Attachment(media) => Some(media),
            _ => None,
        }
    }

    /// Returns the correlated tool result if this is a tool-result part.
    pub fn as_tool_result(&self) -> Option<&ToolResult> {
        match self {
            ChatInputPart::ToolResult(result) => Some(result),
            _ => None,
        }
    }

    /// Returns true if this is a tool result part.
    pub fn is_tool_result(&self) -> bool {
        matches!(self, ChatInputPart::ToolResult(_))
    }
}

/// A correlated tool result. Bounded and nonrecursive: it cannot contain another
/// result, a function call, or generated reasoning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The call ID this result answers.
    pub call_id: String,
    /// Optional function name, preserved when the provider supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the tool reported an error.
    #[serde(default)]
    pub is_error: bool,
    /// Ordered result parts (text and validated attachments only).
    #[serde(default)]
    pub parts: Vec<ToolResultPart>,
    #[serde(default, flatten)]
    pub extensions: Extensions,
}

impl ToolResult {
    /// Create an empty successful result for a call.
    pub fn new(call_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: None,
            is_error: false,
            parts: Vec::new(),
            extensions: Extensions::new(),
        }
    }

    /// Create a text-only result for a call.
    pub fn text(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(call_id).with_text(text)
    }

    /// Mark this result as an error.
    pub fn error(mut self) -> Self {
        self.is_error = true;
        self
    }

    /// Set the optional function name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Append a text part.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.parts.push(ToolResultPart::Text { text: text.into() });
        self
    }

    /// Append a validated attachment part.
    pub fn with_attachment(mut self, media: MediaPart) -> Self {
        self.parts.push(ToolResultPart::Attachment(Box::new(media)));
        self
    }

    /// Concatenate the text parts.
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(ToolResultPart::as_text)
            .collect::<Vec<_>>()
            .join("")
    }
}

/// One ordered part of a tool result. Deliberately nonrecursive.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    /// Text result content.
    Text { text: String },
    /// A validated attachment result.
    Attachment(Box<MediaPart>),
}

impl ToolResultPart {
    /// Create a text result part.
    ///
    /// This is the ergonomic constructor for the common case where a tool
    /// produces a single block of text output.
    pub fn text(text: impl Into<String>) -> Self {
        ToolResultPart::Text { text: text.into() }
    }

    /// Create a validated attachment result part.
    pub fn attachment(media: MediaPart) -> Self {
        ToolResultPart::Attachment(Box::new(media))
    }

    /// Returns the text if this is a text part.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ToolResultPart::Text { text } => Some(text),
            ToolResultPart::Attachment(_) => None,
        }
    }

    /// Returns the attachment if this is an attachment part.
    pub fn as_attachment(&self) -> Option<&MediaPart> {
        match self {
            ToolResultPart::Attachment(media) => Some(media),
            ToolResultPart::Text { .. } => None,
        }
    }
}

impl From<String> for ToolResultPart {
    fn from(text: String) -> Self {
        ToolResultPart::text(text)
    }
}

impl From<&str> for ToolResultPart {
    fn from(text: &str) -> Self {
        ToolResultPart::text(text)
    }
}

impl From<MediaPart> for ToolResultPart {
    fn from(media: MediaPart) -> Self {
        ToolResultPart::attachment(media)
    }
}

impl From<Box<MediaPart>> for ToolResultPart {
    fn from(media: Box<MediaPart>) -> Self {
        ToolResultPart::Attachment(media)
    }
}

// ---------------------------------------------------------------------------
// Exclusive message payload
// ---------------------------------------------------------------------------

/// The single authoritative payload of a chat turn.
///
/// A user or tool-result turn carries canonical [`ChatInputPart`]s. An assistant
/// turn carries a [`ChatOutput`]. The two forms are exclusive: callers cannot
/// independently mutate a portable projection and structured output, so a stale
/// projection can never hide authoritative continuation state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatMessagePayload {
    /// Supplied input parts (ordinary user/tool-result turns, or a portable
    /// projection of an assistant turn).
    Input(Vec<ChatInputPart>),
    /// Structured generated output (assistant turns).
    Output(Box<ChatOutput>),
}

impl ChatMessagePayload {
    /// Build a canonical input payload.
    pub fn input(parts: Vec<ChatInputPart>) -> Self {
        ChatMessagePayload::Input(parts)
    }

    /// Build a structured output payload.
    pub fn output(output: ChatOutput) -> Self {
        ChatMessagePayload::Output(Box::new(output))
    }

    /// Borrow the payload as canonical input parts, if this is an input payload.
    pub fn as_input(&self) -> Option<&[ChatInputPart]> {
        match self {
            ChatMessagePayload::Input(parts) => Some(parts),
            ChatMessagePayload::Output(_) => None,
        }
    }

    /// Borrow the payload as structured output, if this is an output payload.
    pub fn as_output(&self) -> Option<&ChatOutput> {
        match self {
            ChatMessagePayload::Output(output) => Some(output),
            ChatMessagePayload::Input(_) => None,
        }
    }

    /// Whether this payload carries structured generated output.
    pub fn is_output(&self) -> bool {
        matches!(self, ChatMessagePayload::Output(_))
    }

    /// Convert this payload into canonical portable input parts.
    ///
    /// Structured output is projected on demand; provider-only continuation is
    /// intentionally not part of the projection. Input payloads are returned
    /// unchanged.
    pub fn into_portable(self) -> Vec<ChatInputPart> {
        match self {
            ChatMessagePayload::Input(parts) => parts,
            ChatMessagePayload::Output(output) => output.portable_input_parts(),
        }
    }

    /// Borrow this payload as canonical portable input parts, projecting
    /// structured output on demand.
    pub fn portable_parts(&self) -> Vec<ChatInputPart> {
        match self {
            ChatMessagePayload::Input(parts) => parts.clone(),
            ChatMessagePayload::Output(output) => output.portable_input_parts(),
        }
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
    fn payload_output_box_round_trips_through_internally_tagged_serde() {
        let payload = ChatMessagePayload::output(ChatOutput::from_projections(
            None,
            Some("boxed answer".into()),
            None,
            None,
            Some(FinishReason::Stop),
        ));

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("kind").and_then(Value::as_str), Some("output"));

        let restored: ChatMessagePayload = serde_json::from_value(json).unwrap();
        assert_eq!(restored, payload);
        // Boxing keeps the payload comparable to the `Input` variant instead
        // of reserving the whole `ChatOutput` inline for every message.
        assert!(std::mem::size_of::<ChatMessagePayload>() < std::mem::size_of::<ChatOutput>());
    }

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
        assert_eq!(defaulted.media_type().unwrap().type_(), "text");

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
        assert!(matches!(decoded.source(), MediaSource::ProviderFile { .. }));
        assert!(matches!(
            decoded.source(),
            MediaSource::ProviderFile { origin: actual, .. } if actual == &origin
        ));
    }

    #[test]
    fn media_deserialization_enforces_the_same_invariants_as_construction() {
        // Inline bytes without a media type fail through serde exactly as they do
        // through the constructor.
        let inline_without_type = serde_json::json!({
            "kind": "image",
            "source": {"type": "inline", "data": [1, 2, 3]}
        });
        assert!(
            serde_json::from_value::<MediaPart>(inline_without_type).is_err(),
            "inline media must require a media type through serde"
        );

        // A data URL whose declared type conflicts with the separately supplied
        // type fails through serde.
        let conflicting = serde_json::json!({
            "kind": "image",
            "media_type": "image/jpeg",
            "source": {"type": "data_url", "url": "data:image/png;base64,iVBORw0KGgo="}
        });
        assert!(
            serde_json::from_value::<MediaPart>(conflicting).is_err(),
            "conflicting data URL media type must fail through serde"
        );
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
        let media_type = decoded_inline.media_type().unwrap();
        assert_eq!(media_type.type_(), "application");
        assert_eq!(media_type.subtype(), "pdf");
        assert_eq!(media_type.parameter("charset"), Some("binary"));
        assert_eq!(decoded_inline.filename.as_deref(), Some("report.pdf"));
        assert_eq!(decoded_inline.detail.as_deref(), Some("high"));

        // Provider reference scope is preserved exactly, including origin.
        let decoded_reference = roundtrip(&reference);
        assert_eq!(decoded_reference, reference);
        match &decoded_reference.source() {
            MediaSource::ProviderFile { file_id, origin: o } => {
                assert_eq!(file_id, "file_abc");
                assert_eq!(o, &origin);
            }
            other => panic!("expected provider file reference, got {other:?}"),
        }

        // An unresolved provider reference retains only its provider-scoped ID.
        assert!(matches!(
            decoded_reference.source(),
            MediaSource::ProviderFile { .. }
        ));

        // URL source form, filename, and absent MIME metadata survive as-is.
        let decoded_url = roundtrip(&url_media);
        assert_eq!(decoded_url, url_media);
        assert!(decoded_url.media_type().is_none());
        assert_eq!(decoded_url.filename.as_deref(), Some("pic.png"));
        assert!(matches!(decoded_url.source(), MediaSource::Url { .. }));
    }

    struct LimitedProvider;

    #[async_trait]
    impl ChatProvider for LimitedProvider {
        async fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<ChatOutput, LLMError> {
            Ok(ChatOutput::from_projections(
                None,
                Some("legacy text".into()),
                Some(vec![ToolCall {
                    id: "call_legacy".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "legacy_tool".into(),
                        arguments: "{\"value\":1}".into(),
                    },
                }]),
                None,
                Some(FinishReason::ToolCalls),
            ))
        }
    }

    #[async_trait]
    impl CompletionProvider for LimitedProvider {
        async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
            Ok(CompletionResponse {
                text: "legacy completion".into(),
            })
        }
    }

    #[async_trait]
    impl EmbeddingProvider for LimitedProvider {
        async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
            Ok(Vec::new())
        }
    }

    impl crate::LLMProvider for LimitedProvider {}

    #[tokio::test]
    async fn legacy_provider_remains_usable_through_dyn_llm_provider() {
        let provider: &dyn crate::LLMProvider = &LimitedProvider;
        let output = provider.chat(&[]).await.unwrap();

        assert!(!output.requires_item_aware_fidelity());
        assert_eq!(output.text().as_deref(), Some("legacy text"));
        assert_eq!(output.tool_calls().unwrap()[0].id, "call_legacy");
        assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn projections_build_limited_output_without_native_identities() {
        let output = ChatOutput::from_projections(
            Some("thinking".into()),
            Some("hello".into()),
            None,
            Some(Usage {
                input_tokens: 3,
                output_tokens: 5,
                ..Usage::default()
            }),
            Some(FinishReason::Stop),
        );

        assert_eq!(output.thinking().as_deref(), Some("thinking"));
        assert_eq!(output.text().as_deref(), Some("hello"));
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(output.usage.as_ref().unwrap().input_tokens, 3);
        assert!(output.items.iter().all(|item| !item.is_native()));
        let ChatOutputItem::Reasoning(reasoning) = &output.items[0] else {
            panic!("expected reasoning");
        };
        assert!(reasoning.summary.is_empty());
        assert_eq!(
            reasoning
                .content
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>(),
            vec!["thinking"]
        );
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
    fn executable_calls_require_completed_response() {
        let call = ToolCall {
            id: "call_1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "lookup".into(),
                arguments: "{}".into(),
            },
        };
        let mut output = ChatOutput::from_projections(
            None,
            None,
            Some(vec![call]),
            None,
            Some(FinishReason::ToolCalls),
        );
        output.status = Some(ChatOutputStatus::Incomplete);
        assert!(
            output.tool_calls().is_some(),
            "partial calls remain inspectable"
        );
        assert!(output.executable_tool_calls().is_none());

        output.status = Some(ChatOutputStatus::Completed);
        assert_eq!(output.executable_tool_calls().unwrap().len(), 1);
    }

    #[test]
    fn portable_output_strips_native_state_but_keeps_call_identity() {
        let output = ChatOutput {
            response_id: Some("resp_1".into()),
            provenance: Some(ChatOutputProvenance::default()),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("reasoning_1".into()),
                    summary: vec![ChatReasoningPart::text("visible")],
                    content: Vec::new(),
                    encrypted_content: Some("secret".into()),
                    signature: Some("signature".into()),
                    status: None,
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("item_1".into()),
                    call_id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                    status: Some(ChatOutputStatus::Completed),
                    extensions: Extensions::new(),
                }),
            ],
            ..ChatOutput::default()
        }
        .into_portable();

        assert!(output.provenance.is_none());
        assert!(!output.requires_item_aware_fidelity());
        let call = output.function_calls().next().unwrap();
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.name, "lookup");
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
        let message = crate::chat::ChatMessage::from_assistant_output(
            serde_json::from_value::<ChatOutput>(fixture.clone()).unwrap(),
        );

        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: crate::chat::ChatMessage = serde_json::from_str(&encoded).unwrap();
        let output = decoded
            .output()
            .expect("structured output survives")
            .clone();

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
        const REASONING_EXT: &str = "sentinel-reasoning-extension-secret";
        const THOUGHT_SIG: &str = "sentinel-google-thought-signature";

        let fixture = serde_json::json!({
            "status": "completed",
            "items": [
                {
                    "type": "reasoning",
                    "id": "reasoning_1",
                    "summary": [{"text": "visible summary"}],
                    "content": [],
                    "encrypted_content": ENCRYPTED,
                    "signature": SIGNATURE,
                    "google_thought_signature": REASONING_EXT
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "lookup",
                    "arguments": "{}",
                    "google_thought_signature": THOUGHT_SIG
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
        assert!(
            !debug.contains(REASONING_EXT),
            "reasoning extensions must not appear in Debug output"
        );
        assert!(
            !debug.contains(THOUGHT_SIG),
            "function-call extensions must not appear in Debug output"
        );
        // Visible summary text is display content and may still be shown.
        assert!(debug.contains("visible summary"));

        // Authorized persistence is unaffected: serde keeps the exact bytes.
        let encoded = serde_json::to_string(&output).unwrap();
        assert!(encoded.contains(ENCRYPTED));
        assert!(encoded.contains(SIGNATURE));
        assert!(encoded.contains(OPAQUE));
        assert!(encoded.contains(REASONING_EXT));
        assert!(encoded.contains(THOUGHT_SIG));
    }
}
