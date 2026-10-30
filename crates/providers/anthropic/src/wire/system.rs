//! System prompt, cache-control and citation wire types.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

// --- System prompt types (Anthropic API union: string | TextBlockParam[]) ---

/// Time-to-live for cache control breakpoints.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub enum CacheTTL {
    /// 5 minutes
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 1 hour
    #[serde(rename = "1h")]
    OneHour,
}

/// Cache control configuration for ephemeral caching.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CacheControlEphemeral {
    #[serde(rename = "type")]
    pub control_type: String,
    /// Time-to-live for the cache control breakpoint. Defaults to 5m.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTTL>,
}

/// Citation referencing a character location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationCharLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_char_index: u64,
    pub start_char_index: u64,
}

/// Citation referencing a page location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationPageLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_page_number: u64,
    pub start_page_number: u64,
}

/// Citation referencing a content block location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationContentBlockLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_block_index: u64,
    pub start_block_index: u64,
}

/// Citation referencing a web search result location.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationWebSearchResultLocationParam {
    pub cited_text: String,
    pub encrypted_index: String,
    pub title: String,
    pub url: String,
}

/// Citation referencing a search result location.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationSearchResultLocationParam {
    pub cited_text: String,
    pub end_block_index: u64,
    pub search_result_index: u64,
    pub source: String,
    pub start_block_index: u64,
    pub title: String,
}

/// Union of all citation parameter types, discriminated by the `type` field.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum TextCitationParam {
    #[serde(rename = "char_location")]
    CharLocation(CitationCharLocationParam),
    #[serde(rename = "page_location")]
    PageLocation(CitationPageLocationParam),
    #[serde(rename = "content_block_location")]
    ContentBlockLocation(CitationContentBlockLocationParam),
    #[serde(rename = "web_search_result_location")]
    WebSearchResultLocation(CitationWebSearchResultLocationParam),
    #[serde(rename = "search_result_location")]
    SearchResultLocation(CitationSearchResultLocationParam),
}

/// A text content block used in system prompts, with optional cache control and citations.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct TextBlockParam {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<TextCitationParam>>,
}

/// Anthropic system prompt: either a plain string or an array of TextBlockParam.
///
/// Deserializes from three JSON shapes:
/// - `"string"` → `Text(String)`
/// - `["s1", "s2"]` → `Blocks` with each string wrapped as a `TextBlockParam`
/// - `[{"type":"text","text":"...","cache_control":{...}}]` → `Blocks(Vec<TextBlockParam>)`
#[derive(Debug, Clone, JsonSchema, Serialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicSystemPrompt {
    /// Plain text system prompt
    Text(String),
    /// Array of text content blocks with optional cache control and citations
    Blocks(Vec<TextBlockParam>),
}

pub(crate) fn schema_for_anthropic_system(
    generator: &mut schemars::SchemaGenerator,
) -> schemars::Schema {
    let portable = generator.subschema_for::<querymt::params::SystemPrompt>();
    let native = generator.subschema_for::<TextBlockParam>();
    serde_json::from_value(serde_json::json!({"anyOf": [
        {"type": "null"}, portable, native,
        {"type": "array", "items": {"anyOf": [portable, native]}}
    ]}))
    .expect("valid Anthropic instruction schema")
}

pub(crate) fn cache_control_from_hint(hint: &querymt::chat::CacheHint) -> CacheControlEphemeral {
    let querymt::chat::CacheHint::Ephemeral { ttl_seconds } = hint;
    CacheControlEphemeral {
        control_type: "ephemeral".into(),
        ttl: match ttl_seconds {
            Some(seconds) if *seconds > 300 => Some(CacheTTL::OneHour),
            Some(_) => Some(CacheTTL::FiveMinutes),
            None => None,
        },
    }
}

impl<'de> Deserialize<'de> for AnthropicSystemPrompt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let value = if value.is_object() {
            Value::Array(vec![value])
        } else {
            value
        };
        match value {
            Value::String(s) => Ok(AnthropicSystemPrompt::Text(s)),
            Value::Array(arr) => {
                let mut blocks = Vec::with_capacity(arr.len());
                for value in arr {
                    if value.get("type").is_some() {
                        let hint = value
                            .get("cache")
                            .filter(|v| !v.is_null())
                            .map(|hint| {
                                serde_json::from_value::<querymt::chat::CacheHint>(hint.clone())
                            })
                            .transpose()
                            .map_err(serde::de::Error::custom)?;
                        let mut block: TextBlockParam =
                            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                        if block.cache_control.is_none() {
                            block.cache_control = hint.as_ref().map(cache_control_from_hint);
                        }
                        blocks.push(block);
                        continue;
                    }
                    let part: querymt::params::SystemPrompt =
                        serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                    let cache_control = part.cache().map(cache_control_from_hint);
                    blocks.push(TextBlockParam {
                        block_type: "text".into(),
                        text: part.text().to_owned(),
                        cache_control,
                        citations: None,
                    });
                }
                Ok(AnthropicSystemPrompt::Blocks(blocks))
            }
            other => Err(serde::de::Error::custom(format!(
                "expected string or array for system prompt, got {}",
                other
            ))),
        }
    }
}
