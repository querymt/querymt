//! Request tool definitions and strict tool-use selection.

use crate::config::Anthropic;
use crate::strict_schema;
use querymt::chat::Tool;
use serde::Serialize;
use serde_json::Value;

/// Anthropic-specific tool format that matches their API structure
#[derive(Serialize, Debug)]
pub(crate) struct AnthropicTool<'a> {
    pub(crate) name: String,
    pub(crate) description: &'a str,
    /// Borrowed original schema, or an owned converted copy for strict tools.
    #[serde(rename = "input_schema")]
    pub(crate) schema: std::borrow::Cow<'a, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) strict: Option<bool>,
}

impl Anthropic {
    /// Builds the request tools, applying strictness where every check passes.
    ///
    /// A tool is eligible when its definition sets `strict: true` or its
    /// unprefixed name is in `strict_tools`. Per-tool schema incompatibility
    /// falls back for that tool only; unsupported models or exceeded budgets
    /// fall back for the whole request. Every fallback sends the original,
    /// unmodified schemas.
    ///
    /// Strict tool use works over OAuth as well (verified against the live
    /// endpoint with `scripts/verify-anthropic-strict.sh`). OAuth only changes
    /// tool names (see `TOOL_PREFIX`); `strict_tools` is matched against the
    /// unprefixed name.
    pub(crate) fn build_tools<'a>(&self, tools: &'a [Tool]) -> Vec<AnthropicTool<'a>> {
        let plain = || -> Vec<AnthropicTool<'a>> {
            tools
                .iter()
                .map(|tool| AnthropicTool {
                    name: self.prefix_tool_name(&tool.function.name),
                    description: &tool.function.description,
                    schema: std::borrow::Cow::Borrowed(&tool.function.parameters),
                    strict: None,
                })
                .collect()
        };

        let names: &[String] = self.strict_tools.as_deref().unwrap_or_default();
        let eligible: Vec<bool> = tools
            .iter()
            .map(|tool| tool.function.strict == Some(true) || names.contains(&tool.function.name))
            .collect();
        if !eligible.iter().any(|e| *e) {
            return plain();
        }

        if !self
            .parsed_model()
            .is_some_and(|model| model.supports_strict_tools())
        {
            log::debug!(target: "qmt_anthropic", "strict tools omitted: model {} does not support them", self.model);
            return plain();
        }

        // Convert each eligible schema on a copy; incompatible tools fall back
        // individually and are excluded from the budget counts.
        let converted: Vec<Option<Value>> = tools
            .iter()
            .zip(&eligible)
            .map(|(tool, eligible)| {
                if !*eligible {
                    return None;
                }
                match strict_schema::convert(&tool.function.parameters) {
                    Ok(schema) => Some(schema),
                    Err(err) => {
                        log::warn!(
                            target: "qmt_anthropic",
                            "tool {} sent without strict: unsupported schema ({err})",
                            tool.function.name
                        );
                        None
                    }
                }
            })
            .collect();

        let strict_schemas: Vec<Value> = converted.iter().flatten().cloned().collect();
        if strict_schemas.is_empty() {
            return plain();
        }
        if let Err(limit) = strict_schema::budget(&strict_schemas) {
            log::warn!(target: "qmt_anthropic", "strict tools omitted for this request: {limit}");
            return plain();
        }

        tools
            .iter()
            .zip(converted)
            .map(|(tool, converted)| {
                let (schema, strict) = match converted {
                    Some(schema) => (std::borrow::Cow::Owned(schema), Some(true)),
                    None => (std::borrow::Cow::Borrowed(&tool.function.parameters), None),
                };
                AnthropicTool {
                    name: self.prefix_tool_name(&tool.function.name),
                    description: &tool.function.description,
                    schema,
                    strict,
                }
            })
            .collect()
    }
}
