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
mod prompt;
mod quorum;
mod schema;
mod single;
mod tools;

pub use execution::*;
pub use extensions::*;
pub use hooks::*;
pub use loader::{ConfigSource, interpolate_env_vars, load_config};
pub use mcp::*;
pub use mesh::*;
pub use prompt::SystemPart;
pub(crate) use prompt::{deserialize_system_parts, resolve_system_parts};
pub use quorum::*;
pub use schema::{schema_for_system_parts, schema_for_value};
pub use single::*;
pub use tools::*;

/// Top-level config discriminator
#[derive(Debug)]
pub enum Config {
    Single(Box<SingleAgentConfig>),
    Multi(Box<QuorumConfig>),
}

fn default_true() -> bool {
    true
}

pub(crate) fn default_assume_mutating() -> bool {
    true
}

#[cfg(test)]
mod tests;
