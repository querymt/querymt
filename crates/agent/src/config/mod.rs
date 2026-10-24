//! Configuration file support for agents
//!
//! Supports both single-agent and multi-agent (quorum) configurations from TOML files.

use crate::acp::protocol::{EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio};
use anyhow::{Context, Result, anyhow};
use regex::{Captures, Regex};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

mod execution;
mod extensions;
mod hooks;
mod loader;
mod mcp;
mod mesh;
mod permissions;
mod prompt;
mod quorum;
mod repository_knowledge;
mod schema;
mod single;
mod tools;
mod workspace;

pub use execution::*;
pub use extensions::*;
pub use hooks::*;
pub use loader::{ConfigSource, interpolate_env_vars, load_config};
pub use mcp::*;
pub use mesh::*;
pub use permissions::*;
pub use prompt::SystemPart;
pub(crate) use prompt::{deserialize_system_parts, resolve_system_parts};
pub use quorum::*;
pub use repository_knowledge::*;
pub use schema::{schema_for_system_parts, schema_for_value};
pub use single::*;
pub use tools::*;
pub use workspace::WorkspaceSettings;

/// Top-level config discriminator
#[derive(Debug)]
pub enum Config {
    Single(Box<SingleAgentConfig>),
    Multi(Box<QuorumConfig>),
}

impl Config {
    /// Every MCP server the configuration declares: the top-level `[[mcp]]`
    /// entries and, for a quorum, each delegate's own `mcp` entries.
    ///
    /// A server shared by several delegates is yielded once per declaration.
    pub fn mcp_servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        let (top, delegates): (&[McpServerConfig], &[DelegateConfig]) = match self {
            Self::Single(config) => (&config.mcp, &[]),
            Self::Multi(config) => (&config.mcp, &config.delegates),
        };
        top.iter()
            .chain(delegates.iter().flat_map(|delegate| delegate.mcp.iter()))
    }

    /// Protocol (`.agents/`) settings of the configuration.
    pub(crate) fn dotagents(&self) -> &crate::dotagents::DotagentsSettings {
        match self {
            Self::Single(config) => &config.dotagents,
            Self::Multi(config) => &config.dotagents,
        }
    }

    /// Working directory the configuration selects, if any.
    pub(crate) fn cwd(&self) -> Option<&Path> {
        match self {
            Self::Single(config) => config.agent.cwd.as_deref(),
            Self::Multi(config) => config.quorum.cwd.as_deref(),
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests;
