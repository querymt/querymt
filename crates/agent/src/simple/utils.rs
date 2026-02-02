//! Shared utility functions for the simple API

use crate::model::{AgentMessage, MessagePart};
use crate::tools::CapabilityRequirement;
use crate::tools::builtins::all_builtin_tools;
use anyhow::{Result, anyhow};
use once_cell::sync::Lazy;
use querymt::LLMParams;
use querymt::chat::ChatRole;
use querymt::plugin::{
    extism_impl::host::ExtismLoader, host::PluginRegistry, host::native::NativeLoader,
};
use std::collections::HashSet;
use std::path::PathBuf;

/// Converts a path to an absolute path, canonicalizing if it exists
pub(super) fn to_absolute_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        path.canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize path {:?}: {}", path, e))
    } else {
        std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current directory: {}", e))?
            .join(&path)
            .canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize path {:?}: {}", path, e))
    }
}

pub(super) async fn default_registry() -> Result<PluginRegistry> {
    let mut registry = PluginRegistry::from_default_path()
        .map_err(|e| anyhow!("Failed to load plugin registry: {}", e))?;
    registry.register_loader(Box::new(ExtismLoader));
    registry.register_loader(Box::new(NativeLoader));
    // Removed eager plugin loading - plugins are now loaded on-demand for faster startup
    // registry.load_all_plugins().await;
    Ok(registry)
}

pub(super) fn latest_assistant_message(messages: &[AgentMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|msg| msg.role == ChatRole::Assistant)
        .map(|msg| {
            msg.parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text { content } => Some(content.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
}

pub(super) fn infer_required_capabilities(tools: &[String]) -> HashSet<CapabilityRequirement> {
    let mut caps = HashSet::new();
    for tool in tools {
        if tool_requires_filesystem(tool) {
            caps.insert(CapabilityRequirement::Filesystem);
        }
    }
    caps
}

/// Set of builtin tool names that require filesystem access.
/// Lazily computed from `all_builtin_tools()` to stay in sync with tool definitions.
static FILESYSTEM_TOOLS: Lazy<HashSet<String>> = Lazy::new(|| {
    all_builtin_tools()
        .into_iter()
        .filter(|tool| {
            tool.required_capabilities()
                .contains(&CapabilityRequirement::Filesystem)
        })
        .map(|tool| tool.name().to_string())
        .collect()
});

pub(super) fn tool_requires_filesystem(tool_name: &str) -> bool {
    FILESYSTEM_TOOLS.contains(tool_name)
}

use super::config::AgentConfig;

pub(super) fn build_llm_config(config: &AgentConfig) -> Result<LLMParams> {
    config
        .llm_config
        .clone()
        .ok_or_else(|| anyhow!("LLM configuration required (call .provider() first)"))
}
