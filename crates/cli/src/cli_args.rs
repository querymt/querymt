use crate::utils::parse_kv;
use clap::{Parser, Subcommand};
use serde_json::Value;

/// Command line arguments for the LLM CLI
#[derive(Parser, Debug)]
#[clap(
    name = "qmt",
    about = "Interactive CLI interface for chatting with LLM providers",
    allow_hyphen_values = true,
    args_conflicts_with_subcommands = true
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

    /// System prompt to set context
    #[arg(short, long)]
    pub system: Option<String>,

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

    #[arg(long)]
    pub provider_config: Option<String>,

    #[arg(short = 'o', value_parser = parse_kv, action = clap::ArgAction::Append)]
    pub options: Vec<(String, Value)>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Set a secret key/value pair
    Set { key: String, value: String },
    /// Get a secret value by key
    Get { key: String },
    /// Delete a secret key
    Delete { key: String },
    /// List available providers
    Providers,
    /// List available models for providers
    Models,
    /// Get or set the default provider
    Default {
        #[arg()]
        provider: Option<String>,
    },
}
