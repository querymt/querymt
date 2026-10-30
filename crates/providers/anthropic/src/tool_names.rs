//! Tool-name prefixing (OAuth) and wire-to-declared name mapping.

use crate::config::Anthropic;
use querymt::chat::Tool;
use querymt::error::LLMError;
use serde_json::Value;
use std::collections::HashMap;

/// Tool name prefix used for OAuth requests to avoid conflicts with server-side tools
pub(crate) const TOOL_PREFIX: &str = "mcp_";

impl Anthropic {
    /// Prefixes a tool name with TOOL_PREFIX if using OAuth
    pub(crate) fn prefix_tool_name(&self, name: &str) -> String {
        if self.is_oauth() {
            let mut chars = name.chars();
            format!(
                "{}{}{}",
                TOOL_PREFIX,
                chars
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_default(),
                chars.as_str()
            )
        } else {
            name.to_string()
        }
    }

    /// Wire-to-original names for the effective request tools, never shared
    /// between requests or streams.
    pub(crate) fn declared_tool_names(
        &self,
        tools: Option<&[Tool]>,
    ) -> Result<HashMap<String, String>, LLMError> {
        let mut names = HashMap::new();
        for tool in tools.or(self.tools.as_deref()).unwrap_or_default() {
            let original = &tool.function.name;
            let wire = self.prefix_tool_name(original);
            if let Some(previous) = names.insert(wire.clone(), original.clone()) {
                return Err(LLMError::InvalidRequest(format!(
                    "Tool names {previous:?} and {original:?} both encode as {wire:?}",
                )));
            }
        }
        Ok(names)
    }

    pub(crate) fn tool_names_from_context(
        &self,
        context: Value,
    ) -> Result<HashMap<String, String>, LLMError> {
        if context.is_null() {
            self.declared_tool_names(None)
        } else {
            Ok(serde_json::from_value(context)?)
        }
    }

    /// Reverses [`prefix_tool_name`] for tool names echoed back by the API.
    ///
    /// Names that never carried [`TOOL_PREFIX`] were never transformed and are
    /// returned unchanged so their capitalization is preserved exactly. Prefixed
    /// names are restored through the request's wire-to-original mapping,
    /// falling back to legacy lowercasing only for unknown prefixed names.
    pub(crate) fn strip_tool_prefix(
        name: &str,
        declared: Option<&HashMap<String, String>>,
    ) -> String {
        let Some(stripped) = name.strip_prefix(TOOL_PREFIX) else {
            return name.to_string();
        };
        if let Some(original) = declared.and_then(|names| names.get(name)) {
            return original.clone();
        }
        if stripped == "StructuredOutput" {
            return stripped.to_string();
        }
        let mut chars = stripped.chars();
        format!(
            "{}{}",
            chars
                .next()
                .map(|c| c.to_lowercase().to_string())
                .unwrap_or_default(),
            chars.as_str()
        )
    }
}
