use std::io::{self, IsTerminal};

use clap::Parser;
use colored::*;
use querymt::{
    builder::LLMBuilder, mcp::adapter::McpToolAdapter, mcp::config::Config as MCPConfig,
};
use serde_json::Value;
use tokio;

mod chat;
mod cli_args;
mod provider;
mod secret_store;
mod tracing;
mod utils;

use chat::{chat_pipe, interactive_loop};
use cli_args::{CliArgs, Commands};
use provider::{get_api_key, get_provider_info, get_provider_registry};
use secret_store::SecretStore;
use tracing::setup_logging;
use utils::get_provider_api_key;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    let args = CliArgs::parse();
    let registry = get_provider_registry(&args).await?;

    if let Some(cmd) = &args.command {
        match cmd {
            Commands::Set { key, value } => {
                let mut store = SecretStore::new()?;
                store.set(key, value)?;
                println!("{} Secret '{}' has been set.", "✓".bright_green(), key);
                return Ok(());
            }
            Commands::Get { key } => {
                let store = SecretStore::new()?;
                match store.get(key) {
                    Some(val) => println!("{}: {}", key, val),
                    None => println!("{} Secret '{}' not found", "!".bright_yellow(), key),
                }
                return Ok(());
            }
            Commands::Delete { key } => {
                let mut store = SecretStore::new()?;
                store.delete(key)?;
                println!("{} Secret '{}' has been deleted.", "✓".bright_green(), key);
                return Ok(());
            }
            Commands::Providers => {
                for factory in registry.list() {
                    println!("- {}", factory.name());
                }
                return Ok(());
            }
            Commands::Models => {
                let mut cfg: Value = Default::default();
                for factory in registry.list() {
                    print!("{}: ", factory.name());
                    let models = match factory.as_http() {
                        Some(http_factory) => {
                            match get_provider_api_key(http_factory) {
                                Some(api_key) => cfg = serde_json::json!({"api_key": api_key}),
                                _ => (),
                            }
                            factory.list_models(&cfg).await
                        }
                        None => factory.list_models(&cfg).await,
                    };

                    match models {
                        Ok(models) if !models.is_empty() => {
                            println!();
                            for model in models {
                                println!("  - {}:{}", factory.name(), model);
                            }
                        }
                        Ok(_) => {
                            println!("(no models returned)");
                        }
                        Err(e) => {
                            println!("error listing models: {}", e);
                        }
                    }
                }
                return Ok(());
            }
            Commands::Default { provider } => {
                if let Some(p) = provider {
                    let mut store = SecretStore::new()?;
                    store.set_default_provider(p)?;
                    return Ok(());
                } else if provider.is_none() {
                    let store = SecretStore::new()?;
                    match store.get_default_provider() {
                        Some(p) => println!("Default provider: {}", p),
                        None => println!("{} No default provider set", "!".bright_yellow()),
                    }
                    return Ok(());
                }
                eprintln!(
                    "{} Usage: qmt default <provider:model>",
                    "Error:".bright_red()
                );
                return Ok(());
            }
        }
    }

    // Build provider + LLMBuilder
    let (provider_name, model_name) = get_provider_info(&args)
        .ok_or("No provider specified. Use --provider or default provider.")?;
    let mut builder = LLMBuilder::new().provider(provider_name.clone());
    if let Some(m) = model_name.or(args.model.clone()) {
        builder = builder.model(m);
    }
    if let Some(sys) = &args.system {
        builder = builder.system(sys.clone());
    }
    if let Some(key) = get_api_key(&provider_name, &args, &*registry) {
        builder = builder.api_key(key);
    }
    if let Some(url) = &args.base_url {
        builder = builder.base_url(url.clone());
    }
    if let Some(t) = args.temperature {
        builder = builder.temperature(t);
    }
    if let Some(max) = args.max_tokens {
        builder = builder.max_tokens(max);
    }
    if let Some(tp) = args.top_p {
        builder = builder.top_p(tp);
    }
    if let Some(tk) = args.top_k {
        builder = builder.top_k(tk);
    }
    for (k, v) in &args.options {
        builder = builder.parameter(k.clone(), v.clone());
    }

    // MCP tools injection

    let clients;
    if let Some(cfg) = &args.mcp_config {
        let cfg = MCPConfig::load(cfg.clone()).await?;
        clients = cfg.create_mcp_clients().await?;
        for (_name, client) in clients {
            let server = client.peer();
            let tools = server.list_all_tools().await?;
            for t in tools {
                if let Ok(adapter) = McpToolAdapter::try_new(t, server.clone()) {
                    builder = builder.add_tool(adapter);
                }
            }
        }
    }
    let provider = builder.build(&*registry)?;
    let is_pipe = !io::stdin().is_terminal();

    if is_pipe || args.prompt.is_some() {
        return chat_pipe(&provider, args.prompt.as_ref()).await;
    }

    interactive_loop(&provider, &provider_name).await
}
