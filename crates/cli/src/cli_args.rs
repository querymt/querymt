use crate::utils::parse_kv;
use clap::{Parser, Subcommand};
use clap_complete::Shell;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ToolPolicyState {
    Ask,
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolConfig {
    pub default: Option<ToolPolicyState>,
    pub tools: Option<HashMap<String, HashMap<String, ToolPolicyState>>>,
}

/// Command line arguments for the LLM CLI
#[derive(Parser, Debug)]
#[clap(
    name = "qmt",
    about = "Interactive CLI interface for chatting with LLM providers",
    allow_hyphen_values = true
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// The prompt to send to the LLM. If not provided, will enter interactive mode.
    #[arg()]
    pub prompt: Option<String>,

    /// LLM provider name
    #[arg(short = 'p', long = "provider")]
    pub backend: Option<String>,

    /// Model name to use
    #[arg(long)]
    pub model: Option<String>,

    /// System prompt to set context. Can be specified multiple times for multi-part prompts.
    #[arg(short, long, action = clap::ArgAction::Append)]
    pub system: Vec<String>,

    /// API key for the provider
    #[arg(long)]
    pub api_key: Option<String>,

    /// Base URL for the API
    #[arg(long)]
    pub base_url: Option<String>,

    /// Temperature setting (0.0-1.0)
    #[arg(long)]
    pub temperature: Option<f32>,

    #[arg(long)]
    pub top_p: Option<f32>,

    #[arg(long)]
    pub top_k: Option<u32>,

    /// Maximum tokens in the response
    #[arg(long)]
    pub max_tokens: Option<u32>,

    #[arg(long)]
    pub mcp_config: Option<String>,

    /// Custom providers config path
    #[arg(long, global = true)]
    pub provider_config: Option<String>,

    #[arg(short = 'o', value_parser = parse_kv, action = clap::ArgAction::Append)]
    pub options: Vec<(String, Value)>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage OAuth authentication
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Manage secrets and credentials
    Secrets {
        #[command(subcommand)]
        command: SecretsCommands,
    },
    /// List available providers
    Providers,
    /// List available models for providers
    Models,
    /// Get or set the default provider
    Default {
        #[arg()]
        provider: Option<String>,
    },
    /// Create an embedding for the provided text
    Embed {
        /// Document separator that marks the end of document
        #[arg(long, default_value = "\n")]
        separator: Option<String>,
        #[arg(long)]
        encoding_format: Option<String>,
        /// Number of dimensions for the embedding
        #[arg(short, long)]
        dimensions: Option<u32>,
        /// Provider override (e.g. "openai" or "openai:text-embedding-ada-002")
        #[arg(short = 'p', long = "provider")]
        provider: Option<String>,
        /// Model override (e.g. "text-embedding-ada-002")
        #[arg(short = 'm', long = "model")]
        model: Option<String>,
        /// Inline text to embed (otherwise read stdin)
        #[arg()]
        text: Option<String>,
    },
    /// Update provider plugins
    Update,
    /// Generate shell completions
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
pub enum AuthCommands {
    /// Login to a provider using OAuth
    Login {
        /// Provider to authenticate with (e.g., "anthropic", "openai")
        provider: String,
        /// OAuth mode (provider-specific, e.g., "max" or "console" for Anthropic)
        #[arg(long, default_value = "max")]
        mode: String,
    },
    /// Logout from an OAuth provider (remove stored tokens)
    Logout {
        /// Provider to logout from
        provider: String,
    },
    /// Check OAuth authentication status
    Status {
        /// Provider to check (defaults to all supported providers)
        provider: Option<String>,
        /// Skip automatic token refresh (show raw stored status)
        #[arg(long, default_value = "false")]
        no_refresh: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SecretsCommands {
    /// Set a secret key/value pair
    Set { key: String, value: String },
    /// Get a secret value by key
    Get { key: String },
    /// Delete a secret key
    Delete { key: String },
}
