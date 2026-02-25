use anyhow::{Result, anyhow};
use clap::{CommandFactory, Parser};
use colored::*;
use querymt::{
    builder::LLMBuilder,
    mcp::{adapter::McpToolAdapter, config::Config as MCPConfig},
    plugin::host::{OciDownloadPhase, OciDownloadProgress, OciProgressCallback},
};

use spinners::{Spinner, Spinners};
use std::fs;
use std::io::{self, IsTerminal};
use std::sync::Arc;

mod auth;
mod chat;
mod cli_args;
mod embed;
mod provider;
mod secret_store;
mod tracing;
mod utils;

use chat::{chat_pipe, interactive_loop};
use cli_args::{AuthCommands, CliArgs, Commands, SecretsCommands, ToolConfig, ToolPolicyState};
use embed::embed_pipe;
use provider::{
    apply_provider_config, get_api_key, get_provider_info, get_provider_registry, split_provider,
};
use secret_store::SecretStore;
use tracing::setup_logging;
use utils::{ToolLoadingStats, get_provider_api_key, parse_tool_names};

fn load_tool_config() -> Result<ToolConfig, Box<dyn std::error::Error>> {
    match querymt_utils::providers::find_config_in_home(&["tools-policy.toml"]) {
        Ok(cfg_file) => {
            let content = fs::read_to_string(cfg_file)?;
            // TODO: Generalize to use `yaml`, `json` and `jsonc`
            let config: ToolConfig = toml::from_str(&content)?;
            Ok(config)
        }
        Err(_) => {
            // Default config if file not found
            Ok(ToolConfig {
                default: Some(ToolPolicyState::Ask),
                tools: None,
            })
        }
    }
}

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

    if let Some(Commands::Auth { command }) = &args.command {
        match command {
            AuthCommands::Login { provider, mode } => {
                let oauth_provider =
                    match querymt_utils::oauth::get_oauth_provider(provider, Some(mode)) {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("{} {}", "Error:".bright_red(), e);
                            return Ok(());
                        }
                    };

                let mut store = SecretStore::new()?;
                let console_ui = auth::ConsoleOAuthUI;
                querymt_utils::oauth::authenticate(
                    oauth_provider.as_ref(),
                    &mut store,
                    &console_ui,
                )
                .await?;
                return Ok(());
            }
            AuthCommands::Logout { provider } => {
                // Verify the provider supports OAuth
                if querymt_utils::oauth::get_oauth_provider(provider, None).is_err() {
                    eprintln!(
                        "{} Provider '{}' does not support OAuth",
                        "Error:".bright_red(),
                        provider
                    );
                    return Ok(());
                }

                let mut store = SecretStore::new()?;
                store.delete_oauth_tokens(provider.as_str())?;
                println!(
                    "{} Logged out from {}",
                    "✓".bright_green(),
                    provider.bright_cyan()
                );
                return Ok(());
            }
            AuthCommands::Status {
                provider,
                no_refresh,
            } => {
                let mut store = SecretStore::new()?;
                let console_ui = auth::ConsoleOAuthUI;
                querymt_utils::oauth::show_auth_status(
                    &mut store,
                    provider.as_deref(),
                    !no_refresh,
                    &console_ui,
                )
                .await?;
                return Ok(());
            }
        }
    }

    match querymt::providers::update_providers_if_stale().await {
        Ok(true) => {
            log::info!("Providers - downloaded and cached new data");
        }
        Ok(false) => {
            log::info!("Providers - using existing cached data");
        }
        Err(e) => {
            log::error!("Failed to update providers data: {}", e);
        }
    }

    let registry = get_provider_registry(&args).await?;
    let tool_config = load_tool_config()?;

    if let Some(cmd) = &args.command {
        match cmd {
            Commands::Secrets { command } => match command {
                SecretsCommands::Set { key, value } => {
                    let mut store = SecretStore::new()?;
                    store.set(key, value)?;
                    println!("{} Secret '{}' has been set.", "✓".bright_green(), key);
                    return Ok(());
                }
                SecretsCommands::Get { key } => {
                    let store = SecretStore::new()?;
                    match store.get(key) {
                        Some(val) => println!("{}: {}", key, val),
                        None => println!("{} Secret '{}' not found", "!".bright_yellow(), key),
                    }
                    return Ok(());
                }
                SecretsCommands::Delete { key } => {
                    let mut store = SecretStore::new()?;
                    store.delete(key)?;
                    println!("{} Secret '{}' has been deleted.", "✓".bright_green(), key);
                    return Ok(());
                }
            },
            Commands::Providers => {
                // Load all plugins since we need to list all providers
                registry.load_all_plugins().await;
                for factory in registry.list() {
                    println!("- {}", factory.name());
                }
                return Ok(());
            }
            Commands::Models => {
                // Load all plugins since we need to list models from all providers
                registry.load_all_plugins().await;
                for factory in registry.list() {
                    print!("{}: ", factory.name());
                    let cfg = match factory.as_http() {
                        Some(http_factory) => {
                            if let Some(api_key) =
                                get_provider_api_key(factory.name(), http_factory)
                            {
                                serde_json::json!({"api_key": api_key})
                            } else {
                                serde_json::json!({})
                            }
                        }
                        None => serde_json::json!({}),
                    };
                    let cfg_str = serde_json::to_string(&cfg)?;
                    let models = factory.list_models(&cfg_str).await;

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
                builder = apply_provider_config(builder, &registry, &prov_name)?;
                if let Some(m) = opt_model {
                    builder = builder.model(m);
                }
                if let Some(key) = get_api_key(&prov_name, &args, &registry).await {
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

                let provider = builder.build(&registry).await?;
                let embeddings = embed_pipe(&provider, text.as_ref(), separator.as_ref()).await?;

                // pretty-print as JSON
                println!("{}", serde_json::to_string_pretty(&embeddings)?);
                return Ok(());
            }
            Commands::Update => {
                println!("{}", "Updating providers config...".bright_blue());
                match querymt_utils::providers::fetch_providers_repository(None).await {
                    Ok(content) => {
                        let cfg_path =
                            querymt_utils::providers::config_dir()?.join("providers.json");
                        std::fs::write(cfg_path.clone(), content)?;
                        println!(
                            "{} {}",
                            "✓".bright_green(),
                            format!("Providers config updated at {}", cfg_path.display())
                        );
                    }
                    Err(e) => {
                        eprintln!("{} {}", "Error:".bright_red(), e);
                    }
                }

                println!("{}", "Updating OCI provider plugins...".bright_blue());
                for provider_cfg in &registry.config.providers {
                    if provider_cfg.path.starts_with("oci://") {
                        let name = provider_cfg.name.clone();
                        let mut spinner =
                            Spinner::new(Spinners::Dots, format!("Updating {}...", name.bold()));

                        let image_reference = provider_cfg
                            .path
                            .strip_prefix("oci://")
                            .unwrap()
                            .to_string();

                        let name_for_cb = name.clone();
                        let progress: OciProgressCallback =
                            Arc::new(move |p: OciDownloadProgress| match &p.phase {
                                OciDownloadPhase::Downloading => {
                                    if let Some(pct) = p.percent {
                                        eprint!(
                                            "\r  {} Downloading... {:.1}%   ",
                                            name_for_cb, pct
                                        );
                                    }
                                }
                                OciDownloadPhase::Extracting => {
                                    eprint!("\r  {} Extracting...          ", name_for_cb);
                                }
                                OciDownloadPhase::Persisting => {
                                    eprint!("\r  {} Persisting...          ", name_for_cb);
                                }
                                _ => {}
                            });

                        match registry
                            .oci_downloader
                            .pull_and_extract(
                                &image_reference,
                                None,
                                &registry.cache_path,
                                true,
                                Some(progress),
                            )
                            .await
                        {
                            Ok(_) => {
                                eprint!("\r");
                                spinner.stop_and_persist(
                                    "🚀",
                                    format!("{} has been updated.", name.bold()),
                                );
                            }
                            Err(e) => {
                                eprint!("\r");
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
            Commands::Auth { command: _ } => {
                unreachable!("Auth commands are handled before plugin loading")
            }
            // This command is handled before the match statement
            Commands::Completion { .. } => unreachable!(),
        }
    }

    // Build provider + LLMBuilder
    let (prov_name, opt_model) = resolve_provider_and_model(&args, None, None)?;
    let mut builder = LLMBuilder::new().provider(prov_name.clone());
    builder = apply_provider_config(builder, &registry, &prov_name)?;
    if let Some(m) = opt_model.or(args.model.clone()) {
        builder = builder.model(m);
    }
    for sys in &args.system {
        builder = builder.system(sys.clone());
    }
    if let Some(key) = get_api_key(&prov_name, &args, &registry).await {
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

    // Validate provider-specific requirements
    if prov_name == "llama_cpp" {
        let has_model = args.options.iter().any(|(k, _)| k == "model") || args.model.is_some();
        if !has_model {
            eprintln!(
                "{} The llama_cpp provider requires 'model' to be specified.",
                "Error:".bright_red()
            );
            eprintln!("\n{}", "Usage examples:".bright_cyan());
            eprintln!("  {}", "Local file:".bright_yellow());
            eprintln!("    qmt -p llama_cpp -o model=\"/path/to/model.gguf\"");
            eprintln!("\n  {}", "HuggingFace model:".bright_yellow());
            eprintln!(
                "    qmt -p llama_cpp -o model=\"Qwen/Qwen3-VL-8B-Instruct-GGUF:Qwen3VL-8B-Instruct-Q4_K_M.gguf\""
            );
            eprintln!(
                "\n{}",
                "Or configure in ~/.qmt/providers.toml:".bright_cyan()
            );
            eprintln!("    [[providers]]");
            eprintln!("    name = \"llama_cpp\"");
            eprintln!("    path = \"target/debug/libqmt_llama_cpp.dylib\"");
            eprintln!("    [providers.config]");
            eprintln!("    model = \"/path/to/model.gguf\"");
            return Err("Missing required parameter 'model' for llama_cpp provider".into());
        }
    }

    // MCP tools injection
    let mut tool_stats = ToolLoadingStats::new();
    let mcp_clients;
    if let Some(cfg) = &args.mcp_config {
        let cfg = MCPConfig::load(cfg.clone()).await?;
        mcp_clients = cfg.create_mcp_clients().await?;
        for (server_name, client) in mcp_clients.iter() {
            tool_stats.increment_server();
            let server = client.peer();
            let tools = server.list_all_tools().await?;
            for t in tools {
                let Some((effective_server, effective_tool)) =
                    parse_tool_names(server_name, &t.name)
                else {
                    tool_stats.increment_tool(false);
                    log::warn!(
                        "Invalid tool name format for server {}: {}",
                        server_name,
                        t.name
                    );
                    continue;
                };

                let state = tool_config
                    .tools
                    .as_ref()
                    .and_then(|tools| tools.get(effective_server))
                    .and_then(|server_tools| server_tools.get(effective_tool))
                    .or(tool_config.default.as_ref())
                    .unwrap_or(&ToolPolicyState::Ask);

                if *state == ToolPolicyState::Deny {
                    tool_stats.increment_tool(false);
                    log::debug!(
                        "Skipping denied tool: {}::{}",
                        effective_server,
                        effective_tool
                    );
                    continue;
                }

                tool_stats.increment_tool(true);
                if let Ok(adapter) = McpToolAdapter::try_new(t, server.clone(), server_name.clone())
                {
                    builder = builder.add_tool(adapter);
                }
            }
        }
    }
    tool_stats.log_summary();
    let provider = builder.build(&registry).await?;
    let is_pipe = !io::stdin().is_terminal();

    if is_pipe || args.prompt.is_some() {
        return chat_pipe(&provider, args.prompt.as_ref(), &tool_config).await;
    }

    interactive_loop(&provider, &prov_name, &tool_config).await
}
