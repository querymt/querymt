use anyhow::{anyhow, Result};
use clap::{CommandFactory, Parser};
use colored::*;
use querymt::{
    builder::LLMBuilder,
    mcp::{adapter::McpToolAdapter, config::Config as MCPConfig},
};
use serde_json::Value;
use spinners::{Spinner, Spinners};
use std::io::{self, IsTerminal};
use tokio;

mod chat;
mod cli_args;
mod embed;
mod provider;
mod secret_store;
mod tracing;
mod utils;

use chat::{chat_pipe, interactive_loop};
use cli_args::{CliArgs, Commands};
use embed::embed_pipe;
use provider::{get_api_key, get_provider_info, get_provider_registry, split_provider};
use secret_store::SecretStore;
use tracing::setup_logging;
use utils::get_provider_api_key;

fn resolve_provider_and_model(
    global: &CliArgs,
    subcmd_provider: Option<&String>,
    subcmd_model: Option<&String>,
) -> Result<(String, Option<String>)> {
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;

    if let Some((p, m)) = get_provider_info(global) {
        provider = Some(p);
        model = m;
    }

    if let Some(p) = subcmd_provider {
        let (p2, m2) = split_provider(p);
        provider = Some(p2);
        if m2.is_some() {
            model = m2;
        }
    }

    if let Some(m) = subcmd_model {
        model = Some(m.clone());
    }

    match provider {
        Some(p) => Ok((p, model)),
        None => Err(anyhow!(
            "No provider specified. Use --provider or set a default"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logging();
    let args = CliArgs::parse();

    // Handle completion generation and exit early.
    if let Some(Commands::Completion { shell }) = &args.command {
        let mut cmd = CliArgs::command();
        clap_complete::generate(*shell, &mut cmd, "qmt", &mut io::stdout());
        return Ok(());
    }

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
            Commands::Embed {
                encoding_format,
                dimensions,
                provider: sc_provider,
                model: sc_model,
                separator,
                text,
            } => {
                let (prov_name, opt_model) =
                    resolve_provider_and_model(&args, sc_provider.as_ref(), sc_model.as_ref())?;
                let mut builder = LLMBuilder::new().provider(prov_name.clone());
                if let Some(m) = opt_model {
                    builder = builder.model(m);
                }
                if let Some(key) = get_api_key(&prov_name, &args, &registry) {
                    builder = builder.api_key(key);
                }
                if let Some(url) = &args.base_url {
                    builder = builder.base_url(url.clone());
                }
                if let Some(ef) = encoding_format {
                    builder = builder.embedding_encoding_format(ef);
                }
                if let Some(dim) = dimensions {
                    builder = builder.embedding_dimensions(*dim);
                }

                let provider = builder.build(&registry)?;
                let embeddings = embed_pipe(&provider, text.as_ref(), separator.as_ref()).await?;

                // pretty-print as JSON
                println!("{}", serde_json::to_string_pretty(&embeddings)?);
                return Ok(());
            }
            Commands::Update => {
                println!("{}", "Updating OCI provider plugins...".bright_blue());
                for provider_cfg in &registry.config.providers {
                    if provider_cfg.path.starts_with("oci://") {
                        let name = provider_cfg.name.clone();
                        // start spinner (choose preset you like)
                        let mut spinner =
                            Spinner::new(Spinners::Dots, format!("Updating {}...", name.bold()));

                        let image_reference = provider_cfg
                            .path
                            .strip_prefix("oci://")
                            .unwrap()
                            .to_string();
                        match registry
                            .oci_downloader
                            .pull_and_extract(&image_reference, None, &registry.cache_path, true)
                            .await
                        {
                            Ok(_) => {
                                // stop and show success
                                spinner.stop_and_persist(
                                    "🚀",
                                    format!("{} has been updated.", name.bold()),
                                );
                            }
                            Err(e) => {
                                spinner.stop_and_persist(
                                    "💥",
                                    format!("Failed updating {}", name.bold()),
                                );
                                eprintln!("  {} {}", "Error:".bright_red(), e);
                            }
                        }
                    }
                }
                println!("{}", "Update check complete.".bright_blue());
                return Ok(());
            }
            // This command is handled before the match statement
            Commands::Completion { .. } => unreachable!(),
        }
    }

    // Build provider + LLMBuilder
    let (prov_name, opt_model) = resolve_provider_and_model(&args, None, None)?;
    let mut builder = LLMBuilder::new().provider(prov_name.clone());
    if let Some(m) = opt_model.or(args.model.clone()) {
        builder = builder.model(m);
    }
    if let Some(sys) = &args.system {
        builder = builder.system(sys.clone());
    }
    if let Some(key) = get_api_key(&prov_name, &args, &registry) {
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
    let mcp_clients;
    if let Some(cfg) = &args.mcp_config {
        let cfg = MCPConfig::load(cfg.clone()).await?;
        mcp_clients = cfg.create_mcp_clients().await?;
        for (_name, client) in mcp_clients.iter() {
            let server = client.peer();
            let tools = server.list_all_tools().await?;
            for t in tools {
                if let Ok(adapter) = McpToolAdapter::try_new(t, server.clone()) {
                    builder = builder.add_tool(adapter);
                }
            }
        }
    }
    let provider = builder.build(&registry)?;
    let is_pipe = !io::stdin().is_terminal();

    if is_pipe || args.prompt.is_some() {
        return chat_pipe(&provider, args.prompt.as_ref()).await;
    }

    interactive_loop(&provider, &prov_name).await
}
